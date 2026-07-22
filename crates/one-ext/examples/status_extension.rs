use std::sync::Arc;

use async_trait::async_trait;
use one_core::tool::{Tool, ToolCall, ToolDefinition, ToolOutput};
use one_ext::{
    Extension, ExtensionContext, ExtensionEvent, ExtensionRegistryBuilder, ExtensionRuntime,
    PreToolDecision, PromptFragment,
};
use serde_json::json;

struct DemoExtension;

#[async_trait]
impl Extension for DemoExtension {
    fn name(&self) -> &str {
        "demo"
    }

    async fn on_load(&self, ctx: &ExtensionContext<'_>) -> one_ext::Result<()> {
        println!("demo loaded cwd={}", ctx.cwd.display());
        Ok(())
    }

    async fn on_event(&self, event: &ExtensionEvent) -> one_ext::Result<()> {
        match event {
            ExtensionEvent::ToolStart { tool_call } => {
                println!("tool start: {}", tool_call.name);
            }
            ExtensionEvent::TurnStart { turn } => {
                println!("turn start: {turn}");
            }
            _ => {}
        }
        Ok(())
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(PingTool)]
    }

    fn contribute_context(&self) -> Vec<PromptFragment> {
        vec![PromptFragment {
            source: "demo".into(),
            text: "You can call the `ping` tool from the demo extension.".into(),
        }]
    }

    async fn before_tool(&self, call: &ToolCall) -> one_ext::Result<PreToolDecision> {
        if call.name == "bash" {
            // Example: refuse rm -rf /
            let cmd = call
                .arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if cmd.contains("rm -rf /") {
                return Ok(PreToolDecision::Deny {
                    message: "demo extension blocked dangerous bash".into(),
                });
            }
        }
        Ok(PreToolDecision::Allow)
    }
}

struct PingTool;

#[async_trait]
impl Tool for PingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ping".to_string(),
            description: "Demo extension ping.".to_string(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn execute(&self, _call: &ToolCall) -> one_core::error::Result<ToolOutput> {
        Ok(ToolOutput::text("pong"))
    }
}

#[tokio::main]
async fn main() {
    let mut builder = ExtensionRegistryBuilder::new();
    builder.install(Arc::new(DemoExtension));
    let registry = builder.build();
    let runtime = ExtensionRuntime::from_registry(
        registry,
        one_ext::HooksConfig::default(),
        std::env::current_dir().unwrap_or_else(|_| ".".into()),
    );

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let data = runtime.data().clone();
    let ctx = ExtensionContext {
        cwd: &cwd,
        session_file: None,
        data: &data,
    };
    runtime.load_all(&ctx).await.expect("load");
    println!("loaded extensions: {:?}", runtime.names());
    println!(
        "tools: {:?}",
        runtime
            .tools()
            .iter()
            .map(|t| t.definition().name)
            .collect::<Vec<_>>()
    );
    println!(
        "overlay:\n{}",
        runtime.system_prompt_overlay().unwrap_or_default()
    );
}
