//! OpenAI Codex provider — ChatGPT subscription backend (`openai-codex-responses`).
//!
//! POST `https://chatgpt.com/backend-api/codex/responses` with:
//! - `Authorization: Bearer <oauth access>`
//! - `chatgpt-account-id`
//! - `OpenAI-Beta: responses=experimental`
//!
//! Request/response shape matches OpenAI Responses (SSE). Reuses the Responses
//! body/SSE path from [`crate::openai`] via a thin dedicated client.

#[cfg(feature = "http-providers")]
mod inner {
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicBool;

    use async_trait::async_trait;
    use one_core::agent::{
        Citation, CompletionRequest, CompletionResponse, LlmProvider, ServerTool, TokenUsage,
    };
    use one_core::error::{OneError, Result};
    use one_core::message::{ContentBlock, StopReason};
    use one_core::streaming::{ServerToolStatus, StreamEvent};
    use reqwest::Client;
    use serde_json::{json, Value};

    use crate::auth::{extract_account_id, CODEX_BASE_URL, PROVIDER_OPENAI_CODEX};
    use crate::openai::OpenAiProvider;

    /// Codex subscription provider (ChatGPT Plus/Pro OAuth).
    pub struct OpenAiCodexProvider {
        /// Reuse OpenAI Responses encoding + SSE parsing.
        inner: OpenAiProvider,
        client: Client,
        api_key: String,
        account_id: String,
        model: String,
        base_url: String,
        session_id: String,
    }

    impl OpenAiCodexProvider {
        pub fn new(
            api_key: impl Into<String>,
            model: impl Into<String>,
            account_id: impl Into<String>,
        ) -> Self {
            Self::with_base(api_key, model, account_id, CODEX_BASE_URL)
        }

        pub fn with_base(
            api_key: impl Into<String>,
            model: impl Into<String>,
            account_id: impl Into<String>,
            base_url: impl Into<String>,
        ) -> Self {
            let api_key = api_key.into();
            let model = model.into();
            let account_id = {
                let id = account_id.into();
                if id.is_empty() {
                    extract_account_id(&api_key).unwrap_or_default()
                } else {
                    id
                }
            };
            let base_url = base_url.into();
            // Wire through OpenAiProvider only for body helpers / model name surface.
            let inner = OpenAiProvider::with_base(api_key.clone(), model.clone(), base_url.clone())
                .with_wire_api(crate::openai::OpenaiWireApi::Responses)
                .with_provider_id(PROVIDER_OPENAI_CODEX)
                .with_reasoning_model(true);

            Self {
                inner,
                client: Client::new(),
                api_key,
                account_id,
                model,
                base_url,
                session_id: crate::cache::new_session_affinity_id(),
            }
        }

        /// Build from OAuth access token (account id decoded from JWT when missing).
        pub fn from_oauth_token(
            access_token: impl Into<String>,
            model: impl Into<String>,
            account_id: Option<String>,
        ) -> Result<Self> {
            let access = access_token.into();
            let account = account_id
                .or_else(|| extract_account_id(&access))
                .ok_or_else(|| {
                    OneError::Provider(
                        "openai-codex: missing chatgpt_account_id in OAuth token".into(),
                    )
                })?;
            Ok(Self::new(access, model, account))
        }

        fn codex_url(&self) -> String {
            resolve_codex_url(&self.base_url)
        }

        fn apply_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
            let ua = format!("one ({}/{})", std::env::consts::OS, std::env::consts::ARCH);
            req.bearer_auth(&self.api_key)
                .header("chatgpt-account-id", &self.account_id)
                .header("OpenAI-Beta", "responses=experimental")
                .header("originator", "one")
                .header("User-Agent", ua)
                .header("session-id", &self.session_id)
                .header("x-client-request-id", &self.session_id)
                .header("accept", "text/event-stream")
        }
    }

    #[async_trait]
    impl LlmProvider for OpenAiCodexProvider {
        fn name(&self) -> &str {
            PROVIDER_OPENAI_CODEX
        }

        fn model(&self) -> &str {
            &self.model
        }

        fn server_tools(&self) -> Vec<one_core::agent::ServerTool> {
            crate::openai::server_tools_for(
                crate::openai::ProviderApi::OpenaiResponses,
                &self.model,
            )
        }

        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            self.complete_codex(request, false, &mut |_| {}, None).await
        }

        async fn complete_streaming(
            &self,
            request: CompletionRequest,
            on_event: &mut (dyn FnMut(StreamEvent) + Send),
            abort: Option<&AtomicBool>,
        ) -> Result<CompletionResponse> {
            self.complete_codex(request, true, on_event, abort).await
        }
    }

    impl OpenAiCodexProvider {
        async fn complete_codex(
            &self,
            mut request: CompletionRequest,
            stream: bool,
            on_event: &mut (dyn FnMut(StreamEvent) + Send),
            abort: Option<&AtomicBool>,
        ) -> Result<CompletionResponse> {
            if self.account_id.is_empty() {
                return Err(OneError::Provider(
                    "openai-codex: account id required (run /login openai-codex)".into(),
                ));
            }

            let url = self.codex_url();
            let mut attempt = 0usize;
            let response = loop {
                let body = build_codex_body(&request, &self.model, stream, &self.session_id);
                crate::cache::record_cache_debug(
                    PROVIDER_OPENAI_CODEX,
                    "request",
                    Some(&body),
                    None,
                    Some(json!({
                        "model": self.model,
                        "base_url": self.base_url,
                        "wire": "openai-codex-responses",
                        "stream": stream,
                    })),
                );
                let response = crate::sse::send_with_abort(
                    self.apply_headers(self.client.post(&url)).json(&body),
                    abort,
                )
                .await?;
                if response.status().is_success() {
                    break response;
                }
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                if attempt == 0
                    && !request.server_tools.is_empty()
                    && should_fallback_server_tools(
                        status.as_u16(),
                        &text,
                        abort.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)),
                    )
                {
                    request.server_tools.clear();
                    attempt += 1;
                    continue;
                }
                return Err(OneError::Provider(format!("openai-codex {status}: {text}")));
            };

            // Non-stream: parse final JSON (rare for Codex which prefers stream).
            if !stream {
                let value: Value = response
                    .json()
                    .await
                    .map_err(|err| OneError::Provider(err.to_string()))?;
                return parse_codex_non_stream(&value, self.name(), &self.model);
            }

            // Stream via Responses SSE events (same as OpenAI Responses).
            let mut full_text = String::new();
            let mut thinking_text = String::new();
            let mut tool_acc: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
            let mut status: Option<String> = None;
            let mut usage = TokenUsage::default();
            let mut citations = Vec::new();

            let aborted = matches!(
                crate::sse::read_sse_response(
                    response,
                    &mut |data| {
                        let Ok(value) = serde_json::from_str::<Value>(data) else {
                            return;
                        };
                        let etype = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        let chunk_usage = parse_usage(&value);
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
                            "response.reasoning_summary_text.delta"
                            | "response.reasoning_text.delta"
                            | "response.reasoning.delta" => {
                                if let Some(delta) =
                                    value.get("delta").and_then(|v| v.as_str()).or_else(|| {
                                        value.pointer("/delta/text").and_then(|v| v.as_str())
                                    })
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
                                    .unwrap_or(0)
                                    as usize;
                                let item = value.get("item");
                                if let Some((tool, status)) =
                                    item.and_then(|item| server_tool_update(item, false))
                                {
                                    on_event(StreamEvent::ServerTool { tool, status });
                                }
                                if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                                    == Some("function_call")
                                {
                                    let entry = tool_acc.entry(index).or_default();
                                    if let Some(id) =
                                        item.and_then(|i| i.get("call_id")).and_then(|v| v.as_str())
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
                                    .unwrap_or(0)
                                    as usize;
                                if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                                    tool_acc.entry(index).or_default().arguments.push_str(delta);
                                }
                            }
                            "response.function_call_arguments.done" => {
                                let index = value
                                    .get("output_index")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as usize;
                                if let Some(args) = value.get("arguments").and_then(|v| v.as_str())
                                {
                                    let entry = tool_acc.entry(index).or_default();
                                    if !args.is_empty() {
                                        entry.arguments = args.to_string();
                                    }
                                }
                            }
                            "response.output_item.done" => {
                                let index = value
                                    .get("output_index")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as usize;
                                let item = value.get("item");
                                if let Some((tool, status)) =
                                    item.and_then(|item| server_tool_update(item, true))
                                {
                                    on_event(StreamEvent::ServerTool { tool, status });
                                }
                                if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                                    == Some("function_call")
                                {
                                    let entry = tool_acc.entry(index).or_default();
                                    if let Some(id) =
                                        item.and_then(|i| i.get("call_id")).and_then(|v| v.as_str())
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
                                    if thinking_text.is_empty() {
                                        if let Some(summary) = item
                                            .and_then(|i| i.get("summary"))
                                            .and_then(|s| s.as_array())
                                        {
                                            for part in summary {
                                                if let Some(t) =
                                                    part.get("text").and_then(|x| x.as_str())
                                                {
                                                    thinking_text.push_str(t);
                                                }
                                            }
                                        }
                                    }
                                } else if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                                    == Some("message")
                                {
                                    if let Some(item) = item {
                                        collect_completed_message(
                                            item,
                                            &mut full_text,
                                            &mut citations,
                                        );
                                    }
                                }
                            }
                            "response.completed" | "response.done" | "response.incomplete" => {
                                status = value
                                    .pointer("/response/status")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                                    .or_else(|| {
                                        value
                                            .get("status")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string())
                                    });
                                if let Some(u) = value.pointer("/response/usage") {
                                    let mut uu = TokenUsage::default();
                                    merge_usage(&mut uu, u);
                                    if !uu.is_zero() {
                                        usage = uu;
                                    }
                                }
                            }
                            "response.failed" | "error" => {
                                if let Some(msg) = value
                                    .pointer("/response/error/message")
                                    .or_else(|| value.get("message"))
                                    .and_then(|v| v.as_str())
                                {
                                    on_event(StreamEvent::TextDelta(format!(
                                        "[openai-codex error] {msg}"
                                    )));
                                }
                            }
                            _ => {}
                        }
                    },
                    abort,
                )
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

            let mut response = assemble(
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
            response.citations = citations;

            // Silence unused field warning for inner (kept for future shared helpers).
            let _ = self.inner.model();

            Ok(response)
        }
    }

    #[derive(Default)]
    struct PartialToolCall {
        id: String,
        name: String,
        arguments: String,
    }

    fn resolve_codex_url(base_url: &str) -> String {
        let raw = if base_url.trim().is_empty() {
            CODEX_BASE_URL
        } else {
            base_url.trim()
        };
        let normalized = raw.trim_end_matches('/');
        if normalized.ends_with("/codex/responses") {
            normalized.to_string()
        } else if normalized.ends_with("/codex") {
            format!("{normalized}/responses")
        } else {
            format!("{normalized}/codex/responses")
        }
    }

    fn build_codex_body(
        request: &CompletionRequest,
        model: &str,
        stream: bool,
        session_id: &str,
    ) -> Value {
        // Build Responses-style input via temporary OpenAiProvider helpers by
        // constructing the same shape OpenAI Responses uses.
        let input = map_responses_input(&request.messages);
        let instructions = if request.system_prompt.trim().is_empty() {
            "You are a helpful assistant."
        } else {
            request.system_prompt.as_str()
        };

        let mut body = json!({
            "model": model,
            "store": false,
            "stream": stream,
            "instructions": instructions,
            "input": input,
            "text": { "verbosity": "low" },
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": session_id,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });

        let native_web_search = request.server_tools.contains(&ServerTool::WebSearch);
        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .filter(|tool| !(native_web_search && tool.name == "web_search"))
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect();
        for tool in &request.server_tools {
            tools.push(match tool {
                ServerTool::WebSearch => json!({"type":"web_search"}),
                ServerTool::XSearch => json!({"type":"x_search"}),
            });
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if native_web_search {
            body["include"]
                .as_array_mut()
                .expect("include array")
                .push(json!("web_search_call.action.sources"));
        }

        crate::thinking::apply_responses_thinking(&mut body, request.thinking_level);
        body
    }

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

    fn parse_codex_non_stream(
        value: &Value,
        provider: &str,
        model: &str,
    ) -> Result<CompletionResponse> {
        let mut text = String::new();
        let mut thinking = String::new();
        let mut tools = Vec::new();
        let mut citations = Vec::new();

        if let Some(output) = value.get("output").and_then(|v| v.as_array()) {
            for item in output {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("message") => {
                        if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                            for p in parts {
                                append_citations(p, &mut citations);
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

        let mut response = assemble(
            provider,
            model,
            text,
            thinking,
            tools,
            finish,
            parse_usage(value),
        );
        response.citations = citations;
        Ok(response)
    }

    fn parse_usage(value: &Value) -> TokenUsage {
        let mut usage = TokenUsage::default();
        if let Some(u) = value.get("usage") {
            merge_usage(&mut usage, u);
        }
        if usage.is_zero() {
            if let Some(u) = value.pointer("/response/usage") {
                merge_usage(&mut usage, u);
            }
        }
        usage
    }

    fn merge_usage(usage: &mut TokenUsage, u: &Value) {
        usage.input_tokens = u
            .get("input_tokens")
            .or_else(|| u.get("prompt_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(usage.input_tokens);
        usage.output_tokens = u
            .get("output_tokens")
            .or_else(|| u.get("completion_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(usage.output_tokens);
        if let Some(details) = u
            .get("input_tokens_details")
            .or_else(|| u.get("prompt_tokens_details"))
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

    fn assemble(
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
            let arguments = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
            let id = if call.id.is_empty() {
                format!("call_{}", uuid_ish())
            } else {
                call.id
            };
            content.push(ContentBlock::ToolCall {
                id,
                name: call.name,
                arguments,
            });
        }
        let stop_reason = match finish_reason {
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::Length,
            Some("aborted") => StopReason::Aborted,
            _ => StopReason::Stop,
        };
        CompletionResponse {
            provider: provider.into(),
            model: model.into(),
            content,
            stop_reason,
            usage,
            citations: Vec::new(),
        }
    }

    fn uuid_ish() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{n:x}")
    }

    fn should_fallback_server_tools(status: u16, body: &str, cancelled: bool) -> bool {
        if cancelled || !matches!(status, 400 | 404 | 422) {
            return false;
        }
        let body = body.to_ascii_lowercase();
        let unsupported = body.contains("unsupported")
            || body.contains("not supported")
            || body.contains("unknown")
            || body.contains("unrecognized")
            || body.contains("invalid tool");
        let search_tool =
            body.contains("tool") || body.contains("web_search") || body.contains("x_search");
        unsupported && search_tool
    }

    fn server_tool_update(
        item: &Value,
        done_event: bool,
    ) -> Option<(ServerTool, ServerToolStatus)> {
        let tool = match item.get("type").and_then(Value::as_str)? {
            "web_search_call" => ServerTool::WebSearch,
            "x_search_call" => ServerTool::XSearch,
            _ => return None,
        };
        let status = match item.get("status").and_then(Value::as_str) {
            Some("failed") | Some("error") => ServerToolStatus::Failed,
            Some("completed") | Some("done") => ServerToolStatus::Completed,
            _ if done_event => ServerToolStatus::Completed,
            _ => ServerToolStatus::Started,
        };
        Some((tool, status))
    }

    fn append_citations(part: &Value, citations: &mut Vec<Citation>) {
        let Some(annotations) = part.get("annotations").and_then(Value::as_array) else {
            return;
        };
        for annotation in annotations {
            if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
                continue;
            }
            let source = annotation.get("url_citation").unwrap_or(annotation);
            let Some(url) = source.get("url").and_then(Value::as_str) else {
                continue;
            };
            let citation = Citation {
                url: url.to_string(),
                title: source
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(url)
                    .to_string(),
                start_index: source
                    .get("start_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize,
                end_index: source.get("end_index").and_then(Value::as_u64).unwrap_or(0) as usize,
            };
            if !citations.contains(&citation) {
                citations.push(citation);
            }
        }
    }

    fn collect_completed_message(
        item: &Value,
        full_text: &mut String,
        citations: &mut Vec<Citation>,
    ) {
        let collect_text = full_text.is_empty();
        let Some(parts) = item.get("content").and_then(Value::as_array) else {
            return;
        };
        for part in parts {
            append_citations(part, citations);
            if !collect_text {
                continue;
            }
            let part_type = part.get("type").and_then(Value::as_str);
            if part_type == Some("output_text") || part_type == Some("refusal") {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    full_text.push_str(text);
                }
                if let Some(text) = part.get("refusal").and_then(Value::as_str) {
                    full_text.push_str(text);
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use one_core::agent::{ServerTool, ThinkingLevel};
        use one_core::tool::ToolDefinition;

        #[test]
        fn resolve_url_variants() {
            assert_eq!(
                resolve_codex_url("https://chatgpt.com/backend-api"),
                "https://chatgpt.com/backend-api/codex/responses"
            );
            assert_eq!(
                resolve_codex_url("https://chatgpt.com/backend-api/codex"),
                "https://chatgpt.com/backend-api/codex/responses"
            );
            assert_eq!(
                resolve_codex_url("https://chatgpt.com/backend-api/codex/responses"),
                "https://chatgpt.com/backend-api/codex/responses"
            );
        }

        #[test]
        fn codex_body_injects_native_search_and_filters_local_search() {
            let request = CompletionRequest {
                system_prompt: "sys".into(),
                messages: vec![one_core::AgentMessage::user_text("search")],
                tools: vec![
                    ToolDefinition {
                        name: "web_search".into(),
                        description: "local".into(),
                        parameters: json!({"type":"object"}),
                    },
                    ToolDefinition {
                        name: "web_fetch".into(),
                        description: "fetch".into(),
                        parameters: json!({"type":"object"}),
                    },
                ],
                server_tools: vec![ServerTool::WebSearch],
                thinking_level: ThinkingLevel::Off,
            };
            let body = build_codex_body(&request, "gpt-5", true, "session");
            let tools = body["tools"].as_array().unwrap();
            assert!(tools.iter().any(|tool| tool["type"] == "web_search"));
            assert!(tools
                .iter()
                .any(|tool| tool["type"] == "function" && tool["name"] == "web_fetch"));
            assert!(!tools
                .iter()
                .any(|tool| tool["type"] == "function" && tool["name"] == "web_search"));
            assert!(body["include"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "web_search_call.action.sources"));
        }

        #[test]
        fn codex_non_stream_parses_citations() {
            let response = parse_codex_non_stream(
                &json!({
                    "status":"completed",
                    "output":[{
                        "type":"message",
                        "content":[{
                            "type":"output_text",
                            "text":"answer",
                            "annotations":[{
                                "type":"url_citation",
                                "url":"https://example.com",
                                "title":"Example",
                                "start_index":0,
                                "end_index":6
                            }]
                        }]
                    }]
                }),
                "openai-codex",
                "gpt-5",
            )
            .unwrap();
            assert_eq!(response.citations.len(), 1);
            assert_eq!(response.citations[0].url, "https://example.com");
        }

        #[test]
        fn codex_completed_message_collects_annotations_after_text_deltas() {
            let mut text = "answer".to_string();
            let mut citations = Vec::new();
            collect_completed_message(
                &json!({
                    "type": "message",
                    "content": [{
                        "type": "output_text",
                        "text": "answer",
                        "annotations": [{
                            "type": "url_citation",
                            "url": "https://example.com/stream",
                            "title": "Stream source",
                            "start_index": 0,
                            "end_index": 6
                        }]
                    }]
                }),
                &mut text,
                &mut citations,
            );
            assert_eq!(text, "answer");
            assert_eq!(citations.len(), 1);
        }
    }
}

#[cfg(feature = "http-providers")]
pub use inner::OpenAiCodexProvider;

#[cfg(not(feature = "http-providers"))]
pub struct OpenAiCodexProvider;

#[cfg(not(feature = "http-providers"))]
impl OpenAiCodexProvider {
    pub fn new(
        _api_key: impl Into<String>,
        _model: impl Into<String>,
        _account_id: impl Into<String>,
    ) -> Self {
        Self
    }
}
