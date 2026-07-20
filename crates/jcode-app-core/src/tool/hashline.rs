//! Hashline edit tool: line-anchored patches with whole-file content tags.
//!
//! `action: "read"` returns a `[path#TAG]` header plus `N:content` rows and
//! records a snapshot; `action: "apply"` applies one or more `[path#TAG]` patch
//! sections (with snapshot-based recovery when the file drifted). The patch
//! grammar and tag scheme live in the `jcode-hashline` crate.

use super::{Tool, ToolContext, ToolOutput};
use crate::bus::{Bus, BusEvent, FileOp, FileTouch};
use anyhow::{Result, bail};
use async_trait::async_trait;
use jcode_hashline::{SnapshotStore, apply_section, compute_tag, parse};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::OnceLock;

/// Max `N:content` rows emitted by a single `read` (mirrors the `read` tool cap).
const MAX_READ_ROWS: usize = 5000;

/// Process-global snapshot store shared across all `hashline` tool calls, so a
/// `read` in one turn can back a stale-tag recovery in a later `apply`.
fn store() -> &'static SnapshotStore {
    static STORE: OnceLock<SnapshotStore> = OnceLock::new();
    STORE.get_or_init(SnapshotStore::new)
}

pub struct HashlineTool;

impl HashlineTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct HashlineInput {
    action: String,
    #[serde(default, alias = "file_path")]
    path: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    patch: Option<String>,
}

#[async_trait]
impl Tool for HashlineTool {
    fn name(&self) -> &str {
        "hashline"
    }

    fn description(&self) -> &str {
        HASHLINE_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["read", "apply"],
                    "description": "\"read\" a file (emits a hashed, line-numbered view) or \"apply\" a hashline patch."
                },
                "path": {
                    "type": "string",
                    "description": "For action=read: the file to read."
                },
                "offset": {
                    "type": "integer",
                    "description": "read: 0-based line offset to start from. Default 0."
                },
                "limit": {
                    "type": "integer",
                    "description": "read: max lines to return. Default and cap 5000."
                },
                "patch": {
                    "type": "string",
                    "description": "For action=apply: the hashline patch text (one or more [path#TAG] sections)."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: HashlineInput = serde_json::from_value(input)?;
        match params.action.as_str() {
            "read" => read(&params, &ctx).await,
            "apply" => apply(&params, &ctx).await,
            other => bail!("hashline: unknown action `{other}` (expected \"read\" or \"apply\")"),
        }
    }
}

async fn read(params: &HashlineInput, ctx: &ToolContext) -> Result<ToolOutput> {
    let given = params
        .path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("hashline read: `path` is required"))?;
    let resolved = ctx.resolve_path(Path::new(given));
    if !resolved.exists() {
        bail!("hashline read: file not found: {given}");
    }
    let content = tokio::fs::read_to_string(&resolved).await?;
    let tag = compute_tag(&content);

    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(MAX_READ_ROWS).min(MAX_READ_ROWS);
    let end_excl = offset.saturating_add(limit);

    let mut out = format!("[{given}#{tag}]\n");
    let mut total = 0usize;
    for (i, line) in content.lines().enumerate() {
        total = i + 1;
        if i < offset || i >= end_excl {
            continue;
        }
        out.push_str(&format!("{}:{}\n", i + 1, line));
    }
    if end_excl < total {
        out.push_str(&format!(
            "... {} more lines (use offset={} to continue)\n",
            total - end_excl,
            end_excl
        ));
    }

    // Record a whole-file snapshot for later stale-tag recovery.
    store().record(&resolved.to_string_lossy(), &content);

    Bus::global().publish(BusEvent::FileTouch(FileTouch {
        session_id: ctx.session_id.clone(),
        path: resolved.clone(),
        op: FileOp::Read,
        intent: None,
        summary: Some(format!("hashline read {given} (#{tag}, {total} lines)")),
        detail: None,
    }));

    Ok(ToolOutput::new(out).with_title(format!("hashline read {given}")))
}

async fn apply(params: &HashlineInput, ctx: &ToolContext) -> Result<ToolOutput> {
    let patch = params
        .patch
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("hashline apply: `patch` is required"))?;
    let sections = parse(patch)?;

    // Validate + compute every section before touching disk (all-or-nothing).
    struct Pending {
        given: String,
        resolved: std::path::PathBuf,
        content: String,
        op_count: usize,
        new_tag: String,
        old_tag: String,
        recovered: bool,
    }
    let mut pending: Vec<Pending> = Vec::with_capacity(sections.len());
    for section in &sections {
        let resolved = ctx.resolve_path(Path::new(&section.path));
        if !resolved.exists() {
            bail!("hashline apply: file not found: {}", section.path);
        }
        let live = tokio::fs::read_to_string(&resolved).await?;
        let key = resolved.to_string_lossy().to_string();
        let snapshot = store().lookup(&key, &section.tag);
        let applied = apply_section(section, &live, snapshot.as_deref())?;
        pending.push(Pending {
            given: section.path.clone(),
            new_tag: compute_tag(&applied.content),
            old_tag: compute_tag(&live),
            recovered: applied.recovered,
            op_count: section.ops.len(),
            resolved,
            content: applied.content,
        });
    }

    // Commit: write, refresh snapshot, announce.
    let mut summary = String::new();
    for p in &pending {
        tokio::fs::write(&p.resolved, &p.content).await?;
        store().record(&p.resolved.to_string_lossy(), &p.content);

        Bus::global().publish(BusEvent::FileTouch(FileTouch {
            session_id: ctx.session_id.clone(),
            path: p.resolved.clone(),
            op: FileOp::Edit,
            intent: None,
            summary: Some(format!(
                "hashline apply {} ({} ops → #{})",
                p.given, p.op_count, p.new_tag
            )),
            detail: None,
        }));

        summary.push_str(&format!(
            "{}: applied {} op{} → #{}",
            p.given,
            p.op_count,
            if p.op_count == 1 { "" } else { "s" },
            p.new_tag
        ));
        if p.recovered {
            summary.push_str(&format!(
                " (recovered from stale tag #{} via snapshot)",
                p.old_tag
            ));
        }
        summary.push('\n');
    }

    let title = pending
        .first()
        .map(|p| format!("hashline apply {}", p.given))
        .unwrap_or_else(|| "hashline apply".to_string());
    Ok(ToolOutput::new(summary).with_title(title))
}

#[cfg(test)]
#[path = "hashline_tests.rs"]
mod tests;

const HASHLINE_DESCRIPTION: &str = "\
Line-anchored file editing with cheap whole-file validation. Prefer this over string-replace edits.

Two actions:

action=read {path, offset?, limit?}
  Returns a header line `[path#TAG]` followed by `N:content` rows (1-indexed original line numbers, content untruncated). TAG is a 4-hex-digit content hash of the whole file. Read a file this way, then build a patch whose section header repeats that exact `[path#TAG]`.

action=apply {patch}
  Applies one or more file sections. Each section:
    [relative/path#TAG]
    <op>
    +body line
    +body line
  Line numbers are 1-indexed against the ORIGINAL file; every op in a section anchors to those original coordinates, so you do NOT re-number after earlier ops. Ops may not overlap. Body rows are each prefixed with `+`; there are no `-` or context rows.

Ops:
  SWAP A.=B:        replace original lines A..=B with the +body
  SWAP.BLK A:       replace the indentation block starting at line A with the +body
  DEL A.=B          delete original lines A..=B
  DEL A             delete original line A
  DEL.BLK A         delete the indentation block starting at line A
  INS.PRE A:        insert +body before line A
  INS.POST A:       insert +body after line A
  INS.HEAD:         insert +body at the top of the file
  INS.TAIL:         insert +body at the end of the file
  INS.BLK.POST A:   insert +body after the indentation block starting at line A

If TAG no longer matches the file, apply recovers automatically when you read the file earlier (replaying your edits onto the current content); otherwise it errors and you must `read` again and rebuild the patch.

Example — given read output:
  [src/lib.rs#3F2A]
  1:fn add(a: i32, b: i32) -> i32 {
  2:    a - b
  3:}
patch to fix the bug and add a doc line:
  [src/lib.rs#3F2A]
  SWAP 2.=2:
  +    a + b
  INS.HEAD:
  +// math helpers
";
