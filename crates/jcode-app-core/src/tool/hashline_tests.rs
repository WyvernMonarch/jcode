use super::HashlineTool;
use crate::tool::{Tool, ToolContext, ToolExecutionMode};
use jcode_hashline::compute_tag;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;

fn test_ctx(root: &Path) -> ToolContext {
    ToolContext {
        session_id: "test".to_string(),
        message_id: "test".to_string(),
        tool_call_id: "test".to_string(),
        working_dir: Some(root.to_path_buf()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    }
}

fn ten_lines() -> String {
    (1..=10).map(|i| format!("line{i}\n")).collect()
}

#[tokio::test]
async fn read_emits_header_and_records_snapshot() {
    let dir = TempDir::new().unwrap();
    let content = "alpha\nbeta\ngamma\n";
    std::fs::write(dir.path().join("f.rs"), content).unwrap();
    let tag = compute_tag(content);

    let out = HashlineTool::new()
        .execute(json!({"action": "read", "path": "f.rs"}), test_ctx(dir.path()))
        .await
        .unwrap();

    assert!(out.output.starts_with(&format!("[f.rs#{tag}]\n")), "header: {}", out.output);
    assert!(out.output.contains("1:alpha\n"));
    assert!(out.output.contains("3:gamma\n"));
}

#[tokio::test]
async fn read_then_apply_round_trip() {
    let dir = TempDir::new().unwrap();
    let content = "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n";
    std::fs::write(dir.path().join("f.rs"), content).unwrap();
    let tag = compute_tag(content);

    let patch = format!("[f.rs#{tag}]\nSWAP 2.=2:\n+    a + b\nINS.HEAD:\n+// math\n");
    let out = HashlineTool::new()
        .execute(json!({"action": "apply", "patch": patch}), test_ctx(dir.path()))
        .await
        .unwrap();

    assert!(out.output.contains("applied 2 ops"), "summary: {}", out.output);
    let on_disk = std::fs::read_to_string(dir.path().join("f.rs")).unwrap();
    assert_eq!(on_disk, "// math\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
}

#[tokio::test]
async fn apply_recovers_after_out_of_band_edit() {
    let dir = TempDir::new().unwrap();
    let snap = ten_lines();
    std::fs::write(dir.path().join("f.rs"), &snap).unwrap();
    let snap_tag = compute_tag(&snap);

    // Read records the snapshot.
    HashlineTool::new()
        .execute(json!({"action": "read", "path": "f.rs"}), test_ctx(dir.path()))
        .await
        .unwrap();

    // File drifts out of band, far from the edit target (line 9).
    std::fs::write(dir.path().join("f.rs"), snap.replace("line9\n", "line9-oob\n")).unwrap();

    // Patch is anchored to the stale snapshot tag.
    let patch = format!("[f.rs#{snap_tag}]\nSWAP 5.=5:\n+LINE5-EDIT\n");
    let out = HashlineTool::new()
        .execute(json!({"action": "apply", "patch": patch}), test_ctx(dir.path()))
        .await
        .unwrap();

    assert!(out.output.contains("recovered from stale tag"), "summary: {}", out.output);
    let on_disk = std::fs::read_to_string(dir.path().join("f.rs")).unwrap();
    assert!(on_disk.contains("LINE5-EDIT\n"), "edit applied: {on_disk}");
    assert!(on_disk.contains("line9-oob\n"), "unrelated drift preserved: {on_disk}");
}

#[tokio::test]
async fn apply_stale_without_snapshot_errors() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("never_read.rs"), "real content\nsecond\n").unwrap();

    // A tag that does not match the file, and the file was never `read` in this
    // process, so no snapshot exists.
    let patch = "[never_read.rs#0000]\nDEL 1\n";
    let err = HashlineTool::new()
        .execute(json!({"action": "apply", "patch": patch}), test_ctx(dir.path()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Re-read"), "{err}");
}

#[tokio::test]
async fn apply_multi_file_patch() {
    let dir = TempDir::new().unwrap();
    let a = "a1\na2\n";
    let b = "b1\nb2\n";
    std::fs::write(dir.path().join("a.rs"), a).unwrap();
    std::fs::write(dir.path().join("b.rs"), b).unwrap();

    let patch = format!(
        "[a.rs#{}]\nDEL 1\n[b.rs#{}]\nINS.TAIL:\n+b3\n",
        compute_tag(a),
        compute_tag(b)
    );
    let out = HashlineTool::new()
        .execute(json!({"action": "apply", "patch": patch}), test_ctx(dir.path()))
        .await
        .unwrap();

    assert!(out.output.contains("a.rs: applied 1 op"), "{}", out.output);
    assert!(out.output.contains("b.rs: applied 1 op"), "{}", out.output);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "a2\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("b.rs")).unwrap(), "b1\nb2\nb3\n");
}
