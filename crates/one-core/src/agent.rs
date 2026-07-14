use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;

use crate::error::{OneError, Result};
use crate::events::{AgentEvent, EventListener};
use crate::message::{
    AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolResultMessage,
};
use crate::tool::{Tool, ToolCall, ToolOutput};

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are an AI coding assistant. Use the provided tools to read, write, edit files, run shell commands, and search or fetch the web when you need current information. Be concise and precise.";

/// Reasoning / extended-thinking intensity (provider-specific mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "0" => Some(ThinkingLevel::Off),
            "low" | "1" => Some(ThinkingLevel::Low),
            "medium" | "med" | "2" => Some(ThinkingLevel::Medium),
            "high" | "3" => Some(ThinkingLevel::High),
            _ => None,
        }
    }

    pub fn cycle_next(self) -> Self {
        match self {
            ThinkingLevel::Off => ThinkingLevel::Low,
            ThinkingLevel::Low => ThinkingLevel::Medium,
            ThinkingLevel::Medium => ThinkingLevel::High,
            ThinkingLevel::High => ThinkingLevel::Off,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: String,
    pub max_turns: usize,
    pub thinking_level: ThinkingLevel,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            max_turns: 32,
            thinking_level: ThinkingLevel::Off,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<crate::tool::ToolDefinition>,
    pub thinking_level: ThinkingLevel,
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub provider: String,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;

    async fn complete_streaming(
        &self,
        request: CompletionRequest,
        on_event: &mut (dyn FnMut(crate::streaming::StreamEvent) + Send),
        abort: Option<&AtomicBool>,
    ) -> Result<CompletionResponse> {
        let response = self.complete(request).await?;
        let text = extract_text(&response.content);
        if !text.is_empty() {
            crate::streaming::emit_text_chunks(&text, 8, on_event, abort);
        }
        if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            let mut partial = response;
            partial.stop_reason = StopReason::Aborted;
            return Ok(partial);
        }
        Ok(response)
    }
}

pub struct Agent {
    pub config: AgentConfig,
    pub messages: Vec<AgentMessage>,
    pub is_busy: bool,
    tools: Vec<Arc<dyn Tool>>,
    listeners: Vec<EventListener>,
    steering_queue: Arc<Mutex<Vec<String>>>,
    followup_queue: Arc<Mutex<Vec<String>>>,
    abort_flag: Arc<AtomicBool>,
}

impl Agent {
    pub fn new(config: AgentConfig, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            config,
            messages: Vec::new(),
            is_busy: false,
            tools,
            listeners: Vec::new(),
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            followup_queue: Arc::new(Mutex::new(Vec::new())),
            abort_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn abort_handle(&self) -> Arc<AtomicBool> {
        self.abort_flag.clone()
    }

    pub fn abort(&self) {
        self.abort_flag.store(true, Ordering::Relaxed);
    }

    pub fn clear_abort(&self) {
        self.abort_flag.store(false, Ordering::Relaxed);
    }

    pub fn is_aborted(&self) -> bool {
        self.abort_flag.load(Ordering::Relaxed)
    }

    pub fn steer(&self, text: impl Into<String>) {
        Self::push_queue(&self.steering_queue, text);
    }

    pub fn follow_up(&self, text: impl Into<String>) {
        Self::push_queue(&self.followup_queue, text);
    }

    pub fn steering_queue_handle(&self) -> Arc<Mutex<Vec<String>>> {
        self.steering_queue.clone()
    }

    pub fn followup_queue_handle(&self) -> Arc<Mutex<Vec<String>>> {
        self.followup_queue.clone()
    }

    pub fn has_queued_messages(&self) -> bool {
        !self.steering_queue.lock().expect("steering queue lock").is_empty()
            || !self.followup_queue.lock().expect("followup queue lock").is_empty()
    }

    pub fn push_queue(queue: &Arc<Mutex<Vec<String>>>, text: impl Into<String>) {
        queue
            .lock()
            .expect("queue lock")
            .push(text.into());
    }

    pub fn subscribe(&mut self, listener: EventListener) {
        self.listeners.push(listener);
    }

    pub fn clear_listeners(&mut self) {
        self.listeners.clear();
    }

    pub fn tool_definitions(&self) -> Vec<crate::tool::ToolDefinition> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    pub async fn prompt(&mut self, provider: &dyn LlmProvider, text: &str) -> Result<String> {
        self.messages.push(AgentMessage::user_text(text));
        self.run(provider).await
    }

    pub async fn run(&mut self, provider: &dyn LlmProvider) -> Result<String> {
        self.clear_abort();
        self.emit(AgentEvent::AgentStart);
        self.is_busy = true;
        let start_len = self.messages.len();
        let mut final_text;

        for turn in 0..self.config.max_turns {
            if self.is_aborted() {
                return self.finish_aborted(start_len);
            }

            self.drain_steering();
            self.emit(AgentEvent::TurnStart { turn });

            let request = CompletionRequest {
                system_prompt: self.config.system_prompt.clone(),
                messages: self.messages.clone(),
                tools: self.tool_definitions(),
                thinking_level: self.config.thinking_level,
            };

            let response = {
                let listeners: Vec<_> = self.listeners.iter().collect();
                provider
                    .complete_streaming(
                        request,
                        &mut |event| match event {
                            crate::streaming::StreamEvent::TextDelta(delta) => {
                                let agent_event = AgentEvent::TextDelta {
                                    delta: delta.clone(),
                                };
                                for listener in &listeners {
                                    listener(&agent_event);
                                }
                            }
                            crate::streaming::StreamEvent::ThinkingDelta(delta) => {
                                let agent_event = AgentEvent::ThinkingDelta {
                                    delta: delta.clone(),
                                };
                                for listener in &listeners {
                                    listener(&agent_event);
                                }
                            }
                        },
                        Some(&self.abort_flag),
                    )
                    .await?
            };

            if self.is_aborted() || response.stop_reason == StopReason::Aborted {
                let assistant = AgentMessage::Assistant(AssistantMessage {
                    content: response.content.clone(),
                    provider: response.provider.clone(),
                    model: response.model.clone(),
                    stop_reason: StopReason::Aborted,
                    timestamp: crate::message::now_ms(),
                });
                self.messages.push(assistant);
                return self.finish_aborted(start_len);
            }

            let assistant = AgentMessage::Assistant(AssistantMessage {
                content: response.content.clone(),
                provider: response.provider.clone(),
                model: response.model.clone(),
                stop_reason: response.stop_reason,
                timestamp: crate::message::now_ms(),
            });
            self.messages.push(assistant.clone());

            let tool_calls = extract_tool_calls(&response.content);
            let mut tool_results = Vec::new();

            if tool_calls.is_empty() {
                final_text = extract_text(&response.content);
                self.emit(AgentEvent::TurnEnd {
                    turn,
                    assistant,
                    tool_results,
                });
                if self.drain_followup() {
                    continue;
                }
                self.is_busy = false;
                self.emit(AgentEvent::AgentEnd {
                    new_messages: self.messages[start_len..].to_vec(),
                });
                return Ok(final_text);
            }

            for call in tool_calls {
                if self.is_aborted() {
                    self.emit(AgentEvent::TurnEnd {
                        turn,
                        assistant: assistant.clone(),
                        tool_results,
                    });
                    return self.finish_aborted(start_len);
                }

                self.emit(AgentEvent::ToolExecutionStart {
                    tool_call: call.clone(),
                });

                let (output, is_error) = match self.execute_tool(&call).await {
                    Ok(output) => {
                        // bash non-zero / signal → tool_result.is_error for the model + TUI.
                        let failed = tool_output_indicates_error(&call.name, &output);
                        (output, failed)
                    }
                    Err(err) => (ToolOutput::text(err.to_string()), true),
                };

                if self.is_aborted() {
                    self.emit(AgentEvent::ToolExecutionEnd {
                        tool_call: call.clone(),
                        output: output.clone(),
                        is_error,
                    });
                    let result = AgentMessage::ToolResult(ToolResultMessage {
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        content: output.content.clone(),
                        is_error,
                        timestamp: crate::message::now_ms(),
                    });
                    self.messages.push(result.clone());
                    tool_results.push(result);
                    self.emit(AgentEvent::TurnEnd {
                        turn,
                        assistant,
                        tool_results,
                    });
                    return self.finish_aborted(start_len);
                }

                self.emit(AgentEvent::ToolExecutionEnd {
                    tool_call: call.clone(),
                    output: output.clone(),
                    is_error,
                });

                let result = AgentMessage::ToolResult(ToolResultMessage {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: output.content.clone(),
                    is_error,
                    timestamp: crate::message::now_ms(),
                });
                self.messages.push(result.clone());
                tool_results.push(result);

                if !self.steering_queue.lock().expect("steering queue lock").is_empty() {
                    break;
                }
            }

            self.emit(AgentEvent::TurnEnd {
                turn,
                assistant,
                tool_results,
            });
        }

        self.is_busy = false;
        Err(OneError::MaxTurns {
            max: self.config.max_turns,
        })
    }

    fn finish_aborted(&mut self, start_len: usize) -> Result<String> {
        self.is_busy = false;
        self.emit(AgentEvent::AgentEnd {
            new_messages: self.messages[start_len..].to_vec(),
        });
        Err(OneError::Aborted)
    }

    fn drain_steering(&mut self) {
        let mut queue = self.steering_queue.lock().expect("steering queue lock");
        while let Some(text) = queue.pop() {
            self.messages.push(AgentMessage::user_text(text));
        }
    }

    fn drain_followup(&mut self) -> bool {
        let mut queue = self.followup_queue.lock().expect("followup queue lock");
        if queue.is_empty() {
            return false;
        }
        while let Some(text) = queue.pop() {
            self.messages.push(AgentMessage::user_text(text));
        }
        true
    }

    async fn execute_tool(&self, call: &ToolCall) -> Result<ToolOutput> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.definition().name == call.name)
            .ok_or_else(|| OneError::Tool {
                tool: call.name.clone(),
                message: "tool not registered".to_string(),
            })?;

        tool.execute(call).await
    }

    fn emit(&mut self, event: AgentEvent) {
        for listener in &self.listeners {
            listener(&event);
        }
    }
}

/// Detect soft failures that still return `Ok(ToolOutput)` (e.g. bash exit ≠ 0).
fn tool_output_indicates_error(tool_name: &str, output: &ToolOutput) -> bool {
    match tool_name {
        "bash" | "shell" => {
            if let Some(details) = &output.details {
                if let Some(ok) = details.get("ok").and_then(|v| v.as_bool()) {
                    return !ok;
                }
                match details.get("exitCode") {
                    Some(v) if v.is_null() => return true,
                    Some(v) => {
                        if let Some(code) = v.as_i64() {
                            return code != 0;
                        }
                    }
                    None => {}
                }
            }
            // Fallback: leading `exit N` line in text.
            let text = output.as_text();
            if let Some(rest) = text.strip_prefix("exit ") {
                let code = rest
                    .split(|c: char| c.is_whitespace())
                    .next()
                    .unwrap_or("");
                if code == "signal" {
                    return true;
                }
                if let Ok(n) = code.parse::<i64>() {
                    return n != 0;
                }
            }
            false
        }
        _ => false,
    }
}

pub fn extract_tool_calls(content: &[ContentBlock]) -> Vec<ToolCall> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { id, name, arguments } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

pub fn extract_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Helper for providers that stream text deltas to listeners.
pub async fn drain_text_deltas<S>(mut stream: S, on_delta: &mut dyn FnMut(&str))
where
    S: futures::Stream<Item = String> + Unpin,
{
    while let Some(delta) = stream.next().await {
        on_delta(&delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abort_stops_agent_run() {
        struct AbortingProvider;

        #[async_trait::async_trait]
        impl LlmProvider for AbortingProvider {
            fn name(&self) -> &str {
                "abort-test"
            }

            fn model(&self) -> &str {
                "test"
            }

            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                unreachable!("streaming only")
            }

            async fn complete_streaming(
                &self,
                _request: CompletionRequest,
                on_event: &mut (dyn FnMut(crate::streaming::StreamEvent) + Send),
                _abort: Option<&AtomicBool>,
            ) -> Result<CompletionResponse> {
                on_event(crate::streaming::StreamEvent::TextDelta("partial".to_string()));
                Ok(CompletionResponse {
                    provider: self.name().to_string(),
                    model: self.model().to_string(),
                    content: vec![ContentBlock::Text {
                        text: "partial".to_string(),
                    }],
                    stop_reason: StopReason::Aborted,
                })
            }
        }

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let result = agent.prompt(&AbortingProvider, "hi").await;
        assert!(matches!(result, Err(OneError::Aborted)));
        assert!(!agent.is_busy);
        assert_eq!(agent.messages.len(), 2);
    }

    #[test]
    fn extracts_tool_calls_from_content() {
        let content = vec![
            ContentBlock::Text {
                text: "checking".to_string(),
            },
            ContentBlock::ToolCall {
                id: "1".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({ "command": "ls" }),
            },
        ];

        let calls = extract_tool_calls(&content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
    }
}