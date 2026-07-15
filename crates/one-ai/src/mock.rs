use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider, TokenUsage};
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
        // Stream thinking first (when present), then text — matches real providers.
        for block in &response.content {
            if let ContentBlock::Thinking { thinking, .. } = block {
                emit_thinking_chunks_async(
                    thinking,
                    3,
                    on_event,
                    abort,
                    std::time::Duration::from_millis(8),
                )
                .await;
            }
        }
        let text = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        // Char-safe chunks + small delay so the TUI typewriter can paint between deltas.
        one_core::streaming::emit_text_chunks_async(
            &text,
            2,
            on_event,
            abort,
            std::time::Duration::from_millis(12),
        )
        .await;
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
                usage: TokenUsage {
                    input_tokens: 32,
                    output_tokens: 24,
                    ..Default::default()
                },
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
                usage: TokenUsage {
                    input_tokens: 40,
                    output_tokens: 12,
                    ..Default::default()
                },
            });
        }

        let mut content = Vec::new();
        if request.thinking_level.is_enabled() {
            content.push(ContentBlock::thinking(format!(
                "(mock think · {}) weighing: {last_user}",
                request.thinking_level.as_str()
            )));
        }
        content.push(ContentBlock::Text {
            text: format!("(mock) Received: {last_user}"),
        });

        Ok(CompletionResponse {
            provider: self.name().to_string(),
            model: self.model.clone(),
            content,
            stop_reason: StopReason::Stop,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 16,
                ..Default::default()
            },
        })
    }
}

async fn emit_thinking_chunks_async(
    text: &str,
    chunk_chars: usize,
    on_event: &mut (dyn FnMut(one_core::streaming::StreamEvent) + Send),
    abort: Option<&AtomicBool>,
    delay: std::time::Duration,
) {
    use one_core::streaming::StreamEvent;
    use std::sync::atomic::Ordering;

    let n = chunk_chars.max(1);
    let mut buf = String::with_capacity(n * 4);
    let mut count = 0usize;
    for ch in text.chars() {
        if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            break;
        }
        buf.push(ch);
        count += 1;
        if count >= n {
            on_event(StreamEvent::ThinkingDelta(std::mem::take(&mut buf)));
            count = 0;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
    }
    if !buf.is_empty() {
        on_event(StreamEvent::ThinkingDelta(buf));
    }
}