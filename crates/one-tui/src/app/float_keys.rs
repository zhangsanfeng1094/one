//! Floating menu keyboard navigation and confirm / dispatch.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::float::FloatKind;
use crate::state::RunOutcome;

impl super::App {
    pub(crate) fn handle_float_key(&mut self, key: KeyEvent) -> RunOutcome {
        let editing = self.settings_inline_op.is_some()
            || self.settings_form_edit.is_some()
            || self.float.as_ref().map(|f| f.edit_mode).unwrap_or(false);
        // Search/edit bar owns ←→ when typing a value or a non-empty filter.
        let text_focus = editing || self.float.as_ref().is_some_and(|f| !f.search.is_empty());

        // Ctrl+F → GET {base}/models for the focused provider.
        // Works on provider detail, local model list, and remote results (re-fetch).
        // Accepts 'f'/'F'+CONTROL and legacy ASCII 0x06 (some terminals).
        if !editing
            && Self::is_ctrl_f(key)
            && self
                .float
                .as_ref()
                .is_some_and(|f| Self::float_allows_fetch_models(f.kind))
        {
            return self.provider_fetch_models_outcome();
        }

        match key.code {
            // ←→ / Home / End move the search/edit caret while text has focus.
            KeyCode::Left if text_focus => {
                if let Some(f) = self.float.as_mut() {
                    f.move_search_cursor(-1);
                }
                RunOutcome::Noop
            }
            KeyCode::Right if text_focus => {
                if let Some(f) = self.float.as_mut() {
                    f.move_search_cursor(1);
                }
                RunOutcome::Noop
            }
            KeyCode::Home if text_focus => {
                if let Some(f) = self.float.as_mut() {
                    f.search_cursor_home();
                }
                RunOutcome::Noop
            }
            KeyCode::End if text_focus => {
                if let Some(f) = self.float.as_mut() {
                    f.search_cursor_end();
                }
                RunOutcome::Noop
            }
            // Esc / ← (nav only): cancel field edit, else one level up.
            // Detail → ask CLI to reopen list with a fresh snapshot (not cache).
            KeyCode::Char('q') | KeyCode::Char('Q')
                if !editing
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && self
                        .float
                        .as_ref()
                        .is_some_and(|f| f.kind == FloatKind::SubagentDetail) =>
            {
                RunOutcome::OpenSubagentList
            }
            KeyCode::Esc => {
                if self
                    .float
                    .as_ref()
                    .is_some_and(|f| f.kind == FloatKind::BackgroundDetail)
                {
                    return RunOutcome::OpenBackgroundList;
                }
                if self
                    .float
                    .as_ref()
                    .is_some_and(|f| f.kind == FloatKind::SubagentDetail)
                {
                    return RunOutcome::OpenSubagentList;
                }
                if !self.settings_go_back() {
                    self.close_float();
                }
                RunOutcome::Noop
            }
            KeyCode::Left if !text_focus => {
                if self
                    .float
                    .as_ref()
                    .is_some_and(|f| f.kind == FloatKind::BackgroundDetail)
                {
                    return RunOutcome::OpenBackgroundList;
                }
                if self
                    .float
                    .as_ref()
                    .is_some_and(|f| f.kind == FloatKind::SubagentDetail)
                {
                    return RunOutcome::OpenSubagentList;
                }
                if !self.settings_go_back() {
                    self.close_float();
                }
                RunOutcome::Noop
            }
            KeyCode::Up if !editing => {
                if let Some(f) = self.float.as_mut() {
                    f.move_selection(-1);
                }
                RunOutcome::Noop
            }
            KeyCode::Down if !editing => {
                if let Some(f) = self.float.as_mut() {
                    f.move_selection(1);
                }
                RunOutcome::Noop
            }
            KeyCode::PageUp if !editing => {
                self.scroll_float_page(true);
                RunOutcome::Noop
            }
            KeyCode::PageDown if !editing => {
                self.scroll_float_page(false);
                RunOutcome::Noop
            }
            KeyCode::Home if !editing && !text_focus => {
                if let Some(f) = self.float.as_mut() {
                    f.selected = 0;
                }
                RunOutcome::Noop
            }
            KeyCode::End if !editing && !text_focus => {
                if let Some(f) = self.float.as_mut() {
                    let n = f.filtered_entries().len();
                    if n > 0 {
                        f.selected = n - 1;
                    }
                }
                RunOutcome::Noop
            }
            KeyCode::Backspace => {
                let empty = self
                    .float
                    .as_ref()
                    .map(|f| f.search.is_empty())
                    .unwrap_or(true);
                if empty && !editing {
                    if self
                        .float
                        .as_ref()
                        .is_some_and(|f| f.kind == FloatKind::BackgroundDetail)
                    {
                        return RunOutcome::OpenBackgroundList;
                    }
                    if self
                        .float
                        .as_ref()
                        .is_some_and(|f| f.kind == FloatKind::SubagentDetail)
                    {
                        return RunOutcome::OpenSubagentList;
                    }
                    if !self.settings_go_back() {
                        self.close_float();
                    }
                } else if let Some(f) = self.float.as_mut() {
                    f.pop_search();
                }
                RunOutcome::Noop
            }
            KeyCode::Delete => {
                if let Some(f) = self.float.as_mut() {
                    f.delete_search_forward();
                }
                RunOutcome::Noop
            }
            // `/ps` or `/tasks`: `x` kills selected item when not filtering.
            KeyCode::Char('x') | KeyCode::Char('X')
                if !editing
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && self.float.as_ref().is_some_and(|f| {
                        matches!(
                            f.kind,
                            FloatKind::Background
                                | FloatKind::BackgroundDetail
                                | FloatKind::Subagent
                                | FloatKind::SubagentDetail
                        ) && f.search.is_empty()
                    }) =>
            {
                self.background_or_subagent_kill_selection()
            }
            // Provider → Models: Space toggles whether the model appears in Ctrl+L
            // (does not open detail, does not type-to-filter).
            KeyCode::Char(' ')
                if !editing
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && self
                        .float
                        .as_ref()
                        .is_some_and(|f| f.kind == FloatKind::SettingsModels) =>
            {
                let id = self.float.as_ref().and_then(|f| {
                    f.filtered_entries()
                        .get(f.selected)
                        .map(|e| e.item.id.clone())
                });
                match id.as_deref() {
                    Some(id) if id.starts_with("m:") => self.toggle_model_ctrl_l_visibility(id),
                    _ => RunOutcome::Noop,
                }
            }
            // Detail log viewers — no type-to-filter.
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && !self.float.as_ref().is_some_and(|f| {
                        matches!(
                            f.kind,
                            FloatKind::BackgroundDetail | FloatKind::SubagentDetail
                        )
                    }) =>
            {
                if let Some(f) = self.float.as_mut() {
                    f.push_search(ch);
                }
                RunOutcome::Noop
            }
            KeyCode::Enter | KeyCode::Tab => {
                if self.settings_inline_op.is_some() {
                    self.commit_settings_inline_edit()
                } else if self.settings_form_edit.is_some() {
                    self.commit_settings_form_edit()
                } else {
                    self.confirm_float_selection()
                }
            }
            _ => RunOutcome::Noop,
        }
    }

    /// `/ps` or `/tasks` list/detail: kill the focused item (`x`).
    pub(crate) fn background_or_subagent_kill_selection(&mut self) -> RunOutcome {
        let kind = self.float.as_ref().map(|f| f.kind);
        let id = match kind {
            Some(FloatKind::BackgroundDetail) => self.bg_ps_detail_id.clone(),
            Some(FloatKind::Background) => self
                .float
                .as_ref()
                .and_then(|f| f.selected_entry())
                .map(|e| e.item.id)
                .filter(|id| id != "_empty" && !id.is_empty()),
            Some(FloatKind::SubagentDetail) => self.task_detail_id.clone(),
            Some(FloatKind::Subagent) => self
                .float
                .as_ref()
                .and_then(|f| f.selected_entry())
                .map(|e| e.item.id)
                .filter(|id| id != "_empty" && !id.is_empty()),
            _ => None,
        };
        let Some(id) = id else {
            self.set_notice("nothing to kill");
            return RunOutcome::Noop;
        };
        match kind {
            Some(FloatKind::Subagent | FloatKind::SubagentDetail) => {
                RunOutcome::KillSubagent { id }
            }
            _ => RunOutcome::KillBackground { id },
        }
    }

    /// Legacy name kept for tests — bash only.
    #[allow(dead_code)]
    pub(crate) fn background_kill_selection(&mut self) -> RunOutcome {
        let kind = self.float.as_ref().map(|f| f.kind);
        let id = match kind {
            Some(FloatKind::BackgroundDetail) => self.bg_ps_detail_id.clone(),
            Some(FloatKind::Background) => self
                .float
                .as_ref()
                .and_then(|f| f.selected_entry())
                .map(|e| e.item.id)
                .filter(|id| id != "_empty" && !id.is_empty()),
            _ => None,
        };
        match id {
            Some(id) => RunOutcome::KillBackground { id },
            None => {
                self.set_notice("nothing to kill");
                RunOutcome::Noop
            }
        }
    }

    /// Confirm current float selection → nested float or slash Prompt.
    pub(crate) fn confirm_float_selection(&mut self) -> RunOutcome {
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
                // Prefer docked select; if center model float still used, switch via outcome.
                self.close_float();
                if let Some((p, m)) = entry.item.id.split_once(':') {
                    return RunOutcome::SwitchModel {
                        provider: p.to_string(),
                        model: Some(m.to_string()),
                    };
                }
                RunOutcome::SwitchModel {
                    provider: entry.item.id.clone(),
                    model: None,
                }
            }
            FloatKind::Thinking => {
                self.close_float();
                self.input.clear();
                RunOutcome::Prompt(format!("/thinking {}", entry.item.id))
            }
            FloatKind::Login => {
                if entry.item.id == "_empty" || entry.item.id.is_empty() {
                    return RunOutcome::Noop;
                }
                self.close_float();
                self.input.clear();
                // Re-enter slash path with an explicit provider → suspend + login flow.
                RunOutcome::Prompt(format!("/login {}", entry.item.id))
            }
            FloatKind::Logout => {
                if entry.item.id == "_empty" || entry.item.id.is_empty() {
                    return RunOutcome::Noop;
                }
                self.close_float();
                self.input.clear();
                RunOutcome::Prompt(format!("/logout {}", entry.item.id))
            }
            FloatKind::Sessions => {
                self.close_float();
                self.input.clear();
                RunOutcome::Prompt(format!("/resume {}", entry.item.id))
            }
            FloatKind::Tree => {
                self.close_float();
                self.input.clear();
                RunOutcome::Prompt(format!("/tree {}", entry.item.id))
            }
            FloatKind::Rewind => {
                self.close_float();
                self.input.clear();
                RunOutcome::Prompt(format!("/rewind {}", entry.item.id))
            }
            FloatKind::Info => {
                self.close_float();
                RunOutcome::Noop
            }
            FloatKind::NewSessionConfirm => {
                self.close_float();
                if entry.item.id != "new" {
                    return RunOutcome::Noop;
                }
                // A fresh session must not carry an unsent draft or its
                // attachments into the new conversation.
                self.input.clear();
                self.input_cursor = 0;
                self.pending_images.clear();
                self.image_jobs.clear();
                self.pending_texts.clear();
                self.committed_images.clear();
                self.leave_history_browse();
                self.cursor_on = true;
                RunOutcome::Prompt("/new".into())
            }
            FloatKind::Background => {
                if entry.item.id == "_empty" || entry.item.id.is_empty() {
                    return RunOutcome::Noop;
                }
                // CLI re-fetches a fresh bash stdout/stderr snapshot.
                RunOutcome::OpenBackgroundDetail {
                    id: entry.item.id.clone(),
                }
            }
            FloatKind::BackgroundDetail => {
                if let Some(id) = self.bg_ps_detail_id.clone() {
                    return RunOutcome::OpenBackgroundDetail { id };
                }
                RunOutcome::OpenBackgroundList
            }
            FloatKind::Subagent => {
                if entry.item.id == "_empty" || entry.item.id.is_empty() {
                    return RunOutcome::Noop;
                }
                RunOutcome::OpenSubagentDetail {
                    id: entry.item.id.clone(),
                }
            }
            FloatKind::SubagentDetail => {
                if let Some(id) = self.task_detail_id.clone() {
                    return RunOutcome::OpenSubagentDetail { id };
                }
                RunOutcome::OpenSubagentList
            }
            FloatKind::Settings => self.confirm_settings_root(&entry.item.id),
            FloatKind::SettingsToolOutput => self.confirm_settings_tool_output(&entry.item.id),
            FloatKind::SettingsCompaction => self.confirm_settings_compaction(&entry.item.id),
            FloatKind::SettingsProviders => self.confirm_settings_providers(&entry.item.id),
            FloatKind::SettingsProviderDetail => {
                self.confirm_settings_provider_detail(&entry.item.id)
            }
            FloatKind::SettingsProviderCompat => {
                self.confirm_settings_provider_compat(&entry.item.id)
            }
            FloatKind::SettingsProviderApi => self.confirm_settings_provider_api(&entry.item.id),
            FloatKind::SettingsThinkingFormat => {
                self.confirm_settings_thinking_format(&entry.item.id)
            }
            FloatKind::SettingsMaxTokensField => {
                self.confirm_settings_max_tokens_field(&entry.item.id)
            }
            FloatKind::SettingsRemoteModels => self.confirm_settings_remote_models(&entry.item.id),
            FloatKind::SettingsModels => self.confirm_settings_models(&entry.item.id),
            FloatKind::SettingsModelDetail => self.confirm_settings_model_detail(&entry.item.id),
            FloatKind::SettingsModelAdd => self.confirm_settings_model_add(&entry.item.id),
            FloatKind::SettingsDeleteConfirm => self.confirm_settings_delete(),
            FloatKind::Skills => self.confirm_skills_toggle(&entry.item.id),
            FloatKind::Agents => self.confirm_agents_item(&entry.item.id),
            FloatKind::Features => self.confirm_features_toggle(&entry.item.id),
            FloatKind::Mcp => self.confirm_mcp_action(&entry.item.id),
            FloatKind::McpImport => self.confirm_mcp_import(&entry.item.id),
            FloatKind::Help | FloatKind::Commands | FloatKind::Custom => {
                self.dispatch_command_item(&entry.item.id, &entry.item.hint)
            }
        }
    }

    /// Shared handler for command-palette / help rows.
    pub(crate) fn dispatch_command_item(&mut self, id: &str, hint: &str) -> RunOutcome {
        match id {
            "model" | "switch_model" => {
                self.open_model_select();
                RunOutcome::Noop
            }
            "settings" => {
                self.open_settings_float();
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
            "login" => {
                // CLI fills rows then opens float; emit slash so status can be attached.
                self.close_float();
                self.input.clear();
                RunOutcome::Prompt("/login".into())
            }
            "logout" => {
                self.close_float();
                self.input.clear();
                RunOutcome::Prompt("/logout".into())
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
            "skills" => {
                self.open_skills_float();
                RunOutcome::Noop
            }
            "agents" => {
                // Need runtime refresh of rows — slash path ensures list is up to date.
                self.close_float();
                self.input.clear();
                RunOutcome::Prompt("/agents".into())
            }
            "mcp" => {
                self.close_float();
                RunOutcome::OpenMcpPanel
            }
            "ps" => {
                self.close_float();
                RunOutcome::OpenBackgroundList
            }
            "tasks" | "jobs" | "subagents" => {
                self.close_float();
                RunOutcome::OpenSubagentList
            }
            // These need runtime data → emit slash so CLI opens the right float.
            "resume" | "session" | "tree" | "rewind" | "new" | "name" | "export" | "compact"
            | "reload" | "skill" | "plan" | "act" | "build" => {
                self.close_float();
                self.input.clear();
                let cmd = if hint.starts_with('/') {
                    hint.to_string()
                } else {
                    format!("/{id}")
                };
                if cmd.ends_with(' ') {
                    self.input = cmd;
                    self.input_cursor_end();
                    RunOutcome::Noop
                } else {
                    RunOutcome::Prompt(cmd)
                }
            }
            _ => {
                self.close_float();
                if hint.starts_with('/') {
                    if hint == "/model" || hint.starts_with("/model ") {
                        self.open_model_select();
                        RunOutcome::Noop
                    } else if hint == "/settings" || hint.starts_with("/settings ") {
                        self.open_settings_float();
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
}
