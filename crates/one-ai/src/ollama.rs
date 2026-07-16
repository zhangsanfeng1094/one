use async_trait::async_trait;
use futures::StreamExt;
use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider};
use one_core::error::{OneError, Result};
use one_core::message::{ContentBlock, StopReason};
use std::sync::atomic::{AtomicBool, Ordering};

use one_core::streaming::StreamEvent;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_MODEL: &str = "llama3.2";
const DEFAULT_BASE: &str = "http://127.0.0.1:11434";

pub struct OllamaProvider {
    client: Client,
    base_url: String,
    model: String,
}

impl OllamaProvider {
    pub fn from_env() -> Self {
        let base_url = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| DEFAULT_BASE.to_string());
        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Self::new(base_url, model)
    }

    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
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
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
        let body = build_body(&request, &self.model, true);

        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| OneError::Provider(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(OneError::Provider(format!("ollama {status}: {text}")));
        }

        let mut stream = response.bytes_stream();
        let mut full_text = String::new();
        let mut thinking_text = String::new();
        let mut aborted = false;

        while let Some(chunk) = stream.next().await {
            if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                aborted = true;
                break;
            }
            let chunk = chunk.map_err(|e| OneError::Provider(e.to_string()))?;
            for line in chunk.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let event: OllamaStreamEvent = match serde_json::from_slice(line) {
                    Ok(event) => event,
                    Err(_) => continue,
                };
                if let Some(msg) = event.message {
                    if let Some(thinking) = msg.thinking {
                        if !thinking.is_empty() {
                            thinking_text.push_str(&thinking);
                            on_event(StreamEvent::ThinkingDelta(thinking));
                        }
                    }
                    if let Some(content) = msg.content {
                        if !content.is_empty() {
                            full_text.push_str(&content);
                            on_event(StreamEvent::TextDelta(content));
                        }
                    }
                }
            }
        }

        let mut content = Vec::new();
        if !thinking_text.is_empty() {
            content.push(ContentBlock::thinking(thinking_text));
        }
        if !full_text.is_empty() {
            content.push(ContentBlock::Text { text: full_text });
        }

        Ok(CompletionResponse {
            provider: self.name().to_string(),
            model: self.model.clone(),
            content,
            stop_reason: if aborted {
                StopReason::Aborted
            } else {
                StopReason::Stop
            },
            usage: one_core::agent::TokenUsage::default(),
        })
    }
}

fn build_body(request: &CompletionRequest, model: &str, stream: bool) -> Value {
    let messages: Vec<Value> = std::iter::once(json!({
        "role": "system",
        "content": request.system_prompt,
    }))
    .chain(request.messages.iter().filter_map(map_message))
    .collect();

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    });
    crate::thinking::apply_ollama_thinking(&mut body, request.thinking_level);
    body
}

fn map_message(message: &one_core::AgentMessage) -> Option<Value> {
    match message {
        one_core::AgentMessage::User(user) => Some(crate::media::ollama_user_message(user)),
        one_core::AgentMessage::Assistant(assistant) => {
            let text = assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            let thinking = crate::thinking::thinking_text(&assistant.content);
            let mut msg = json!({"role":"assistant","content":text});
            if !thinking.is_empty() {
                // Ollama think models accept prior thinking for continuity.
                msg["thinking"] = json!(thinking);
            }
            Some(msg)
        }
        one_core::AgentMessage::ToolResult(result) => {
            let text = crate::media::tool_result_plain(&result.content);
            let images: Vec<String> = crate::media::collect_images(&result.content)
                .into_iter()
                .map(|(_, data)| data)
                .collect();
            if images.is_empty() {
                Some(json!({
                    "role": "user",
                    "content": format!("[tool:{}] {}", result.tool_name, text),
                }))
            } else {
                Some(json!({
                    "role": "user",
                    "content": format!("[tool:{}] {}", result.tool_name, text),
                    "images": images,
                }))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct OllamaStreamEvent {
    message: Option<OllamaMessage>,
}

#[derive(Debug, Deserialize)]
struct OllamaMessage {
    content: Option<String>,
    /// Native think channel (DeepSeek-R1 / QwQ / etc. via Ollama).
    #[serde(default)]
    thinking: Option<String>,
}