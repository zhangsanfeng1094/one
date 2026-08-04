//! Transcript mutations: user/assistant/tool rows, streaming, busy lifecycle.

use std::time::{Duration, Instant};

use crate::message::{
    summarize_tool_output, truncate_tool_output_for_ui, ChatLineTarget, Message, MessageRole,
    ToolStatus,
};
use crate::state::RunOutcome;
use crate::tool_view;

use super::helpers::{extract_job_id_from_task_output, format_duration, split_tool_text};
use super::{RetryWait, STATUS_BUSY, STATUS_IDLE};

impl super::App {
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(Message::user(text));
        self.scroll_to_bottom();
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) {
        let mut msg = Message::assistant(text);
        msg.footer = Some(self.turn_footer(None));
        self.messages.push(msg);
    }

    pub fn push_system(&mut self, text: impl Into<String>) {
        self.messages.push(Message::system(text));
    }

    pub fn push_tool(&mut self, text: impl Into<String>) {
        // Backward-compatible: `name(args)` or free text → running tool row.
        let text = text.into();
        let (name, detail) = split_tool_text(&text);
        self.messages
            .push(Message::tool(name, detail, ToolStatus::Running));
    }

    pub fn push_tool_call(&mut self, name: impl Into<String>, args: impl Into<String>) {
        self.push_tool_call_with_id(name, args, None);
    }

    pub fn push_tool_call_with_id(
        &mut self,
        name: impl Into<String>,
        args: impl Into<String>,
        call_id: Option<String>,
    ) {
        // Close thinking + assistant bubbles so tool rows sit between
        // completed segments, and the next tool-round thinking starts clean
        // (otherwise deltas keep appending into the same buffer and the next
        // bubble re-shows the previous segment's full text).
        self.seal_stream_segment();
        self.messages
            .push(Message::tool_with_id(name, args, ToolStatus::Running, call_id));
    }

    /// Finalize in-progress thinking / assistant stream bubbles and reset
    /// buffers (used between tool rounds).
    pub fn seal_stream_segment(&mut self) {
        // Thinking must end before tools — interleaved think→tool→think
        // rounds each own a separate bubble with only that round's deltas.
        self.finish_thinking_stream();

        if self.stream_buffer.is_empty() {
            // Seal a trailing empty assistant streaming marker if present.
            if let Some(last) = self.messages.last_mut() {
                if last.role == MessageRole::Assistant && last.streaming {
                    last.streaming = false;
                }
            }
            return;
        }
        self.sync_stream_message();
        if let Some(last) = self.messages.last_mut() {
            if last.role == MessageRole::Assistant && last.streaming {
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

    pub fn finish_tool_with_output(&mut self, name: &str, error: bool, output: Option<String>) {
        self.finish_tool_with_output_id(name, error, output, None);
    }

    /// Prefer matching by `call_id` so parallel same-name tools finish the right row.
    pub fn finish_tool_with_output_id(
        &mut self,
        name: &str,
        error: bool,
        output: Option<String>,
        call_id: Option<&str>,
    ) {
        let status = if error {
            ToolStatus::Error
        } else {
            ToolStatus::Done
        };
        let apply = |msg: &mut Message| {
            msg.tool_status = Some(status);
            msg.seal_duration();
            let args = msg.content.clone();
            let tool_name = msg.tool_name.clone().unwrap_or_else(|| name.to_string());
            // Preserve / recover job_id from tool output for post-run reopen.
            if tool_name == "task" {
                if msg.tool_job_id.is_none() {
                    if let Some(raw) = output.as_ref() {
                        if let Some(id) = extract_job_id_from_task_output(raw) {
                            msg.tool_job_id = Some(id);
                        }
                    }
                }
                msg.tool_ungroup = true;
            }
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
                // Task with job_id: prefer collapsed one-liner; click reopens log.
                msg.tool_expanded = if tool_name == "task" && msg.tool_job_id.is_some() {
                    false
                } else {
                    expand
                };
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

        // 1) Exact call_id match (correct for parallel batches / duplicate names).
        if let Some(id) = call_id {
            if !id.is_empty() {
                for msg in self.messages.iter_mut().rev() {
                    if msg.role == MessageRole::Tool
                        && msg.tool_status == Some(ToolStatus::Running)
                        && msg.tool_call_id.as_deref() == Some(id)
                    {
                        apply(msg);
                        return;
                    }
                }
            }
        }
        // 2) Name match among running tools (oldest first for same-name sequences
        //    when ends arrive in call order without ids).
        for msg in self.messages.iter_mut() {
            if msg.role == MessageRole::Tool
                && msg.tool_status == Some(ToolStatus::Running)
                && msg.tool_name.as_deref() == Some(name)
            {
                apply(msg);
                return;
            }
        }
        // 3) Fallback: mark any last running tool.
        for msg in self.messages.iter_mut().rev() {
            if msg.role == MessageRole::Tool && msg.tool_status == Some(ToolStatus::Running) {
                apply(msg);
                return;
            }
        }
        if error {
            let mut msg = Message::tool_with_id(
                name,
                "failed",
                ToolStatus::Error,
                call_id.map(|s| s.to_string()),
            );
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

    /// Expand a multi-tool chip into individual rows (bodies stay collapsed).
    pub(crate) fn expand_tool_group(&mut self, start: usize, len: usize) {
        for msg in &mut self.messages[start..start + len] {
            msg.tool_ungroup = true;
        }
    }

    /// Collapse an ungrouped multi-tool stack back into a chip.
    pub(crate) fn collapse_tool_group(&mut self, start: usize, len: usize) {
        for msg in &mut self.messages[start..start + len] {
            msg.tool_ungroup = false;
            msg.tool_expanded = false;
        }
    }

    /// Toggle last tool body, or expand/collapse a multi-tool group chip.
    pub fn toggle_last_tool_expand(&mut self) {
        if let Some((start, len)) = self.last_tool_streak() {
            if tool_view::streak_can_collapse(&self.messages, start, len) {
                self.expand_tool_group(start, len);
                return;
            }
            if tool_view::streak_group_eligible(&self.messages, start, len)
                && self.messages[start..start + len]
                    .iter()
                    .any(|m| m.tool_ungroup || m.tool_expanded)
            {
                self.collapse_tool_group(start, len);
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

    /// Toggle multi-tool group at streak start (chip expand / header collapse).
    pub fn toggle_tool_group_at(&mut self, start: usize) {
        let Some((s, len)) = self.tool_streak_covering(start) else {
            return;
        };
        if s != start {
            return;
        }
        if tool_view::streak_can_collapse(&self.messages, start, len) {
            self.expand_tool_group(start, len);
            return;
        }
        if tool_view::streak_group_eligible(&self.messages, start, len)
            && self.messages[start..start + len]
                .iter()
                .any(|m| m.tool_ungroup || m.tool_expanded)
        {
            self.collapse_tool_group(start, len);
        }
    }

    /// Toggle the tool message at `msg_index` (click target for a tool row).
    ///
    /// For `task` rows with a live `tool_job_id`, click opens the job detail
    /// panel (Grok Build-style) instead of only expanding the summary.
    pub fn toggle_tool_at(&mut self, msg_index: usize) {
        if self
            .messages
            .get(msg_index)
            .map(|m| m.role != MessageRole::Tool)
            .unwrap_or(true)
        {
            return;
        }
        // Collapsed multi-tool chip owned by first tool index → expand group.
        if let Some((start, len)) = self.tool_streak_covering(msg_index) {
            if tool_view::streak_can_collapse(&self.messages, start, len) {
                self.expand_tool_group(start, len);
                return;
            }
        }
        // task · job_id → open **subagent** live log (`/tasks`, not `/ps` bash).
        if let Some(msg) = self.messages.get(msg_index) {
            if msg.tool_name.as_deref() == Some("task") {
                if let Some(id) = msg.tool_job_id.clone() {
                    self.queue_busy_ui(RunOutcome::OpenSubagentDetail { id });
                    return;
                }
            }
        }
        if let Some(msg) = self.messages.get_mut(msg_index) {
            msg.tool_expanded = !msg.tool_expanded;
        }
        self.chat_focus = Some(msg_index);
    }

    /// Attach / refresh live job metadata on a running `task` tool row.
    pub fn update_task_tool_live(
        &mut self,
        job_id: &str,
        summary: impl Into<String>,
        bind_if_unbound: bool,
    ) {
        let summary = summary.into();
        for msg in self.messages.iter_mut().rev() {
            if msg.role != MessageRole::Tool || msg.tool_name.as_deref() != Some("task") {
                continue;
            }
            let matches = msg.tool_job_id.as_deref() == Some(job_id)
                || (bind_if_unbound
                    && msg.tool_job_id.is_none()
                    && msg.tool_status == Some(ToolStatus::Running));
            if !matches {
                continue;
            }
            if msg.tool_job_id.is_none() {
                msg.tool_job_id = Some(job_id.to_string());
            }
            // Keep auto-expanded so activity is visible without an extra click.
            msg.tool_ungroup = true;
            if msg.tool_status == Some(ToolStatus::Running) {
                msg.tool_summary = Some(summary);
            }
            return;
        }
    }

    /// Toggle a thinking block at `msg_index` (click / enter).
    pub fn toggle_thinking_at(&mut self, msg_index: usize) {
        if let Some(msg) = self.messages.get_mut(msg_index) {
            if msg.role == MessageRole::Thinking && !msg.streaming {
                msg.thinking_expanded = !msg.thinking_expanded;
            }
        }
        self.chat_focus = Some(msg_index);
    }

    /// Interactive transcript rows (tools + finished thinking) for j/k focus.
    pub fn focusable_message_indices(&self) -> Vec<usize> {
        self.messages
            .iter()
            .enumerate()
            .filter(|(_, m)| match m.role {
                MessageRole::Tool => true,
                MessageRole::Thinking if !m.streaming => true,
                _ => false,
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Move chat focus by `delta` among focusable rows. Enters browse mode.
    pub fn move_chat_focus(&mut self, delta: i32) {
        let targets = self.focusable_message_indices();
        if targets.is_empty() {
            return;
        }
        let cur = self
            .chat_focus
            .and_then(|f| targets.iter().position(|&i| i == f))
            .unwrap_or(if delta >= 0 {
                targets.len().saturating_sub(1)
            } else {
                0
            });
        let next = if delta >= 0 {
            (cur + 1).min(targets.len() - 1)
        } else {
            cur.saturating_sub(1)
        };
        self.chat_focus = Some(targets[next]);
        self.follow_bottom = false;
        // Nudge scroll so the focused row stays discoverable (best-effort).
        // Exact line mapping is recomputed on next draw from chat_line_owners.
        if let Some(owner_line) = self
            .chat_line_owners
            .iter()
            .position(|o| matches!(o, Some(ChatLineTarget::Message(i)) if *i == targets[next]))
        {
            let view_h = self.chat_view_height.max(1);
            if owner_line < self.chat_view_start {
                self.chat_scroll = owner_line;
            } else if owner_line >= self.chat_view_start + view_h {
                self.chat_scroll = owner_line.saturating_sub(view_h.saturating_sub(1));
            }
        }
    }

    /// Toggle expand on the focused chat row (or last tool when none).
    pub fn toggle_focused_or_last(&mut self) -> bool {
        if let Some(idx) = self.chat_focus {
            match self.messages.get(idx).map(|m| m.role) {
                Some(MessageRole::Thinking) => {
                    self.toggle_thinking_at(idx);
                    return true;
                }
                Some(MessageRole::Tool) => {
                    self.toggle_tool_at(idx);
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    pub fn append_stream(&mut self, delta: &str) {
        self.clear_retry_wait();
        // Text arriving after thinking finalizes the thinking bubble.
        if !self.thinking_buffer.is_empty() {
            self.finish_thinking_stream();
        }
        self.stream_buffer.push_str(delta);
        if self.follow_bottom {
            self.scroll_to_bottom();
        }
    }

    pub fn append_thinking_stream(&mut self, delta: &str) {
        self.clear_retry_wait();
        self.thinking_buffer.push_str(delta);
        if self.follow_bottom {
            self.scroll_to_bottom();
        }
    }

    pub fn sync_stream_message(&mut self) {
        self.sync_thinking_message();
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

    pub fn sync_thinking_message(&mut self) {
        if self.thinking_buffer.is_empty() {
            return;
        }

        // Update the open streaming thinking bubble even if a later row
        // (e.g. tool) was already inserted — never invent a second bubble
        // that re-dumps the same cumulative buffer.
        if let Some(msg) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Thinking && m.streaming)
        {
            msg.content = self.thinking_buffer.clone();
            msg.thinking_expanded = true;
            return;
        }

        self.messages
            .push(Message::streaming_thinking(&self.thinking_buffer));
    }

    pub(crate) fn finish_thinking_stream(&mut self) {
        if self.thinking_buffer.is_empty() {
            // Still close an orphan streaming thinking bubble (no more deltas).
            if let Some(msg) = self
                .messages
                .iter_mut()
                .rev()
                .find(|m| m.role == MessageRole::Thinking && m.streaming)
            {
                msg.streaming = false;
                msg.thinking_expanded = self.show_thinking;
                msg.seal_duration();
            }
            return;
        }
        self.sync_thinking_message();
        if let Some(msg) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Thinking && m.streaming)
        {
            msg.streaming = false;
            msg.thinking_expanded = self.show_thinking;
            msg.seal_duration();
        }
        self.thinking_buffer.clear();
    }

    pub fn finish_stream(&mut self) {
        self.finish_stream_with_interrupted(false);
    }

    pub fn finish_stream_with_interrupted(&mut self, interrupted: bool) {
        self.finish_thinking_stream();
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
    }

    pub(crate) fn attach_turn_footer(&mut self, interrupted: bool) {
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
                msg.seal_duration();
            }
        }
    }

    pub(crate) fn turn_footer(&self, interrupted: Option<bool>) -> String {
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
        self.thinking_buffer.clear();
        self.remove_trailing_empty_stream();
    }

    pub(crate) fn remove_trailing_empty_stream(&mut self) {
        if let Some(last) = self.messages.last() {
            if last.streaming && last.content.is_empty() {
                self.messages.pop();
            }
        }
    }

    pub fn begin_busy(&mut self) {
        self.busy = true;
        self.stream_buffer.clear();
        self.thinking_buffer.clear();
        self.remove_trailing_empty_stream();
        self.status = STATUS_BUSY.into();
        self.follow_bottom = true;
        self.turn_started = Some(Instant::now());
        self.spinner_frame = 0;
        self.clear_retry_wait();
        self.scroll_to_bottom();
    }

    pub fn end_busy(&mut self) {
        self.busy = false;
        self.status = STATUS_IDLE.into();
        self.clear_retry_wait();
    }

    /// Show a live retry countdown while the agent waits before re-sampling.
    pub fn begin_retry_wait(&mut self, retry: usize, max_retries: usize, delay: Duration) {
        self.retry_wait = Some(RetryWait {
            retry,
            max_retries,
            ready_at: Instant::now() + delay,
        });
    }

    pub fn clear_retry_wait(&mut self) {
        self.retry_wait = None;
    }

    /// `(retry, max_retries, seconds_remaining)` for the animated status chip.
    pub fn retry_wait_status(&self) -> Option<(usize, usize, u64)> {
        let retry = self.retry_wait.as_ref()?;
        let remaining = retry.ready_at.saturating_duration_since(Instant::now());
        let seconds = ((remaining.as_millis() as u64).saturating_add(999)) / 1000;
        Some((retry.retry, retry.max_retries, seconds))
    }

    pub fn take_followup(&mut self) -> Option<String> {
        self.followup_pending.take()
    }

    pub fn take_steer(&mut self) -> Option<String> {
        self.steer_pending.take()
    }

    /// Queue a UI action while the agent is streaming (CLI drains via [`take_busy_ui`]).
    pub fn queue_busy_ui(&mut self, outcome: RunOutcome) {
        if matches!(outcome, RunOutcome::Noop) {
            return;
        }
        self.busy_ui_queue.push_back(outcome);
    }

    /// Pop the next busy-time UI action (oldest first).
    pub fn take_busy_ui(&mut self) -> Option<RunOutcome> {
        self.busy_ui_queue.pop_front()
    }

    pub fn request_abort(&mut self) {
        self.abort_pending = true;
    }

    pub fn take_abort(&mut self) -> bool {
        std::mem::take(&mut self.abort_pending)
    }

    /// Request immediate interactive exit (Ctrl+C). Distinct from soft abort (`q` / Esc).
    pub fn request_force_quit(&mut self) {
        self.force_quit_pending = true;
        // Also trip abort so in-flight agent work stops if the process is about to leave.
        self.abort_pending = true;
    }

    pub fn take_force_quit(&mut self) -> bool {
        std::mem::take(&mut self.force_quit_pending)
    }

    pub fn force_quit_pending(&self) -> bool {
        self.force_quit_pending
    }
}
