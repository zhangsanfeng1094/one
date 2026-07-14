//! Application state for the interactive chat TUI.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::float::{FloatKind, FloatMenu};
use crate::message::{
    summarize_tool_output, truncate_tool_output_for_ui, AlertLevel, Message, MessageRole,
    ToolStatus,
};
use crate::slash::{self, ModelChoice, PopupKind, PopupRow};
use crate::tool_view;

#[derive(Debug, Clone)]
pub enum RunOutcome {
    Prompt(String),
    FollowUp(String),
    Steer(String),
    /// Cycle thinking level (Shift+Tab).
    CycleThinking,
    Quit,
    Noop,
}

impl RunOutcome {
    pub fn is_actionable(&self) -> bool {
        match self {
            RunOutcome::Noop => false,
            RunOutcome::Prompt(t) | RunOutcome::FollowUp(t) | RunOutcome::Steer(t) => !t.is_empty(),
            RunOutcome::CycleThinking | RunOutcome::Quit => true,
        }
    }
}

const STATUS_IDLE: &str = "";
const STATUS_BUSY: &str = "";
/// How long toast stays visible unless replaced.
const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(4);

/// Ephemeral top-right notice — never agent context.
#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub level: AlertLevel,
    pub created: Instant,
}

impl Toast {
    pub fn alive(&self) -> bool {
        self.created.elapsed() < TOAST_TTL
    }
}

pub struct App {
    pub title: String,
    pub messages: Vec<Message>,
    pub input: String,
    pub status: String,
    pub stream_buffer: String,
    pub busy: bool,
    /// Lines scrolled **up from the bottom** of the transcript (0 = stick to end).
    ///
    /// Must be line-based, not message-based: a single long assistant reply can be
    /// taller than the viewport, and Ratatui `List` cannot partial-scroll one item.
    pub chat_scroll: usize,
    pub follow_bottom: bool,
    /// Last drawn chat viewport height (rows). Used for PageUp/PageDown page size.
    pub chat_view_height: usize,
    /// Last drawn transcript line count (after wrap).
    pub chat_total_lines: usize,
    /// Parallel to display lines: which `messages` index owns each transcript line
    /// (for click-to-expand). `None` = spacer / non-interactive.
    pub chat_line_owners: Vec<Option<usize>>,
    /// Top of chat viewport in the full line list (updated each draw).
    pub chat_view_start: usize,
    pub cursor_on: bool,
    /// Compact model label for turn footers (usually just the model id).
    pub mode_label: String,
    /// Agent / mode name shown in turn footer & prompt meta (OpenCode: "Build").
    pub agent_label: String,
    /// Spinner frame index while busy.
    pub spinner_frame: usize,
    /// Selected **row** index in the popup (may point at a header — navigation skips those).
    pub slash_selected: usize,
    /// Models from registry / models.json for `/model` picker.
    pub model_catalog: Vec<ModelChoice>,
    /// Ephemeral toast (top-right). **Not** chat context, **not** agent messages.
    pub toast: Option<Toast>,
    /// Centered floating secondary menu (model picker, command palette, …).
    pub float: Option<FloatMenu>,
    /// Current provider id (for model picker "current" marker).
    pub current_provider: String,
    /// Current model id.
    pub current_model: String,
    /// Thinking level label: off | low | medium | high.
    pub thinking_level: String,
    /// Estimated context tokens (messages).
    pub usage_tokens: usize,
    /// Optional context window for % display (0 = unknown).
    pub context_window: usize,
    turn_started: Option<Instant>,
    followup_pending: Option<String>,
    steer_pending: Option<String>,
    abort_pending: bool,
}

fn classify_toast_level(text: &str) -> AlertLevel {
    let t = text.trim().to_ascii_lowercase();
    if t.starts_with("error") || t.contains("failed") || t.contains("overflow") {
        AlertLevel::Error
    } else if t.starts_with("warn")
        || t.contains("interrupt")
        || t.contains("overflow")
        || t.starts_with("thinking →")
    {
        AlertLevel::Warn
    } else {
        AlertLevel::Info
    }
}

impl App {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            messages: Vec::new(),
            input: String::new(),
            status: STATUS_IDLE.into(),
            stream_buffer: String::new(),
            busy: false,
            chat_scroll: 0,
            follow_bottom: true,
            chat_view_height: 0,
            chat_total_lines: 0,
            chat_line_owners: Vec::new(),
            chat_view_start: 0,
            cursor_on: true,
            mode_label: String::new(),
            agent_label: "Build".into(),
            spinner_frame: 0,
            slash_selected: 0,
            model_catalog: Vec::new(),
            toast: None,
            float: None,
            current_provider: String::new(),
            current_model: String::new(),
            thinking_level: "off".into(),
            usage_tokens: 0,
            context_window: 0,
            turn_started: None,
            followup_pending: None,
            steer_pending: None,
            abort_pending: false,
        }
    }

    pub fn set_thinking_level(&mut self, level: impl Into<String>) {
        self.thinking_level = level.into();
    }

    pub fn set_usage_tokens(&mut self, tokens: usize) {
        self.usage_tokens = tokens;
    }

    pub fn set_context_window(&mut self, window: usize) {
        self.context_window = window;
    }

    pub fn set_model_catalog(&mut self, catalog: Vec<ModelChoice>) {
        self.model_catalog = catalog;
    }

    pub fn set_current_model(&mut self, provider: impl Into<String>, model: impl Into<String>) {
        self.current_provider = provider.into();
        self.current_model = model.into();
    }

    /// Show a short top-right toast. Does **not** append to chat/history
    /// and does **not** enter the LLM context. Auto-expires after a few seconds.
    pub fn set_notice(&mut self, text: impl Into<String>) {
        let text = text.into();
        let level = classify_toast_level(&text);
        self.toast = Some(Toast {
            text,
            level,
            created: Instant::now(),
        });
    }

    pub fn clear_notice(&mut self) {
        self.toast = None;
    }

    /// Drop expired toast (call each frame).
    pub fn tick_toast(&mut self) {
        if self.toast.as_ref().is_some_and(|t| !t.alive()) {
            self.toast = None;
        }
    }

    /// Active toast text if still within TTL.
    pub fn toast_active(&self) -> Option<&Toast> {
        self.toast.as_ref().filter(|t| t.alive())
    }

    /// Mid-transcript UI card (errors / warnings). **Never** agent context —
    /// only painted in the TUI. Prefer this over stuffing failures into the
    /// bottom status strip for anything the user must actually read.
    pub fn push_alert(&mut self, level: AlertLevel, text: impl Into<String>) {
        self.seal_stream_segment();
        self.messages.push(Message::alert(level, text));
        self.scroll_to_bottom();
    }

    pub fn push_error_alert(&mut self, text: impl Into<String>) {
        self.push_alert(AlertLevel::Error, text);
    }

    pub fn float_open(&self) -> bool {
        self.float.is_some()
    }

    pub fn open_model_picker(&mut self) {
        let cur = if !self.current_provider.is_empty() && !self.current_model.is_empty() {
            Some((self.current_provider.as_str(), self.current_model.as_str()))
        } else {
            None
        };
        self.float = Some(FloatMenu::model_picker(&self.model_catalog, cur));
        self.clear_notice();
    }

    pub fn open_command_palette(&mut self) {
        self.float = Some(FloatMenu::commands_palette());
        self.clear_notice();
    }

    pub fn open_help_float(&mut self) {
        self.float = Some(FloatMenu::help_menu());
        self.clear_notice();
    }

    pub fn open_thinking_float(&mut self) {
        self.float = Some(FloatMenu::thinking_picker(&self.thinking_level));
        self.clear_notice();
    }

    /// `(id, label, detail, hint)` — id is used for `/resume <id>`.
    pub fn open_sessions_float(&mut self, sessions: &[(String, String, String, String)]) {
        self.float = Some(FloatMenu::sessions_picker(sessions));
        self.clear_notice();
    }

    /// `(id, label, detail)` for branch entries.
    pub fn open_tree_float(&mut self, entries: &[(String, String, String)]) {
        self.float = Some(FloatMenu::tree_picker(entries));
        self.clear_notice();
    }

    pub fn open_info_float(&mut self, title: impl Into<String>, rows: &[(String, String)]) {
        self.float = Some(FloatMenu::info_panel(title, rows));
        self.clear_notice();
    }

    pub fn close_float(&mut self) {
        self.float = None;
    }

    /// Popup rows for current input (commands or models grouped by provider).
    pub fn popup_rows(&self) -> Vec<PopupRow> {
        slash::popup_rows(&self.input, &self.model_catalog)
    }

    pub fn slash_menu_visible(&self) -> bool {
        !self.popup_rows().is_empty()
    }

    pub fn popup_kind(&self) -> Option<PopupKind> {
        slash::popup_kind(&self.input)
    }

    fn clamp_slash_selection(&mut self) {
        let rows = self.popup_rows();
        let selectable = slash::selectable_indices(&rows);
        if selectable.is_empty() {
            self.slash_selected = 0;
            return;
        }
        // If current index is not selectable (header), snap to nearest selectable.
        if !rows
            .get(self.slash_selected)
            .map(|r| r.selectable())
            .unwrap_or(false)
        {
            // Prefer next selectable, else previous.
            if let Some(&i) = selectable.iter().find(|&&i| i >= self.slash_selected) {
                self.slash_selected = i;
            } else {
                self.slash_selected = *selectable.last().unwrap();
            }
        } else if self.slash_selected >= rows.len() {
            self.slash_selected = *selectable.last().unwrap();
        }
    }

    fn move_slash_selection(&mut self, delta: isize) {
        let rows = self.popup_rows();
        let selectable = slash::selectable_indices(&rows);
        if selectable.is_empty() {
            return;
        }
        let cur = selectable
            .iter()
            .position(|&i| i == self.slash_selected)
            .unwrap_or(0);
        let next = if delta < 0 {
            cur.saturating_sub((-delta) as usize)
        } else {
            (cur + delta as usize).min(selectable.len() - 1)
        };
        self.slash_selected = selectable[next];
    }

    fn apply_slash_completion(&mut self) {
        let rows = self.popup_rows();
        if rows.is_empty() {
            return;
        }
        self.clamp_slash_selection();
        if let Some(row) = rows.get(self.slash_selected) {
            if let Some(text) = slash::completion_for_row(row) {
                self.input = text;
                self.slash_selected = 0;
                self.cursor_on = true;
                self.clamp_slash_selection();
            }
        }
    }

    pub fn set_mode_label(&mut self, label: impl Into<String>) {
        self.mode_label = label.into();
    }

    pub fn set_agent_label(&mut self, label: impl Into<String>) {
        self.agent_label = label.into();
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(Message::user(text));
        self.scroll_to_bottom();
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) {
        let mut msg = Message::assistant(text);
        msg.footer = Some(self.turn_footer(None));
        self.messages.push(msg);
        self.scroll_to_bottom();
    }

    pub fn push_system(&mut self, text: impl Into<String>) {
        self.messages.push(Message::system(text));
        self.scroll_to_bottom();
    }

    pub fn push_tool(&mut self, text: impl Into<String>) {
        // Backward-compatible: `name(args)` or free text → running tool row.
        let text = text.into();
        let (name, detail) = split_tool_text(&text);
        self.messages
            .push(Message::tool(name, detail, ToolStatus::Running));
        self.scroll_to_bottom();
    }

    pub fn push_tool_call(&mut self, name: impl Into<String>, args: impl Into<String>) {
        // Close any in-progress assistant bubble so tool rows don't sit
        // inside a still-streaming message, and next text starts clean.
        self.seal_stream_segment();
        self.messages
            .push(Message::tool(name, args, ToolStatus::Running));
        self.scroll_to_bottom();
    }

    /// Finalize the current streaming assistant text as a completed bubble
    /// and reset the stream buffer (used between tool rounds).
    pub fn seal_stream_segment(&mut self) {
        if self.stream_buffer.is_empty() {
            // Still seal a trailing empty streaming marker if present.
            if let Some(last) = self.messages.last_mut() {
                if last.streaming {
                    last.streaming = false;
                }
            }
            return;
        }
        self.sync_stream_message();
        if let Some(last) = self.messages.last_mut() {
            if last.streaming {
                last.streaming = false;
            }
        }
        self.stream_buffer.clear();
    }

    /// Mark the latest matching running tool as done / error.
    ///
    /// `output` is optional UI preview (already separate from agent `ToolResult`,
    /// which always carries the full payload for the model).
    pub fn finish_tool(&mut self, name: &str, error: bool) {
        self.finish_tool_with_output(name, error, None);
    }

    pub fn finish_tool_with_output(
        &mut self,
        name: &str,
        error: bool,
        output: Option<String>,
    ) {
        let status = if error {
            ToolStatus::Error
        } else {
            ToolStatus::Done
        };
        let apply = |msg: &mut Message| {
            msg.tool_status = Some(status);
            let args = msg.content.clone();
            let tool_name = msg.tool_name.clone().unwrap_or_else(|| name.to_string());
            if let Some(raw) = output.clone() {
                let mut stored = truncate_tool_output_for_ui(&raw, 4_000);
                let (summary, expand) = if let Some((s, e, better)) =
                    tool_view::summarize_tool_special(&tool_name, &args, &stored, error)
                {
                    if let Some(b) = better {
                        stored = truncate_tool_output_for_ui(&b, 4_000);
                    }
                    (s, e)
                } else {
                    summarize_tool_output(&stored, error)
                };
                msg.tool_output = Some(stored);
                msg.tool_summary = Some(summary);
                msg.tool_expanded = expand;
            } else if error {
                msg.tool_summary = Some("failed".into());
                msg.tool_expanded = true;
            } else if let Some((s, e, better)) =
                tool_view::summarize_tool_special(&tool_name, &args, "", false)
            {
                if let Some(b) = better {
                    msg.tool_output = Some(truncate_tool_output_for_ui(&b, 4_000));
                }
                msg.tool_summary = Some(s);
                msg.tool_expanded = e;
            } else {
                msg.tool_summary = Some("ok".into());
                msg.tool_expanded = false;
            }
        };

        for msg in self.messages.iter_mut().rev() {
            if msg.role == MessageRole::Tool
                && msg.tool_status == Some(ToolStatus::Running)
                && msg.tool_name.as_deref() == Some(name)
            {
                apply(msg);
                return;
            }
        }
        // Fallback: mark any last running tool.
        for msg in self.messages.iter_mut().rev() {
            if msg.role == MessageRole::Tool && msg.tool_status == Some(ToolStatus::Running) {
                apply(msg);
                return;
            }
        }
        if error {
            let mut msg = Message::tool(name, "failed", ToolStatus::Error);
            if let Some(raw) = output {
                let stored = truncate_tool_output_for_ui(&raw, 4_000);
                let (summary, expand) = summarize_tool_output(&stored, true);
                msg.tool_output = Some(stored);
                msg.tool_summary = Some(summary);
                msg.tool_expanded = expand;
            } else {
                msg.tool_summary = Some("failed".into());
                msg.tool_expanded = true;
            }
            self.messages.push(msg);
        }
    }

    /// Toggle last tool body, or expand/collapse a multi-tool group chip.
    pub fn toggle_last_tool_expand(&mut self) {
        if let Some((start, len)) = self.last_tool_streak() {
            if tool_view::streak_can_collapse(&self.messages, start, len) {
                // Chip → individual rows (bodies stay collapsed).
                for msg in &mut self.messages[start..start + len] {
                    msg.tool_ungroup = true;
                }
                return;
            }
            let all_ungrouped = self.messages[start..start + len]
                .iter()
                .all(|m| m.tool_ungroup || m.tool_expanded);
            if len >= tool_view::COLLAPSE_GROUP_MIN
                && all_ungrouped
                && self.messages[start..start + len]
                    .iter()
                    .all(|m| matches!(m.tool_status, Some(ToolStatus::Done)))
            {
                // Individual rows → chip again.
                for msg in &mut self.messages[start..start + len] {
                    msg.tool_ungroup = false;
                    msg.tool_expanded = false;
                }
                return;
            }
        }
        for msg in self.messages.iter_mut().rev() {
            if msg.role == MessageRole::Tool {
                msg.tool_expanded = !msg.tool_expanded;
                return;
            }
        }
    }

    /// Toggle the tool message at `msg_index` (click target).
    pub fn toggle_tool_at(&mut self, msg_index: usize) {
        if self
            .messages
            .get(msg_index)
            .map(|m| m.role != MessageRole::Tool)
            .unwrap_or(true)
        {
            return;
        }
        // Collapsed multi-tool chip → show individual rows.
        if let Some((start, len)) = self.tool_streak_covering(msg_index) {
            if tool_view::streak_can_collapse(&self.messages, start, len) {
                for m in &mut self.messages[start..start + len] {
                    m.tool_ungroup = true;
                }
                return;
            }
        }
        if let Some(msg) = self.messages.get_mut(msg_index) {
            msg.tool_expanded = !msg.tool_expanded;
        }
    }

    /// Click at row offset within the chat viewport (0 = top visible line).
    pub fn click_chat_row(&mut self, row_in_view: usize) {
        let line = self.chat_view_start.saturating_add(row_in_view);
        if let Some(Some(msg_i)) = self.chat_line_owners.get(line).copied() {
            self.toggle_tool_at(msg_i);
        }
    }

    fn last_tool_streak(&self) -> Option<(usize, usize)> {
        let last_tool = self
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::Tool)?;
        // Walk back to streak start.
        let mut start = last_tool;
        while start > 0 && self.messages[start - 1].role == MessageRole::Tool {
            start -= 1;
        }
        let len = last_tool - start + 1;
        Some((start, len))
    }

    fn tool_streak_covering(&self, idx: usize) -> Option<(usize, usize)> {
        if self.messages.get(idx)?.role != MessageRole::Tool {
            return None;
        }
        let mut start = idx;
        while start > 0 && self.messages[start - 1].role == MessageRole::Tool {
            start -= 1;
        }
        let len = tool_view::tool_streak_len(&self.messages, start);
        Some((start, len))
    }

    pub fn append_stream(&mut self, delta: &str) {
        self.stream_buffer.push_str(delta);
        if self.follow_bottom {
            self.scroll_to_bottom();
        }
    }

    pub fn sync_stream_message(&mut self) {
        if self.stream_buffer.is_empty() {
            return;
        }

        if let Some(last) = self.messages.last_mut() {
            if last.role == MessageRole::Assistant && last.streaming {
                last.content = self.stream_buffer.clone();
                return;
            }
        }

        self.messages
            .push(Message::streaming_assistant(&self.stream_buffer));
    }

    pub fn finish_stream(&mut self) {
        self.finish_stream_with_interrupted(false);
    }

    pub fn finish_stream_with_interrupted(&mut self, interrupted: bool) {
        if self.stream_buffer.is_empty() {
            self.remove_trailing_empty_stream();
            // Still stamp footer on last assistant if any.
            self.attach_turn_footer(interrupted);
            return;
        }

        self.sync_stream_message();
        if let Some(last) = self.messages.last_mut() {
            if last.streaming {
                last.streaming = false;
            }
        }
        self.stream_buffer.clear();
        self.attach_turn_footer(interrupted);
        self.scroll_to_bottom();
    }

    fn attach_turn_footer(&mut self, interrupted: bool) {
        let footer = self.turn_footer(if interrupted { Some(true) } else { None });
        // Attach to the last non-streaming assistant in this turn tail.
        for msg in self.messages.iter_mut().rev() {
            if msg.role == MessageRole::Assistant && !msg.streaming {
                msg.footer = Some(footer);
                break;
            }
            if msg.role == MessageRole::User {
                break;
            }
        }
        // Complete any still-running tools.
        for msg in self.messages.iter_mut() {
            if msg.role == MessageRole::Tool && msg.tool_status == Some(ToolStatus::Running) {
                msg.tool_status = Some(ToolStatus::Done);
            }
        }
    }

    fn turn_footer(&self, interrupted: Option<bool>) -> String {
        let mut parts = vec![self.agent_label.clone()];
        if !self.mode_label.is_empty() {
            parts.push(self.mode_label.clone());
        }
        if let Some(started) = self.turn_started {
            parts.push(format_duration(started.elapsed()));
        }
        if interrupted == Some(true) {
            parts.push("interrupted".into());
        }
        parts.join(" · ")
    }

    pub fn clear_stream(&mut self) {
        self.stream_buffer.clear();
        self.remove_trailing_empty_stream();
    }

    fn remove_trailing_empty_stream(&mut self) {
        if let Some(last) = self.messages.last() {
            if last.streaming && last.content.is_empty() {
                self.messages.pop();
            }
        }
    }

    pub fn begin_busy(&mut self) {
        self.busy = true;
        self.stream_buffer.clear();
        self.remove_trailing_empty_stream();
        self.status = STATUS_BUSY.into();
        self.follow_bottom = true;
        self.turn_started = Some(Instant::now());
        self.spinner_frame = 0;
        self.scroll_to_bottom();
    }

    pub fn end_busy(&mut self) {
        self.busy = false;
        self.status = STATUS_IDLE.into();
    }

    pub fn take_followup(&mut self) -> Option<String> {
        self.followup_pending.take()
    }

    pub fn take_steer(&mut self) -> Option<String> {
        self.steer_pending.take()
    }

    pub fn request_abort(&mut self) {
        self.abort_pending = true;
    }

    pub fn take_abort(&mut self) -> bool {
        std::mem::take(&mut self.abort_pending)
    }

    pub fn scroll_to_bottom(&mut self) {
        self.follow_bottom = true;
        self.chat_scroll = 0;
    }

    pub fn scroll_to_top(&mut self) {
        self.follow_bottom = false;
        let max = self.max_scroll();
        if max == 0 {
            self.follow_bottom = true;
            self.chat_scroll = 0;
        } else {
            self.chat_scroll = max;
        }
    }

    /// How many lines above the bottom can still be revealed.
    pub fn max_scroll(&self) -> usize {
        self.chat_total_lines
            .saturating_sub(self.chat_view_height.max(1))
    }

    /// True when the transcript is taller than the chat viewport.
    pub fn can_scroll(&self) -> bool {
        self.chat_total_lines > self.chat_view_height && self.chat_view_height > 0
    }

    /// Page size for PgUp/PgDn — almost a full viewport, at least 1.
    pub fn page_lines(&self) -> usize {
        self.chat_view_height.saturating_sub(1).max(1)
    }

    /// Scroll the transcript up by `lines` display rows (older content).
    pub fn scroll_up(&mut self, lines: usize) {
        if lines == 0 {
            return;
        }
        self.follow_bottom = false;
        self.chat_scroll = self.chat_scroll.saturating_add(lines);
        // Clamp when metrics are known (updated each draw); otherwise draw clamps.
        if self.chat_total_lines > 0 && self.chat_view_height > 0 {
            self.chat_scroll = self.chat_scroll.min(self.max_scroll());
        }
    }

    /// Scroll the transcript down by `lines` display rows (newer content).
    pub fn scroll_down(&mut self, lines: usize) {
        self.chat_scroll = self.chat_scroll.saturating_sub(lines);
        if self.chat_scroll == 0 {
            self.follow_bottom = true;
        }
    }

    /// Mouse wheel / trackpad: positive `delta` = scroll up (older).
    pub fn scroll_by_wheel(&mut self, delta: i32) {
        if delta > 0 {
            self.scroll_up(delta as usize);
        } else if delta < 0 {
            self.scroll_down((-delta) as usize);
        }
    }

    pub fn toggle_cursor(&mut self) {
        self.cursor_on = !self.cursor_on;
        if self.busy {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        // Preserve newlines for multi-line paste (normalize \r\n / \r → \n).
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in normalized.chars() {
            match ch {
                '\n' => self.input.push('\n'),
                c if !c.is_control() => self.input.push(c),
                _ => {}
            }
        }
        self.cursor_on = true;
        self.clear_notice();
    }

    /// How many visual lines the prompt input currently needs (capped).
    pub fn input_line_count(&self) -> usize {
        let n = self.input.split('\n').count().max(1);
        n.min(6)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> RunOutcome {
        if matches!(key.kind, crossterm::event::KeyEventKind::Release) {
            return RunOutcome::Noop;
        }

        // Floating modal captures all keys while open (all `/` UX lives here).
        if self.float_open() {
            return self.handle_float_key(key);
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => RunOutcome::Quit,
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) && self.busy => {
                self.submit_steer()
            }
            // Ctrl+P → command palette float
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_command_palette();
                RunOutcome::Noop
            }
            // Ctrl+L → model picker float
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_model_picker();
                RunOutcome::Noop
            }
            // Ctrl+J → insert newline (multi-line compose)
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.push('\n');
                self.cursor_on = true;
                self.clear_notice();
                RunOutcome::Noop
            }
            // Ctrl+O → expand/collapse last tool output body
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_last_tool_expand();
                RunOutcome::Noop
            }
            // Shift+Tab → cycle thinking level
            KeyCode::BackTab => RunOutcome::CycleThinking,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => self.submit_followup(),
            // Shift+Enter → newline (when terminal reports SHIFT)
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.input.push('\n');
                self.cursor_on = true;
                self.clear_notice();
                RunOutcome::Noop
            }

            KeyCode::Enter => {
                let t = self.input.trim();
                // Any bare / incomplete slash → open command float instead of sending.
                if t == "/" {
                    self.input.clear();
                    self.open_command_palette();
                    return RunOutcome::Noop;
                }
                if t == "/model" || t == "/model " {
                    self.input.clear();
                    self.open_model_picker();
                    return RunOutcome::Noop;
                }
                self.submit_prompt()
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.input.pop();
                self.cursor_on = true;
                self.clear_notice();
                RunOutcome::Noop
            }
            // `/` on empty input → open command float (primary slash entry).
            KeyCode::Char('/')
                if self.input.is_empty()
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.open_command_palette();
                RunOutcome::Noop
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.push(ch);
                self.cursor_on = true;
                self.clear_notice();
                RunOutcome::Noop
            }
            // History navigation — transcript is kept fully; only the viewport is windowed.
            KeyCode::PageUp => {
                self.scroll_up(self.page_lines());
                RunOutcome::Noop
            }
            KeyCode::PageDown => {
                self.scroll_down(self.page_lines());
                RunOutcome::Noop
            }
            KeyCode::Home => {
                self.scroll_to_top();
                RunOutcome::Noop
            }
            KeyCode::End => {
                self.scroll_to_bottom();
                RunOutcome::Noop
            }
            // ↑/↓ scroll chat history (also wheel via DECSET 1007).
            KeyCode::Up => {
                self.scroll_up(3);
                RunOutcome::Noop
            }
            KeyCode::Down => {
                self.scroll_down(3);
                RunOutcome::Noop
            }
            _ => RunOutcome::Noop,
        }
    }

    fn handle_float_key(&mut self, key: KeyEvent) -> RunOutcome {
        match key.code {
            KeyCode::Esc => {
                self.close_float();
                RunOutcome::Noop
            }
            KeyCode::Up => {
                if let Some(f) = self.float.as_mut() {
                    f.move_selection(-1);
                }
                RunOutcome::Noop
            }
            KeyCode::Down => {
                if let Some(f) = self.float.as_mut() {
                    f.move_selection(1);
                }
                RunOutcome::Noop
            }
            KeyCode::Backspace | KeyCode::Delete => {
                let empty = self
                    .float
                    .as_ref()
                    .map(|f| f.search.is_empty())
                    .unwrap_or(true);
                if empty {
                    // Backspace on empty search closes the float (like dismissing /).
                    self.close_float();
                } else if let Some(f) = self.float.as_mut() {
                    f.pop_search();
                }
                RunOutcome::Noop
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(f) = self.float.as_mut() {
                    f.push_search(ch);
                }
                RunOutcome::Noop
            }
            KeyCode::Enter | KeyCode::Tab => self.confirm_float_selection(),
            _ => RunOutcome::Noop,
        }
    }

    /// Confirm current float selection → nested float or slash Prompt.
    fn confirm_float_selection(&mut self) -> RunOutcome {
        let (kind, entry) = {
            let f = match self.float.as_ref() {
                Some(f) => f,
                None => return RunOutcome::Noop,
            };
            (f.kind, f.selected_entry())
        };
        let Some(entry) = entry else {
            return RunOutcome::Noop;
        };

        match kind {
            FloatKind::Models => {
                self.close_float();
                let cmd = format!("/model {}", entry.item.id);
                self.input.clear();
                RunOutcome::Prompt(cmd)
            }
            FloatKind::Thinking => {
                self.close_float();
                self.input.clear();
                RunOutcome::Prompt(format!("/thinking {}", entry.item.id))
            }
            FloatKind::Sessions => {
                self.close_float();
                self.input.clear();
                // id is path or session id accepted by /resume.
                RunOutcome::Prompt(format!("/resume {}", entry.item.id))
            }
            FloatKind::Tree => {
                self.close_float();
                self.input.clear();
                RunOutcome::Prompt(format!("/tree {}", entry.item.id))
            }
            FloatKind::Info => {
                // Read-only panel — Enter dismisses.
                self.close_float();
                RunOutcome::Noop
            }
            FloatKind::Help | FloatKind::Commands | FloatKind::Custom => {
                self.dispatch_command_item(&entry.item.id, &entry.item.hint)
            }
        }
    }

    /// Shared handler for command-palette / help rows.
    fn dispatch_command_item(&mut self, id: &str, hint: &str) -> RunOutcome {
        match id {
            "model" => {
                self.open_model_picker();
                RunOutcome::Noop
            }
            "help" => {
                self.open_help_float();
                RunOutcome::Noop
            }
            "thinking" => {
                self.open_thinking_float();
                RunOutcome::Noop
            }
            "quit" | "exit" => {
                self.close_float();
                RunOutcome::Quit
            }
            "clear" => {
                self.close_float();
                self.messages.clear();
                self.chat_scroll = 0;
                self.set_notice("chat cleared");
                RunOutcome::Noop
            }
            // These need runtime data → emit slash so CLI opens the right float.
            "resume" | "session" | "tree" | "new" | "name" | "export" | "compact"
            | "reload" | "skill" => {
                self.close_float();
                self.input.clear();
                let cmd = if hint.starts_with('/') {
                    hint.to_string()
                } else {
                    format!("/{id}")
                };
                // Trailing space commands (name) leave input for typing? Prefer float/prompt.
                if cmd.ends_with(' ') {
                    self.input = cmd;
                    RunOutcome::Noop
                } else {
                    RunOutcome::Prompt(cmd)
                }
            }
            _ => {
                self.close_float();
                if hint.starts_with('/') {
                    if hint == "/model" || hint.starts_with("/model ") {
                        self.open_model_picker();
                        RunOutcome::Noop
                    } else {
                        self.input.clear();
                        RunOutcome::Prompt(hint.to_string())
                    }
                } else {
                    RunOutcome::Noop
                }
            }
        }
    }

    pub fn handle_busy_key(&mut self, key: KeyEvent) {
        if matches!(key.kind, crossterm::event::KeyEventKind::Release) {
            return;
        }

        if self.float_open() {
            let _ = self.handle_float_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.request_abort();
                self.set_notice("interrupting…");
            }
            KeyCode::Esc => {
                self.request_abort();
                self.set_notice("interrupting…");
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = self.submit_steer();
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                let _ = self.submit_followup();
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.cursor_on = true;
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.push(ch);
                self.cursor_on = true;
            }
            KeyCode::PageUp => self.scroll_up(self.page_lines()),
            KeyCode::PageDown => self.scroll_down(self.page_lines()),
            KeyCode::Home => self.scroll_to_top(),
            KeyCode::End => self.scroll_to_bottom(),
            KeyCode::Up => self.scroll_up(3),
            KeyCode::Down => self.scroll_down(3),
            _ => {}
        }
    }

    fn submit_prompt(&mut self) -> RunOutcome {
        // Keep multi-line body; only trim ends.
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            return RunOutcome::Noop;
        }
        if text == "/quit" || text == "/exit" {
            return RunOutcome::Quit;
        }
        if text == "/help" {
            self.set_notice(
                "/session /resume /new /model /compact /thinking · skills auto-read · /skill:name force · Ctrl+J nl",
            );
            return RunOutcome::Noop;
        }
        if text == "/clear" {
            self.messages.clear();
            self.chat_scroll = 0;
            self.follow_bottom = true;
            self.set_notice("chat cleared");
            return RunOutcome::Noop;
        }
        // Bare /model → float picker (secondary menu in the float UI).
        if text == "/model" || text == "/model " {
            self.open_model_picker();
            return RunOutcome::Noop;
        }
        // UI slash commands — handled by one-cli without adding a chat turn.
        if is_ui_slash(&text) {
            return RunOutcome::Prompt(text);
        }
        // Skills, prompt templates, and normal messages are user turns.
        self.push_user(&text);
        RunOutcome::Prompt(text)
    }

    fn submit_followup(&mut self) -> RunOutcome {
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            return RunOutcome::Noop;
        }
        self.push_user(&text);
        self.followup_pending = Some(text.clone());
        RunOutcome::FollowUp(text)
    }

    fn submit_steer(&mut self) -> RunOutcome {
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            return RunOutcome::Noop;
        }
        self.push_user(&text);
        self.steer_pending = Some(text.clone());
        RunOutcome::Steer(text)
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{}ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let m = (secs / 60.0).floor() as u64;
        let s = secs % 60.0;
        format!("{m}m{s:.0}s")
    }
}

fn split_tool_text(content: &str) -> (String, String) {
    let content = content.trim();
    if let Some(open) = content.find('(') {
        if content.ends_with(')') && open > 0 {
            let name = content[..open].trim().to_string();
            let inner = content[open + 1..content.len() - 1].trim().to_string();
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return (name, inner);
            }
        }
    }
    let mut parts = content.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("tool").to_string();
    let detail = parts.next().unwrap_or("").to_string();
    (name, detail)
}

pub type InteractiveApp = App;

/// Slash commands handled by the CLI as UI ops (not agent user turns).
fn is_ui_slash(text: &str) -> bool {
    let cmd = text
        .split_whitespace()
        .next()
        .unwrap_or(text)
        .split(':')
        .next()
        .unwrap_or(text);
    matches!(
        cmd,
        "/session"
            | "/resume"
            | "/new"
            | "/name"
            | "/model"
            | "/thinking"
            | "/compact"
            | "/tree"
            | "/export"
            | "/reload"
            | "/clear"
            | "/help"
            | "/quit"
            | "/exit"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        }
    }

    #[test]
    fn enter_submits_prompt() {
        let mut app = App::new("test");
        app.input = "hello".into();
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            RunOutcome::Prompt(t) => assert_eq!(t, "hello"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(app.messages.last().unwrap().content, "hello");
    }

    #[test]
    fn page_up_disables_follow() {
        let mut app = App::new("test");
        for i in 0..20 {
            app.push_system(format!("m{i}"));
        }
        assert!(app.follow_bottom);
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(!app.follow_bottom);
        app.scroll_to_bottom();
        assert!(app.follow_bottom);
    }

    #[test]
    fn paste_preserves_newlines() {
        let mut app = App::new("test");
        app.handle_paste("foo\nbar");
        assert_eq!(app.input, "foo\nbar");
    }

    #[test]
    fn ctrl_j_inserts_newline() {
        let mut app = App::new("test");
        app.input = "a".into();
        app.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "a\n");
    }

    #[test]
    fn stream_sync_marks_assistant_streaming() {
        let mut app = App::new("test");
        app.begin_busy();
        app.append_stream("hi");
        app.sync_stream_message();
        let last = app.messages.last().unwrap();
        assert!(last.streaming);
        assert_eq!(last.content, "hi");
        app.finish_stream();
        assert!(!app.messages.last().unwrap().streaming);
        assert!(app.messages.last().unwrap().footer.is_some());
    }

    #[test]
    fn tool_lifecycle() {
        let mut app = App::new("test");
        app.push_tool_call("bash", "ls");
        assert_eq!(
            app.messages.last().unwrap().tool_status,
            Some(ToolStatus::Running)
        );
        app.finish_tool("bash", false);
        assert_eq!(
            app.messages.last().unwrap().tool_status,
            Some(ToolStatus::Done)
        );
    }

    #[test]
    fn tool_error_auto_expands_output() {
        let mut app = App::new("test");
        app.push_tool_call("bash", "cargo test");
        app.finish_tool_with_output(
            "bash",
            true,
            Some("error: could not compile `one`\n  --> src/lib.rs:1".into()),
        );
        let last = app.messages.last().unwrap();
        assert_eq!(last.tool_status, Some(ToolStatus::Error));
        assert!(last.tool_expanded);
        assert!(last.tool_output.as_ref().unwrap().contains("could not compile"));
        assert!(last.tool_summary.as_ref().unwrap().contains("error"));
    }

    #[test]
    fn alert_is_ui_only_role() {
        let mut app = App::new("test");
        app.push_error_alert("provider timeout");
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::Alert);
        assert_eq!(last.alert_level, Some(AlertLevel::Error));
    }

    #[test]
    fn edit_tool_gets_diff_summary() {
        let mut app = App::new("test");
        app.push_tool_call(
            "edit",
            r#"{"path":"src/a.rs","old_string":"fn a(){}","new_string":"fn a(){\n  1\n}"}"#,
        );
        app.finish_tool_with_output("edit", false, Some("Updated src/a.rs".into()));
        let last = app.messages.last().unwrap();
        assert_eq!(last.tool_status, Some(ToolStatus::Done));
        let summary = last.tool_summary.as_deref().unwrap_or("");
        assert!(summary.contains("edited") || summary.contains("a.rs"), "{summary}");
        let out = last.tool_output.as_deref().unwrap_or("");
        assert!(out.contains('+') || out.contains("Updated"), "{out}");
    }

    #[test]
    fn toast_expires_and_classifies_error() {
        let mut app = App::new("test");
        app.set_notice("error: boom");
        let t = app.toast_active().unwrap();
        assert_eq!(t.level, AlertLevel::Error);
        assert!(t.text.contains("boom"));
        // Force expiry.
        if let Some(toast) = app.toast.as_mut() {
            toast.created = Instant::now() - TOAST_TTL - std::time::Duration::from_secs(1);
        }
        app.tick_toast();
        assert!(app.toast_active().is_none());
    }

    #[test]
    fn three_done_tools_form_collapsible_group() {
        let mut app = App::new("test");
        for (name, args) in [
            ("read", r#"{"path":"a.rs"}"#),
            ("bash", r#"{"command":"ls"}"#),
            ("grep", r#"{"pattern":"x"}"#),
        ] {
            app.push_tool_call(name, args);
            app.finish_tool_with_output(name, false, Some("ok\nline2".into()));
            // Force collapsed body so group can form.
            if let Some(last) = app.messages.last_mut() {
                last.tool_expanded = false;
                last.tool_ungroup = false;
            }
        }
        assert!(tool_view::streak_can_collapse(&app.messages, 0, 3));
        app.toggle_last_tool_expand();
        assert!(app.messages.iter().all(|m| m.tool_ungroup));
        assert!(!tool_view::streak_can_collapse(&app.messages, 0, 3));
    }
}
