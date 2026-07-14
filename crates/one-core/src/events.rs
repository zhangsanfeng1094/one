use crate::message::AgentMessage;
use crate::tool::{ToolCall, ToolOutput};

#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd { new_messages: Vec<AgentMessage> },
    TurnStart { turn: usize },
    TurnEnd {
        turn: usize,
        assistant: AgentMessage,
        tool_results: Vec<AgentMessage>,
    },
    TextDelta { delta: String },
    ThinkingDelta { delta: String },
    ToolExecutionStart { tool_call: ToolCall },
    ToolExecutionEnd {
        tool_call: ToolCall,
        output: ToolOutput,
        is_error: bool,
    },
}

pub type EventListener = Box<dyn Fn(&AgentEvent) + Send + Sync>;