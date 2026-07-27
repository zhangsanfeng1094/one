//! Line-streaming background monitor (Grok `monitor` style).
//!
//! Runs a shell command without blocking the agent turn. Each stdout line is
//! pushed to the shared notification queue as a `<system-reminder>` so the next
//! LLM turn sees events. Use tight filters (`grep --line-buffered`) to avoid
//! flood auto-stop.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::tasks::{BackgroundTaskRegistry, DEFAULT_MONITOR_MAX_EVENTS};

/// Default wall-clock for non-persistent monitors (10 hours, Grok-like).
const DEFAULT_TIMEOUT_SECS: u64 = 36_000;

pub struct MonitorTool {
    registry: Arc<BackgroundTaskRegistry>,
    cwd: PathBuf,
}

impl MonitorTool {
    pub fn new(registry: Arc<BackgroundTaskRegistry>, cwd: PathBuf) -> Self {
        Self { registry, cwd }
    }
}

#[async_trait]
impl Tool for MonitorTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "monitor".to_string(),
            description: "Stream a long-running shell command's stdout **line by line** into the \
conversation as notifications (Grok-style). Returns a `task_id` (`mon_*`) immediately. \
Prefer tight filters (`grep --line-buffered`, `awk`) — raw unbounded log tails will hit \
the event flood guard and auto-stop. \
Stop with `bash_kill` / `job_kill` using the task_id. Completions also appear as \
`[Monitor stopped]`. Not a substitute for short one-shot commands (use `bash`). \
Session end / `/new` kills running monitors."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command whose stdout lines become events"
                    },
                    "description": {
                        "type": "string",
                        "description": "Short human-readable purpose (for logs/UI)"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Hard wall-clock seconds before kill. Default 36000 (10h). \
Set 0 with persistent=true for no wall limit (still killed on session end)."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Alias for timeout in milliseconds (Grok-style)"
                    },
                    "persistent": {
                        "type": "boolean",
                        "description": "If true and timeout not set, run without wall limit until kill/session end. Default false."
                    },
                    "max_events": {
                        "type": "integer",
                        "description": format!(
                            "Auto-stop after this many stdout lines (default {DEFAULT_MONITOR_MAX_EVENTS})"
                        )
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let command = call
            .arguments
            .get("command")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("monitor", "missing `command`"))?
            .to_string();

        let description = call
            .arguments
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let persistent = call
            .arguments
            .get("persistent")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let max_events = call
            .arguments
            .get("max_events")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MONITOR_MAX_EVENTS);

        let timeout_secs = resolve_timeout(&call.arguments, persistent);

        let id = self
            .registry
            .spawn_monitor(command.clone(), self.cwd.clone(), timeout_secs, max_events)
            .await
            .map_err(|e| tool_error("monitor", e))?;

        let mut text = format!(
            "Monitor started\ntask_id: {id}\nstatus: running\nmax_events: {max_events}\n"
        );
        if let Some(secs) = timeout_secs {
            text.push_str(&format!("timeout_secs: {secs}\n"));
        } else {
            text.push_str("timeout_secs: none (persistent / until kill)\n");
        }
        text.push_str(&format!("command: {command}\n"));
        if let Some(d) = &description {
            text.push_str(&format!("description: {d}\n"));
        }
        text.push_str(
            "Each stdout line will appear as a [Monitor event] notice on a later turn. \
             Prefer filtered streams. Use bash_kill/job_kill to stop.",
        );

        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "task_id": id,
                "status": "started",
                "max_events": max_events,
                "timeout_secs": timeout_secs,
                "persistent": persistent,
                "command": command,
                "description": description,
                "ok": true,
            }),
        ))
    }
}

fn resolve_timeout(args: &serde_json::Value, persistent: bool) -> Option<u64> {
    if let Some(ms) = args.get("timeout_ms").and_then(|v| v.as_u64()) {
        if ms == 0 {
            return if persistent { None } else { Some(DEFAULT_TIMEOUT_SECS) };
        }
        return Some((ms.saturating_add(999)) / 1000);
    }
    if let Some(secs) = args.get("timeout_secs").and_then(|v| v.as_u64()) {
        if secs == 0 {
            return if persistent { None } else { Some(DEFAULT_TIMEOUT_SECS) };
        }
        return Some(secs);
    }
    if persistent {
        None
    } else {
        Some(DEFAULT_TIMEOUT_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;
    use serde_json::json;

    #[tokio::test]
    async fn monitor_emits_line_events() {
        let reg = Arc::new(BackgroundTaskRegistry::new());
        let dir = std::env::temp_dir();
        let tool = MonitorTool::new(reg.clone(), dir);
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "monitor".into(),
                arguments: json!({
                    "command": "printf 'alpha\\nbeta\\n'",
                    "max_events": 50,
                    "timeout_secs": 30
                }),
            })
            .await
            .unwrap();
        assert!(out.as_text().contains("task_id: mon_"), "{}", out.as_text());
        let id = out
            .details
            .as_ref()
            .and_then(|d| d.get("task_id"))
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        // Wait for process to finish and lines to flush.
        let _ = reg.wait(&id, Some(10)).await.unwrap();
        // Give line reader a moment after wait.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let notes = reg.notification_queue().lock().unwrap().clone();
        let events: Vec<_> = notes
            .iter()
            .filter(|n| n.contains("[Monitor event"))
            .collect();
        assert!(
            events.len() >= 2,
            "expected >=2 monitor events, got notes={notes:?}"
        );
        assert!(notes.iter().any(|n| n.contains("alpha")));
        assert!(notes.iter().any(|n| n.contains("beta")));
        assert!(notes.iter().any(|n| n.contains("[Monitor stopped]")));
    }
}
