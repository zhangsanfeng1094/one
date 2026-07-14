//! OpenAI provider with selectable wire API (Pi-style).
//!
//! - [`OpenaiWireApi::Completions`] → `POST /v1/chat/completions`
//! - [`OpenaiWireApi::Responses`]   → `POST /v1/responses`  (default for official OpenAI)
//!
//! Configured via constructor / CLI `--openai-api` / `models.json` `api` field.

use serde::{Deserialize, Serialize};

/// Which OpenAI-compatible HTTP API to call.
///
/// Mirrors Pi's `api` field: `openai-completions` | `openai-responses`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenaiWireApi {
    /// Chat Completions — most compatible (Ollama, OpenRouter, proxies).
    #[serde(alias = "openai-completions", alias = "chat-completions", alias = "completions")]
    Completions,
    /// Responses API — default for first-party OpenAI models (Pi default).
    #[default]
    #[serde(alias = "openai-responses", alias = "responses")]
    Responses,
}

impl OpenaiWireApi {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completions => "openai-completions",
            Self::Responses => "openai-responses",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai-completions" | "chat-completions" | "completions" | "chat" => {
                Some(Self::Completions)
            }
            "openai-responses" | "responses" | "response" => Some(Self::Responses),
            _ => None,
        }
    }
}

impl std::fmt::Display for OpenaiWireApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod wire_api_tests {
    use super::OpenaiWireApi;

    #[test]
    fn parse_aliases() {
        assert_eq!(
            OpenaiWireApi::parse("openai-responses"),
            Some(OpenaiWireApi::Responses)
        );
        assert_eq!(
            OpenaiWireApi::parse("completions"),
            Some(OpenaiWireApi::Completions)
        );
        assert_eq!(
            OpenaiWireApi::parse("chat-completions"),
            Some(OpenaiWireApi::Completions)
        );
        assert_eq!(OpenaiWireApi::parse("nope"), None);
    }
}

#[cfg(feature = "http-providers")]
mod inner {
    use std::collections::BTreeMap;

    use std::sync::atomic::AtomicBool;

    use async_trait::async_trait;
    use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider};
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
        /// Chat Completions vs Responses (configurable).
        wire_api: OpenaiWireApi,
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
        }

        pub fn with_base(
            api_key: impl Into<String>,
            model: impl Into<String>,
            base_url: impl Into<String>,
        ) -> Self {
            Self {
                client: Client::new(),
                api_key: api_key.into(),
                model: model.into(),
                base_url: base_url.into(),
                // Compatible endpoints (OpenRouter / Ollama) use Completions by default
                // when constructed via with_base; first-party from_env uses Responses.
                wire_api: OpenaiWireApi::Completions,
            }
        }

        pub fn with_wire_api(mut self, wire_api: OpenaiWireApi) -> Self {
            self.wire_api = wire_api;
            self
        }

        pub fn wire_api(&self) -> OpenaiWireApi {
            self.wire_api
        }

        pub fn model(&self) -> &str {
            &self.model
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
                OpenaiWireApi::Completions => {
                    self.complete_chat(request, false, &mut |_| {}, None).await
                }
                OpenaiWireApi::Responses => {
                    self.complete_responses(request, false, &mut |_| {}, None).await
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
                OpenaiWireApi::Completions => {
                    self.complete_chat(request, true, on_event, abort).await
                }
                OpenaiWireApi::Responses => {
                    self.complete_responses(request, true, on_event, abort).await
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
            let body = build_chat_body(&request, &self.model, stream);
            let url = format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            );
            let response = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|err| OneError::Provider(err.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(OneError::Provider(format!(
                    "openai chat/completions {status}: {text}"
                )));
            }

            if !stream {
                let value: Value = response
                    .json()
                    .await
                    .map_err(|err| OneError::Provider(err.to_string()))?;
                return parse_chat_non_stream(&value, self.name(), &self.model);
            }

            let mut full_text = String::new();
            let mut finish_reason: Option<String> = None;
            let mut tool_acc: BTreeMap<usize, PartialToolCall> = BTreeMap::new();

            let aborted = matches!(
                crate::sse::read_sse_response(response, &mut |data| {
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    return;
                };
                if let Some(reason) = value
                    .pointer("/choices/0/finish_reason")
                    .and_then(|v| v.as_str())
                    .filter(|r| !r.is_empty() && *r != "null")
                {
                    finish_reason = Some(reason.to_string());
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

            let mut response = assemble_response(
                self.name(),
                &self.model,
                full_text,
                tool_acc.into_values().collect(),
                finish_reason.as_deref(),
            );
            if aborted {
                response.stop_reason = StopReason::Aborted;
            }
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
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
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
            let mut tool_acc: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
            let mut status: Option<String> = None;

            let aborted = matches!(
                crate::sse::read_sse_response(response, &mut |data| {
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    return;
                };
                let etype = value.get("type").and_then(|t| t.as_str()).unwrap_or("");

                match etype {
                    "response.output_text.delta" | "response.refusal.delta" => {
                        if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                            if !delta.is_empty() {
                                full_text.push_str(delta);
                                on_event(StreamEvent::TextDelta(delta.to_string()));
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

            let mut response = assemble_response(
                self.name(),
                &self.model,
                full_text,
                tool_acc.into_values().collect(),
                finish,
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

    fn assemble_response(
        provider: &str,
        model: &str,
        full_text: String,
        tools: Vec<PartialToolCall>,
        finish_reason: Option<&str>,
    ) -> CompletionResponse {
        let mut content = Vec::new();
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

        Ok(assemble_response(provider, model, text, tools, finish))
    }

    fn parse_responses_non_stream(
        value: &Value,
        provider: &str,
        model: &str,
    ) -> Result<CompletionResponse> {
        let mut text = String::new();
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

        Ok(assemble_response(provider, model, text, tools, finish))
    }

    fn build_chat_body(request: &CompletionRequest, model: &str, stream: bool) -> Value {
        let mut messages = vec![json!({
            "role": "system",
            "content": request.system_prompt,
        })];
        messages.extend(request.messages.iter().filter_map(map_chat_message));

        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": stream,
        });

        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }

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

        body
    }

    fn map_chat_message(message: &one_core::AgentMessage) -> Option<Value> {
        match message {
            one_core::AgentMessage::User(user) => {
                let text = user_text(user);
                Some(json!({ "role": "user", "content": text }))
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

                let content_value = if text.is_empty() {
                    Value::Null
                } else {
                    json!(text)
                };
                let mut message = json!({ "role": "assistant", "content": content_value });
                if !tool_calls.is_empty() {
                    message["tool_calls"] = json!(tool_calls);
                }
                Some(message)
            }
            one_core::AgentMessage::ToolResult(result) => Some(json!({
                "role": "tool",
                "tool_call_id": result.tool_call_id,
                "content": tool_result_text(result),
            })),
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
                        "content": user_text(user),
                    }));
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
                        "output": tool_result_text(result),
                    }));
                }
            }
        }
        input
    }

    fn user_text(user: &one_core::message::UserMessage) -> String {
        match &user.content {
            one_core::message::UserContent::Text(text) => text.clone(),
            one_core::message::UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    one_core::message::TextOrImage::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn tool_result_text(result: &one_core::message::ToolResultMessage) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                one_core::message::TextOrImage::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
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
}
