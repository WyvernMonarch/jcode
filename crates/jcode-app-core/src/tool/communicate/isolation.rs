//! Opt-in worktree isolation for spawned swarm members.
//!
//! When a member is spawned with `isolated=true`, we build a `jcode_worktree::Isolation`
//! (a git worktree seeded from the parent checkout, dirty state and all), point the spawned
//! agent's `working_dir` at it, and stash the handle in a process-global registry keyed by the
//! new session id. When the coordinator later calls `swarm action="collect"` for that session,
//! we compute the isolation's delta, `apply_back` it to the parent checkout (conflict-safe: no
//! force, no 3-way), and tear the worktree down.
//!
//! Why tool-side and not the server spawn path: threading `isolated` through
//! `Request::CommSpawn` + the swarm completion machinery would touch the protocol enum and
//! `server/swarm.rs` (shared, out of footprint, and edited concurrently). Doing it in the tool
//! keeps everything to this crate's tool module and needs no protocol change. `collect` is the
//! explicit trigger instead of auto-apply-on-completion for the same reason.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// A spawned member's isolated worktree plus the parent repo root to apply its delta back onto
/// (`Isolation` keeps `repo_root` private, so we carry our own copy for `apply_back`).
pub(super) struct IsolatedSpawn {
    isolation: jcode_worktree::Isolation,
    repo_root: PathBuf,
}

impl IsolatedSpawn {
    /// The isolated working tree the spawned agent should run in.
    pub(super) fn path(&self) -> &Path {
        &self.isolation.path
    }

    /// Tear down the worktree. Best-effort/idempotent (delegates to `Isolation::cleanup`).
    pub(super) fn cleanup(&self) {
        let _ = self.isolation.cleanup();
    }
}

/// Build an isolated worktree for a spawn whose target `working_dir` is inside a git repo.
/// Errors when `working_dir` is not in a git repo or the worktree can't be seeded (e.g. an
/// unborn HEAD); callers fall back to a plain spawn in that case.
pub(super) fn create_isolated_spawn(working_dir: &Path, task_id: &str) -> anyhow::Result<IsolatedSpawn> {
    let repo_root = git_repo_root(working_dir).ok_or_else(|| {
        anyhow::anyhow!("{} is not inside a git repository", working_dir.display())
    })?;
    let isolation = jcode_worktree::Isolation::create(&repo_root, task_id)
        .map_err(|e| anyhow::anyhow!("failed to create isolation worktree: {e}"))?;
    Ok(IsolatedSpawn { isolation, repo_root })
}

/// Compute the member's delta, apply it back to the parent checkout, then always clean up.
/// Returns a human-readable message for the coordinator (success, "no changes", or a conflict
/// report naming the saved patch file). Only a genuine failure (e.g. delta computation) is `Err`.
pub(super) fn collect(spawn: IsolatedSpawn, session_id: &str) -> anyhow::Result<String> {
    let result = collect_inner(&spawn, session_id);
    spawn.cleanup(); // always, on success and conflict paths alike
    result
}

fn collect_inner(spawn: &IsolatedSpawn, session_id: &str) -> anyhow::Result<String> {
    let delta = spawn
        .isolation
        .delta()
        .map_err(|e| anyhow::anyhow!("failed to compute isolation delta for {session_id}: {e}"))?;
    if delta.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(format!("Isolated member {session_id} made no changes; nothing to apply back."));
    }
    match jcode_worktree::apply_back(&spawn.repo_root, &delta) {
        Ok(()) => Ok(format!(
            "Applied isolated member {session_id}'s changes back to {} ({} delta bytes).",
            spawn.repo_root.display(),
            delta.len()
        )),
        Err(jcode_worktree::WorktreeError::Conflict(stderr)) => {
            // cleanup() removes the isolation temp dir, so persist the patch OUTSIDE it (a
            // sibling temp file) or it would be deleted before the coordinator can use it.
            let patch_path = conflict_patch_path(session_id);
            let saved_line = match std::fs::write(&patch_path, &delta) {
                Ok(()) => format!("Delta patch saved to {}.", patch_path.display()),
                Err(e) => format!("Also failed to save the delta patch: {e}."),
            };
            Ok(format!(
                "Isolated member {session_id}'s changes CONFLICT with the parent checkout; nothing was applied.\n\
                 {saved_line}\n\
                 Resolve, then apply manually with `git apply --binary <patch>`.\n\n\
                 git stderr:\n{stderr}"
            ))
        }
        Err(e) => Err(anyhow::anyhow!(
            "failed to apply isolated member {session_id}'s changes back: {e}"
        )),
    }
}

fn conflict_patch_path(session_id: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("jcode-wt-conflict-{}-{}.patch", sanitize(session_id), suffix))
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn git_repo_root(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

// ponytail: process-global registry. Spawn and collect for one coordinator session always run in
// the same process (server-hosted agent or a `jcode run` worker), so the map is consistent. It
// does NOT survive a server reload or cross a process boundary — collect then reports "not
// tracked" and the orphan worktree is left for `git worktree prune`. Persist to disk only if
// cross-reload collection is ever needed.
fn registry() -> &'static Mutex<HashMap<String, IsolatedSpawn>> {
    static REG: OnceLock<Mutex<HashMap<String, IsolatedSpawn>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn register(session_id: &str, spawn: IsolatedSpawn) {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(session_id.to_string(), spawn);
}

/// Remove and return the isolation handle for `session_id`, so `collect` consumes it exactly once.
pub(super) fn take(session_id: &str) -> Option<IsolatedSpawn> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn scratch_repo() -> TempDir {
        let td = TempDir::new().unwrap();
        let p = td.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.email", "t@t"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        fs::write(p.join("a.txt"), "l1\nl2\nl3\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-qm", "init"]);
        td
    }

    #[test]
    fn registry_round_trip() {
        let repo = scratch_repo();
        let spawn = create_isolated_spawn(repo.path(), "reg").unwrap();
        assert!(take("member-x").is_none());
        register("member-x", spawn);
        let got = take("member-x").expect("registered handle");
        assert!(take("member-x").is_none(), "take must consume the handle once");
        got.cleanup();
    }

    #[test]
    fn git_repo_root_detection() {
        let repo = scratch_repo();
        assert_eq!(
            git_repo_root(repo.path()).unwrap().canonicalize().unwrap(),
            repo.path().canonicalize().unwrap()
        );
        let non_repo = TempDir::new().unwrap();
        assert!(git_repo_root(non_repo.path()).is_none());
    }

    #[test]
    fn collect_applies_new_file_back() {
        let repo = scratch_repo();
        let spawn = create_isolated_spawn(repo.path(), "task").unwrap();
        // Member creates a new file inside the isolation.
        fs::write(spawn.path().join("new.txt"), "made by member\n").unwrap();
        let msg = collect(spawn, "member-1").unwrap();
        assert!(msg.contains("Applied"), "unexpected collect message: {msg}");
        assert_eq!(fs::read_to_string(repo.path().join("new.txt")).unwrap(), "made by member\n");
    }

    #[test]
    fn collect_no_changes_is_noop() {
        let repo = scratch_repo();
        let spawn = create_isolated_spawn(repo.path(), "task").unwrap();
        let msg = collect(spawn, "member-2").unwrap();
        assert!(msg.contains("no changes"), "unexpected message: {msg}");
    }

    #[test]
    fn collect_conflict_saves_patch_and_reports() {
        let repo = scratch_repo();
        let spawn = create_isolated_spawn(repo.path(), "task").unwrap();
        // Member rewrites a.txt one way...
        fs::write(spawn.path().join("a.txt"), "MEMBER\nl2\nl3\n").unwrap();
        // ...while the parent diverges on the same lines, so apply_back must conflict.
        fs::write(repo.path().join("a.txt"), "PARENT\nl2\nl3\n").unwrap();
        let msg = collect(spawn, "member-3").unwrap();
        assert!(msg.contains("CONFLICT"), "expected conflict report, got: {msg}");
        // The reported patch path must exist (persisted outside the cleaned-up temp dir).
        let path = msg
            .lines()
            .find_map(|l| l.strip_prefix("Delta patch saved to ").map(|p| p.trim_end_matches('.')))
            .expect("patch path line");
        assert!(Path::new(path).exists(), "patch file {path} should survive cleanup");
        // Parent file untouched by the failed apply.
        assert_eq!(fs::read_to_string(repo.path().join("a.txt")).unwrap(), "PARENT\nl2\nl3\n");
        let _ = fs::remove_file(path);
    }
}
