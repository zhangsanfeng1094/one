#[cfg(feature = "http-providers")]
mod inner {
    use std::sync::atomic::AtomicBool;

    use async_trait::async_trait;
    use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider};
    use one_core::error::{OneError, Result};
    use one_core::message::{ContentBlock, StopReason};
    use one_core::streaming::StreamEvent;
    use reqwest::Client;
    use serde::Deserialize;
    use serde_json::{json, Value};

    const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

    pub struct AnthropicProvider {
        client: Client,
        api_key: String,
        model: String,
    }

    impl AnthropicProvider {
        pub fn from_env() -> Result<Self> {
            let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                OneError::Provider("ANTHROPIC_API_KEY is not set".to_string())
            })?;
            Ok(Self::new(api_key, DEFAULT_MODEL))
        }

        pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
            Self {
                client: Client::new(),
                api_key: api_key.into(),
                model: model.into(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for AnthropicProvider {
        fn name(&self) -> &str {
            "anthropic"
        }

        fn model(&self) -> &str {
            &self.model
        }

        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            self.complete_streaming(request, &mut |_| {}, None).await
        }

        async fn complete_streaming(
            &self,
            request: CompletionRequest,
            on_event: &mut (dyn FnMut(StreamEvent) + Send),
            abort: Option<&AtomicBool>,
        ) -> Result<CompletionResponse> {
            let body = build_request_body(&request, &self.model, true);
            let response = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
                .map_err(|err| OneError::Provider(err.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(OneError::Provider(format!("anthropic {status}: {text}")));
            }

            let mut full_text = String::new();
            let mut tool_calls: Vec<ContentBlock> = Vec::new();
            let mut stop_reason = StopReason::Stop;

            let aborted = matches!(
                crate::sse::read_sse_response(response, &mut |data| {
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    return;
                };
                match value.get("type").and_then(|t| t.as_str()) {
                    Some("content_block_delta") => {
                        if let Some(text) = value
                            .pointer("/delta/text")
                            .and_then(|t| t.as_str())
                        {
                            full_text.push_str(text);
                            on_event(StreamEvent::TextDelta(text.to_string()));
                        }
                    }
                    Some("content_block_start") => {
                        if value.pointer("/content_block/type").and_then(|t| t.as_str())
                            == Some("tool_use")
                        {
                            let id = value
                                .pointer("/content_block/id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = value
                                .pointer("/content_block/name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            tool_calls.push(ContentBlock::ToolCall {
                                id,
                                name,
                                arguments: json!({}),
                            });
                        }
                    }
                    Some("message_delta") => {
                        if let Some(reason) =
                            value.pointer("/delta/stop_reason").and_then(|v| v.as_str())
                        {
                            stop_reason = map_stop_reason(reason, !tool_calls.is_empty());
                        }
                    }
                    _ => {}
                }
            }, abort)
                .await,
                Err(OneError::Aborted)
            );

            let mut content = Vec::new();
            if !full_text.is_empty() {
                content.push(ContentBlock::Text { text: full_text });
            }
            content.extend(tool_calls);

            if aborted {
                stop_reason = StopReason::Aborted;
            } else if matches!(stop_reason, StopReason::Stop)
                && content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }))
            {
                stop_reason = StopReason::ToolUse;
            }

            Ok(CompletionResponse {
                provider: self.name().to_string(),
                model: self.model.clone(),
                content,
                stop_reason,
            })
        }
    }

    fn build_request_body(request: &CompletionRequest, model: &str, stream: bool) -> Value {
        use one_core::agent::ThinkingLevel;

        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();

        let messages: Vec<Value> = request
            .messages
            .iter()
            .filter_map(map_message)
            .collect();

        let mut body = json!({
            "model": model,
            "max_tokens": 8192,
            "system": request.system_prompt,
            "messages": messages,
            "tools": tools,
            "stream": stream,
        });

        // Map thinking level → Anthropic extended thinking budget (when enabled).
        if let Some(budget) = match request.thinking_level {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some(5_000),
            ThinkingLevel::Medium => Some(10_000),
            ThinkingLevel::High => Some(20_000),
        } {
            body.as_object_mut().unwrap().insert(
                "thinking".into(),
                json!({ "type": "enabled", "budget_tokens": budget }),
            );
            // max_tokens must exceed thinking budget.
            body.as_object_mut().unwrap().insert(
                "max_tokens".into(),
                json!(budget + 4096),
            );
        }

        body
    }

    fn map_stop_reason(reason: &str, has_tools: bool) -> StopReason {
        match reason {
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::Length,
            "end_turn" => StopReason::Stop,
            _ if has_tools => StopReason::ToolUse,
            _ => StopReason::Stop,
        }
    }

    fn map_message(message: &one_core::AgentMessage) -> Option<Value> {
        match message {
            one_core::AgentMessage::User(user) => {
                let text = match &user.content {
                    one_core::message::UserContent::Text(text) => text.clone(),
                    one_core::message::UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            one_core::message::TextOrImage::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                Some(json!({ "role": "user", "content": text }))
            }
            one_core::AgentMessage::Assistant(assistant) => {
                let mut blocks = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text } => {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                        ContentBlock::ToolCall { id, name, arguments } => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": arguments,
                            }));
                        }
                        ContentBlock::Thinking { .. } => {}
                    }
                }
                Some(json!({ "role": "assistant", "content": blocks }))
            }
            one_core::AgentMessage::ToolResult(result) => Some(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": result.tool_call_id,
                    "content": result.content.iter().filter_map(|block| match block {
                        one_core::message::TextOrImage::Text { text } => Some(text.clone()),
                        _ => None,
                    }).collect::<Vec<_>>().join("\n"),
                    "is_error": result.is_error,
                }]
            })),
        }
    }

    #[derive(Debug, Deserialize)]
    struct AnthropicResponse {
        content: Vec<AnthropicContentBlock>,
        stop_reason: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum AnthropicContentBlock {
        Text { text: String },
        ToolUse {
            id: String,
            name: String,
            input: Value,
        },
    }

    fn map_response(payload: AnthropicResponse) -> (Vec<ContentBlock>, StopReason) {
        let mut content = Vec::new();
        for block in payload.content {
            match block {
                AnthropicContentBlock::Text { text } => {
                    content.push(ContentBlock::Text { text });
                }
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    content.push(ContentBlock::ToolCall {
                        id,
                        name,
                        arguments: input,
                    });
                }
            }
        }

        let stop_reason = match payload.stop_reason.as_deref() {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::Length,
            Some("end_turn") => StopReason::Stop,
            _ if content.iter().any(|block| matches!(block, ContentBlock::ToolCall { .. })) => {
                StopReason::ToolUse
            }
            _ => StopReason::Stop,
        };

        (content, stop_reason)
    }
}

#[cfg(feature = "http-providers")]
pub use inner::AnthropicProvider;

#[cfg(not(feature = "http-providers"))]
pub struct AnthropicProvider;

#[cfg(not(feature = "http-providers"))]
impl AnthropicProvider {
    pub fn from_env() -> one_core::error::Result<Self> {
        Err(one_core::error::OneError::Provider(
            "rebuild with --features http-providers to enable Anthropic".to_string(),
        ))
    }
}