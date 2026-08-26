//! Chat viewport: text selection, copy, scroll, and click targets.

use crate::message::{ChatLineTarget, MessageRole};
use crate::state::{display_col_to_caret, SelectPos};
use crate::tool_view;

impl super::App {
    /// Absolute terminal row → 0-based row in the painted transcript.
    ///
    /// `None` when the pointer is on the grok header, sticky prompt, or
    /// prompt/footer — those are not chat lines.
    pub fn mouse_to_chat_row(&self, mouse_row: u16) -> Option<usize> {
        let y = self.chat_content_y;
        let h = self.chat_view_height;
        if h == 0 || mouse_row < y {
            return None;
        }
        let rel = (mouse_row - y) as usize;
        (rel < h).then_some(rel)
    }

    pub fn view_row_to_line(&self, row_in_view: usize) -> Option<usize> {
        if row_in_view < self.chat_top_pad {
            return None;
        }
        let line = self
            .chat_view_start
            .saturating_add(row_in_view - self.chat_top_pad);
        if line < self.chat_total_lines {
            Some(line)
        } else {
            None
        }
    }

    /// Map a terminal mouse column to a caret on the given display line.
    ///
    /// `terminal_col` is the absolute screen column from the mouse event.
    pub fn view_col_to_caret(&self, line: usize, terminal_col: u16) -> usize {
        let display_col = terminal_col.saturating_sub(self.chat_content_x) as usize;
        let text = self
            .chat_line_text
            .get(line)
            .map(|s| s.as_str())
            .unwrap_or("");
        display_col_to_caret(text, display_col)
    }

    pub(crate) fn pos_at(&self, row_in_view: usize, terminal_col: u16) -> Option<SelectPos> {
        let line = self.view_row_to_line(row_in_view)?;
        let col = self.view_col_to_caret(line, terminal_col);
        Some(SelectPos::new(line, col))
    }

    /// Like [`pos_at`], but clamps onto the nearest transcript line when the
    /// pointer sits on empty pad / past the last content row.
    pub(crate) fn pos_at_clamped(
        &self,
        row_in_view: usize,
        terminal_col: u16,
    ) -> Option<SelectPos> {
        if self.chat_total_lines == 0 {
            return None;
        }
        if let Some(pos) = self.pos_at(row_in_view, terminal_col) {
            return Some(pos);
        }
        let max_line = self.chat_total_lines.saturating_sub(1);
        let last_visible = self
            .chat_view_start
            .saturating_add(self.chat_view_height.saturating_sub(1))
            .min(max_line);
        let line = if row_in_view < self.chat_top_pad {
            self.chat_view_start.min(max_line)
        } else {
            // Past content in the viewport (short transcript) or after last line.
            last_visible
        };
        let col = self.view_col_to_caret(line, terminal_col);
        Some(SelectPos::new(line, col))
    }

    /// Click at row offset within the chat viewport (0 = top visible line).
    pub fn click_chat_row(&mut self, row_in_view: usize) {
        let Some(line) = self.view_row_to_line(row_in_view) else {
            return;
        };
        if let Some(Some(target)) = self.chat_line_owners.get(line).copied() {
            match target {
                ChatLineTarget::ToolGroup(start) => {
                    self.chat_focus = Some(start);
                    self.toggle_tool_group_at(start);
                }
                ChatLineTarget::User(msg_i) => {
                    self.chat_focus = Some(msg_i);
                    self.toggle_user_or_turn_fold_at(msg_i);
                }
                ChatLineTarget::UserContent(msg_i) => {
                    self.chat_focus = Some(msg_i);
                    self.toggle_user_content_fold_at(msg_i);
                }
                ChatLineTarget::Message(msg_i) => match self.messages.get(msg_i).map(|m| m.role) {
                    Some(MessageRole::Thinking) => self.toggle_thinking_at(msg_i),
                    Some(MessageRole::Tool) => self.toggle_tool_at(msg_i),
                    Some(MessageRole::User) => {
                        if let Some(msg) = self.messages.get_mut(msg_i) {
                            msg.info_expanded = !msg.info_expanded;
                        }
                    }
                    Some(MessageRole::Alert) => {
                        // dismiss alerts if clickable — leave existing behaviour via tool path no-op
                    }
                    _ => self.toggle_tool_at(msg_i),
                },
            }
        }
    }

    /// Begin in-app selection at a viewport cell (mouse down).
    pub fn select_begin(&mut self, row_in_view: usize, terminal_col: u16) {
        let Some(pos) = self.pos_at_clamped(row_in_view, terminal_col) else {
            self.clear_selection();
            return;
        };
        self.select_anchor = Some(pos);
        self.select_end = Some(pos);
        self.select_dragging = false;
    }

    /// Extend selection.
    ///
    /// `from_drag`: true for Drag/Moved while held (always a select gesture).
    /// false for mouse-up release (only a real range counts as select).
    pub fn select_update(&mut self, row_in_view: usize, terminal_col: u16, from_drag: bool) {
        let Some(pos) = self.pos_at_clamped(row_in_view, terminal_col) else {
            return;
        };
        if self.select_anchor.is_none() {
            self.select_anchor = Some(pos);
        }
        self.select_end = Some(pos);
        if from_drag {
            // Any pointer motion while held → select → auto-copy on release.
            self.select_dragging = true;
        }
        if let (Some(a), Some(b)) = (self.select_anchor, self.select_end) {
            if a != b {
                self.select_dragging = true;
            }
        }
        // Freeze the viewport while dragging. If we only clear `follow_bottom`
        // and leave `chat_scroll` at 0 (the live-follow sentinel), the next
        // paint jumps to the top of the transcript — selection then spans
        // almost everything. Mirror `scroll_up`: pin scroll at the current
        // bottom edge when leaving live follow.
        if self.select_dragging {
            self.freeze_viewport_for_select();
        }
    }

    /// Leave live follow without jumping the chat window (used by drag-select).
    fn freeze_viewport_for_select(&mut self) {
        if self.follow_bottom {
            self.chat_scroll = self.max_scroll();
            self.follow_bottom = false;
            self.sync_view_start_from_scroll();
        }
    }

    /// Keep hit-testing in sync after programmatic scroll during a drag
    /// (paint only updates `chat_view_start` on the next frame).
    fn sync_view_start_from_scroll(&mut self) {
        if self.chat_view_height == 0 {
            return;
        }
        let max = self.max_scroll();
        self.chat_view_start = if self.follow_bottom {
            max
        } else {
            self.chat_scroll.min(max)
        };
    }

    /// Drag while the left button is held: edge auto-scroll + extend selection.
    ///
    /// Without this, selection stops at the first/last *visible* row — the user
    /// can only select one viewport of lines. When the pointer sits on the top/
    /// bottom edge (or leaves the chat pane into the prompt), we scroll the
    /// transcript so selection can grow beyond the current window.
    ///
    /// `chat_h` is the chat pane height in terminal rows (same metric as mouse.y).
    pub fn select_drag(&mut self, mouse_row: u16, mouse_col: u16, chat_h: u16) {
        if chat_h == 0 || self.chat_total_lines == 0 {
            return;
        }
        // Enter drag mode before freeze/scroll so live-follow does not jump.
        self.select_dragging = true;
        self.freeze_viewport_for_select();

        // Rows at the rim (and anything outside the chat pane) auto-scroll.
        // Outside below the chat (prompt/footer) counts as "keep going down".
        const EDGE: u16 = 1;
        let at_top = mouse_row < EDGE;
        let at_bottom = mouse_row + EDGE >= chat_h || mouse_row >= chat_h;
        if at_top {
            self.scroll_up(1);
            self.sync_view_start_from_scroll();
        } else if at_bottom {
            // scroll_down may re-enable follow_bottom at the true end; re-freeze
            // so a subsequent paint does not treat us as live-follow with scroll=0
            // mid-gesture (selection must stay anchored to absolute lines).
            let was_follow = self.follow_bottom;
            self.scroll_down(1);
            if self.follow_bottom && !was_follow {
                // Reached true bottom — keep the viewport pinned for hit-testing.
                self.chat_scroll = self.max_scroll();
                self.follow_bottom = false;
            }
            self.sync_view_start_from_scroll();
        }

        let row = if mouse_row >= chat_h {
            chat_h.saturating_sub(1) as usize
        } else {
            mouse_row as usize
        };
        self.select_update(row, mouse_col, true);
    }

    /// Mouse-up after a press that may have left the chat pane.
    pub fn select_finish_at(&mut self, mouse_row: u16, mouse_col: u16, chat_h: u16) {
        if chat_h == 0 {
            self.clear_selection();
            return;
        }
        let row = if mouse_row >= chat_h {
            chat_h.saturating_sub(1) as usize
        } else {
            mouse_row as usize
        };
        self.select_finish(row, mouse_col);
    }

    /// Ordered half-open selection endpoints, if any.
    ///
    /// Returns `None` when there is no anchor, or when anchor == end (empty).
    pub fn selection_span(&self) -> Option<(SelectPos, SelectPos)> {
        let a = self.select_anchor?;
        let b = self.select_end.unwrap_or(a);
        if a == b {
            return None;
        }
        let (mut lo, mut hi) = if a.cmp_doc(b) == std::cmp::Ordering::Less {
            (a, b)
        } else {
            (b, a)
        };
        // Clamp lines into the known transcript when metrics are available.
        if self.chat_total_lines > 0 {
            let max_line = self.chat_total_lines.saturating_sub(1);
            lo.line = lo.line.min(max_line);
            hi.line = hi.line.min(max_line);
        }
        if !self.chat_line_text.is_empty() {
            let clamp_col = |p: SelectPos| {
                let len = self
                    .chat_line_text
                    .get(p.line)
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
                SelectPos::new(p.line, p.col.min(len))
            };
            lo = clamp_col(lo);
            hi = clamp_col(hi);
        }
        if lo == hi {
            return None;
        }
        Some((lo, hi))
    }

    /// Inclusive absolute line range covering the selection (for coarse checks).
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let (lo, hi) = self.selection_span()?;
        Some((lo.line, hi.line))
    }

    pub fn clear_selection(&mut self) {
        self.select_anchor = None;
        self.select_end = None;
        self.select_dragging = false;
    }

    /// True when selection spans more than one display line.
    pub fn selection_is_multi_line(&self) -> bool {
        self.selection_span()
            .is_some_and(|(lo, hi)| hi.line > lo.line)
    }

    /// True when there is a non-empty character selection.
    pub fn has_selection(&self) -> bool {
        self.selection_span().is_some()
    }

    /// Plain text for the selected region (joined with `\n` across lines).
    pub fn selection_text(&self) -> Option<String> {
        let (lo, hi) = self.selection_span()?;
        if self.chat_line_text.is_empty() {
            return None;
        }
        let last = self.chat_line_text.len().saturating_sub(1);
        let lo_line = lo.line.min(last);
        let hi_line = hi.line.min(last);

        let slice_chars = |s: &str, start: usize, end: usize| -> String {
            let n = s.chars().count();
            let start = start.min(n);
            let end = end.min(n).max(start);
            s.chars().skip(start).take(end - start).collect()
        };

        let text = if lo_line == hi_line {
            let s = self.chat_line_text[lo_line].as_str();
            slice_chars(s, lo.col, hi.col)
        } else {
            let mut parts = Vec::new();
            // First line: from lo.col to end.
            let first = self.chat_line_text[lo_line].as_str();
            parts.push(slice_chars(first, lo.col, first.chars().count()));
            // Middle lines: full.
            for line in &self.chat_line_text[lo_line + 1..hi_line] {
                parts.push(line.clone());
            }
            // Last line: from start to hi.col.
            let last_s = self.chat_line_text[hi_line].as_str();
            parts.push(slice_chars(last_s, 0, hi.col));
            parts.join("\n")
        };

        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Queue selection (or last assistant) for clipboard copy (OSC 52 + host fallbacks).
    /// Always auto-copies when there is a real selection — default UX.
    pub fn request_copy_selection(&mut self) -> bool {
        if let Some(text) = self.selection_text() {
            let lines = text.lines().count().max(1);
            let n = text.chars().count();
            self.clipboard_pending = Some(text);
            // Toast updated after terminal flush with ok/err; provisional notice:
            if lines > 1 {
                self.set_notice(format!("copying {lines} lines…"));
            } else {
                self.set_notice(format!("copying {n} chars…"));
            }
            return true;
        }
        // Fallback: last assistant bubble (keybinding with no selection).
        if let Some(msg) = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant && !m.content.is_empty())
        {
            let n = msg.content.chars().count();
            self.clipboard_pending = Some(msg.content.clone());
            self.set_notice(format!("copying last reply ({n} chars)…"));
            return true;
        }
        self.set_notice("nothing to copy");
        false
    }

    /// Finish pointer gesture: drag with a real range → **auto-copy**;
    /// plain click (no movement / empty range) → tool expand.
    pub fn select_finish(&mut self, row_in_view: usize, terminal_col: u16) {
        // Apply release cell without forcing drag (click stays click).
        self.select_update(row_in_view, terminal_col, false);
        // Selecting text always copies on release when there is a non-empty range
        // (or any drag gesture that produced a range).
        if self.select_dragging && self.has_selection() {
            let _ = self.request_copy_selection();
            self.select_dragging = false;
            return;
        }
        // Pure click or empty drag: clear + expand tools.
        self.clear_selection();
        self.click_chat_row(row_in_view);
    }

    pub(crate) fn last_tool_streak(&self) -> Option<(usize, usize)> {
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

    pub(crate) fn tool_streak_covering(&self, idx: usize) -> Option<(usize, usize)> {
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
            self.chat_scroll = 0;
        }
    }

    /// Scroll directly to the given transcript line (e.g. user message start).
    pub fn scroll_to_line(&mut self, line: usize) {
        self.follow_bottom = false;
        self.chat_scroll = line.min(self.max_scroll());
    }

    /// Jump to the start of the user query pinned in the sticky bar.
    pub fn scroll_to_sticky(&mut self) -> bool {
        if let Some(line) = self.chat_sticky_line {
            self.scroll_to_line(line);
            true
        } else {
            false
        }
    }

    /// Click handler for the sticky bar. If `mouse_row` matches the rendered
    /// sticky bar row, jump directly to that user message start.
    pub fn click_sticky(&mut self, mouse_row: u16) -> bool {
        if self.chat_sticky_y == Some(mouse_row) {
            self.scroll_to_sticky()
        } else {
            false
        }
    }

    /// Jump to the first tool row of the current turn.
    pub fn scroll_to_turn_tools(&mut self) {
        let Some(line) = self.chat_turn_tools_line else {
            return;
        };
        self.follow_bottom = false;
        self.chat_scroll = line.min(self.max_scroll());
    }

    /// Jump to the assistant answer of the current turn.
    pub fn scroll_to_turn_answer(&mut self) {
        let Some(line) = self.chat_turn_answer_line else {
            return;
        };
        self.follow_bottom = false;
        self.chat_scroll = line.min(self.max_scroll());
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
        if self.follow_bottom {
            self.follow_bottom = false;
            self.chat_scroll = self.max_scroll();
        }
        self.chat_scroll = self.chat_scroll.saturating_sub(lines);
    }

    /// Scroll the transcript down by `lines` display rows (newer content).
    pub fn scroll_down(&mut self, lines: usize) {
        if self.follow_bottom {
            return;
        }
        let max = self.max_scroll();
        self.chat_scroll = self.chat_scroll.saturating_add(lines).min(max);
        if self.chat_scroll == max && !self.messages.is_empty() {
            self.follow_bottom = true;
            self.chat_scroll = 0;
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

    /// True while a centered float menu is open (steals wheel from chat).
    pub fn has_float(&self) -> bool {
        self.float.is_some()
    }

    /// Mouse wheel over a float: move selection / log scroll anchor.
    ///
    /// `up = true` → older / previous rows (selection decreases).
    pub fn scroll_float_wheel(&mut self, up: bool, lines: usize) {
        if lines == 0 {
            return;
        }
        if self
            .float
            .as_ref()
            .is_some_and(|f| f.kind == crate::float::FloatKind::Context)
        {
            let delta = if up {
                -(lines as isize)
            } else {
                lines as isize
            };
            self.scroll_context(delta);
            return;
        }
        let Some(f) = self.float.as_mut() else {
            return;
        };
        let delta = if up {
            -(lines as isize)
        } else {
            lines as isize
        };
        f.move_selection(delta);
    }

    /// Clamp `/context` overlay scroll to the last-drawn body.
    pub(crate) fn scroll_context(&mut self, delta: isize) {
        let max = self
            .context_line_count
            .saturating_sub(self.context_view_height.max(1));
        let Some(f) = self.float.as_mut() else {
            return;
        };
        if f.kind != crate::float::FloatKind::Context {
            return;
        }
        if delta < 0 {
            f.selected = f.selected.saturating_sub((-delta) as usize);
        } else {
            f.selected = (f.selected + delta as usize).min(max);
        }
    }

    pub(crate) fn scroll_context_end(&mut self) {
        let max = self
            .context_line_count
            .saturating_sub(self.context_view_height.max(1));
        if let Some(f) = self.float.as_mut() {
            if f.kind == crate::float::FloatKind::Context {
                f.selected = max;
            }
        }
    }

    /// Page-scroll the open float (PgUp/PgDn).
    pub fn scroll_float_page(&mut self, up: bool) {
        // ~one viewport; float list is typically ≤28 rows.
        self.scroll_float_wheel(up, 10);
    }

    pub fn toggle_cursor(&mut self) {
        // Blink only while the main prompt owns focus. Elsewhere keep
        // `cursor_on = true` so the caret is immediately visible when focus
        // returns (no mid-off phase), and do not advance the blink phase
        // during float / select / j/k transcript browse.
        if self.prompt_focused() {
            self.cursor_on = !self.cursor_on;
        } else {
            self.cursor_on = true;
        }
        if self.busy {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
        }
    }
}
