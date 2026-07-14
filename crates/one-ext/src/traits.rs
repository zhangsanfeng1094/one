use std::sync::Arc;

use async_trait::async_trait;
use one_core::tool::Tool;
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum ExtensionEvent {
    AgentStart,
    AgentEnd,
    TurnStart { turn: usize },
    TurnEnd { turn: usize },
    ToolExecutionStart { tool_name: String },
    ToolExecutionEnd { tool_name: String, is_error: bool },
}

pub struct ExtensionContext<'a> {
    pub cwd: &'a std::path::Path,
    pub session_file: Option<&'a std::path::Path>,
}

#[async_trait]
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;

    async fn on_load(&self, _ctx: &ExtensionContext<'_>) -> crate::Result<()> {
        Ok(())
    }

    async fn on_event(&self, _event: &ExtensionEvent) -> crate::Result<()> {
        Ok(())
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    fn commands(&self) -> Vec<ExtensionCommand> {
        Vec::new()
    }

    /// Optional persistent state stored as session `custom` entries.
    fn custom_state(&self) -> Option<(String, Value)> {
        None
    }

    fn restore_state(&self, _custom_type: &str, _data: &Value) -> crate::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ExtensionCommand {
    pub name: String,
    pub description: String,
    pub handler: fn() -> String,
}