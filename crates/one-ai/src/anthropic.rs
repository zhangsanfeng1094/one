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
    const DEFAULT_BASE: &str = "https://api.anthropic.com";

    pub struct AnthropicProvider {
        client: Client,
        api_key: String,
        model: String,
        /// API root without trailing slash (e.g. `https://api.anthropic.com` or a proxy).
        base_url: String,
        /// Pi `AnthropicMessagesCompat` resolved flags.
        compat: crate::compat::ResolvedAnthropicCompat,
        /// Sticky session id when `send_session_affinity_headers` is on.
        session_id: String,
    }

    impl AnthropicProvider {
        pub fn from_env() -> Result<Self> {
            let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                OneError::Provider("ANTHROPIC_API_KEY is not set".to_string())
            })?;
            Ok(Self::new(api_key, DEFAULT_MODEL))
        }

        pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
            Self::with_base(api_key, model, DEFAULT_BASE)
        }

        pub fn with_base(
            api_key: impl Into<String>,
            model: impl Into<String>,
            base_url: impl Into<String>,
        ) -> Self {
            let base = base_url.into().trim_end_matches('/').to_string();
            Self {
                client: Client::new(),
                api_key: api_key.into(),
                model: model.into(),
                base_url: base,
                compat: crate::compat::ResolvedAnthropicCompat::default(),
                session_id: crate::cache::new_session_affinity_id(),
            }
        }

        pub fn with_compat(mut self, compat: crate::compat::ResolvedAnthropicCompat) -> Self {
            self.compat = compat;
            self
        }

        fn messages_url(&self) -> String {
            // Accept either `https://api.anthropic.com` or `…/v1`.
            let base = self.base_url.trim_end_matches('/');
            if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
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
            let body = build_request_body(&request, &self.model, true, &self.compat);
            crate::cache::record_cache_debug(
                "anthropic",
                "request",
                Some(&body),
                None,
                Some(json!({
                    "model": self.model,
                    "base_url": self.base_url,
                    "long_cache": self.compat.supports_long_cache_retention,
                    "cache_on_tools": self.compat.supports_cache_control_on_tools,
                })),
            );
            let mut req = self
                .client
                .post(self.messages_url())
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01");
            // OpenCode Zen/Go Messages path accepts x-api-key; also send Bearer + client tags.
            if self.base_url.contains("opencode.ai") {
                req = req
                    .bearer_auth(&self.api_key)
                    .header("x-opencode-client", "one")
                    .header("x-opencode-session", &self.session_id);
            }
            // Legacy fine-grained tool streaming when eager tool input is unsupported.
            if !self.compat.supports_eager_tool_input_streaming && !request.tools.is_empty() {
                req = req.header(
                    "anthropic-beta",
                    "fine-grained-tool-streaming-2025-05-14",
                );
            }
            if self.compat.send_session_affinity_headers {
                req = req.header("x-session-affinity", &self.session_id);
            }
            let response = crate::sse::send_with_abort(req.json(&body), abort).await?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                crate::cache::record_cache_debug(
                    "anthropic",
                    "error",
                    Some(&body),
                    None,
                    Some(json!({ "status": status.as_u16(), "body_head": text.chars().take(500).collect::<String>() })),
                );
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

            crate::cache::record_cache_debug(
                "anthropic",
                "response",
                Some(&body),
                Some(&usage),
                Some(json!({
                    "model": self.model,
                    "stop_reason": format!("{stop_reason:?}"),
                    "aborted": aborted,
                })),
            );

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

    fn build_request_body(
        request: &CompletionRequest,
        model: &str,
        stream: bool,
        compat: &crate::compat::ResolvedAnthropicCompat,
    ) -> Value {
        let cache = crate::cache::anthropic_cache_control(compat.supports_long_cache_retention);

        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                let mut t = json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                });
                // Per-tool eager input streaming (default true in Pi).
                if compat.supports_eager_tool_input_streaming {
                    t["eager_input_streaming"] = json!(true);
                }
                t
            })
            .collect();

        let mut messages: Vec<Value> = request
            .messages
            .iter()
            .filter_map(|m| map_message(m, compat))
            .collect();
        // Stabilize string→block shape for *all* turns, then place breakpoints.
        // (Only marking the last message as blocks would flip wire shape next turn
        // and invalidate the conversation prefix.)
        crate::cache::apply_anthropic_message_cache(
            &mut messages,
            &mut tools,
            &cache,
            compat.supports_cache_control_on_tools,
        );

        // System as content blocks with cache_control (string form cannot carry breakpoints).
        let system = if request.system_prompt.trim().is_empty() {
            Value::Null
        } else {
            crate::cache::anthropic_system_with_cache(&request.system_prompt, &cache)
        };

        let mut body = json!({
            "model": model,
            "max_tokens": 8192,
            "messages": messages,
            "tools": tools,
            "stream": stream,
        });
        if !system.is_null() {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("system".into(), system);
            }
        }

        // Map unified thinking level → Anthropic extended thinking.
        if compat.force_adaptive_thinking && request.thinking_level.is_enabled() {
            // Adaptive thinking (Claude Opus 4.7+ style): type adaptive + output_config.effort.
            if let Some(obj) = body.as_object_mut() {
                obj.insert("thinking".into(), json!({ "type": "adaptive" }));
                if let Some(effort) = request.thinking_level.effort() {
                    obj.insert(
                        "output_config".into(),
                        json!({ "effort": effort }),
                    );
                }
            }
        } else {
            let _ = crate::thinking::apply_anthropic_thinking(&mut body, request.thinking_level);
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

    fn map_message(
        message: &one_core::AgentMessage,
        compat: &crate::compat::ResolvedAnthropicCompat,
    ) -> Option<Value> {
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
                            } else if compat.allow_empty_signature {
                                // Some Anthropic-compatible proxies emit empty signatures
                                // and still expect thinking blocks on replay.
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": thinking,
                                    "signature": "",
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use one_core::message::{UserContent, UserMessage};
        use one_core::tool::ToolDefinition;

        #[test]
        fn request_body_injects_cache_control() {
            let req = CompletionRequest {
                system_prompt: "you are helpful".into(),
                messages: vec![one_core::AgentMessage::User(UserMessage {
                    content: UserContent::Text("hi".into()),
                    timestamp: 0,
                })],
                tools: vec![
                    ToolDefinition {
                        name: "read".into(),
                        description: "read".into(),
                        parameters: json!({"type": "object"}),
                    },
                    ToolDefinition {
                        name: "bash".into(),
                        description: "bash".into(),
                        parameters: json!({"type": "object"}),
                    },
                ],
                thinking_level: one_core::agent::ThinkingLevel::Off,
            };
            let body = build_request_body(
                &req,
                "claude-test",
                false,
                &crate::compat::ResolvedAnthropicCompat::default(),
            );

            // system is content blocks with cache_control
            assert_eq!(body["system"][0]["type"], "text");
            assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");

            // last tool marked
            let tools = body["tools"].as_array().unwrap();
            assert!(tools[0].get("cache_control").is_none());
            assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");

            // last message marked; user text kept
            let msgs = body["messages"].as_array().unwrap();
            let content = &msgs[0]["content"];
            assert_eq!(content[0]["text"], "hi");
            assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        }

        #[test]
        fn multi_turn_prefix_shape_stable() {
            let compat = crate::compat::ResolvedAnthropicCompat::default();
            let turn1 = CompletionRequest {
                system_prompt: "sys".into(),
                messages: vec![one_core::AgentMessage::User(UserMessage {
                    content: UserContent::Text("hi".into()),
                    timestamp: 0,
                })],
                tools: vec![],
                thinking_level: one_core::agent::ThinkingLevel::Off,
            };
            let body1 = build_request_body(&turn1, "m", false, &compat);

            let turn2 = CompletionRequest {
                system_prompt: "sys".into(),
                messages: vec![
                    one_core::AgentMessage::User(UserMessage {
                        content: UserContent::Text("hi".into()),
                        timestamp: 0,
                    }),
                    one_core::AgentMessage::Assistant(one_core::message::AssistantMessage {
                        content: vec![ContentBlock::Text {
                            text: "yo".into(),
                        }],
                        provider: "anthropic".into(),
                        model: "m".into(),
                        stop_reason: StopReason::Stop,
                        timestamp: 0,
                    }),
                ],
                tools: vec![],
                thinking_level: one_core::agent::ThinkingLevel::Off,
            };
            let body2 = build_request_body(&turn2, "m", false, &compat);

            // First user message must stay a text-block array (not flip back to string).
            assert!(body1["messages"][0]["content"].is_array());
            assert!(body2["messages"][0]["content"].is_array());
            assert_eq!(body1["messages"][0]["content"][0]["text"], "hi");
            assert_eq!(body2["messages"][0]["content"][0]["text"], "hi");
            // System text unchanged across turns.
            assert_eq!(body1["system"][0]["text"], body2["system"][0]["text"]);
        }

        #[test]
        fn long_retention_adds_ttl() {
            let mut compat = crate::compat::ResolvedAnthropicCompat::default();
            compat.supports_long_cache_retention = true;
            let req = CompletionRequest {
                system_prompt: "sys".into(),
                messages: vec![],
                tools: vec![],
                thinking_level: one_core::agent::ThinkingLevel::Off,
            };
            let body = build_request_body(&req, "m", false, &compat);
            assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        }
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

    pub fn new(_api_key: impl Into<String>, _model: impl Into<String>) -> Self {
        Self
    }

    pub fn with_base(
        _api_key: impl Into<String>,
        _model: impl Into<String>,
        _base_url: impl Into<String>,
    ) -> Self {
        Self
    }

    pub fn with_compat(self, _compat: crate::compat::ResolvedAnthropicCompat) -> Self {
        self
    }
}