use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider};
use one_core::error::Result;
use one_core::message::{ContentBlock, StopReason};
use serde_json::json;

/// Deterministic provider for local development and tests.
pub struct MockProvider {
    model: String,
}

impl MockProvider {
    pub fn new() -> Self {
        Self {
            model: "mock-v1".to_string(),
        }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest,
        on_event: &mut (dyn FnMut(one_core::streaming::StreamEvent) + Send),
        abort: Option<&AtomicBool>,
    ) -> Result<CompletionResponse> {
        let response = self.build_response(&request).await?;
        let text = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        one_core::streaming::emit_text_chunks(&text, 4, on_event, abort);
        if abort.is_some_and(|flag| {
            use std::sync::atomic::Ordering;
            flag.load(Ordering::Relaxed)
        }) {
            let mut partial = response;
            partial.stop_reason = StopReason::Aborted;
            return Ok(partial);
        }
        Ok(response)
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        self.build_response(&request).await
    }
}

impl MockProvider {
    async fn build_response(&self, request: &CompletionRequest) -> Result<CompletionResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                one_core::AgentMessage::User(user) => match &user.content {
                    one_core::message::UserContent::Text(text) => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or("");

        let lower = last_user.to_lowercase();
        let has_tool_results_after_user = request
            .messages
            .iter()
            .rev()
            .take_while(|message| !matches!(message, one_core::AgentMessage::User(_)))
            .any(|message| matches!(message, one_core::AgentMessage::ToolResult(_)));

        if has_tool_results_after_user {
            return Ok(CompletionResponse {
                provider: self.name().to_string(),
                model: self.model.clone(),
                content: vec![ContentBlock::Text {
                    text: "Here is the directory listing from the previous tool run.".to_string(),
                }],
                stop_reason: StopReason::Stop,
            });
        }

        if lower.contains("list") && (lower.contains("file") || lower.contains("dir")) {
            return Ok(CompletionResponse {
                provider: self.name().to_string(),
                model: self.model.clone(),
                content: vec![ContentBlock::ToolCall {
                    id: "call_mock_ls".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({ "command": "ls -la" }),
                }],
                stop_reason: StopReason::ToolUse,
            });
        }

        Ok(CompletionResponse {
            provider: self.name().to_string(),
            model: self.model.clone(),
            content: vec![ContentBlock::Text {
                text: format!("(mock) Received: {last_user}"),
            }],
            stop_reason: StopReason::Stop,
        })
    }
}