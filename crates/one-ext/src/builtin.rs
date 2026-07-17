//! Built-in extensions compiled into the binary.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use one_core::tool::{Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::{json, Value};

use crate::events::{
    ExtensionCommand, ExtensionContext, ExtensionEvent, PromptFragment,
};
use crate::traits::Extension;

/// Lightweight status extension: tools + context + event counters.
pub struct StatusExtension {
    state: std::sync::Mutex<Option<Value>>,
    tool_starts: AtomicU64,
    turns: AtomicU64,
}

impl StatusExtension {
    pub fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(None),
            tool_starts: AtomicU64::new(0),
            turns: AtomicU64::new(0),
        }
    }
}

impl Default for StatusExtension {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extension for StatusExtension {
    fn name(&self) -> &str {
        "status"
    }

    async fn on_load(&self, ctx: &ExtensionContext<'_>) -> crate::Result<()> {
        if let Ok(mut state) = self.state.lock() {
            *state = Some(json!({
                "cwd": ctx.cwd.display().to_string(),
                "loaded_at": chrono_like_now(),
            }));
        }
        Ok(())
    }

    async fn on_event(&self, event: &ExtensionEvent) -> crate::Result<()> {
        match event {
            ExtensionEvent::TurnStart { .. } => {
                self.turns.fetch_add(1, Ordering::Relaxed);
            }
            ExtensionEvent::ToolStart { .. } | ExtensionEvent::ToolEnd { .. } => {
                // ToolEnd is primary; still count ToolStart if fired.
                if matches!(event, ExtensionEvent::ToolStart { .. }) {
                    self.tool_starts.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(StatusTool {
            tool_starts: self.tool_starts.load(Ordering::Relaxed),
            turns: self.turns.load(Ordering::Relaxed),
        })]
    }

    fn contribute_context(&self) -> Vec<PromptFragment> {
        vec![PromptFragment {
            source: "status".into(),
            text: "The `status` tool reports extension runtime health (pid, turn/tool counters)."
                .into(),
        }]
    }

    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: "ext-status".into(),
            description: "Print extension runtime status".into(),
            handler: |_| {
                format!(
                    "one extension runtime ok, pid={}",
                    std::process::id()
                )
            },
        }]
    }

    fn custom_state(&self) -> Option<(String, Value)> {
        let state = self.state.lock().ok()?;
        state
            .clone()
            .map(|data| ("ext.status".to_string(), data))
    }

    fn restore_state(&self, custom_type: &str, data: &Value) -> crate::Result<()> {
        if custom_type == "ext.status" {
            if let Ok(mut state) = self.state.lock() {
                *state = Some(data.clone());
            }
        }
        Ok(())
    }
}

struct StatusTool {
    tool_starts: u64,
    turns: u64,
}

#[async_trait]
impl Tool for StatusTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "status".to_string(),
            description: "Return extension runtime status (pid, counters).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
            }),
        }
    }

    async fn execute(&self, _call: &ToolCall) -> one_core::error::Result<ToolOutput> {
        Ok(ToolOutput::text(format!(
            "one extension runtime ok, pid={}, turns_seen={}, tools_seen={}",
            std::process::id(),
            self.turns,
            self.tool_starts
        )))
    }
}

fn chrono_like_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve a built-in extension by stable name.
pub fn builtin_by_name(name: &str) -> Option<Arc<dyn Extension>> {
    match name {
        "status" => Some(Arc::new(StatusExtension::new())),
        _ => None,
    }
}
