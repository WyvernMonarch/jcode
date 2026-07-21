//! Worktree-isolated task execution: baseline capture, synthetic-tree delta, apply-back.
//! Ported from omp `task/worktree.ts`. See OMP_MERGE_PLAN.md "Worktree delta spec".
//!
//! The point of the synthetic-tree delta: a parallel task runs in a `git worktree` that is
//! seeded to match the parent checkout *exactly*, dirty state and all. When the task finishes
//! we diff two synthetic trees — one reconstructed from the captured baseline, one from the
//! isolation's current state — so the parent's pre-existing dirty state cancels out and only
//! the task's own edits survive in the delta. Nothing here touches the parent repo's index or
//! working tree except [`apply_back`].

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Errors from worktree operations. [`WorktreeError::Conflict`] is only produced by
/// [`apply_back`] when `git apply --check` rejects the delta; it carries git's stderr verbatim.
#[derive(Debug)]
pub enum WorktreeError {
    /// `git apply` refused the delta against the parent checkout. Carries git's stderr.
    Conflict(String),
    /// Any other failure (git invocation, io, non-success exit).
    Other(anyhow::Error),
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorktreeError::Conflict(s) => write!(f, "apply_back conflict:\n{s}"),
            WorktreeError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WorktreeError {}

impl From<anyhow::Error> for WorktreeError {
    fn from(e: anyhow::Error) -> Self {
        WorktreeError::Other(e)
    }
}

impl From<std::io::Error> for WorktreeError {
    fn from(e: std::io::Error) -> Self {
        WorktreeError::Other(e.into())
    }
}

pub type Result<T> = std::result::Result<T, WorktreeError>;

/// The parent checkout's state at spawn time: HEAD plus the three patches that reconstruct its
/// full dirty working tree (staged, unstaged, and untracked-as-new-file patches).
#[derive(Debug, Clone)]
pub struct Baseline {
    /// Full HEAD sha at capture time.
    pub head: String,
    /// `git diff --cached --binary` (HEAD → index).
    pub staged: Vec<u8>,
    /// `git diff --binary` (index → working tree).
    pub unstaged: Vec<u8>,
    /// Concatenated `git diff --no-index --binary /dev/null <file>` for each non-ignored
    /// untracked file, so they can be replayed as new-file additions.
    pub untracked: Vec<u8>,
}

impl Baseline {
    /// Capture the parent checkout's HEAD and dirty state. Read-only: never mutates the repo.
    pub fn capture(repo_root: &Path) -> Result<Baseline> {
        let head = git_stdout(repo_root, &["rev-parse", "HEAD"], None)?;
        let head = String::from_utf8_lossy(&head).trim().to_string();
        if head.is_empty() {
            return Err(
                anyhow::anyhow!("git rev-parse HEAD returned empty (unborn branch?)").into()
            );
        }

        let staged = git_stdout(repo_root, &["diff", "--cached", "--binary"], None)?;
        let unstaged = git_stdout(repo_root, &["diff", "--binary"], None)?;

        // Untracked (non-ignored) files: `-uall` expands directories to individual files;
        // ignored files are excluded by default (that's the .gitignore respect the spec wants).
        let status = git_stdout(repo_root, &["status", "--porcelain", "-uall"], None)?;
        let status = String::from_utf8_lossy(&status);
        let mut untracked = Vec::new();
        for line in status.lines() {
            // Untracked entries are "?? <path>".
            if let Some(path) = line.strip_prefix("?? ") {
                let path = dequote_porcelain(path);
                // `git diff --no-index` exits 1 when files differ (always true vs /dev/null) —
                // that is success for our purposes, not an error.
                let patch = git_stdout_allow(
                    repo_root,
                    &["diff", "--no-index", "--binary", "/dev/null", &path],
                    None,
                    &[0, 1],
                )?;
                untracked.extend_from_slice(&patch);
            }
        }

        Ok(Baseline { head, staged, unstaged, untracked })
    }
}

/// An isolated `git worktree` seeded to match the parent checkout exactly (including dirty
/// state). Task edits happen under [`Isolation::path`]; [`Isolation::delta`] extracts only the
/// task's own changes; [`Isolation::cleanup`] tears it down.
#[derive(Debug)]
pub struct Isolation {
    /// The isolated working tree the task edits.
    pub path: PathBuf,
    /// Temp parent directory holding the worktree (removed on cleanup).
    parent_tmp: PathBuf,
    /// Main repo, for worktree admin (`git worktree add/remove/prune`).
    repo_root: PathBuf,
    baseline: Baseline,
}

impl Isolation {
    /// Create an isolated worktree on the baseline HEAD, then replay the baseline patches so the
    /// isolation matches the parent's full working tree (staged + unstaged + untracked).
    pub fn create(repo_root: &Path, task_id: &str) -> Result<Isolation> {
        let baseline = Baseline::capture(repo_root)?;

        let parent_tmp = std::env::temp_dir().join(format!(
            "jcode-wt-{}-{}-{}",
            sanitize(task_id),
            std::process::id(),
            unique_suffix(),
        ));
        std::fs::create_dir_all(&parent_tmp)?;
        let path = parent_tmp.join("tree");

        // Detached so we never create (and later have to clean up) a branch.
        git_ok(
            repo_root,
            &["worktree", "add", "--detach", "-q", path_str(&path)?, &baseline.head],
            None,
        )?;

        // Replay the parent's dirty state into the isolation's working tree. Applied in order,
        // each patch's context matches the tree the previous one produced (HEAD → index →
        // working, then the new untracked files).
        for patch in [&baseline.staged, &baseline.unstaged, &baseline.untracked] {
            if !patch.is_empty() {
                git_ok(&path, &["apply", "--binary"], Some(patch)).map_err(|e| {
                    // Seeding failed — don't leave a half-built worktree lying around.
                    let _ = remove_worktree(repo_root, &path, &parent_tmp);
                    e
                })?;
            }
        }

        Ok(Isolation { path, parent_tmp, repo_root: repo_root.to_path_buf(), baseline })
    }

    /// Synthetic-tree diff: reconstruct the baseline tree (HEAD + baseline patches) and the
    /// isolation's current tree in two throwaway indexes, then diff them. Pre-existing dirty
    /// state is present in *both* trees and cancels out, leaving only the task's edits.
    pub fn delta(&self) -> Result<Vec<u8>> {
        // tree_base: HEAD + the three baseline patches, staged into a temp index.
        let idx_base = self.parent_tmp.join(format!("idx-base-{}", unique_suffix()));
        let env_base = [("GIT_INDEX_FILE", idx_base.as_os_str())];
        git_ok_env(&self.path, &["read-tree", &self.baseline.head], None, &env_base)?;
        for patch in [&self.baseline.staged, &self.baseline.unstaged, &self.baseline.untracked] {
            if !patch.is_empty() {
                git_ok_env(
                    &self.path,
                    &["apply", "--cached", "--binary"],
                    Some(patch),
                    &env_base,
                )?;
            }
        }
        let tree_base = git_stdout_env(&self.path, &["write-tree"], None, &env_base)?;
        let tree_base = String::from_utf8_lossy(&tree_base).trim().to_string();

        // tree_now: everything currently in the isolation's working tree (respects .gitignore,
        // same as the untracked capture, so the two trees are symmetric).
        let idx_now = self.parent_tmp.join(format!("idx-now-{}", unique_suffix()));
        let env_now = [("GIT_INDEX_FILE", idx_now.as_os_str())];
        git_ok_env(&self.path, &["add", "-A"], None, &env_now)?;
        let tree_now = git_stdout_env(&self.path, &["write-tree"], None, &env_now)?;
        let tree_now = String::from_utf8_lossy(&tree_now).trim().to_string();

        let delta = git_stdout(
            &self.path,
            &["diff-tree", "-p", "--binary", &tree_base, &tree_now],
            None,
        )?;

        let _ = std::fs::remove_file(&idx_base);
        let _ = std::fs::remove_file(&idx_now);
        Ok(delta)
    }

    /// Tear down the worktree. Safe to call more than once (best-effort, idempotent).
    pub fn cleanup(&self) -> Result<()> {
        remove_worktree(&self.repo_root, &self.path, &self.parent_tmp)
    }
}

/// Apply a task delta back onto the parent checkout. Runs `git apply --check` first; on failure
/// returns [`WorktreeError::Conflict`] carrying git's stderr and touches nothing (no force, no
/// 3-way). This is the *only* function that mutates the parent working tree.
pub fn apply_back(repo_root: &Path, delta: &[u8]) -> Result<()> {
    if delta.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(()); // empty delta: nothing to land
    }
    // Dry-run first so a conflict leaves the parent untouched.
    let check = run_git(repo_root, &["apply", "--binary", "--check"], Some(delta), &[])?;
    if !check.status.success() {
        return Err(WorktreeError::Conflict(String::from_utf8_lossy(&check.stderr).into_owned()));
    }
    let apply = run_git(repo_root, &["apply", "--binary"], Some(delta), &[])?;
    if !apply.status.success() {
        return Err(WorktreeError::Conflict(String::from_utf8_lossy(&apply.stderr).into_owned()));
    }
    Ok(())
}

// ---- git plumbing helpers ------------------------------------------------------------------

fn remove_worktree(repo_root: &Path, path: &Path, parent_tmp: &Path) -> Result<()> {
    // Best-effort: a second call (worktree already gone) must not error.
    if let Ok(p) = path_str(path) {
        let _ = run_git(repo_root, &["worktree", "remove", "--force", p], None, &[]);
    }
    let _ = run_git(repo_root, &["worktree", "prune"], None, &[]);
    let _ = std::fs::remove_dir_all(parent_tmp);
    Ok(())
}

/// Run git, returning the full [`std::process::Output`]. `stdin` (if any) is written fully then
/// the pipe is closed before we read stdout/stderr.
// ponytail: writes all of stdin before draining stdout — fine for patch-sized inputs (< pipe
// buffer). If multi-MB binary deltas ever deadlock, thread the stdin write.
fn run_git(
    dir: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    env: &[(&str, &std::ffi::OsStr)],
) -> Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut child = cmd.spawn().map_err(|e| {
        WorktreeError::Other(anyhow::anyhow!("failed to spawn git {}: {e}", args.join(" ")))
    })?;
    if let Some(input) = stdin {
        let mut si = child.stdin.take().expect("stdin piped");
        si.write_all(input)?;
        // drop si here to close the pipe
    }
    Ok(child.wait_with_output()?)
}

fn ensure_success(args: &[&str], out: std::process::Output) -> Result<Vec<u8>> {
    if !out.status.success() {
        return Err(WorktreeError::Other(anyhow::anyhow!(
            "git {} failed ({}): {}",
            args.join(" "),
            out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            String::from_utf8_lossy(&out.stderr).trim(),
        )));
    }
    Ok(out.stdout)
}

fn git_stdout(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
    let out = run_git(dir, args, stdin, &[])?;
    ensure_success(args, out)
}

fn git_stdout_env(
    dir: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    env: &[(&str, &std::ffi::OsStr)],
) -> Result<Vec<u8>> {
    let out = run_git(dir, args, stdin, env)?;
    ensure_success(args, out)
}

/// Like [`git_stdout`] but treats any exit code in `allowed` as success (for `diff --no-index`,
/// which returns 1 when files differ).
fn git_stdout_allow(
    dir: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    allowed: &[i32],
) -> Result<Vec<u8>> {
    let out = run_git(dir, args, stdin, &[])?;
    match out.status.code() {
        Some(c) if allowed.contains(&c) => Ok(out.stdout),
        _ => Err(WorktreeError::Other(anyhow::anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim(),
        ))),
    }
}

fn git_ok(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<()> {
    git_stdout(dir, args, stdin).map(|_| ())
}

fn git_ok_env(
    dir: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    env: &[(&str, &std::ffi::OsStr)],
) -> Result<()> {
    git_stdout_env(dir, args, stdin, env).map(|_| ())
}

fn path_str(p: &Path) -> Result<&str> {
    p.to_str()
        .ok_or_else(|| WorktreeError::Other(anyhow::anyhow!("non-UTF8 path: {}", p.display())))
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// `git status --porcelain` quotes paths with special chars in a C-style double-quoted form.
/// Untracked paths in our test/real repos are plain; handle the common quoted case minimally.
fn dequote_porcelain(path: &str) -> String {
    if let Some(inner) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
        inner.replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        path.to_string()
    }
}

#[cfg(test)]
#[path = "worktree_tests.rs"]
mod worktree_tests;
