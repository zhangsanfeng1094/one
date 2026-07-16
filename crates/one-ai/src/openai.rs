//! LLM wire protocols + OpenAI HTTP provider (Pi-style `api` field).
//!
//! - [`ProviderApi::OpenaiCompletions`] → `POST {base}/chat/completions`
//! - [`ProviderApi::OpenaiResponses`]   → `POST {base}/responses`
//! - [`ProviderApi::AnthropicMessages`] → `POST {base}/v1/messages`
//! - [`ProviderApi::GeminiGenerateContent`] → `POST {base}/models/{model}:generateContent`
//!
//! Configured via constructor / CLI `--openai-api` / `models.json` `api` / `providerType`.

use serde::{Deserialize, Serialize};

/// Wire protocol that drives request/response encoding for a provider.
///
/// Stored in `models.json` as `api` (and optionally mirrored as `providerType`).
/// Users pick one of these fixed values — never free-form strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderApi {
    /// OpenAI Chat Completions — widest compatibility (Ollama, OpenRouter, DeepSeek, proxies).
    #[serde(
        rename = "openai-completions",
        alias = "openai-compatible",
        alias = "chat-completions",
        alias = "completions"
    )]
    OpenaiCompletions,
    /// OpenAI Responses API — default for first-party OpenAI (Pi default).
    #[default]
    #[serde(rename = "openai-responses", alias = "responses")]
    OpenaiResponses,
    /// Anthropic Messages API (`/v1/messages`).
    #[serde(rename = "anthropic-messages", alias = "anthropic", alias = "messages")]
    AnthropicMessages,
    /// Google Gemini native `generateContent` / `streamGenerateContent`.
    #[serde(
        rename = "gemini-generate-content",
        alias = "gemini",
        alias = "google-gemini",
        alias = "generate-content",
        alias = "generateContent"
    )]
    GeminiGenerateContent,
}

/// Backward-compatible alias used across the crate.
pub type OpenaiWireApi = ProviderApi;

impl ProviderApi {
    pub const ALL: &'static [Self] = &[
        Self::OpenaiCompletions,
        Self::OpenaiResponses,
        Self::AnthropicMessages,
        Self::GeminiGenerateContent,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiCompletions => "openai-completions",
            Self::OpenaiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GeminiGenerateContent => "gemini-generate-content",
        }
    }

    /// Short human label for pickers.
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenaiCompletions => "OpenAI Chat Completions (compatible)",
            Self::OpenaiResponses => "OpenAI Responses API",
            Self::AnthropicMessages => "Anthropic Messages API",
            Self::GeminiGenerateContent => "Gemini generateContent (native)",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai-completions"
            | "openai-compatible"
            | "chat-completions"
            | "completions"
            | "chat" => Some(Self::OpenaiCompletions),
            "openai-responses" | "responses" | "response" => Some(Self::OpenaiResponses),
            "anthropic-messages" | "anthropic" | "messages" => Some(Self::AnthropicMessages),
            "gemini-generate-content"
            | "gemini"
            | "google-gemini"
            | "generate-content"
            | "generatecontent" => Some(Self::GeminiGenerateContent),
            _ => None,
        }
    }

    /// Whether this protocol is handled by the OpenAI HTTP client.
    pub fn is_openai_wire(self) -> bool {
        matches!(self, Self::OpenaiCompletions | Self::OpenaiResponses)
    }
}

impl std::fmt::Display for ProviderApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ProviderApi {
    /// Alias for [`Self::OpenaiCompletions`] (legacy name used in call sites).
    #[allow(non_upper_case_globals)]
    pub const Completions: Self = Self::OpenaiCompletions;
    /// Alias for [`Self::OpenaiResponses`] (legacy name used in call sites).
    #[allow(non_upper_case_globals)]
    pub const Responses: Self = Self::OpenaiResponses;
}

#[cfg(test)]
mod wire_api_tests {
    use super::ProviderApi;

    #[test]
    fn parse_aliases() {
        assert_eq!(
            ProviderApi::parse("openai-responses"),
            Some(ProviderApi::OpenaiResponses)
        );
        assert_eq!(
            ProviderApi::parse("completions"),
            Some(ProviderApi::OpenaiCompletions)
        );
        assert_eq!(
            ProviderApi::parse("openai-compatible"),
            Some(ProviderApi::OpenaiCompletions)
        );
        assert_eq!(
            ProviderApi::parse("chat-completions"),
            Some(ProviderApi::OpenaiCompletions)
        );
        assert_eq!(
            ProviderApi::parse("anthropic-messages"),
            Some(ProviderApi::AnthropicMessages)
        );
        assert_eq!(
            ProviderApi::parse("anthropic"),
            Some(ProviderApi::AnthropicMessages)
        );
        assert_eq!(
            ProviderApi::parse("gemini-generate-content"),
            Some(ProviderApi::GeminiGenerateContent)
        );
        assert_eq!(
            ProviderApi::parse("gemini"),
            Some(ProviderApi::GeminiGenerateContent)
        );
        assert_eq!(ProviderApi::parse("nope"), None);
    }
}

#[cfg(feature = "http-providers")]
mod inner {
    use std::collections::BTreeMap;

    use std::sync::atomic::AtomicBool;

    use async_trait::async_trait;
    use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider, TokenUsage};
    use one_core::error::{OneError, Result};
    use one_core::message::{ContentBlock, StopReason};
    use one_core::streaming::StreamEvent;
    use reqwest::Client;
    use serde_json::{json, Value};

    use super::OpenaiWireApi;

    const DEFAULT_MODEL: &str = "gpt-4o";

    pub struct OpenAiProvider {
        client: Client,
        api_key: String,
        model: String,
        base_url: String,
        /// Provider id used for compat auto-detect (`openai`, `ollama`, …).
        provider_id: String,
        /// Chat Completions vs Responses (configurable).
        wire_api: OpenaiWireApi,
        /// How to encode thinking level on chat/completions bodies (legacy bridge).
        thinking_wire: crate::thinking::ThinkingWire,
        /// Pi-style resolved `compat` for chat/completions.
        compat: crate::compat::ResolvedOpenAiCompat,
        /// Whether this model supports extended reasoning (Pi `reasoning`).
        reasoning_model: bool,
        /// Sticky session id for providers that honor affinity headers.
        session_id: String,
    }

    impl OpenAiProvider {
        pub fn from_env() -> Result<Self> {
            let api_key = std::env::var("OPENAI_API_KEY")
                .map_err(|_| OneError::Provider("OPENAI_API_KEY is not set".to_string()))?;
            // ONE_OPENAI_API=openai-completions|openai-responses (optional)
            let wire = std::env::var("ONE_OPENAI_API")
                .ok()
                .and_then(|s| OpenaiWireApi::parse(&s))
                .unwrap_or(OpenaiWireApi::Responses);
            Ok(Self::new(api_key, DEFAULT_MODEL).with_wire_api(wire))
        }

        pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
            Self::with_base(api_key, model, "https://api.openai.com/v1")
                .with_thinking_wire(crate::thinking::ThinkingWire::ReasoningEffort)
        }

        pub fn with_base(
            api_key: impl Into<String>,
            model: impl Into<String>,
            base_url: impl Into<String>,
        ) -> Self {
            let model = model.into();
            let base_url = base_url.into();
            let provider_id = "openai".to_string();
            let compat = crate::compat::OpenAiCompletionsCompat::default().resolve(
                &provider_id,
                &base_url,
                &model,
            );
            Self {
                client: Client::new(),
                api_key: api_key.into(),
                model,
                base_url,
                provider_id,
                // Compatible endpoints (OpenRouter / Ollama) use Completions by default
                // when constructed via with_base; first-party from_env uses Responses.
                wire_api: OpenaiWireApi::Completions,
                // Official-style `reasoning_effort` is the most widely accepted.
                // OpenRouter overrides to ThinkingWire::OpenRouter; callers can set Auto
                // for dual-shape proxies.
                thinking_wire: crate::thinking::ThinkingWire::ReasoningEffort,
                compat,
                reasoning_model: false,
                session_id: crate::cache::new_session_affinity_id(),
            }
        }

        fn apply_request_headers(
            &self,
            req: reqwest::RequestBuilder,
        ) -> reqwest::RequestBuilder {
            let mut req = req.bearer_auth(&self.api_key);
            if self.compat.send_session_affinity_headers {
                use crate::compat::SessionAffinityFormat;
                let header = match self.compat.session_affinity_format.unwrap_or_default() {
                    SessionAffinityFormat::Openrouter => "x-session-id",
                    SessionAffinityFormat::Openai | SessionAffinityFormat::OpenaiNosession => {
                        "x-session-affinity"
                    }
                };
                req = req.header(header, &self.session_id);
            }
            req
        }

        pub fn with_wire_api(mut self, wire_api: OpenaiWireApi) -> Self {
            self.wire_api = wire_api;
            self
        }

        pub fn with_thinking_wire(mut self, thinking_wire: crate::thinking::ThinkingWire) -> Self {
            self.thinking_wire = thinking_wire;
            // Bridge legacy ThinkingWire into compat.thinking_format when set.
            use crate::compat::ThinkingFormat;
            use crate::thinking::ThinkingWire;
            match thinking_wire {
                ThinkingWire::OpenRouter => {
                    self.compat.thinking_format = ThinkingFormat::Openrouter;
                }
                ThinkingWire::ReasoningEffort => {
                    self.compat.thinking_format = ThinkingFormat::Openai;
                }
                ThinkingWire::Off => {
                    self.compat.supports_reasoning_effort = false;
                }
                ThinkingWire::Auto => {
                    // Keep detect / explicit format; Auto is dual-shape at apply time.
                }
            }
            self
        }

        /// Set provider id used for logging / detect refresh.
        pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
            self.provider_id = provider_id.into();
            self
        }

        /// Apply fully resolved Pi `compat` (from models.json + detect).
        pub fn with_compat(mut self, compat: crate::compat::ResolvedOpenAiCompat) -> Self {
            self.compat = compat;
            self
        }

        /// Whether the model supports extended thinking (gates developer role, etc.).
        pub fn with_reasoning_model(mut self, reasoning: bool) -> Self {
            self.reasoning_model = reasoning;
            self
        }

        pub fn wire_api(&self) -> OpenaiWireApi {
            self.wire_api
        }

        pub fn model(&self) -> &str {
            &self.model
        }

        pub fn compat(&self) -> &crate::compat::ResolvedOpenAiCompat {
            &self.compat
        }
    }

    #[async_trait]
    impl LlmProvider for OpenAiProvider {
        fn name(&self) -> &str {
            "openai"
        }

        fn model(&self) -> &str {
            &self.model
        }

        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            match self.wire_api {
                OpenaiWireApi::OpenaiCompletions => {
                    self.complete_chat(request, false, &mut |_| {}, None).await
                }
                OpenaiWireApi::OpenaiResponses => {
                    self.complete_responses(request, false, &mut |_| {}, None).await
                }
                OpenaiWireApi::AnthropicMessages | OpenaiWireApi::GeminiGenerateContent => {
                    Err(OneError::Provider(
                        format!(
                            "{} is not handled by OpenAiProvider (use the native provider)",
                            self.wire_api.as_str()
                        )
                        .into(),
                    ))
                }
            }
        }

        async fn complete_streaming(
            &self,
            request: CompletionRequest,
            on_event: &mut (dyn FnMut(StreamEvent) + Send),
            abort: Option<&AtomicBool>,
        ) -> Result<CompletionResponse> {
            match self.wire_api {
                OpenaiWireApi::OpenaiCompletions => {
                    self.complete_chat(request, true, on_event, abort).await
                }
                OpenaiWireApi::OpenaiResponses => {
                    self.complete_responses(request, true, on_event, abort).await
                }
                OpenaiWireApi::AnthropicMessages | OpenaiWireApi::GeminiGenerateContent => {
                    Err(OneError::Provider(
                        format!(
                            "{} is not handled by OpenAiProvider (use the native provider)",
                            self.wire_api.as_str()
                        )
                        .into(),
                    ))
                }
            }
        }
    }

    // ── Chat Completions ──────────────────────────────────────────────────

    impl OpenAiProvider {
        async fn complete_chat(
            &self,
            request: CompletionRequest,
            stream: bool,
            on_event: &mut (dyn FnMut(StreamEvent) + Send),
            abort: Option<&AtomicBool>,
        ) -> Result<CompletionResponse> {
            let body = build_chat_body(
                &request,
                &self.model,
                stream,
                &self.compat,
                self.reasoning_model,
                self.thinking_wire,
                &self.base_url,
            );
            crate::cache::record_cache_debug(
                &self.provider_id,
                "request",
                Some(&body),
                None,
                Some(json!({
                    "model": self.model,
                    "base_url": self.base_url,
                    "wire": "chat/completions",
                    "cache_control_format": self.compat.cache_control_format,
                })),
            );
            let url = format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            );
            let response = self
                .apply_request_headers(self.client.post(&url))
                .json(&body)
                .send()
                .await
                .map_err(|err| OneError::Provider(err.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                crate::cache::record_cache_debug(
                    &self.provider_id,
                    "error",
                    Some(&body),
                    None,
                    Some(json!({ "status": status.as_u16(), "body_head": text.chars().take(500).collect::<String>() })),
                );
                return Err(OneError::Provider(format!(
                    "openai chat/completions {status}: {text}"
                )));
            }

            if !stream {
                let value: Value = response
                    .json()
                    .await
                    .map_err(|err| OneError::Provider(err.to_string()))?;
                let parsed = parse_chat_non_stream(&value, self.name(), &self.model)?;
                crate::cache::record_cache_debug(
                    &self.provider_id,
                    "response",
                    Some(&body),
                    Some(&parsed.usage),
                    Some(json!({ "model": self.model, "wire": "chat/completions", "stream": false })),
                );
                return Ok(parsed);
            }

            let mut full_text = String::new();
            let mut thinking_text = String::new();
            let mut finish_reason: Option<String> = None;
            let mut tool_acc: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
            let mut usage = TokenUsage::default();

            let aborted = matches!(
                crate::sse::read_sse_response(response, &mut |data| {
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    return;
                };
                let chunk_usage = parse_openai_usage(&value);
                if !chunk_usage.is_zero() {
                    usage = chunk_usage;
                }
                if let Some(reason) = value
                    .pointer("/choices/0/finish_reason")
                    .and_then(|v| v.as_str())
                    .filter(|r| !r.is_empty() && *r != "null")
                {
                    finish_reason = Some(reason.to_string());
                }
                // Open thinking channels used across OpenAI-compat (DeepSeek, OR, …).
                if let Some(delta) = extract_chat_reasoning_delta(&value) {
                    if !delta.is_empty() {
                        thinking_text.push_str(&delta);
                        on_event(StreamEvent::ThinkingDelta(delta));
                    }
                }
                if let Some(delta) = value
                    .pointer("/choices/0/delta/content")
                    .and_then(|v| v.as_str())
                {
                    if !delta.is_empty() {
                        full_text.push_str(delta);
                        on_event(StreamEvent::TextDelta(delta.to_string()));
                    }
                }
                if let Some(arr) = value
                    .pointer("/choices/0/delta/tool_calls")
                    .and_then(|v| v.as_array())
                {
                    for item in arr {
                        let index = item.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let entry = tool_acc.entry(index).or_default();
                        if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                            if !id.is_empty() {
                                entry.id = id.to_string();
                            }
                        }
                        if let Some(name) =
                            item.pointer("/function/name").and_then(|v| v.as_str())
                        {
                            if !name.is_empty() {
                                entry.name = name.to_string();
                            }
                        }
                        if let Some(args) = item
                            .pointer("/function/arguments")
                            .and_then(|v| v.as_str())
                        {
                            entry.arguments.push_str(args);
                        }
                    }
                }
            }, abort)
                .await,
                Err(OneError::Aborted)
            );

            let mut response = assemble_response_with_usage(
                self.name(),
                &self.model,
                full_text,
                thinking_text,
                tool_acc.into_values().collect(),
                finish_reason.as_deref(),
                usage,
            );
            if aborted {
                response.stop_reason = StopReason::Aborted;
            }
            crate::cache::record_cache_debug(
                &self.provider_id,
                "response",
                Some(&body),
                Some(&response.usage),
                Some(json!({
                    "model": self.model,
                    "wire": "chat/completions",
                    "stream": true,
                    "aborted": aborted,
                })),
            );
            Ok(response)
        }
    }

    // ── Responses API ─────────────────────────────────────────────────────

    impl OpenAiProvider {
        async fn complete_responses(
            &self,
            request: CompletionRequest,
            stream: bool,
            on_event: &mut (dyn FnMut(StreamEvent) + Send),
            abort: Option<&AtomicBool>,
        ) -> Result<CompletionResponse> {
            let body = build_responses_body(&request, &self.model, stream);
            let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
            let response = self
                .apply_request_headers(self.client.post(&url))
                .json(&body)
                .send()
                .await
                .map_err(|err| OneError::Provider(err.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(OneError::Provider(format!(
                    "openai responses {status}: {text}"
                )));
            }

            if !stream {
                let value: Value = response
                    .json()
                    .await
                    .map_err(|err| OneError::Provider(err.to_string()))?;
                return parse_responses_non_stream(&value, self.name(), &self.model);
            }

            // Streaming: Responses SSE events (response.output_text.delta, …)
            let mut full_text = String::new();
            let mut thinking_text = String::new();
            let mut tool_acc: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
            let mut status: Option<String> = None;
            let mut usage = TokenUsage::default();

            let aborted = matches!(
                crate::sse::read_sse_response(response, &mut |data| {
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    return;
                };
                let etype = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let chunk_usage = parse_openai_usage(&value);
                if !chunk_usage.is_zero() {
                    usage = chunk_usage;
                }

                match etype {
                    "response.output_text.delta" | "response.refusal.delta" => {
                        if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                            if !delta.is_empty() {
                                full_text.push_str(delta);
                                on_event(StreamEvent::TextDelta(delta.to_string()));
                            }
                        }
                    }
                    // Reasoning summary / text deltas (o-series, GPT-5, …).
                    "response.reasoning_summary_text.delta"
                    | "response.reasoning_text.delta"
                    | "response.reasoning.delta" => {
                        if let Some(delta) = value
                            .get("delta")
                            .and_then(|v| v.as_str())
                            .or_else(|| value.pointer("/delta/text").and_then(|v| v.as_str()))
                        {
                            if !delta.is_empty() {
                                thinking_text.push_str(delta);
                                on_event(StreamEvent::ThinkingDelta(delta.to_string()));
                            }
                        }
                    }
                    "response.output_item.added" => {
                        let index = value
                            .get("output_index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let item = value.get("item");
                        if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                            == Some("function_call")
                        {
                            let entry = tool_acc.entry(index).or_default();
                            if let Some(id) = item
                                .and_then(|i| i.get("call_id"))
                                .and_then(|v| v.as_str())
                            {
                                entry.id = id.to_string();
                            }
                            if let Some(name) =
                                item.and_then(|i| i.get("name")).and_then(|v| v.as_str())
                            {
                                entry.name = name.to_string();
                            }
                            if let Some(args) = item
                                .and_then(|i| i.get("arguments"))
                                .and_then(|v| v.as_str())
                            {
                                entry.arguments = args.to_string();
                            }
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        let index = value
                            .get("output_index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                            tool_acc.entry(index).or_default().arguments.push_str(delta);
                        }
                    }
                    "response.function_call_arguments.done" => {
                        let index = value
                            .get("output_index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        if let Some(args) = value.get("arguments").and_then(|v| v.as_str()) {
                            let entry = tool_acc.entry(index).or_default();
                            // Prefer final full arguments when provided.
                            if !args.is_empty() {
                                entry.arguments = args.to_string();
                            }
                        }
                    }
                    "response.output_item.done" => {
                        let index = value
                            .get("output_index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let item = value.get("item");
                        if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                            == Some("function_call")
                        {
                            let entry = tool_acc.entry(index).or_default();
                            if let Some(id) = item
                                .and_then(|i| i.get("call_id"))
                                .and_then(|v| v.as_str())
                            {
                                if !id.is_empty() {
                                    entry.id = id.to_string();
                                }
                            }
                            if let Some(name) =
                                item.and_then(|i| i.get("name")).and_then(|v| v.as_str())
                            {
                                if !name.is_empty() {
                                    entry.name = name.to_string();
                                }
                            }
                            if let Some(args) = item
                                .and_then(|i| i.get("arguments"))
                                .and_then(|v| v.as_str())
                            {
                                if !args.is_empty() {
                                    entry.arguments = args.to_string();
                                }
                            }
                        } else if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                            == Some("reasoning")
                        {
                            // Final reasoning summary if deltas were empty.
                            if thinking_text.is_empty() {
                                if let Some(summary) =
                                    item.and_then(|i| i.get("summary")).and_then(|s| s.as_array())
                                {
                                    for part in summary {
                                        if let Some(t) = part.get("text").and_then(|x| x.as_str()) {
                                            thinking_text.push_str(t);
                                        }
                                    }
                                }
                                if let Some(t) =
                                    item.and_then(|i| i.get("content")).and_then(|c| c.as_str())
                                {
                                    thinking_text.push_str(t);
                                }
                            }
                        } else if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                            == Some("message")
                        {
                            // Finalize text from completed message if stream deltas were empty.
                            if full_text.is_empty() {
                                if let Some(parts) =
                                    item.and_then(|i| i.get("content")).and_then(|c| c.as_array())
                                {
                                    for p in parts {
                                        let t = p.get("type").and_then(|x| x.as_str());
                                        if t == Some("output_text") || t == Some("refusal") {
                                            if let Some(text) =
                                                p.get("text").and_then(|x| x.as_str())
                                            {
                                                full_text.push_str(text);
                                            }
                                            if let Some(text) =
                                                p.get("refusal").and_then(|x| x.as_str())
                                            {
                                                full_text.push_str(text);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    "response.completed" | "response.incomplete" => {
                        status = value
                            .pointer("/response/status")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                    "response.failed" | "error" => {
                        // Surface later via empty content + error if needed; store message.
                        if let Some(msg) = value
                            .pointer("/response/error/message")
                            .or_else(|| value.get("message"))
                            .and_then(|v| v.as_str())
                        {
                            on_event(StreamEvent::TextDelta(format!("[openai error] {msg}")));
                        }
                    }
                    _ => {}
                }
            }, abort)
                .await,
                Err(OneError::Aborted)
            );

            let finish = match status.as_deref() {
                Some("completed") if !tool_acc.is_empty() => Some("tool_calls"),
                Some("completed") => Some("stop"),
                Some("incomplete") => Some("length"),
                Some("failed") => Some("stop"),
                _ if !tool_acc.is_empty() => Some("tool_calls"),
                _ => Some("stop"),
            };

            let mut response = assemble_response_with_usage(
                self.name(),
                &self.model,
                full_text,
                thinking_text,
                tool_acc.into_values().collect(),
                finish,
                usage,
            );
            if aborted {
                response.stop_reason = StopReason::Aborted;
            }
            Ok(response)
        }
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    #[derive(Default)]
    struct PartialToolCall {
        id: String,
        name: String,
        arguments: String,
    }

    /// Pull reasoning/thinking deltas from chat/completions SSE chunks.
    ///
    /// Covers DeepSeek (`reasoning_content`), OpenRouter (`reasoning`), and
    /// a few nested variants used by proxies.
    fn extract_chat_reasoning_delta(value: &Value) -> Option<String> {
        let delta = value.pointer("/choices/0/delta")?;
        // Plain string fields.
        for key in ["reasoning_content", "reasoning", "thinking"] {
            if let Some(s) = delta.get(key).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        // Nested object: reasoning: { content | text }
        if let Some(obj) = delta.get("reasoning").and_then(|v| v.as_object()) {
            for key in ["content", "text"] {
                if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
        }
        None
    }

    fn extract_chat_reasoning_message(message: &Value) -> String {
        for key in ["reasoning_content", "reasoning", "thinking"] {
            if let Some(s) = message.get(key).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        if let Some(obj) = message.get("reasoning").and_then(|v| v.as_object()) {
            for key in ["content", "text"] {
                if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
            }
        }
        String::new()
    }

    fn merge_openai_usage_details(usage: &mut TokenUsage, u: &Value) {
        usage.input_tokens = u
            .get("prompt_tokens")
            .or_else(|| u.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(usage.input_tokens);
        usage.output_tokens = u
            .get("completion_tokens")
            .or_else(|| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(usage.output_tokens);
        // `cached_tokens` is a subset of prompt/input tokens (OpenAI automatic cache).
        if let Some(details) = u
            .get("prompt_tokens_details")
            .or_else(|| u.get("input_tokens_details"))
        {
            if let Some(n) = details
                .get("cached_tokens")
                .or_else(|| details.get("cache_read_tokens"))
                .and_then(|v| v.as_u64())
            {
                usage.cache_read_tokens = n;
            }
        }
    }

    fn parse_openai_usage(value: &Value) -> TokenUsage {
        let mut usage = TokenUsage::default();
        // Chat Completions: usage.prompt_tokens / completion_tokens
        // Responses API: usage.input_tokens / output_tokens
        if let Some(u) = value.get("usage") {
            merge_openai_usage_details(&mut usage, u);
        }
        // Nested under response.completed for Responses streaming.
        if usage.is_zero() {
            if let Some(u) = value.pointer("/response/usage") {
                merge_openai_usage_details(&mut usage, u);
            }
        }
        usage
    }

    fn assemble_response_with_usage(
        provider: &str,
        model: &str,
        full_text: String,
        thinking_text: String,
        tools: Vec<PartialToolCall>,
        finish_reason: Option<&str>,
        usage: TokenUsage,
    ) -> CompletionResponse {
        let mut content = Vec::new();
        if !thinking_text.is_empty() {
            content.push(ContentBlock::thinking(thinking_text));
        }
        if !full_text.is_empty() {
            content.push(ContentBlock::Text { text: full_text });
        }
        for call in tools {
            if call.name.is_empty() {
                continue;
            }
            let arguments = serde_json::from_str(&call.arguments)
                .unwrap_or_else(|_| json!({ "raw": call.arguments }));
            let id = if call.id.is_empty() {
                format!("call_{}", call.name)
            } else {
                call.id
            };
            content.push(ContentBlock::ToolCall {
                id,
                name: call.name,
                arguments,
            });
        }
        let has_tools = content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. }));
        let stop_reason = match finish_reason {
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::Length,
            Some("stop") if has_tools => StopReason::ToolUse,
            Some("stop") => StopReason::Stop,
            _ if has_tools => StopReason::ToolUse,
            _ => StopReason::Stop,
        };
        CompletionResponse {
            provider: provider.to_string(),
            model: model.to_string(),
            content,
            stop_reason,
            usage,
        }
    }

    fn parse_chat_non_stream(
        value: &Value,
        provider: &str,
        model: &str,
    ) -> Result<CompletionResponse> {
        let message = value
            .pointer("/choices/0/message")
            .cloned()
            .unwrap_or(Value::Null);
        let finish = value
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str());

        let mut tools = Vec::new();
        let text = message
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let thinking = extract_chat_reasoning_message(&message);

        if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
            for call in calls {
                tools.push(PartialToolCall {
                    id: call
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("call")
                        .to_string(),
                    name: call
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    arguments: call
                        .pointer("/function/arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}")
                        .to_string(),
                });
            }
        }

        Ok(assemble_response_with_usage(
            provider,
            model,
            text,
            thinking,
            tools,
            finish,
            parse_openai_usage(value),
        ))
    }

    fn parse_responses_non_stream(
        value: &Value,
        provider: &str,
        model: &str,
    ) -> Result<CompletionResponse> {
        let mut text = String::new();
        let mut thinking = String::new();
        let mut tools = Vec::new();

        if let Some(output) = value.get("output").and_then(|v| v.as_array()) {
            for item in output {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("message") => {
                        if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                            for p in parts {
                                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                                    text.push_str(t);
                                }
                            }
                        }
                    }
                    Some("reasoning") => {
                        if let Some(summary) = item.get("summary").and_then(|s| s.as_array()) {
                            for part in summary {
                                if let Some(t) = part.get("text").and_then(|x| x.as_str()) {
                                    thinking.push_str(t);
                                }
                            }
                        }
                        if let Some(t) = item.get("content").and_then(|c| c.as_str()) {
                            thinking.push_str(t);
                        }
                    }
                    Some("function_call") => {
                        tools.push(PartialToolCall {
                            id: item
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("call")
                                .to_string(),
                            name: item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            arguments: item
                                .get("arguments")
                                .and_then(|v| v.as_str())
                                .unwrap_or("{}")
                                .to_string(),
                        });
                    }
                    _ => {}
                }
            }
        }

        let status = value.get("status").and_then(|v| v.as_str());
        let finish = match status {
            Some("completed") if !tools.is_empty() => Some("tool_calls"),
            Some("completed") => Some("stop"),
            Some("incomplete") => Some("length"),
            _ if !tools.is_empty() => Some("tool_calls"),
            _ => Some("stop"),
        };

        Ok(assemble_response_with_usage(
            provider,
            model,
            text,
            thinking,
            tools,
            finish,
            parse_openai_usage(value),
        ))
    }

    fn build_chat_body(
        request: &CompletionRequest,
        model: &str,
        stream: bool,
        compat: &crate::compat::ResolvedOpenAiCompat,
        reasoning_model: bool,
        thinking_wire: crate::thinking::ThinkingWire,
        base_url: &str,
    ) -> Value {
        let role = compat.system_role(reasoning_model);
        let use_anthropic_cache = compat
            .cache_control_format
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("anthropic"));
        let cache = if use_anthropic_cache {
            Some(crate::cache::anthropic_cache_control(
                compat.supports_long_cache_retention,
            ))
        } else {
            None
        };

        let mut messages = Vec::new();
        if !request.system_prompt.trim().is_empty() {
            // Keep system as string here; cache pass below normalizes + marks it.
            messages.push(json!({
                "role": role,
                "content": request.system_prompt,
            }));
        }

        let mut last_role: Option<String> = None;
        for message in &request.messages {
            // Some proxies reject user messages immediately after tool results.
            if compat.requires_assistant_after_tool_result
                && last_role.as_deref() == Some("tool")
                && matches!(message, one_core::AgentMessage::User(_))
            {
                messages.push(json!({
                    "role": "assistant",
                    "content": "I have processed the tool results.",
                }));
            }
            let mapped = map_chat_messages(message, compat, reasoning_model);
            if let Some(last) = mapped.last() {
                last_role = last
                    .get("role")
                    .and_then(|r| r.as_str())
                    .map(|s| s.to_string());
            }
            messages.extend(mapped);
        }

        // OpenRouter Anthropic cache: stabilize *all* non-tool message shapes first
        // (avoids last-only string→array flip), then place system + tools + suffix markers.
        if let Some(ref c) = cache {
            crate::cache::stabilize_messages_for_cache(&mut messages);
            if let Some(sys) = messages.first_mut() {
                let r = sys.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if r == "system" || r == "developer" {
                    let _ = crate::cache::attach_cache_control_to_message(sys, c);
                }
            }
            let _ = crate::cache::attach_cache_control_to_messages_suffix(&mut messages, c);
        }

        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": stream,
        });

        // Ask providers that support it to emit usage on the final stream chunk.
        if stream && compat.supports_usage_in_streaming {
            body["stream_options"] = json!({ "include_usage": true });
        }

        if compat.supports_store {
            body["store"] = json!(false);
        }

        if !request.tools.is_empty() {
            let mut tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    let mut function = json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    });
                    // Only include `strict` when the provider accepts it.
                    if compat.supports_strict_mode {
                        function["strict"] = json!(false);
                    }
                    json!({
                        "type": "function",
                        "function": function,
                    })
                })
                .collect();
            if cache.is_some() {
                if let Some(ref c) = cache {
                    crate::cache::attach_cache_control_to_last_tool(&mut tools, c);
                }
            }
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }

        // Prefer Pi compat thinkingFormat; fall back to legacy ThinkingWire::Auto dual-shape.
        if matches!(thinking_wire, crate::thinking::ThinkingWire::Auto) {
            crate::thinking::apply_chat_thinking(
                &mut body,
                request.thinking_level,
                thinking_wire,
            );
        } else {
            compat.apply_thinking(&mut body, request.thinking_level, reasoning_model);
        }
        compat.apply_routing_and_extras(&mut body, base_url);

        body
    }

    fn build_responses_body(request: &CompletionRequest, model: &str, stream: bool) -> Value {
        // system prompt goes in `instructions` (Responses style)
        let input = map_responses_input(&request.messages);

        let mut body = json!({
            "model": model,
            "instructions": request.system_prompt,
            "input": input,
            "stream": stream,
            "store": false,
        });

        if !request.tools.is_empty() {
            // Flattened tool schema (not nested under function)
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    })
                })
                .collect();
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }

        crate::thinking::apply_responses_thinking(&mut body, request.thinking_level);

        body
    }

    /// May emit 1–2 chat messages (tool result with images → tool + synthetic user).
    fn map_chat_messages(
        message: &one_core::AgentMessage,
        compat: &crate::compat::ResolvedOpenAiCompat,
        reasoning_model: bool,
    ) -> Vec<Value> {
        match message {
            one_core::AgentMessage::User(user) => {
                vec![json!({
                    "role": "user",
                    "content": crate::media::openai_chat_user_content(user),
                })]
            }
            one_core::AgentMessage::Assistant(assistant) => {
                let text = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");

                let reasoning = crate::thinking::thinking_text(&assistant.content);

                let tool_calls: Vec<Value> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments.to_string(),
                            }
                        })),
                        _ => None,
                    })
                    .collect();

                let mut message = if compat.requires_thinking_as_text && !reasoning.is_empty() {
                    // Fold thinking into text so proxies that reject reasoning fields still work.
                    let combined = if text.is_empty() {
                        reasoning.clone()
                    } else {
                        format!("{reasoning}\n\n{text}")
                    };
                    json!({ "role": "assistant", "content": combined })
                } else {
                    let content_value = if text.is_empty() {
                        // Some providers reject null content when tool_calls are present;
                        // prefer empty string when requires_assistant_after_tool_result.
                        if compat.requires_assistant_after_tool_result {
                            json!("")
                        } else {
                            Value::Null
                        }
                    } else {
                        json!(text)
                    };
                    json!({ "role": "assistant", "content": content_value })
                };

                if !compat.requires_thinking_as_text && !reasoning.is_empty() {
                    // DeepSeek / many OpenAI-compat models require reasoning_content
                    // on assistant turns when reasoning was produced.
                    message["reasoning_content"] = json!(reasoning);
                } else if compat.requires_reasoning_content_on_assistant_messages
                    && reasoning_model
                    && message.get("reasoning_content").is_none()
                {
                    message["reasoning_content"] = json!("");
                }

                if !tool_calls.is_empty() {
                    message["tool_calls"] = json!(tool_calls);
                }

                // Skip empty assistant turns with no tool calls (aborted responses).
                let has_content = match message.get("content") {
                    Some(Value::String(s)) => !s.is_empty(),
                    Some(Value::Null) | None => false,
                    Some(_) => true,
                };
                if !has_content && tool_calls.is_empty() {
                    return Vec::new();
                }

                vec![message]
            }
            one_core::AgentMessage::ToolResult(result) => {
                let mut out = Vec::new();
                let images = crate::media::collect_images(&result.content);
                // Chat Completions `tool` role is string-only — keep labels in tool content.
                let mut tool_msg = json!({
                    "role": "tool",
                    "tool_call_id": result.tool_call_id,
                    "content": crate::media::tool_result_plain(&result.content),
                });
                if compat.requires_tool_result_name {
                    tool_msg["name"] = json!(result.tool_name);
                }
                out.push(tool_msg);
                // Vision path: follow with a user message carrying real image payloads.
                if !images.is_empty() {
                    if compat.requires_assistant_after_tool_result {
                        out.push(json!({
                            "role": "assistant",
                            "content": "I have processed the tool results.",
                        }));
                    }
                    let mut parts = vec![json!({
                        "type": "text",
                        "text": format!(
                            "[images from tool `{}` — see attached]",
                            result.tool_name
                        ),
                    })];
                    for (mime, data) in images {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{mime};base64,{data}")
                            }
                        }));
                    }
                    out.push(json!({
                        "role": "user",
                        "content": parts,
                    }));
                }
                out
            }
        }
    }

    /// Convert agent history into Responses `input` items.
    fn map_responses_input(messages: &[one_core::AgentMessage]) -> Vec<Value> {
        let mut input = Vec::new();
        for message in messages {
            match message {
                one_core::AgentMessage::User(user) => {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": crate::media::openai_responses_user_content(user),
                    }));
                }
                one_core::AgentMessage::Assistant(assistant) => {
                    // Prefer replaying signed reasoning items when we have an id.
                    for block in &assistant.content {
                        if let ContentBlock::Thinking {
                            thinking,
                            signature,
                            redacted,
                        } = block
                        {
                            if *redacted {
                                continue;
                            }
                            if let Some(id) = signature.as_ref().filter(|s| !s.is_empty()) {
                                input.push(json!({
                                    "type": "reasoning",
                                    "id": id,
                                    "summary": [{
                                        "type": "summary_text",
                                        "text": thinking,
                                    }],
                                }));
                            }
                        }
                    }

                    let text = assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");

                    if !text.is_empty() {
                        input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{
                                "type": "output_text",
                                "text": text,
                            }],
                        }));
                    }

                    for block in &assistant.content {
                        if let ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } = block
                        {
                            // call_id is the public id; strip optional |item suffix from Pi-style ids
                            let call_id = id.split('|').next().unwrap_or(id);
                            input.push(json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments.to_string(),
                            }));
                        }
                    }
                }
                one_core::AgentMessage::ToolResult(result) => {
                    let call_id = result
                        .tool_call_id
                        .split('|')
                        .next()
                        .unwrap_or(&result.tool_call_id);
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": crate::media::tool_result_plain(&result.content),
                    }));
                    // Responses: function_call_output is text-only — attach images as user input.
                    let images = crate::media::collect_images(&result.content);
                    if !images.is_empty() {
                        let mut parts = vec![json!({
                            "type": "input_text",
                            "text": format!(
                                "[images from tool `{}` — see attached]",
                                result.tool_name
                            ),
                        })];
                        for (mime, data) in images {
                            parts.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{mime};base64,{data}"),
                            }));
                        }
                        input.push(json!({
                            "type": "message",
                            "role": "user",
                            "content": parts,
                        }));
                    }
                }
            }
        }
        input
    }
}

#[cfg(feature = "http-providers")]
pub use inner::OpenAiProvider;

#[cfg(not(feature = "http-providers"))]
pub struct OpenAiProvider;

#[cfg(not(feature = "http-providers"))]
impl OpenAiProvider {
    pub fn from_env() -> one_core::error::Result<Self> {
        Err(one_core::error::OneError::Provider(
            "rebuild with --features http-providers to enable OpenAI".to_string(),
        ))
    }

    pub fn with_wire_api(self, _wire: OpenaiWireApi) -> Self {
        self
    }

    pub fn with_thinking_wire(self, _wire: crate::thinking::ThinkingWire) -> Self {
        self
    }

    pub fn with_provider_id(self, _provider_id: impl Into<String>) -> Self {
        self
    }

    pub fn with_compat(self, _compat: crate::compat::ResolvedOpenAiCompat) -> Self {
        self
    }

    pub fn with_reasoning_model(self, _reasoning: bool) -> Self {
        self
    }

    pub fn with_base(
        _api_key: impl Into<String>,
        _model: impl Into<String>,
        _base_url: impl Into<String>,
    ) -> Self {
        Self
    }
}
