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
                if let Some(content) = event.message.and_then(|m| m.content) {
                    if !content.is_empty() {
                        full_text.push_str(&content);
                        on_event(StreamEvent::TextDelta(content));
                    }
                }
            }
        }

        Ok(CompletionResponse {
            provider: self.name().to_string(),
            model: self.model.clone(),
            content: vec![ContentBlock::Text { text: full_text }],
            stop_reason: if aborted {
                StopReason::Aborted
            } else {
                StopReason::Stop
            },
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

    json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    })
}

fn map_message(message: &one_core::AgentMessage) -> Option<Value> {
    match message {
        one_core::AgentMessage::User(user) => {
            let text = match &user.content {
                one_core::message::UserContent::Text(text) => text.clone(),
                one_core::message::UserContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        one_core::message::TextOrImage::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            Some(json!({"role":"user","content":text}))
        }
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
            Some(json!({"role":"assistant","content":text}))
        }
        one_core::AgentMessage::ToolResult(result) => Some(json!({
            "role": "user",
            "content": format!("[tool:{}] {}", result.tool_name,
                result.content.iter().filter_map(|b| match b {
                    one_core::message::TextOrImage::Text { text } => Some(text.clone()),
                    _ => None,
                }).collect::<Vec<_>>().join("\n"))
        })),
    }
}

#[derive(Debug, Deserialize)]
struct OllamaStreamEvent {
    message: Option<OllamaMessage>,
}

#[derive(Debug, Deserialize)]
struct OllamaMessage {
    content: Option<String>,
}