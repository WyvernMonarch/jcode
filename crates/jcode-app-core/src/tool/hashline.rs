//! Hashline edit tool: line-anchored patches with whole-file content tags.
//! Skeleton — implemented by round-1 agent per OMP_MERGE_PLAN.md.

use anyhow::Result;
use async_trait::async_trait;
use jcode_tool_core::{Tool, ToolContext};
use jcode_tool_types::ToolOutput;
use serde_json::{Value, json};

pub struct HashlineTool;

impl HashlineTool {
    pub fn new() -> Box<dyn Tool> {
        Box::new(Self)
    }
}

#[async_trait]
impl Tool for HashlineTool {
    fn name(&self) -> &str {
        "hashline"
    }

    fn description(&self) -> &str {
        "Read files with line anchors and apply line-anchored hashline patches. (skeleton)"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["read", "apply"] }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, _input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        anyhow::bail!("hashline tool not implemented yet")
    }
}
