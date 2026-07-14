use std::sync::Arc;

use async_trait::async_trait;
use one_core::tool::{Tool, ToolCall, ToolDefinition, ToolOutput};
use one_ext::{Extension, ExtensionContext};
use serde_json::json;

struct StatusExtension;

#[async_trait]
impl Extension for StatusExtension {
    fn name(&self) -> &str {
        "status"
    }

    async fn on_load(&self, ctx: &ExtensionContext<'_>) -> one_ext::Result<()> {
        let _ = ctx;
        Ok(())
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(StatusTool)]
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

fn main() {
    let runtime = one_ext::ExtensionRuntime::new(vec![Arc::new(StatusExtension)]);
    println!("loaded extensions: {:?}", runtime.names());
}