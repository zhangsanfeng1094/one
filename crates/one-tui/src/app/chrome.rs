//! Ephemeral chrome: toasts, alerts, usage meters, float openers, slash dock.

use std::path::PathBuf;
use std::time::Instant;

use crate::float::FloatMenu;
use crate::message::{AlertLevel, Message, MessageRole};
use crate::slash::{self, ModelChoice, PopupKind, PopupRow};
use crate::state::{PendingImage, RunOutcome, Toast};

use super::classify_toast_level;

impl super::App {
    pub fn set_thinking_level(&mut self, level: impl Into<String>) {
        self.thinking_level = level.into();
    }

    /// Toggle default expand for finished thinking (Ctrl+T). Headers always remain.
    ///
    /// Also syncs every non-streaming thinking bubble to the new policy so the
    /// transcript matches what Ctrl+T claims (expand all / collapse all).
    pub fn toggle_show_thinking(&mut self) {
        self.show_thinking = !self.show_thinking;
        for msg in &mut self.messages {
            if msg.role == MessageRole::Thinking && !msg.streaming {
                msg.thinking_expanded = self.show_thinking;
            }
        }
        self.set_notice(if self.show_thinking {
            "thinking expanded"
        } else {
            "thinking collapsed"
        });
    }

    pub fn set_usage_tokens(&mut self, tokens: usize) {
        self.usage_tokens = tokens;
    }

    pub fn set_usage_tokens_estimated(&mut self, estimated: bool) {
        self.usage_tokens_estimated = estimated;
    }

    pub fn set_usage_io(&mut self, input: u64, output: u64) {
        self.usage_input = input;
        self.usage_output = output;
    }

    pub fn set_usage_cache(&mut self, read: u64, write: u64) {
        self.usage_cache_read = read;
        self.usage_cache_write = write;
    }

    pub fn set_usage_cost_usd(&mut self, cost: f64) {
        self.usage_cost_usd = cost;
    }

    pub fn set_context_window(&mut self, window: usize) {
        self.context_window = window;
    }

    pub fn set_model_catalog(&mut self, catalog: Vec<ModelChoice>) {
        self.model_catalog = catalog;
    }

    /// Specs (`provider:id`) shown in Ctrl+L. `None` / empty = all catalog models.
    pub fn set_enabled_models(&mut self, specs: Option<Vec<String>>) {
        self.enabled_models = match specs {
            Some(mut v) => {
                v.retain(|s| !s.trim().is_empty());
                v.sort();
                v.dedup();
                if v.is_empty() {
                    None
                } else {
                    Some(v)
                }
            }
            None => None,
        };
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
    }

    pub fn push_error_alert(&mut self, text: impl Into<String>) {
        self.push_alert(AlertLevel::Error, text);
    }

    pub fn float_open(&self) -> bool {
        self.float.is_some()
    }

    /// Legacy name — opens docked model select (not center float).
    pub fn open_model_picker(&mut self) {
        self.open_model_select();
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

    /// `/login` provider picker — rows: `(id, label, detail, logged_in)`.
    pub fn open_login_float(&mut self, rows: &[(String, String, String, bool)]) {
        self.close_float();
        self.float = Some(FloatMenu::login_picker(rows));
        self.clear_notice();
    }

    /// `/logout` picker — rows: `(id, label, detail)`.
    pub fn open_logout_float(&mut self, rows: &[(String, String, String)]) {
        self.close_float();
        self.float = Some(FloatMenu::logout_picker(rows));
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

    /// Rewind menu: `(entry_id, preview)` newest first — Claude Code Esc Esc.
    pub fn open_rewind_float(&mut self, prompts: &[(String, String)]) {
        self.float = Some(FloatMenu::rewind_picker(prompts));
        self.clear_notice();
    }

    pub fn open_info_float(&mut self, title: impl Into<String>, rows: &[(String, String)]) {
        self.float = Some(FloatMenu::info_panel(title, rows));
        self.clear_notice();
    }

    /// Ctrl+N asks before switching to a fresh session so the current draft
    /// and conversation are never replaced by accident.
    pub fn open_new_session_confirm(&mut self) {
        self.float = Some(FloatMenu::new_session_confirm());
        self.clear_notice();
    }

    /// Put text into the input for re-edit (after rewind). Does not submit.
    pub fn set_input_for_edit(&mut self, text: impl Into<String>) {
        self.set_input_for_edit_with_images(text, Vec::new());
    }

    /// Rewind / restore a prompt with real image **paths** (not display labels).
    ///
    /// `images` is `(mime_type, path)` in chip order; input should already
    /// contain matching `[图片.img]` tokens (from `UserContent::for_reedit`).
    pub fn set_input_for_edit_with_images(
        &mut self,
        text: impl Into<String>,
        images: Vec<(String, String)>,
    ) {
        self.input = text.into();
        self.pending_images.clear();
        self.image_jobs.clear();
        self.pending_texts.clear();
        self.committed_images.clear();
        self.next_image_id = 1;
        for (i, (mime_type, path)) in images.into_iter().enumerate() {
            let id = (i as u32).saturating_add(1);
            let path = PathBuf::from(path);
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("image")
                .to_string();
            self.pending_images.push(PendingImage {
                id,
                mime_type,
                path,
                name,
                loading: false,
            });
            self.next_image_id = id.saturating_add(1).max(2);
        }
        // If input has no chips but we have images, append chips.
        if !self.pending_images.is_empty()
            && one_core::image::image_token_ids_in(&self.input).is_empty()
        {
            for img in &self.pending_images {
                let token = one_core::image::image_token(img.id);
                if !self.input.is_empty() && !self.input.ends_with(|c: char| c.is_whitespace()) {
                    self.input.push(' ');
                }
                self.input.push_str(&token);
                self.input.push(' ');
            }
        }
        self.input_cursor_end();
        self.leave_history_browse();
        self.cursor_on = true;
    }

    pub fn close_float(&mut self) {
        self.float = None;
        self.settings_delete_target = None;
    }

    /// Popup rows for current input (commands or models grouped by provider).
    ///
    /// Model rows respect the Ctrl+L filter ([`Self::enabled_models`]).
    pub fn popup_rows(&self) -> Vec<PopupRow> {
        let filtered: Vec<ModelChoice> = self
            .model_catalog
            .iter()
            .filter(|m| self.model_visible_in_switcher(m))
            .cloned()
            .collect();
        slash::popup_rows(&self.input, &filtered)
    }

    pub fn slash_menu_visible(&self) -> bool {
        // Only while composing a slash command (not when a HITL select is open).
        self.select.is_none() && !self.popup_rows().is_empty()
    }

    /// Height of the `/` command menu docked above the prompt (`0` when closed).
    pub fn slash_dock_height(&self) -> u16 {
        if !self.slash_menu_visible() {
            return 0;
        }
        let n = self.popup_rows().len() as u16;
        n.clamp(1, 10)
    }

    pub fn popup_kind(&self) -> Option<PopupKind> {
        slash::popup_kind(&self.input)
    }

    pub fn clamp_slash_selection(&mut self) {
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

    pub fn move_slash_selection(&mut self, delta: isize) {
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

    /// Fill input from the highlighted slash row.
    pub fn apply_slash_completion(&mut self) {
        let rows = self.popup_rows();
        if rows.is_empty() {
            return;
        }
        self.clamp_slash_selection();
        if let Some(row) = rows.get(self.slash_selected) {
            if let Some(text) = slash::completion_for_row(row) {
                self.input = text;
                self.input_cursor_end();
                self.slash_selected = 0;
                self.cursor_on = true;
                self.clamp_slash_selection();
            }
        }
    }

    /// Enter on slash menu: complete selection, then run or wait for args.
    pub(crate) fn confirm_slash_menu(&mut self) -> RunOutcome {
        if !self.slash_menu_visible() {
            return RunOutcome::Noop;
        }
        self.apply_slash_completion();
        let t = self.input.trim().to_string();
        // Commands that open secondary UI instead of submitting as a prompt.
        match t.as_str() {
            "/model" => {
                self.input.clear();
                self.open_model_select();
                return RunOutcome::Noop;
            }
            "/settings" => {
                self.input.clear();
                self.open_settings_float();
                return RunOutcome::Noop;
            }
            "/skills" => {
                self.input.clear();
                self.open_skills_float();
                return RunOutcome::Noop;
            }
            "/agents" => {
                // Handled via slash in interactive (refresh rows first).
                self.input.clear();
                return RunOutcome::Prompt("/agents".into());
            }
            "/mcp" => {
                self.input.clear();
                return RunOutcome::OpenMcpPanel;
            }
            "/thinking" => {
                self.input.clear();
                self.open_thinking_float();
                return RunOutcome::Noop;
            }
            "/help" => {
                self.input.clear();
                self.open_help_float();
                return RunOutcome::Noop;
            }
            _ => {}
        }
        // Trailing space → still typing args (e.g. `/name `).
        if self.input.ends_with(' ') {
            return RunOutcome::Noop;
        }
        // Complete command → submit as slash prompt for CLI.
        self.submit_prompt()
    }

    pub fn set_mode_label(&mut self, label: impl Into<String>) {
        self.mode_label = label.into();
    }

    pub fn set_agent_label(&mut self, label: impl Into<String>) {
        self.agent_label = label.into();
    }

    /// Send desktop notification using the configured/detected protocol (OSC 9 / OSC 99 / OSC 777 / BEL).
    pub fn notify(&self, title: &str, body: &str) {
        let _ = crate::notification::send_notification(self.notification_protocol, title, body);
    }

    /// Ring the terminal bell (ASCII BEL chime).
    pub fn ring_bell(&self) {
        let _ = crate::notification::ring_bell();
    }
}
