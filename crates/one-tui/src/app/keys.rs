//! Idle-mode keyboard routing (prompt, slash menu, global chords).

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::{RunOutcome, WELCOME_TRY_PROMPTS};

use super::{CTRL_C_DOUBLE_MS, ESC_DOUBLE_MS};

impl super::App {
    pub fn handle_key(&mut self, key: KeyEvent) -> RunOutcome {
        if matches!(key.kind, crossterm::event::KeyEventKind::Release) {
            return RunOutcome::Noop;
        }

        // Progressive Ctrl+C: dismiss overlay / clear draft / double-tap quit.
        // Handled before select/float so Settings (and other floats) always react.
        if Self::is_ctrl_c(key) {
            return self.handle_ctrl_c();
        }
        // Any other key cancels a pending "press again to quit".
        self.last_ctrl_c_at = None;

        // Help: handle before select/float so one chord always opens the catalog.
        // Primary is Alt+H (Ctrl chords are often eaten once by IME / terminal).
        if Self::is_help_key(key) {
            self.select = None;
            self.open_help_float();
            return RunOutcome::Noop;
        }

        // Grok: Ctrl+F on a subagent lifecycle row opens the framed child view.
        if !self.float_open() && Self::is_ctrl_f(key) {
            if let Some(id) = self.focused_subagent_job_id() {
                return RunOutcome::OpenSubagentDetail { id };
            }
        }

        if Self::is_goto_bottom_key(key) {
            self.scroll_to_bottom();
            return RunOutcome::Noop;
        }

        // Docked select (model / field edit / ask) captures keys before float.
        if self.select.is_some() {
            if matches!(
                self.select_kind,
                Some(crate::state::SelectKind::Approval { .. })
            ) && (matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
                && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                let res = crate::select::SelectResult::Confirmed {
                    ids: vec!["always".to_string()],
                    other: None,
                };
                if let Some(outcome) = self.apply_select_result(res) {
                    return outcome;
                }
                return RunOutcome::Noop;
            }
            if let Some(prompt) = self.select.as_mut() {
                if let Some(result) = prompt.handle_key(key) {
                    if let Some(outcome) = self.apply_select_result(result) {
                        return outcome;
                    }
                }
            }
            return RunOutcome::Noop;
        }

        // Center float (Settings / commands / sessions).
        if self.float_open() {
            return self.handle_float_key(key);
        }

        match key.code {
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) && self.busy => {
                self.submit_steer()
            }
            // Ctrl+P → older prompt history (readline).
            KeyCode::Char('p')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.history_prev();
                RunOutcome::Noop
            }
            // Ctrl+N → fresh conversation, with an explicit confirmation.
            KeyCode::Char('n') | KeyCode::Char('N')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.open_new_session_confirm();
                RunOutcome::Noop
            }
            // Ctrl+A / Ctrl+E → caret home / end (readline).
            KeyCode::Char('a')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input_cursor_home();
                self.cursor_on = true;
                RunOutcome::Noop
            }
            KeyCode::Char('e')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input_cursor_end();
                self.cursor_on = true;
                RunOutcome::Noop
            }
            // Ctrl+L → model select (docked above input)
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_model_select();
                RunOutcome::Noop
            }
            // Ctrl+G → Settings center float
            // (Ctrl+, / Ctrl+. often swallowed by IME or never sent by terminals)
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_settings_float();
                RunOutcome::Noop
            }
            // Ctrl+V / Alt+V / Ctrl+Alt+V → host clipboard image (Codex-style).
            // Bracketed paste only carries text; bitmaps need PowerShell/wl-paste/xclip.
            // Prefer Ctrl+Alt+V under WSL when the terminal swallows Ctrl+V for text paste.
            KeyCode::Char(c)
                if c.eq_ignore_ascii_case(&'v')
                    && key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.leave_history_browse();
                let _ = self.try_paste_clipboard_image(true);
                RunOutcome::Noop
            }
            // Ctrl+J → insert newline (multi-line compose)
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.leave_history_browse();
                self.insert_input_char('\n');
                self.clear_notice();
                RunOutcome::Noop
            }
            // Ctrl+O → Toggle always-approve (YOLO) mode, or toggle tool expand if chat focused
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.chat_focus.is_some() {
                    self.toggle_last_tool_expand();
                    RunOutcome::Noop
                } else {
                    RunOutcome::ToggleAlwaysApprove
                }
            }
            // Ctrl+T → show/hide thinking body (Pi-style)
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_show_thinking();
                RunOutcome::Noop
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => self.submit_followup(),
            // Shift+Enter → newline (when terminal reports SHIFT)
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.leave_history_browse();
                self.insert_input_char('\n');
                self.clear_notice();
                RunOutcome::Noop
            }

            KeyCode::Enter => {
                // `/` menu open → complete highlighted row (and maybe run).
                if self.slash_menu_visible() {
                    return self.confirm_slash_menu();
                }
                let t = self.input.trim();
                if t == "/model" || t == "/model " {
                    self.input.clear();
                    self.open_model_select();
                    return RunOutcome::Noop;
                }
                if t == "/settings" || t == "/settings " {
                    self.input.clear();
                    self.open_settings_float();
                    return RunOutcome::Noop;
                }
                if t == "/skills" || t == "/skills " {
                    self.input.clear();
                    self.open_skills_float();
                    return RunOutcome::Noop;
                }
                if t == "/mcp" || t == "/mcp " {
                    self.input.clear();
                    return RunOutcome::OpenMcpPanel;
                }
                // Empty prompt + focused task row → open TV4 framed transcript.
                if t.is_empty() {
                    if let Some(id) = self.focused_subagent_job_id() {
                        return RunOutcome::OpenSubagentDetail { id };
                    }
                    if self.chat_focus.is_some() && self.toggle_focused_or_last() {
                        return RunOutcome::Noop;
                    }
                }
                self.submit_prompt()
            }
            // Shift+Tab (BackTab) → cycle Plan / Build. Plain Tab remains completion.
            KeyCode::BackTab => {
                if self.busy {
                    RunOutcome::Noop
                } else {
                    RunOutcome::CycleAgentMode
                }
            }
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if self.busy {
                    RunOutcome::Noop
                } else {
                    RunOutcome::CycleAgentMode
                }
            }
            // Tab → slash complete, else path / @file completion.
            KeyCode::Tab => {
                if self.slash_menu_visible() {
                    self.apply_slash_completion();
                } else {
                    self.complete_path_token();
                }
                RunOutcome::Noop
            }
            KeyCode::Backspace => {
                self.leave_history_browse();
                self.pop_input();
                self.clamp_slash_selection();
                RunOutcome::Noop
            }
            KeyCode::Delete => {
                self.leave_history_browse();
                self.delete_input_forward();
                self.clamp_slash_selection();
                RunOutcome::Noop
            }
            // Esc Esc: clear draft → history, or open rewind when empty (Claude Code).
            KeyCode::Esc => {
                if self.slash_menu_visible() {
                    // Dismiss slash: clear incomplete command.
                    self.input.clear();
                    self.input_cursor = 0;
                    self.slash_selected = 0;
                    self.clear_notice();
                    return RunOutcome::Noop;
                }
                self.handle_esc()
            }
            // Help chord is handled early via `is_help_key` (before select/float).
            // `/` inserts into input and shows docked command list (not center float).
            KeyCode::Char('/')
                if self.input.is_empty()
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.leave_history_browse();
                self.insert_input_char('/');
                self.slash_selected = 0;
                self.clamp_slash_selection();
                self.clear_notice();
                RunOutcome::Noop
            }
            // Empty welcome: `1`–`3` run sample prompts (matches try list).
            KeyCode::Char(ch @ '1'..='3')
                if self.messages.is_empty()
                    && self.input.is_empty()
                    && !self.busy
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                let idx = (ch as u8 - b'1') as usize;
                if let Some(prompt) = WELCOME_TRY_PROMPTS.get(idx) {
                    self.leave_history_browse();
                    self.input = (*prompt).to_string();
                    self.input_cursor_end();
                    self.cursor_on = true;
                    self.clear_notice();
                    return self.submit_prompt();
                }
                RunOutcome::Noop
            }
            // j/k: navigate transcript focus when the prompt is empty (vim-style browse).
            KeyCode::Char('j')
                if self.input.is_empty()
                    && !self.busy
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.move_chat_focus(1);
                RunOutcome::Noop
            }
            KeyCode::Char('k')
                if self.input.is_empty()
                    && !self.busy
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.move_chat_focus(-1);
                RunOutcome::Noop
            }
            KeyCode::Char('G')
                if self.transcript_browse_focused()
                    && !self.busy
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.clear_chat_focus();
                self.scroll_to_bottom();
                RunOutcome::Noop
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    // Never insert C0 controls (e.g. bare Ctrl+K = 0x0B) into the draft.
                    && !ch.is_control() =>
            {
                self.leave_history_browse();
                self.insert_input_char(ch);
                self.clear_notice();
                if self.input.starts_with('/') {
                    self.clamp_slash_selection();
                }
                RunOutcome::Noop
            }
            // ←→ move caret inside the prompt (Home/End still scroll transcript).
            KeyCode::Left => {
                self.move_input_cursor(-1);
                RunOutcome::Noop
            }
            KeyCode::Right => {
                self.move_input_cursor(1);
                RunOutcome::Noop
            }
            // Transcript scroll (mouse wheel / Page keys).
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
            // ↑/↓: slash menu when open, else prompt history.
            KeyCode::Up => {
                if self.slash_menu_visible() {
                    self.move_slash_selection(-1);
                } else {
                    self.history_prev();
                }
                RunOutcome::Noop
            }
            KeyCode::Down => {
                if self.slash_menu_visible() {
                    self.move_slash_selection(1);
                } else {
                    self.history_next();
                }
                RunOutcome::Noop
            }
            _ => RunOutcome::Noop,
        }
    }

    pub(crate) fn is_ctrl_c(key: KeyEvent) -> bool {
        matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && !key.modifiers.contains(KeyModifiers::ALT)
    }

    /// Alt+G jumps to the live transcript without stealing Ctrl+G Settings or Shift+G uppercase typing.
    pub(crate) fn is_goto_bottom_key(key: KeyEvent) -> bool {
        matches!(key.code, KeyCode::Char('G') | KeyCode::Char('g'))
            && key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::CONTROL)
    }

    /// Ctrl+F — fetch remote models. Also accept legacy ASCII ACK (0x06).
    pub(crate) fn is_ctrl_f(key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('f') | KeyCode::Char('F') => {
                key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
            }
            // Some hosts deliver Ctrl+letter as a bare control char.
            KeyCode::Char('\u{06}') => true,
            _ => false,
        }
    }

    /// Help catalog (status strip: `Alt+H help`).
    ///
    /// Accepts:
    /// - **primary** `Alt+H` / `Alt+h` (usually not stolen by IME; works first press)
    /// - silent fallbacks: `Ctrl+K` (+ legacy VT `0x0B`), `F1`, `Ctrl+/`, `Ctrl+_`, US `0x1F`
    pub(crate) fn is_help_key(key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('h') | KeyCode::Char('H')
                if key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                true
            }
            KeyCode::Char('k') | KeyCode::Char('K')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                true
            }
            // ASCII VT — classic Ctrl+K encoding without a CONTROL flag.
            KeyCode::Char('\u{0b}') => true,
            KeyCode::F(1) => true,
            KeyCode::Char('/') | KeyCode::Char('_')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                true
            }
            KeyCode::Char('\u{1f}') => true,
            _ => false,
        }
    }

    /// Progressive Ctrl+C — never exit on a single accidental press.
    ///
    /// | State | 1st Ctrl+C | 2nd (within ~900ms) |
    /// |-------|------------|---------------------|
    /// | float open (Settings, …) | close float + arm quit | quit |
    /// | select open (model / approval) | cancel select + arm quit | quit |
    /// | input non-empty | clear draft + arm quit | quit |
    /// | otherwise | arm quit + toast | quit |
    ///
    /// Idle → `RunOutcome::Quit`; busy → `request_force_quit` (soft cancel stays Esc).
    pub(crate) fn handle_ctrl_c(&mut self) -> RunOutcome {
        let now = Instant::now();

        // 1) Close any center float entirely (Settings / commands / sessions / …).
        if self.float_open() {
            self.settings_inline_op = None;
            self.settings_form_edit = None;
            self.model_draft = None;
            self.close_float();
            self.arm_ctrl_c_quit(now);
            self.set_notice("Ctrl+C again to quit");
            return RunOutcome::Noop;
        }

        // 2) Cancel docked select (model / approval / ask_user).
        if self.select.is_some() {
            let _ = self.apply_select_result(crate::select::SelectResult::Cancelled);
            self.arm_ctrl_c_quit(now);
            self.set_notice("Ctrl+C again to quit");
            return RunOutcome::Noop;
        }

        // 3) Clear non-empty input draft (SIGINT-style line cancel).
        if !self.input.is_empty() {
            self.input.clear();
            self.pending_images.clear();
            self.pending_texts.clear();
            self.leave_history_browse();
            self.cursor_on = true;
            self.arm_ctrl_c_quit(now);
            self.set_notice("input cleared · Ctrl+C again to quit");
            return RunOutcome::Noop;
        }

        // 4) Double-tap confirm quit.
        let double = self
            .last_ctrl_c_at
            .map(|t| now.duration_since(t).as_millis() <= CTRL_C_DOUBLE_MS)
            .unwrap_or(false);
        if double {
            self.last_ctrl_c_at = None;
            return self.confirm_ctrl_c_quit();
        }
        self.arm_ctrl_c_quit(now);
        self.set_notice("Ctrl+C again to quit");
        RunOutcome::Noop
    }

    pub(crate) fn arm_ctrl_c_quit(&mut self, now: Instant) {
        self.last_ctrl_c_at = Some(now);
    }

    pub(crate) fn confirm_ctrl_c_quit(&mut self) -> RunOutcome {
        if self.busy {
            self.request_force_quit();
            self.set_notice("force quit…");
            RunOutcome::Noop
        } else {
            RunOutcome::Quit
        }
    }

    /// Esc behavior (idle):
    ///
    /// | Input | Esc |
    /// |-------|-----|
    /// | **non-empty** | clear draft → ↑ history **immediately** (always reacts) |
    /// | **empty** | 1st: arm + toast; 2nd within ~900ms: open rewind |
    ///
    /// Non-empty used to require double-Esc like Claude, but that felt dead on the
    /// first press (toast only, easy to miss). Clear is safe + common TUI UX.
    /// Empty still needs a double-tap so a single Esc doesn't open rewind by accident.
    pub(crate) fn handle_esc(&mut self) -> RunOutcome {
        // Always clear draft on first Esc when there is text — no double-tap.
        if !self.input.is_empty() {
            let draft = std::mem::take(&mut self.input);
            self.pending_images.clear();
            self.pending_texts.clear();
            self.push_prompt_history(&draft);
            self.leave_history_browse();
            self.last_esc_at = None;
            self.cursor_on = true;
            self.set_notice("draft cleared · ↑ to recall · Esc Esc rewind");
            return RunOutcome::Noop;
        }

        // Empty input: require double-Esc for rewind (Claude Code).
        let now = Instant::now();
        let double = self
            .last_esc_at
            .map(|t| now.duration_since(t).as_millis() <= ESC_DOUBLE_MS)
            .unwrap_or(false);
        self.last_esc_at = Some(now);

        if !double {
            self.set_notice("Esc again to rewind");
            return RunOutcome::Noop;
        }
        self.last_esc_at = None;
        RunOutcome::OpenRewind
    }
}
