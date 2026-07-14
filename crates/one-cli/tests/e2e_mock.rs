use one_ai::MockProvider;
use one_core::agent::{Agent, AgentConfig};
use one_tools::default_tools;

#[tokio::test]
async fn mock_agent_lists_files_end_to_end() {
    let mut agent = Agent::new(AgentConfig::default(), default_tools(std::env::current_dir().unwrap()));
    let provider = MockProvider::new();
    let output = agent
        .prompt(&provider, "list files in current directory")
        .await
        .expect("agent run");
    assert!(output.contains("directory listing") || output.contains("mock"));
}

#[tokio::test]
async fn bash_requires_approval_without_yes() {
    use one_core::tool::{Tool, ToolCall};
    use one_tools::BashTool;
    use serde_json::json;

    let tool = BashTool::with_auto_approve(std::env::current_dir().unwrap(), false);
    let result = tool
        .execute(&ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({"command": "sudo apt update"}),
        })
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn bash_sandbox_blocks_destructive_command() {
    use one_core::tool::{Tool, ToolCall};
    use one_tools::BashTool;
    use serde_json::json;

    let tool = BashTool::new(std::env::current_dir().unwrap());
    let result = tool
        .execute(&ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({"command": "rm -rf /"}),
        })
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn bash_allows_curl() {
    use one_core::tool::{Tool, ToolCall};
    use one_tools::BashTool;
    use serde_json::json;

    let tool = BashTool::with_auto_approve(std::env::current_dir().unwrap(), true);
    // Should not be blocked at sandbox layer (command may still fail if curl missing).
    let result = tool
        .execute(&ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({"command": "curl --version"}),
        })
        .await;
    // Either success or tool runtime error — not "blocked command pattern"
    if let Err(err) = result {
        assert!(
            !err.to_string().contains("blocked command pattern"),
            "curl should not be hard-blocked: {err}"
        );
    }
}