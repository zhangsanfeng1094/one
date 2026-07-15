#[cfg(feature = "http-providers")]
mod inner {
    use std::sync::atomic::AtomicBool;

    use async_trait::async_trait;
    use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider, TokenUsage};
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

            // Blocks keyed by Anthropic content index so we preserve order
            // (thinking → text → tool_use) and accumulate signatures.
            let mut blocks: std::collections::BTreeMap<usize, PartialBlock> =
                std::collections::BTreeMap::new();
            let mut stop_reason = StopReason::Stop;
            let mut usage = TokenUsage::default();

            let aborted = matches!(
                crate::sse::read_sse_response(response, &mut |data| {
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    return;
                };
                match value.get("type").and_then(|t| t.as_str()) {
                    Some("content_block_start") => {
                        let index = value
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let btype = value
                            .pointer("/content_block/type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        match btype {
                            "text" => {
                                blocks.insert(index, PartialBlock::Text {
                                    text: String::new(),
                                });
                            }
                            "thinking" => {
                                blocks.insert(index, PartialBlock::Thinking {
                                    thinking: String::new(),
                                    signature: String::new(),
                                    redacted: false,
                                });
                            }
                            "redacted_thinking" => {
                                let data = value
                                    .pointer("/content_block/data")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                blocks.insert(index, PartialBlock::Thinking {
                                    thinking: "[Reasoning redacted]".into(),
                                    signature: data,
                                    redacted: true,
                                });
                            }
                            "tool_use" => {
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
                                blocks.insert(index, PartialBlock::Tool {
                                    id,
                                    name,
                                    arguments: String::new(),
                                });
                            }
                            _ => {}
                        }
                    }
                    Some("content_block_delta") => {
                        let index = value
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let dtype = value
                            .pointer("/delta/type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        match dtype {
                            "text_delta" => {
                                if let Some(text) =
                                    value.pointer("/delta/text").and_then(|t| t.as_str())
                                {
                                    if let Some(PartialBlock::Text { text: buf }) =
                                        blocks.get_mut(&index)
                                    {
                                        buf.push_str(text);
                                    } else {
                                        blocks.insert(
                                            index,
                                            PartialBlock::Text {
                                                text: text.to_string(),
                                            },
                                        );
                                    }
                                    on_event(StreamEvent::TextDelta(text.to_string()));
                                }
                            }
                            "thinking_delta" => {
                                if let Some(delta) =
                                    value.pointer("/delta/thinking").and_then(|t| t.as_str())
                                {
                                    if let Some(PartialBlock::Thinking { thinking, .. }) =
                                        blocks.get_mut(&index)
                                    {
                                        thinking.push_str(delta);
                                    } else {
                                        blocks.insert(
                                            index,
                                            PartialBlock::Thinking {
                                                thinking: delta.to_string(),
                                                signature: String::new(),
                                                redacted: false,
                                            },
                                        );
                                    }
                                    on_event(StreamEvent::ThinkingDelta(delta.to_string()));
                                }
                            }
                            "signature_delta" => {
                                if let Some(sig) =
                                    value.pointer("/delta/signature").and_then(|t| t.as_str())
                                {
                                    if let Some(PartialBlock::Thinking { signature, .. }) =
                                        blocks.get_mut(&index)
                                    {
                                        signature.push_str(sig);
                                    }
                                }
                            }
                            "input_json_delta" => {
                                if let Some(partial) = value
                                    .pointer("/delta/partial_json")
                                    .and_then(|t| t.as_str())
                                {
                                    if let Some(PartialBlock::Tool { arguments, .. }) =
                                        blocks.get_mut(&index)
                                    {
                                        arguments.push_str(partial);
                                    }
                                }
                            }
                            _ => {
                                // Fallback: older/proxied streams may only send /delta/text
                                if let Some(text) =
                                    value.pointer("/delta/text").and_then(|t| t.as_str())
                                {
                                    if let Some(PartialBlock::Text { text: buf }) =
                                        blocks.get_mut(&index)
                                    {
                                        buf.push_str(text);
                                    }
                                    on_event(StreamEvent::TextDelta(text.to_string()));
                                }
                            }
                        }
                    }
                    Some("message_start") => {
                        if let Some(u) = value.pointer("/message/usage") {
                            merge_anthropic_usage(&mut usage, u);
                        }
                    }
                    Some("message_delta") => {
                        if let Some(reason) =
                            value.pointer("/delta/stop_reason").and_then(|v| v.as_str())
                        {
                            let has_tools = blocks
                                .values()
                                .any(|b| matches!(b, PartialBlock::Tool { .. }));
                            stop_reason = map_stop_reason(reason, has_tools);
                        }
                        if let Some(u) = value.get("usage") {
                            merge_anthropic_usage(&mut usage, u);
                        }
                    }
                    _ => {}
                }
            }, abort)
                .await,
                Err(OneError::Aborted)
            );

            let content: Vec<ContentBlock> = blocks.into_values().filter_map(|b| b.into_block()).collect();
            let has_tools = content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { .. }));

            if aborted {
                stop_reason = StopReason::Aborted;
            } else if matches!(stop_reason, StopReason::Stop) && has_tools {
                stop_reason = StopReason::ToolUse;
            }

            Ok(CompletionResponse {
                provider: self.name().to_string(),
                model: self.model.clone(),
                content,
                stop_reason,
                usage,
            })
        }
    }

    enum PartialBlock {
        Text {
            text: String,
        },
        Thinking {
            thinking: String,
            signature: String,
            redacted: bool,
        },
        Tool {
            id: String,
            name: String,
            arguments: String,
        },
    }

    impl PartialBlock {
        fn into_block(self) -> Option<ContentBlock> {
            match self {
                PartialBlock::Text { text } => {
                    if text.is_empty() {
                        None
                    } else {
                        Some(ContentBlock::Text { text })
                    }
                }
                PartialBlock::Thinking {
                    thinking,
                    signature,
                    redacted,
                } => {
                    if thinking.is_empty() && signature.is_empty() {
                        return None;
                    }
                    Some(ContentBlock::Thinking {
                        thinking,
                        signature: if signature.is_empty() {
                            None
                        } else {
                            Some(signature)
                        },
                        redacted,
                    })
                }
                PartialBlock::Tool {
                    id,
                    name,
                    arguments,
                } => {
                    if name.is_empty() {
                        return None;
                    }
                    let args = serde_json::from_str(&arguments).unwrap_or_else(|_| {
                        if arguments.is_empty() {
                            json!({})
                        } else {
                            json!({ "raw": arguments })
                        }
                    });
                    Some(ContentBlock::ToolCall {
                        id,
                        name,
                        arguments: args,
                    })
                }
            }
        }
    }

    fn merge_anthropic_usage(usage: &mut TokenUsage, value: &Value) {
        if let Some(n) = value.get("input_tokens").and_then(|v| v.as_u64()) {
            usage.input_tokens = n;
        }
        if let Some(n) = value.get("output_tokens").and_then(|v| v.as_u64()) {
            usage.output_tokens = n;
        }
        if let Some(n) = value
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
        {
            usage.cache_read_tokens = n;
        }
        if let Some(n) = value
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
        {
            usage.cache_write_tokens = n;
        }
    }

    fn build_request_body(request: &CompletionRequest, model: &str, stream: bool) -> Value {
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

        // Map unified thinking level → Anthropic extended thinking budget.
        let _ = crate::thinking::apply_anthropic_thinking(&mut body, request.thinking_level);

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
            one_core::AgentMessage::User(user) => Some(json!({
                "role": "user",
                "content": crate::media::anthropic_user_content(user),
            })),
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
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                            redacted,
                        } => {
                            // Multi-turn continuity: Anthropic requires thinking blocks
                            // (with signature) to be replayed. Drop only if we have neither
                            // text nor signature (e.g. aborted mid-stream with nothing).
                            if *redacted {
                                if let Some(data) = signature.as_ref().filter(|s| !s.is_empty()) {
                                    blocks.push(json!({
                                        "type": "redacted_thinking",
                                        "data": data,
                                    }));
                                }
                            } else if let Some(sig) =
                                signature.as_ref().filter(|s| !s.is_empty())
                            {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": thinking,
                                    "signature": sig,
                                }));
                            } else if !thinking.is_empty() {
                                // Missing signature (aborted stream / non-Anthropic origin):
                                // fall back to plain text so context is not lost.
                                blocks.push(json!({
                                    "type": "text",
                                    "text": format!("<thinking>\n{thinking}\n</thinking>"),
                                }));
                            }
                        }
                    }
                }
                if blocks.is_empty() {
                    None
                } else {
                    Some(json!({ "role": "assistant", "content": blocks }))
                }
            }
            one_core::AgentMessage::ToolResult(result) => Some(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": result.tool_call_id,
                    "content": crate::media::anthropic_tool_result_content(&result.content),
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
        Thinking {
            thinking: String,
            #[serde(default)]
            signature: Option<String>,
        },
        RedactedThinking {
            data: String,
        },
        ToolUse {
            id: String,
            name: String,
            input: Value,
        },
        #[serde(other)]
        Other,
    }

    fn map_response(payload: AnthropicResponse) -> (Vec<ContentBlock>, StopReason) {
        let mut content = Vec::new();
        for block in payload.content {
            match block {
                AnthropicContentBlock::Text { text } => {
                    content.push(ContentBlock::Text { text });
                }
                AnthropicContentBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    content.push(ContentBlock::Thinking {
                        thinking,
                        signature,
                        redacted: false,
                    });
                }
                AnthropicContentBlock::RedactedThinking { data } => {
                    content.push(ContentBlock::redacted_thinking(data));
                }
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    content.push(ContentBlock::ToolCall {
                        id,
                        name,
                        arguments: input,
                    });
                }
                AnthropicContentBlock::Other => {}
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