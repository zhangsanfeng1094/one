use std::sync::Arc;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::tasks::{format_task_output, BackgroundTaskRegistry};

/// Stop a background bash task (Codex `/stop`).
pub struct BashKillTool {
    registry: Arc<BackgroundTaskRegistry>,
}

impl BashKillTool {
    pub fn new(registry: Arc<BackgroundTaskRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for BashKillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash_kill".to_string(),
            description: "Stop a running background bash task by task_id \
(from bash with run_in_background=true). No-op if already finished."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "Background task id to kill"
                    }
                },
                "required": ["task_id"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let task_id = call
            .arguments
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("bash_kill", "missing `task_id`"))?;

        let snap = self
            .registry
            .kill(task_id)
            .await
            .map_err(|err| tool_error("bash_kill", err))?;

        let text = format!(
            "Killed background task\n{}",
            format_task_output(&snap, 8_000)
        );
        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "task_id": snap.id,
                "status": snap.state.as_str(),
                "exitCode": snap.exit_code,
                "ok": true,
            }),
        ))
    }
}
