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
    /// ReAct parked on `wait_tasks` — waiting for background work to finish.
    /// Carries the minimal dependency info: wait mode (all/any) + target ids.
    WaitParkStart {
        mode: crate::agent::WaitInterestMode,
        ids: Vec<String>,
    },
    /// The wait park ended (satisfied, new input, or abort). Waiting state must
    /// leave with this event — it fires exactly once per `WaitParkStart`.
    WaitParkEnd,
    /// A queued steer was drained into the model context (mid-run injection).
    ///
    /// Emitted when the message enters `self.messages`; UI can use this to move
    /// a pending steer row from the busy strip into the transcript timeline at
    /// its real position with an "applied" state.
    SteerApplied {
        text: String,
    },
    UsageUpdate {
        usage: crate::agent::TokenUsage,
        context_tokens: u64,
    },
    /// A context compaction has started.
    CompactionStart,
    /// A context compaction has completed.
    CompactionEnd {
        tokens_before: u64,
        tokens_after: u64,
        kept_turns: usize,
    },
}

pub type EventListener = Box<dyn Fn(&AgentEvent) + Send + Sync>;
