use super::*;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

// ---- scratch-repo helpers (never touch the real jcode repo) --------------------------------

/// Run git in `dir`, asserting success; returns stdout as a String.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, content).unwrap();
}

fn read(dir: &Path, rel: &str) -> String {
    fs::read_to_string(dir.join(rel)).unwrap()
}

/// A fresh git repo with one committed file `a.txt`. Identity is set locally so commits work
/// regardless of the machine's global git config.
fn scratch_repo() -> TempDir {
    let td = TempDir::new().unwrap();
    let p = td.path();
    git(p, &["init", "-q", "-b", "main"]);
    git(p, &["config", "user.email", "t@t"]);
    git(p, &["config", "user.name", "t"]);
    git(p, &["config", "commit.gpgsign", "false"]);
    write(p, "a.txt", "l1\nl2\nl3\n");
    git(p, &["add", "-A"]);
    git(p, &["commit", "-qm", "init"]);
    td
}

// ---- tests ---------------------------------------------------------------------------------

#[test]
fn clean_repo_round_trip() {
    let repo = scratch_repo();
    let root = repo.path();

    let iso = Isolation::create(root, "clean").unwrap();
    // Task edit inside the isolation.
    write(&iso.path, "a.txt", "l1\nl2\nl3\nl4-task\n");

    let delta = iso.delta().unwrap();
    let text = String::from_utf8_lossy(&delta);
    assert!(text.contains("a.txt"), "delta should mention a.txt:\n{text}");
    assert!(text.contains("+l4-task"), "delta should contain the added line:\n{text}");

    apply_back(root, &delta).unwrap();
    assert_eq!(read(root, "a.txt"), "l1\nl2\nl3\nl4-task\n");

    iso.cleanup().unwrap();
}

#[test]
fn dirty_parent_state_cancels_out() {
    let repo = scratch_repo();
    let root = repo.path();

    // Add a second tracked file so we have something unrelated to dirty.
    write(root, "other.txt", "keep\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "add other"]);

    // Parent gets uncommitted, unrelated changes BEFORE spawn: an unstaged edit + untracked file.
    write(root, "other.txt", "keep\nDIRTY-UNSTAGED\n");
    write(root, "junk.txt", "untracked-in-parent\n");

    let iso = Isolation::create(root, "dirty").unwrap();
    // The isolation must have inherited the dirty state exactly.
    assert_eq!(read(&iso.path, "other.txt"), "keep\nDIRTY-UNSTAGED\n");
    assert_eq!(read(&iso.path, "junk.txt"), "untracked-in-parent\n");

    // Task edits a DIFFERENT file.
    write(&iso.path, "a.txt", "l1\nl2\nl3\nTASK\n");

    let delta = iso.delta().unwrap();
    let text = String::from_utf8_lossy(&delta);
    assert!(text.contains("+TASK"), "delta must contain the task edit:\n{text}");
    // Pre-existing dirty state must NOT leak into the delta.
    assert!(!text.contains("other.txt"), "delta must not contain unrelated dirty file:\n{text}");
    assert!(!text.contains("DIRTY-UNSTAGED"), "delta must not contain parent's dirty edit:\n{text}");
    assert!(!text.contains("junk.txt"), "delta must not contain parent's untracked file:\n{text}");

    // Apply-back lands the task edit onto the still-dirty parent.
    apply_back(root, &delta).unwrap();
    assert_eq!(read(root, "a.txt"), "l1\nl2\nl3\nTASK\n");
    // Parent's own dirty state is preserved (apply_back only touched a.txt).
    assert_eq!(read(root, "other.txt"), "keep\nDIRTY-UNSTAGED\n");

    iso.cleanup().unwrap();
}

#[test]
fn conflict_is_reported_not_forced() {
    let repo = scratch_repo();
    let root = repo.path();

    let iso = Isolation::create(root, "conflict").unwrap();
    // Task changes line 2 in the isolation.
    write(&iso.path, "a.txt", "l1\nTASK-L2\nl3\n");
    let delta = iso.delta().unwrap();

    // Parent changes the SAME line differently after spawn.
    write(root, "a.txt", "l1\nPARENT-L2\nl3\n");

    let err = apply_back(root, &delta).unwrap_err();
    match err {
        WorktreeError::Conflict(stderr) => {
            assert!(!stderr.trim().is_empty(), "conflict must carry git stderr");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    // Parent working tree must be untouched by the failed (dry-run-guarded) apply.
    assert_eq!(read(root, "a.txt"), "l1\nPARENT-L2\nl3\n");

    iso.cleanup().unwrap();
}

#[test]
fn untracked_files_created_in_isolation_appear_in_delta() {
    let repo = scratch_repo();
    let root = repo.path();

    let iso = Isolation::create(root, "untracked").unwrap();
    // Brand-new file created by the task.
    write(&iso.path, "created/by_task.txt", "hello from task\n");

    let delta = iso.delta().unwrap();
    let text = String::from_utf8_lossy(&delta);
    assert!(text.contains("created/by_task.txt"), "delta should list the new file:\n{text}");
    assert!(text.contains("new file"), "delta should mark it a new file:\n{text}");
    assert!(text.contains("+hello from task"), "delta should contain its content:\n{text}");

    apply_back(root, &delta).unwrap();
    assert_eq!(read(root, "created/by_task.txt"), "hello from task\n");

    iso.cleanup().unwrap();
}

#[test]
fn cleanup_is_idempotent() {
    let repo = scratch_repo();
    let root = repo.path();

    let iso = Isolation::create(root, "cleanup").unwrap();
    let path = iso.path.clone();
    assert!(path.exists());

    iso.cleanup().unwrap();
    assert!(!path.exists(), "worktree dir should be gone after cleanup");
    // Second call must not error.
    iso.cleanup().unwrap();
    assert!(!path.exists());
}

#[test]
fn baseline_capture_records_head_and_dirty_state() {
    let repo = scratch_repo();
    let root = repo.path();

    // staged change, unstaged change, and an untracked file.
    write(root, "a.txt", "l1\nSTAGED\nl3\n");
    git(root, &["add", "a.txt"]);
    write(root, "a.txt", "l1\nSTAGED\nUNSTAGED\n");
    write(root, "fresh.txt", "brand new\n");

    let base = Baseline::capture(root).unwrap();
    assert_eq!(base.head.len(), 40, "full HEAD sha expected");
    assert!(!base.staged.is_empty(), "staged patch expected");
    assert!(!base.unstaged.is_empty(), "unstaged patch expected");
    let unt = String::from_utf8_lossy(&base.untracked);
    assert!(unt.contains("fresh.txt"), "untracked patch should list fresh.txt:\n{unt}");
    assert!(unt.contains("+brand new"), "untracked patch should contain content:\n{unt}");
}
