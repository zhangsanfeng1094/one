use std::sync::Arc;

use async_trait::async_trait;
use one_core::tool::{Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::{json, Value};

use crate::traits::{Extension, ExtensionContext};

pub struct StatusExtension {
    state: std::sync::Mutex<Option<Value>>,
}

impl StatusExtension {
    pub fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(None),
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
        let _ = ctx;
        Ok(())
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(StatusTool)]
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

struct StatusTool;

#[async_trait]
impl Tool for StatusTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "status".to_string(),
            description: "Return process uptime.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
            }),
        }
    }

    async fn execute(&self, _call: &ToolCall) -> one_core::error::Result<ToolOutput> {
        Ok(ToolOutput::text(format!(
            "one extension runtime ok, pid={}",
            std::process::id()
        )))
    }
}