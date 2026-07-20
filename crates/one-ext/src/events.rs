//! Lifecycle events dispatched to extensions (observe + optional intercept).

use one_core::tool::{ToolCall, ToolOutput};
use serde_json::Value;

/// Lifecycle / tool events fired by [`crate::runtime::ExtensionRuntime`].
#[derive(Debug, Clone)]
pub enum ExtensionEvent {
    /// Conversation session starts (once per open / `/new` / cold load — **not** each prompt).
    SessionStart,
    /// Conversation session ends (`/new` replacing current, `/reload` unload, process teardown).
    SessionEnd,
    /// One LLM→tools loop iteration begins (inside a user prompt).
    TurnStart { turn: usize },
    /// One LLM→tools loop iteration ends.
    TurnEnd { turn: usize },
    /// Tool about to run (observe; intercept uses [`crate::intercept::PreToolDecision`]).
    ToolStart { tool_call: ToolCall },
    /// Tool finished.
    ToolEnd {
        tool_call: ToolCall,
        output: ToolOutput,
        is_error: bool,
    },
    /// Compaction about to run.
    PreCompact,
    /// Compaction finished.
    PostCompact,
    /// User submitted a prompt (before model).
    UserPromptSubmit { text: String },
}

/// Context available while an extension loads or handles lifecycle.
pub struct ExtensionContext<'a> {
    pub cwd: &'a std::path::Path,
    pub session_file: Option<&'a std::path::Path>,
    /// Shared type map for this process/session.
    pub data: &'a crate::data::ExtensionData,
}

/// Slash / custom command contributed by an extension.
#[derive(Debug, Clone)]
pub struct ExtensionCommand {
    pub name: String,
    pub description: String,
    /// Sync handler; return text to inject as a user notice / system message.
    pub handler: fn(&str) -> String,
}

/// Prompt fragment injected into the system prompt.
#[derive(Debug, Clone)]
pub struct PromptFragment {
    pub source: String,
    pub text: String,
}

/// Decision from a PreToolUse interceptor (Codex-style).
#[derive(Debug, Clone)]
pub enum PreToolDecision {
    /// Continue with original (or previously rewritten) args.
    Allow,
    /// Continue with new arguments.
    Rewrite { arguments: Value },
    /// Block the tool; message is returned to the model.
    Deny { message: String },
}

impl Default for PreToolDecision {
    fn default() -> Self {
        Self::Allow
    }
}
