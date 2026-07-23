use std::sync::Arc;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::tasks::{format_task_list, format_task_output, BackgroundTaskRegistry, TaskState};

const DEFAULT_MAX_CHARS: usize = 50_000;

/// Poll / wait for background bash tasks (Claude TaskOutput / Codex re-check).
pub struct BashOutputTool {
    registry: Arc<BackgroundTaskRegistry>,
}

impl BashOutputTool {
    pub fn new(registry: Arc<BackgroundTaskRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for BashOutputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash_output".to_string(),
            description: "Get status and output of a background bash task. \
Stdout/stderr are streamed while the task runs — a snapshot includes output \
captured so far (not only after exit). \
Omit task_id to list all background tasks (like Codex /ps). \
Set timeout_secs > 0 to wait up to that many seconds for the task to finish \
before returning a snapshot (0 or omit = return immediately). \
For long-running servers (npm run dev), poll with timeout_secs=0 or a short wait \
to read progress logs; use bash_kill when done."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "Background task id from bash(run_in_background=true). Omit to list all tasks."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Seconds to wait for completion (default 0 = snapshot now)"
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Max characters of combined stdout/stderr to return (default 50000)"
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let task_id = call
            .arguments
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let timeout_secs = call.arguments.get("timeout_secs").and_then(|v| v.as_u64());

        let max_chars = call
            .arguments
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_CHARS);

        let Some(task_id) = task_id else {
            let list = self.registry.list();
            let text = format_task_list(&list);
            return Ok(ToolOutput::text_with_details(
                text,
                json!({
                    "count": list.len(),
                    "tasks": list.iter().map(|t| json!({
                        "task_id": t.id,
                        "status": t.state.as_str(),
                        "exitCode": t.exit_code,
                        "command": t.command,
                    })).collect::<Vec<_>>(),
                }),
            ));
        };

        // Validate id shape early for clearer errors.
        if task_id.contains(' ') {
            return Err(invalid_args("bash_output", "invalid task_id"));
        }

        let snap = self
            .registry
            .wait(&task_id, timeout_secs)
            .await
            .map_err(|err| tool_error("bash_output", err))?;

        let text = format_task_output(&snap, max_chars);
        let ok = match snap.state {
            TaskState::Completed => snap.exit_code.unwrap_or(1) == 0,
            TaskState::Running => true, // not a failure — still in progress
            TaskState::TimedOut | TaskState::Killed | TaskState::Failed => false,
        };

        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "task_id": snap.id,
                "status": snap.state.as_str(),
                "exitCode": snap.exit_code,
                "command": snap.command,
                "ok": ok,
                "running": snap.state == TaskState::Running,
            }),
        ))
    }
}
