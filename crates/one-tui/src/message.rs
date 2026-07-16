//! Structured chat messages — OpenCode-style roles without label chrome.
//!
//! ## Context vs UI (critical)
//!
//! `App.messages` is the **transcript view only**. It is *not* the agent context.
//! The model sees `one_core::AgentMessage` (user / assistant / tool_call / tool_result).
//!
//! | Role | Shown in TUI | Enters LLM context? |
//! |------|--------------|---------------------|
//! | User / Assistant | yes | yes (via agent, mirrored here for display) |
//! | Tool | yes (summary; result preview) | full result via agent `ToolResult`, not this struct |
//! | System | rare meta lines | only if agent also has it (usually no) |
//! | Alert | turn / tool errors, UI cards | **never** — display only |

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    /// Extended thinking / reasoning block (display only; agent keeps its own copy).
    Thinking,
    System,
    Tool,
    /// Ephemeral UI card (errors, warnings). Never agent context.
    Alert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    /// True while assistant text is still streaming in.
    pub streaming: bool,
    /// Footer under a finished assistant turn: `build · model · 1.2s`.
    pub footer: Option<String>,
    /// Tool lifecycle (only for Tool role).
    pub tool_status: Option<ToolStatus>,
    /// Tool name when role is Tool (parsed or explicit).
    pub tool_name: Option<String>,
    /// Tool result text for TUI (truncated). Full text still lives on agent `ToolResult`.
    pub tool_output: Option<String>,
    /// One-line summary under the tool header (`ok · 12 lines`, `exit error`, …).
    pub tool_summary: Option<String>,
    /// When true, show multi-line tool_output body (errors default expanded).
    pub tool_expanded: bool,
    /// When true, this tool opts out of multi-tool group collapse (still may hide body).
    pub tool_ungroup: bool,
    /// Thinking block expanded (Thinking role); also reused as expand flag.
    pub thinking_expanded: bool,
    /// Alert severity (Alert role only).
    pub alert_level: Option<AlertLevel>,
}

fn blank_message(role: MessageRole, content: String) -> Message {
    Message {
        role,
        content,
        streaming: false,
        footer: None,
        tool_status: None,
        tool_name: None,
        tool_output: None,
        tool_summary: None,
        tool_expanded: false,
        tool_ungroup: false,
        thinking_expanded: false,
        alert_level: None,
    }
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        blank_message(MessageRole::User, content.into())
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        blank_message(MessageRole::Assistant, content.into())
    }

    pub fn thinking(content: impl Into<String>) -> Self {
        blank_message(MessageRole::Thinking, content.into())
    }

    pub fn streaming_thinking(content: impl Into<String>) -> Self {
        let mut m = blank_message(MessageRole::Thinking, content.into());
        m.streaming = true;
        m.thinking_expanded = true; // show live stream while arriving
        m
    }

    pub fn system(content: impl Into<String>) -> Self {
        blank_message(MessageRole::System, content.into())
    }

    pub fn tool(name: impl Into<String>, detail: impl Into<String>, status: ToolStatus) -> Self {
        let name = name.into();
        let detail = detail.into();
        let mut m = blank_message(MessageRole::Tool, detail);
        m.tool_status = Some(status);
        m.tool_name = Some(name);
        m
    }

    /// UI-only card in the transcript (errors, warnings). Not agent context.
    pub fn alert(level: AlertLevel, content: impl Into<String>) -> Self {
        let mut m = blank_message(MessageRole::Alert, content.into());
        m.alert_level = Some(level);
        m
    }

    pub fn streaming_assistant(content: impl Into<String>) -> Self {
        let mut m = blank_message(MessageRole::Assistant, content.into());
        m.streaming = true;
        m
    }

    pub fn with_footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = Some(footer.into());
        self
    }

    /// Role label kept for debugging / export; not drawn in the TUI.
    pub fn prefix(&self) -> &'static str {
        match self.role {
            MessageRole::User => "you",
            MessageRole::Assistant => "assistant",
            MessageRole::Thinking => "thinking",
            MessageRole::System => "system",
            MessageRole::Tool => "tool",
            MessageRole::Alert => "alert",
        }
    }
}

/// Build a short summary + whether the body should auto-expand.
pub fn summarize_tool_output(output: &str, is_error: bool) -> (String, bool) {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return (
            if is_error {
                "failed".into()
            } else {
                "ok".into()
            },
            is_error,
        );
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let n = lines.len();
    let first = lines
        .iter()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let first = truncate_chars(first, 72);
    let summary = if is_error {
        if n <= 1 {
            format!("error · {first}")
        } else {
            format!("error · {n} lines · {first}")
        }
    } else if n <= 1 {
        format!("ok · {first}")
    } else {
        format!("ok · {n} lines")
    };
    // Auto-expand errors so failures are visible mid-transcript, not only in the footer.
    (summary, is_error)
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

/// Cap stored tool output for the TUI (agent still has the full result).
pub fn truncate_tool_output_for_ui(output: &str, max_chars: usize) -> String {
    let trimmed = output.trim();
    if trimmed.len() <= max_chars {
        return trimmed.to_string();
    }
    let mut out = trimmed
        .chars()
        .take(max_chars.saturating_sub(20))
        .collect::<String>();
    out.push_str("\n… (truncated in UI · full result still in model context)");
    out
}
