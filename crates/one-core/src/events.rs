use crate::message::AgentMessage;
use crate::tool::{ToolCall, ToolOutput};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        new_messages: Vec<AgentMessage>,
    },
    TurnStart {
        turn: usize,
    },
    TurnEnd {
        turn: usize,
        assistant: AgentMessage,
        tool_results: Vec<AgentMessage>,
    },
    TextDelta {
        delta: String,
    },
    ThinkingDelta {
        delta: String,
    },
    /// A recoverable model failure will be retried after a short backoff.
    RetryScheduled {
        /// One-based retry number (the first retry is `1`).
        retry: usize,
        max_retries: usize,
        delay: Duration,
        /// Compact user-facing reason, never a full provider payload.
        reason: String,
    },
    /// The scheduled retry's next provider request has started.
    RetryStarted {
        retry: usize,
        max_retries: usize,
    },
    ServerTool {
        provider: String,
        tool: crate::agent::ServerTool,
        status: crate::streaming::ServerToolStatus,
    },
    ToolExecutionStart {
        tool_call: ToolCall,
    },
    ToolExecutionEnd {
        tool_call: ToolCall,
        output: ToolOutput,
        is_error: bool,
    },
    UsageUpdate {
        usage: crate::agent::TokenUsage,
        context_tokens: u64,
    },
}

pub type EventListener = Box<dyn Fn(&AgentEvent) + Send + Sync>;
