//! Busy-mode keyboard handling (queue follow-ups, abort, force-quit).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::RunOutcome;

use super::helpers::is_ui_slash;

impl super::App {
    pub fn handle_busy_key(&mut self, key: KeyEvent) {
        if matches!(key.kind, crossterm::event::KeyEventKind::Release) {
            return;
        }

        // Same progressive Ctrl+C as idle: dismiss overlay / clear steer draft /
        // double-tap force-quit. Soft cancel is Esc only.
        if Self::is_ctrl_c(key) {
            let _ = self.handle_ctrl_c();
            return;
        }
        self.last_ctrl_c_at = None;

        // Help works while busy (same chord encodings as idle).
        if Self::is_help_key(key) {
            self.select = None;
            self.open_help_float();
            return;
        }

        // Ctrl+G → Settings (same as idle; /settings already works mid-stream).
        if matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.select = None;
            self.open_settings_float();
            return;
        }

        if Self::is_goto_bottom_key(key) {
            self.scroll_to_bottom();
            return;
        }

        // Docked select (permission / ask_user / model) takes focus over steer / abort.
        if self.select.is_some() {
            if let Some(prompt) = self.select.as_mut() {
                if let Some(result) = prompt.handle_key(key) {
                    // Model/ConfigOp outcomes are ignored while busy (approval only).
                    let _ = self.apply_select_result(result);
                }
            }
            return;
        }

        // Float panels (including `/ps`) must queue CLI-facing outcomes while busy —
        // `run_busy` discards return values, so we stash them for the tick drain.
        if self.float_open() {
            let outcome = self.handle_float_key(key);
            self.route_busy_outcome(outcome);
            return;
        }

        match key.code {
            KeyCode::Esc => {
                if self.slash_menu_visible() {
                    self.input.clear();
                    self.input_cursor = 0;
                    self.slash_selected = 0;
                    self.clear_notice();
                } else {
                    self.request_abort();
                    self.set_notice("interrupting…");
                }
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = self.submit_steer();
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                let _ = self.submit_followup();
            }
            // Enter while busy: UI slash commands (esp. `/ps`) and slash-menu confirm.
            // Plain text still uses Alt+Enter for follow-up (unchanged).
            KeyCode::Enter => {
                if self.slash_menu_visible() {
                    let outcome = self.confirm_slash_menu();
                    self.route_busy_outcome(outcome);
                    return;
                }
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return;
                }
                if is_ui_slash(&text) {
                    self.input.clear();
                    self.input_cursor = 0;
                    self.queue_busy_ui_from_slash(&text);
                    return;
                }
            }
            KeyCode::Tab => {
                if self.slash_menu_visible() {
                    self.apply_slash_completion();
                }
            }
            KeyCode::Backspace => {
                self.pop_input();
                if self.input.starts_with('/') {
                    self.clamp_slash_selection();
                }
            }
            KeyCode::Delete => {
                self.delete_input_forward();
                if self.input.starts_with('/') {
                    self.clamp_slash_selection();
                }
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && !ch.is_control() =>
            {
                self.insert_input_char(ch);
                if self.input.starts_with('/') {
                    self.clamp_slash_selection();
                }
            }
            KeyCode::Left => self.move_input_cursor(-1),
            KeyCode::Right => self.move_input_cursor(1),
            KeyCode::PageUp => self.scroll_up(self.page_lines()),
            KeyCode::PageDown => self.scroll_down(self.page_lines()),
            KeyCode::Home => self.scroll_to_top(),
            KeyCode::End => self.scroll_to_bottom(),
            // Slash menu owns ↑/↓ when open; otherwise scroll transcript.
            KeyCode::Up => {
                if self.slash_menu_visible() {
                    self.move_slash_selection(-1);
                } else {
                    self.scroll_up(3);
                }
            }
            KeyCode::Down => {
                if self.slash_menu_visible() {
                    self.move_slash_selection(1);
                } else {
                    self.scroll_down(3);
                }
            }
            _ => {}
        }
    }

    /// Map a key outcome into busy-time UI work (no nested agent turns).
    pub(crate) fn route_busy_outcome(&mut self, outcome: RunOutcome) {
        match outcome {
            RunOutcome::Noop => {}
            RunOutcome::Prompt(text) if is_ui_slash(&text) => {
                self.queue_busy_ui_from_slash(&text);
            }
            // Mid-turn user text is follow-up, not a new Prompt turn.
            RunOutcome::Prompt(text) => {
                if !text.is_empty() {
                    self.followup_pending = Some(text);
                }
            }
            RunOutcome::FollowUp(text) => {
                if !text.is_empty() {
                    self.followup_pending = Some(text);
                }
            }
            RunOutcome::Steer(text) => {
                if !text.is_empty() {
                    self.steer_pending = Some(text);
                }
            }
            other if other.is_actionable() => self.queue_busy_ui(other),
            _ => {}
        }
    }

    /// Expand a UI slash into a local float or a CLI-facing busy action.
    pub(crate) fn queue_busy_ui_from_slash(&mut self, text: &str) {
        let parts: Vec<&str> = text.split_whitespace().collect();
        match parts.first().copied() {
            Some("/ps") => {
                if let Some(id) = parts.get(1).copied() {
                    self.queue_busy_ui(RunOutcome::OpenBackgroundDetail { id: id.to_string() });
                } else {
                    self.queue_busy_ui(RunOutcome::OpenBackgroundList);
                }
                self.set_notice("background bash…");
            }
            Some("/tasks") | Some("/jobs") | Some("/subagents") => {
                if let Some(id) = parts.get(1).copied() {
                    self.queue_busy_ui(RunOutcome::OpenSubagentDetail { id: id.to_string() });
                } else {
                    self.queue_busy_ui(RunOutcome::OpenSubagentList);
                }
                self.set_notice("subagents…");
            }
            Some("/settings") if parts.len() == 1 => {
                self.open_settings_float();
            }
            Some("/help") => {
                self.open_help_float();
            }
            Some("/model") if parts.len() == 1 => {
                self.open_model_select();
            }
            Some("/thinking") if parts.len() == 1 => {
                self.open_thinking_float();
            }
            Some("/skills") if parts.len() == 1 => {
                self.open_skills_float();
            }
            Some("/mcp") if parts.len() == 1 => {
                self.queue_busy_ui(RunOutcome::OpenMcpPanel);
            }
            Some("/login") if parts.len() == 1 => {
                // CLI attaches provider rows; queue slash so interactive fills them.
                self.queue_busy_ui(RunOutcome::Prompt(text.to_string()));
            }
            Some("/logout") if parts.len() == 1 => {
                self.queue_busy_ui(RunOutcome::Prompt(text.to_string()));
            }
            // Other UI slashes: hand to CLI (may open floats / notice). Avoids starting
            // a nested agent turn while the current one is still streaming.
            _ => {
                self.queue_busy_ui(RunOutcome::Prompt(text.to_string()));
            }
        }
    }
}
