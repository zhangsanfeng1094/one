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
use crate::tool_gate::{ToolGate, ToolGateDecision};

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
            "low" | "1" | "minimal" => Some(ThinkingLevel::Low),
            "medium" | "med" | "2" => Some(ThinkingLevel::Medium),
            "high" | "3" | "xhigh" | "max" => Some(ThinkingLevel::High),
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

    pub fn is_enabled(self) -> bool {
        !matches!(self, ThinkingLevel::Off)
    }

    /// OpenAI / OpenRouter style effort label (`None` when off).
    pub fn effort(self) -> Option<&'static str> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some("low"),
            ThinkingLevel::Medium => Some("medium"),
            ThinkingLevel::High => Some("high"),
        }
    }

    /// Anthropic-style token budget for extended thinking (`None` when off).
    ///
    /// Defaults align with Pi's budgets (low 2k / medium 8k / high 16k).
    pub fn budget_tokens(self) -> Option<u32> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some(2_048),
            ThinkingLevel::Medium => Some(8_192),
            ThinkingLevel::High => Some(16_384),
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

/// Token accounting returned by providers (when available).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    pub fn add_assign(&mut self, other: &TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub provider: String,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    /// Provider-reported usage for this completion (may be zero if unknown).
    pub usage: TokenUsage,
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
    /// Cumulative provider-reported tokens for this process/session.
    pub token_usage: TokenUsage,
    tools: Vec<Arc<dyn Tool>>,
    listeners: Vec<EventListener>,
    steering_queue: Arc<Mutex<Vec<String>>>,
    followup_queue: Arc<Mutex<Vec<String>>>,
    /// Side-channel notices (e.g. background bash completions), drained before each LLM turn.
    /// Injected as user messages with a clear prefix — not tool_results (providers require pairing).
    notification_queue: Arc<Mutex<Vec<String>>>,
    abort_flag: Arc<AtomicBool>,
    /// Optional pre-tool permission gate (allow/deny/ask → resolve).
    tool_gate: Option<Arc<dyn ToolGate>>,
}

impl Agent {
    pub fn new(config: AgentConfig, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            config,
            messages: Vec::new(),
            is_busy: false,
            token_usage: TokenUsage::default(),
            tools,
            listeners: Vec::new(),
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            followup_queue: Arc::new(Mutex::new(Vec::new())),
            notification_queue: Arc::new(Mutex::new(Vec::new())),
            abort_flag: Arc::new(AtomicBool::new(false)),
            tool_gate: None,
        }
    }

    /// Install a permission gate checked before every tool execution.
    pub fn set_tool_gate(&mut self, gate: Option<Arc<dyn ToolGate>>) {
        self.tool_gate = gate;
    }

    pub fn tool_gate(&self) -> Option<&Arc<dyn ToolGate>> {
        self.tool_gate.as_ref()
    }

    /// Replace the notification queue (wire shared background-task registry).
    pub fn set_notification_queue(&mut self, queue: Arc<Mutex<Vec<String>>>) {
        self.notification_queue = queue;
    }

    pub fn notification_queue_handle(&self) -> Arc<Mutex<Vec<String>>> {
        self.notification_queue.clone()
    }

    /// Push a notice that will be injected before the next LLM call.
    pub fn push_notification(&self, text: impl Into<String>) {
        Self::push_queue(&self.notification_queue, text);
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

    /// Replace the registered tool set (e.g. Plan mode ↔ Act mode).
    pub fn set_tools(&mut self, tools: Vec<Arc<dyn Tool>>) {
        self.tools = tools;
    }

    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    pub async fn prompt(&mut self, provider: &dyn LlmProvider, text: &str) -> Result<String> {
        self.prompt_user(provider, AgentMessage::user_text(text))
            .await
    }

    /// Prompt with pre-built user message (text and/or images).
    pub async fn prompt_user(
        &mut self,
        provider: &dyn LlmProvider,
        user: AgentMessage,
    ) -> Result<String> {
        debug_assert!(matches!(user, AgentMessage::User(_)));
        self.messages.push(user);
        self.run(provider).await
    }

    /// Prompt with text + optional image attachments `(mime_type, base64)`.
    pub async fn prompt_with_images(
        &mut self,
        provider: &dyn LlmProvider,
        text: &str,
        images: Vec<(String, String)>,
    ) -> Result<String> {
        let msg = if images.is_empty() {
            AgentMessage::user_text(text)
        } else {
            AgentMessage::user_with_images(text, images)
        };
        self.prompt_user(provider, msg).await
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
            // Claude-style: background task completions appear as conversation notices.
            self.drain_notifications();
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

            if !response.usage.is_zero() {
                self.token_usage.add_assign(&response.usage);
            }

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
        // Preserve FIFO order (push to end, drain from front).
        let items: Vec<_> = queue.drain(..).collect();
        for text in items {
            self.messages.push(AgentMessage::user_text(text));
        }
    }

    fn drain_notifications(&mut self) {
        let mut queue = self
            .notification_queue
            .lock()
            .expect("notification queue lock");
        let items: Vec<_> = queue.drain(..).collect();
        drop(queue);
        for text in items {
            self.messages.push(AgentMessage::user_text(text));
        }
    }

    fn drain_followup(&mut self) -> bool {
        let mut queue = self.followup_queue.lock().expect("followup queue lock");
        if queue.is_empty() {
            return false;
        }
        let items: Vec<_> = queue.drain(..).collect();
        drop(queue);
        for text in items {
            self.messages.push(AgentMessage::user_text(text));
        }
        true
    }

    async fn execute_tool(&self, call: &ToolCall) -> Result<ToolOutput> {
        if let Some(gate) = &self.tool_gate {
            match gate.check(call).await {
                ToolGateDecision::Allow => {}
                ToolGateDecision::Deny { message } => {
                    return Err(OneError::Tool {
                        tool: call.name.clone(),
                        message,
                    });
                }
            }
        }

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
        "bash" | "shell" | "bash_output" => {
            if let Some(details) = &output.details {
                // Background start is never an error.
                if details
                    .get("background")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    return false;
                }
                // Still running snapshot is not an error.
                if details
                    .get("running")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    return false;
                }
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
                    usage: TokenUsage::default(),
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

    #[test]
    fn background_start_is_not_error() {
        let output = ToolOutput::text_with_details(
            "Background task started\ntask_id: bg_1",
            serde_json::json!({ "background": true, "ok": true, "task_id": "bg_1" }),
        );
        assert!(!tool_output_indicates_error("bash", &output));
    }

    #[test]
    fn bash_output_running_is_not_error() {
        let output = ToolOutput::text_with_details(
            "status: running",
            serde_json::json!({ "running": true, "ok": true, "status": "running" }),
        );
        assert!(!tool_output_indicates_error("bash_output", &output));
    }

    #[tokio::test]
    async fn injects_notifications_before_llm_turn() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct NoticeProvider {
            calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for NoticeProvider {
            fn name(&self) -> &str {
                "notice"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    let has_notice = request.messages.iter().any(|m| match m {
                        AgentMessage::User(u) => u
                            .content
                            .as_plain_text()
                            .contains("[Background task completed]"),
                        _ => false,
                    });
                    assert!(has_notice, "notification should be injected before LLM call");
                }
                Ok(CompletionResponse {
                    provider: self.name().to_string(),
                    model: self.model().to_string(),
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    stop_reason: StopReason::Stop,
                    usage: TokenUsage::default(),
                })
            }
        }

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.push_notification(
            "[Background task completed]\ntask_id: bg_test_1\nexit: 0\n",
        );
        let out = agent
            .prompt(
                &NoticeProvider {
                    calls: AtomicUsize::new(0),
                },
                "hi",
            )
            .await
            .expect("run");
        assert_eq!(out, "done");
        assert!(agent.messages.len() >= 3);
    }
}