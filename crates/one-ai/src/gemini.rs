//! Google Gemini native API: `generateContent` / `streamGenerateContent`.
//!
//! Protocol id: [`crate::ProviderApi::GeminiGenerateContent`] (`gemini-generate-content`).
//!
//! Endpoint shape:
//! - `POST {base}/models/{model}:generateContent`
//! - `POST {base}/models/{model}:streamGenerateContent?alt=sse`
//!
//! Auth: header `x-goog-api-key` (also accepts `?key=` via URL if needed).

#[cfg(feature = "http-providers")]
mod inner {
    use std::sync::atomic::AtomicBool;

    use async_trait::async_trait;
    use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider, TokenUsage};
    use one_core::error::{OneError, Result};
    use one_core::message::{ContentBlock, StopReason, TextOrImage, UserContent};
    use one_core::streaming::StreamEvent;
    use reqwest::Client;
    use serde_json::{json, Map, Value};

    const DEFAULT_MODEL: &str = "gemini-2.5-flash";
    const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

    pub struct GeminiProvider {
        client: Client,
        api_key: String,
        model: String,
        /// API root without trailing slash (e.g. `https://generativelanguage.googleapis.com/v1beta`).
        base_url: String,
    }

    impl GeminiProvider {
        pub fn from_env() -> Result<Self> {
            let api_key = std::env::var("GEMINI_API_KEY")
                .or_else(|_| std::env::var("GOOGLE_API_KEY"))
                .map_err(|_| {
                    OneError::Provider("GEMINI_API_KEY / GOOGLE_API_KEY is not set".into())
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
            let base = normalize_base(&base_url.into());
            Self {
                client: Client::new(),
                api_key: api_key.into(),
                model: model.into(),
                base_url: base,
            }
        }

        fn endpoint(&self, stream: bool) -> String {
            let model = self.model.trim_start_matches("models/");
            let action = if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            };
            let mut url = format!("{}/models/{}:{}", self.base_url, model, action);
            if stream {
                url.push_str("?alt=sse");
            }
            url
        }
    }

    #[async_trait]
    impl LlmProvider for GeminiProvider {
        fn name(&self) -> &str {
            "gemini"
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
            let body = build_request_body(&request);
            let response = crate::sse::send_with_abort(
                self.client
                    .post(self.endpoint(true))
                    .header("x-goog-api-key", &self.api_key)
                    .header("Content-Type", "application/json")
                    .json(&body),
                abort,
            )
            .await?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(OneError::Provider(format!("gemini {status}: {text}")));
            }

            let mut content: Vec<ContentBlock> = Vec::new();
            // Accumulate text/thinking by kind; tool calls as separate blocks.
            let mut text_buf = String::new();
            let mut thinking_buf = String::new();
            let mut stop_reason = StopReason::Stop;
            let mut usage = TokenUsage::default();
            // tool_call_id → index in content (for streaming arg updates)
            let mut tool_index: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();

            let aborted = matches!(
                crate::sse::read_sse_response(
                    response,
                    &mut |data| {
                        let Ok(value) = serde_json::from_str::<Value>(data) else {
                            return;
                        };
                        merge_usage(&mut usage, value.get("usageMetadata"));

                        let Some(candidates) = value.get("candidates").and_then(|c| c.as_array())
                        else {
                            return;
                        };
                        let Some(candidate) = candidates.first() else {
                            return;
                        };

                        if let Some(reason) =
                            candidate.get("finishReason").and_then(|r| r.as_str())
                        {
                            stop_reason = map_finish_reason(reason, false);
                        }

                        let parts = candidate
                            .pointer("/content/parts")
                            .and_then(|p| p.as_array())
                            .cloned()
                            .unwrap_or_default();

                        for part in parts {
                            apply_part(
                                &part,
                                &mut text_buf,
                                &mut thinking_buf,
                                &mut content,
                                &mut tool_index,
                                on_event,
                            );
                        }
                    },
                    abort,
                )
                .await,
                Err(OneError::Aborted)
            );

            // Flush accumulated text / thinking into content (order: thinking then text then tools).
            // Tools may already be in `content` interleaved; rebuild ordered list.
            let mut ordered = Vec::new();
            if !thinking_buf.is_empty() {
                ordered.push(ContentBlock::Thinking {
                    thinking: thinking_buf,
                    signature: None,
                    redacted: false,
                });
            }
            if !text_buf.is_empty() {
                ordered.push(ContentBlock::Text { text: text_buf });
            }
            for block in content {
                if matches!(block, ContentBlock::ToolCall { .. }) {
                    ordered.push(block);
                }
            }

            let has_tools = ordered
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { .. }));

            if aborted {
                stop_reason = StopReason::Aborted;
            } else if has_tools && matches!(stop_reason, StopReason::Stop) {
                stop_reason = StopReason::ToolUse;
            } else if has_tools {
                stop_reason = StopReason::ToolUse;
            }

            Ok(CompletionResponse {
                provider: self.name().to_string(),
                model: self.model.clone(),
                content: ordered,
                stop_reason,
                usage,
            })
        }
    }

    fn apply_part(
        part: &Value,
        text_buf: &mut String,
        thinking_buf: &mut String,
        content: &mut Vec<ContentBlock>,
        tool_index: &mut std::collections::HashMap<String, usize>,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) {
        // Thought / reasoning parts (Gemini 2.5+).
        let is_thought = part
            .get("thought")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
            if text.is_empty() {
                // continue
            } else if is_thought {
                thinking_buf.push_str(text);
                on_event(StreamEvent::ThinkingDelta(text.to_string()));
            } else {
                text_buf.push_str(text);
                on_event(StreamEvent::TextDelta(text.to_string()));
            }
        }

        if let Some(fc) = part.get("functionCall") {
            let name = fc
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                return;
            }
            let id = fc
                .get("id")
                .and_then(|i| i.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("call_{name}_{}", content.len()));
            let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
            if let Some(&idx) = tool_index.get(&id) {
                // Update existing tool call args if re-sent.
                if let Some(ContentBlock::ToolCall { arguments, .. }) = content.get_mut(idx) {
                    *arguments = args;
                }
            } else {
                tool_index.insert(id.clone(), content.len());
                content.push(ContentBlock::ToolCall {
                    id,
                    name,
                    arguments: args,
                });
            }
        }
    }

    fn normalize_base(base: &str) -> String {
        let b = base.trim().trim_end_matches('/');
        // If user pasted the OpenAI-compat base, strip `/openai` so native paths work.
        let b = b.strip_suffix("/openai").unwrap_or(b);
        // Ensure we have a versioned root; bare host → add /v1beta.
        if b.ends_with("generativelanguage.googleapis.com") {
            format!("{b}/v1beta")
        } else {
            b.to_string()
        }
    }

    pub(crate) fn build_request_body(request: &CompletionRequest) -> Value {
        let contents = map_contents(&request.messages);
        let mut body = json!({
            "contents": contents,
        });

        if !request.system_prompt.trim().is_empty() {
            body["systemInstruction"] = json!({
                "parts": [{ "text": request.system_prompt }],
            });
        }

        if !request.tools.is_empty() {
            let decls: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    let mut decl = json!({
                        "name": tool.name,
                        "description": tool.description,
                    });
                    if !tool.parameters.is_null() {
                        decl["parameters"] = sanitize_schema(&tool.parameters);
                    }
                    decl
                })
                .collect();
            body["tools"] = json!([{ "functionDeclarations": decls }]);
            body["toolConfig"] = json!({
                "functionCallingConfig": { "mode": "AUTO" }
            });
        }

        // Thinking budget for models that support it (Gemini 2.5).
        if let Some(budget) = request.thinking_level.budget_tokens() {
            body["generationConfig"] = json!({
                "thinkingConfig": {
                    "thinkingBudget": budget,
                    "includeThoughts": true,
                }
            });
        }

        body
    }

    /// Gemini contents: roles `user` | `model`. Consecutive tool results merge into one user turn.
    fn map_contents(messages: &[one_core::AgentMessage]) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        let mut pending_fn_responses: Vec<Value> = Vec::new();

        let flush_fn = |pending: &mut Vec<Value>, out: &mut Vec<Value>| {
            if pending.is_empty() {
                return;
            }
            out.push(json!({
                "role": "user",
                "parts": std::mem::take(pending),
            }));
        };

        for message in messages {
            match message {
                one_core::AgentMessage::User(user) => {
                    flush_fn(&mut pending_fn_responses, &mut out);
                    let parts = user_parts(user);
                    if !parts.is_empty() {
                        out.push(json!({ "role": "user", "parts": parts }));
                    }
                }
                one_core::AgentMessage::Assistant(assistant) => {
                    flush_fn(&mut pending_fn_responses, &mut out);
                    let mut parts = Vec::new();
                    for block in &assistant.content {
                        match block {
                            ContentBlock::Text { text } => {
                                if !text.is_empty() {
                                    parts.push(json!({ "text": text }));
                                }
                            }
                            ContentBlock::Thinking { thinking, .. } => {
                                if !thinking.is_empty() {
                                    // Replay as thought part when possible.
                                    parts.push(json!({ "text": thinking, "thought": true }));
                                }
                            }
                            ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                            } => {
                                let mut fc = json!({
                                    "name": name,
                                    "args": if arguments.is_null() { json!({}) } else { arguments.clone() },
                                });
                                if !id.is_empty() {
                                    fc["id"] = json!(id);
                                }
                                parts.push(json!({ "functionCall": fc }));
                            }
                        }
                    }
                    if !parts.is_empty() {
                        out.push(json!({ "role": "model", "parts": parts }));
                    }
                }
                one_core::AgentMessage::ToolResult(result) => {
                    let text = result
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            TextOrImage::Text { text } => Some(text.as_str()),
                            TextOrImage::Image { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let response_obj = if result.is_error {
                        json!({ "error": text })
                    } else {
                        // Prefer object if tool returned JSON; else wrap text.
                        match serde_json::from_str::<Value>(&text) {
                            Ok(Value::Object(map)) => Value::Object(map),
                            Ok(other) => json!({ "result": other }),
                            Err(_) => json!({ "output": text }),
                        }
                    };
                    let mut fr = json!({
                        "name": result.tool_name,
                        "response": response_obj,
                    });
                    if !result.tool_call_id.is_empty() {
                        fr["id"] = json!(result.tool_call_id);
                    }
                    pending_fn_responses.push(json!({ "functionResponse": fr }));
                }
            }
        }
        flush_fn(&mut pending_fn_responses, &mut out);
        out
    }

    fn user_parts(user: &one_core::message::UserMessage) -> Vec<Value> {
        match &user.content {
            UserContent::Text(text) => {
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![json!({ "text": text })]
                }
            }
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    TextOrImage::Text { text } => {
                        if text.is_empty() {
                            None
                        } else {
                            Some(json!({ "text": text }))
                        }
                    }
                    TextOrImage::Image { .. } => {
                        let (mime_type, data) = b.resolved_base64().ok()?;
                        Some(json!({
                            "inlineData": {
                                "mimeType": mime_type,
                                "data": data,
                            }
                        }))
                    }
                })
                .collect(),
        }
    }

    /// Strip JSON Schema fields Gemini rejects (`$schema`, `additionalProperties`, …).
    fn sanitize_schema(schema: &Value) -> Value {
        match schema {
            Value::Object(map) => {
                let mut out = Map::new();
                for (k, v) in map {
                    if matches!(
                        k.as_str(),
                        "$schema" | "$id" | "additionalProperties" | "examples" | "default"
                    ) {
                        continue;
                    }
                    out.insert(k.clone(), sanitize_schema(v));
                }
                Value::Object(out)
            }
            Value::Array(arr) => Value::Array(arr.iter().map(sanitize_schema).collect()),
            other => other.clone(),
        }
    }

    fn map_finish_reason(reason: &str, has_tools: bool) -> StopReason {
        match reason {
            "STOP" | "stop" => {
                if has_tools {
                    StopReason::ToolUse
                } else {
                    StopReason::Stop
                }
            }
            "MAX_TOKENS" | "max_tokens" => StopReason::Length,
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
                StopReason::Error
            }
            "MALFORMED_FUNCTION_CALL" => StopReason::Error,
            _ if has_tools => StopReason::ToolUse,
            _ => StopReason::Stop,
        }
    }

    fn merge_usage(usage: &mut TokenUsage, meta: Option<&Value>) {
        let Some(meta) = meta else {
            return;
        };
        if let Some(n) = meta.get("promptTokenCount").and_then(|v| v.as_u64()) {
            usage.input_tokens = n;
        }
        if let Some(n) = meta.get("candidatesTokenCount").and_then(|v| v.as_u64()) {
            usage.output_tokens = n;
        }
        // Thoughts tokens count as output-ish; keep separate if present later.
        if let Some(n) = meta.get("cachedContentTokenCount").and_then(|v| v.as_u64()) {
            usage.cache_read_tokens = n;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use one_core::agent::ThinkingLevel;
        use one_core::message::{AssistantMessage, ToolResultMessage, UserMessage};
        use one_core::tool::ToolDefinition;

        #[test]
        fn builds_text_and_tools() {
            let req = CompletionRequest {
                system_prompt: "sys".into(),
                messages: vec![one_core::AgentMessage::User(UserMessage {
                    content: UserContent::Text("hi".into()),
                    timestamp: 0,
                })],
                tools: vec![ToolDefinition {
                    name: "read".into(),
                    description: "read a file".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "additionalProperties": false,
                        "$schema": "http://json-schema.org/draft-07/schema#"
                    }),
                }],
                thinking_level: ThinkingLevel::Off,
            };
            let body = build_request_body(&req);
            assert_eq!(
                body.pointer("/systemInstruction/parts/0/text")
                    .and_then(|v| v.as_str()),
                Some("sys")
            );
            assert_eq!(
                body.pointer("/contents/0/parts/0/text")
                    .and_then(|v| v.as_str()),
                Some("hi")
            );
            assert_eq!(
                body.pointer("/tools/0/functionDeclarations/0/name")
                    .and_then(|v| v.as_str()),
                Some("read")
            );
            // Sanitized away.
            assert!(body
                .pointer("/tools/0/functionDeclarations/0/parameters/additionalProperties")
                .is_none());
            assert!(body
                .pointer("/tools/0/functionDeclarations/0/parameters/$schema")
                .is_none());
        }

        #[test]
        fn merges_tool_results_into_one_user_turn() {
            let messages = vec![
                one_core::AgentMessage::User(UserMessage {
                    content: UserContent::Text("do it".into()),
                    timestamp: 0,
                }),
                one_core::AgentMessage::Assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: json!({ "path": "a" }),
                    }],
                    provider: "gemini".into(),
                    model: "m".into(),
                    stop_reason: StopReason::ToolUse,
                    timestamp: 0,
                }),
                one_core::AgentMessage::ToolResult(ToolResultMessage {
                    tool_call_id: "c1".into(),
                    tool_name: "read".into(),
                    content: vec![TextOrImage::Text {
                        text: "file contents".into(),
                    }],
                    is_error: false,
                    timestamp: 0,
                }),
                one_core::AgentMessage::ToolResult(ToolResultMessage {
                    tool_call_id: "c2".into(),
                    tool_name: "ls".into(),
                    content: vec![TextOrImage::Text {
                        text: "a\nb".into(),
                    }],
                    is_error: false,
                    timestamp: 0,
                }),
            ];
            let contents = map_contents(&messages);
            assert_eq!(contents.len(), 3);
            assert_eq!(contents[0]["role"], "user");
            assert_eq!(contents[1]["role"], "model");
            assert_eq!(contents[2]["role"], "user");
            let parts = contents[2]["parts"].as_array().unwrap();
            assert_eq!(parts.len(), 2);
            assert_eq!(
                parts[0].pointer("/functionResponse/name").and_then(|v| v.as_str()),
                Some("read")
            );
            assert_eq!(
                parts[1].pointer("/functionResponse/name").and_then(|v| v.as_str()),
                Some("ls")
            );
        }

        #[test]
        fn normalize_strips_openai_suffix() {
            assert_eq!(
                normalize_base("https://generativelanguage.googleapis.com/v1beta/openai"),
                "https://generativelanguage.googleapis.com/v1beta"
            );
            assert_eq!(
                normalize_base("https://generativelanguage.googleapis.com"),
                "https://generativelanguage.googleapis.com/v1beta"
            );
        }
    }
}

#[cfg(feature = "http-providers")]
pub use inner::GeminiProvider;

#[cfg(not(feature = "http-providers"))]
pub struct GeminiProvider;

#[cfg(not(feature = "http-providers"))]
impl GeminiProvider {
    pub fn from_env() -> one_core::error::Result<Self> {
        Err(one_core::error::OneError::Provider(
            "rebuild with --features http-providers to enable Gemini".into(),
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
}
