//! Settings, MCP, skills, features, and agents float UI for [`App`].
//!
//! Open/navigate/confirm handlers for the centered Settings hierarchy live here
//! so `app.rs` stays focused on input, streaming, and chat chrome.

use crate::app::App;
use crate::float::{FloatKind, FloatMenu};
use crate::state::{ConfigOp, ModelDraft, RunOutcome};

impl App {
    /// Open centered Settings (Ctrl+G / bare `/settings`).
    pub fn open_settings_float(&mut self) {
        self.close_float();
        self.clear_select_prompt();
        self.float = Some(FloatMenu::settings_root_with_mcp(
            &self.thinking_level,
            &self.current_provider,
            &self.current_model,
            &self.mcp_summary,
            &self.tool_output_summary(),
            &self.compaction_summary(),
        ));
        self.clear_notice();
    }

    /// Sync tool_output limits into Settings UI state.
    pub fn set_tool_output_limits(&mut self, max_lines: usize, max_bytes: usize) {
        self.tool_output_max_lines = max_lines.max(1);
        self.tool_output_max_bytes = max_bytes.max(1);
    }

    /// Sync compaction strategy into Settings UI state.
    pub fn set_compaction_settings(
        &mut self,
        auto: bool,
        ratio: f64,
        threshold: Option<usize>,
        keep_recent: usize,
        prune: bool,
        prune_protect: usize,
        prune_max_chars: usize,
    ) {
        self.compaction_auto = auto;
        self.compaction_ratio = if ratio.is_finite() && ratio > 0.0 && ratio <= 1.0 {
            ratio
        } else {
            0.70
        };
        self.compaction_threshold = threshold.filter(|n| *n > 0);
        self.compaction_keep_recent = keep_recent.max(1);
        self.compaction_prune = prune;
        self.compaction_prune_protect = prune_protect;
        self.compaction_prune_max_chars = prune_max_chars;
    }

    fn tool_output_summary(&self) -> String {
        let b = self.tool_output_max_bytes;
        let size = if b >= 1024 {
            format!("{:.1}KB", b as f64 / 1024.0)
        } else {
            format!("{b}B")
        };
        format!("{} lines · {size}", self.tool_output_max_lines)
    }

    fn compaction_summary(&self) -> String {
        let auto = if self.compaction_auto {
            "auto"
        } else {
            "manual"
        };
        let thresh = if let Some(n) = self.compaction_threshold {
            if n >= 1000 {
                format!("{}k", n / 1000)
            } else {
                n.to_string()
            }
        } else {
            format!("{}%", (self.compaction_ratio * 100.0).round() as u32)
        };
        let prune = if self.compaction_prune {
            "old-tools prune"
        } else {
            "no prune"
        };
        format!(
            "{auto} {thresh} · keep {} · {prune}",
            self.compaction_keep_recent
        )
    }

    /// Nested Settings → Tool output panel.
    pub fn open_settings_tool_output(&mut self) {
        self.float = Some(FloatMenu::settings_tool_output(
            self.tool_output_max_lines,
            self.tool_output_max_bytes,
        ));
        self.clear_notice();
    }

    /// Nested Settings → Compaction strategy panel.
    pub fn open_settings_compaction(&mut self) {
        let ratio_pct = (self.compaction_ratio * 100.0).round() as u32;
        self.float = Some(FloatMenu::settings_compaction(
            self.compaction_auto,
            ratio_pct,
            self.compaction_threshold,
            self.compaction_keep_recent,
            self.compaction_prune,
            self.compaction_prune_protect,
            self.compaction_prune_max_chars,
        ));
        self.clear_notice();
    }

    /// Populate skills manager rows (path, label, detail, enabled).
    pub fn set_skills_rows(&mut self, rows: Vec<(String, String, String, bool)>) {
        self.skills_rows = rows;
    }

    pub fn set_agents_rows(
        &mut self,
        rows: Vec<(String, String, String, String, String)>,
        project_dir: String,
        user_dir: String,
    ) {
        self.agents_rows = rows;
        self.agents_project_dir = project_dir;
        self.agents_user_dir = user_dir;
    }

    /// Populate features manager rows (id, label, detail, enabled, affects_context).
    pub fn set_features_rows(&mut self, rows: Vec<(String, String, String, bool, bool)>) {
        self.features_rows = rows;
    }

    /// Populate MCP manager rows + settings summary.
    pub fn set_mcp_rows(
        &mut self,
        rows: Vec<(String, String, String, bool)>,
        summary: impl Into<String>,
    ) {
        self.mcp_rows = rows;
        self.mcp_summary = summary.into();
    }

    /// Status-bar chip: `text` empty hides it. `kind`: 1 loading · 2 ok · 3 partial · 4 error.
    pub fn set_mcp_chip(&mut self, text: impl Into<String>, kind: u8) {
        self.mcp_chip_text = text.into();
        self.mcp_chip_kind = if self.mcp_chip_text.is_empty() {
            0
        } else {
            kind
        };
    }

    pub fn clear_mcp_chip(&mut self) {
        self.mcp_chip_text.clear();
        self.mcp_chip_kind = 0;
    }

    /// Background tasks chip (`bg:1 · cargo…`). Empty text hides it.
    /// `kind`: 1 running · 2 idle/recent done · 3 mixed fail · 4 error-only
    pub fn set_bg_chip(&mut self, text: impl Into<String>, kind: u8) {
        self.bg_chip_text = text.into();
        self.bg_chip_kind = if self.bg_chip_text.is_empty() {
            0
        } else {
            kind
        };
    }

    pub fn clear_bg_chip(&mut self) {
        self.bg_chip_text.clear();
        self.bg_chip_kind = 0;
    }

    /// Open `/ps` list panel. `rows`: `(id, label, detail, hint)`.
    pub fn open_background_float(&mut self, rows: &[(String, String, String, String)]) {
        self.bg_ps_list = rows.to_vec();
        self.bg_ps_detail_id = None;
        self.close_float();
        self.clear_select_prompt();
        self.float = Some(FloatMenu::background_picker(rows));
        self.clear_notice();
    }

    /// Open one background task log panel (fresh rows from CLI).
    pub fn open_background_detail_float(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        section: impl Into<String>,
        rows: &[(String, String)],
    ) {
        self.bg_ps_detail_id = Some(id.into());
        self.clear_select_prompt();
        self.float = Some(FloatMenu::background_detail(title, section, rows));
        // Jump selection to the last log line so the tail is visible.
        if let Some(f) = self.float.as_mut() {
            let n = f.filtered_entries().len();
            if n > 0 {
                f.selected = n - 1;
            }
        }
        self.clear_notice();
    }

    /// Re-open last `/ps` list (Esc from detail).
    pub fn reopen_background_list_float(&mut self) {
        self.bg_ps_detail_id = None;
        let rows = self.bg_ps_list.clone();
        self.float = Some(FloatMenu::background_picker(&rows));
        self.clear_notice();
    }

    /// Open skills enable/disable panel (`/skills`).
    pub fn open_skills_float(&mut self) {
        self.close_float();
        self.clear_select_prompt();
        self.float = Some(FloatMenu::skills_manager(&self.skills_rows));
        self.clear_notice();
    }

    /// Open agents catalog panel (`/agents`) — paths + tools for presets.
    pub fn open_agents_float(&mut self) {
        self.close_float();
        self.clear_select_prompt();
        self.float = Some(FloatMenu::agents_manager(
            &self.agents_rows,
            &self.agents_project_dir,
            &self.agents_user_dir,
        ));
        self.clear_notice();
    }

    /// Show one agent detail (path, tools, isolation…) as an info float.
    pub fn open_agent_detail_float(&mut self, id: &str) {
        let Some(row) = self.agents_rows.iter().find(|r| r.0 == id) else {
            self.set_notice(format!("unknown agent `{id}`"));
            return;
        };
        let (name, label, detail, path, source) = row;
        let rows = vec![
            ("name".into(), label.clone()),
            ("source".into(), source.clone()),
            ("path".into(), path.clone()),
            ("summary".into(), detail.clone()),
            (
                "dirs".into(),
                format!(
                    "project={} · user={}",
                    self.agents_project_dir, self.agents_user_dir
                ),
            ),
            (
                "hint".into(),
                "edit JSON/MD on disk · one agent dump/inspect · task agent=<name>".into(),
            ),
        ];
        // Keep name for title
        let _ = name;
        self.open_info_float(&format!("Agent · {label}"), &rows);
    }

    /// Re-open skills panel after a toggle (keeps rows already updated by CLI).
    pub fn reopen_skills_float(&mut self) {
        let prev_selected = self.float.as_ref().map(|f| f.selected).unwrap_or(0);
        self.float = Some(FloatMenu::skills_manager(&self.skills_rows));
        if let Some(f) = self.float.as_mut() {
            let max = f.filtered_entries().len().saturating_sub(1);
            f.selected = prev_selected.min(max);
        }
        self.clear_notice();
    }

    /// Open features enable/disable panel (Settings → Features).
    pub fn open_features_float(&mut self) {
        self.close_float();
        self.clear_select_prompt();
        self.float = Some(FloatMenu::features_manager(&self.features_rows));
        self.clear_notice();
    }

    /// Re-open features panel after a toggle.
    pub fn reopen_features_float(&mut self) {
        let prev_selected = self.float.as_ref().map(|f| f.selected).unwrap_or(0);
        self.float = Some(FloatMenu::features_manager(&self.features_rows));
        if let Some(f) = self.float.as_mut() {
            let max = f.filtered_entries().len().saturating_sub(1);
            f.selected = prev_selected.min(max);
        }
        self.clear_notice();
    }

    /// Open MCP status / enable-disable panel (`/mcp` or Settings → MCP).
    pub fn open_mcp_float(&mut self) {
        self.close_float();
        self.clear_select_prompt();
        self.float = Some(FloatMenu::mcp_manager(&self.mcp_rows));
        self.clear_notice();
    }

    /// Re-open MCP panel after a toggle.
    pub fn reopen_mcp_float(&mut self) {
        let prev_selected = self.float.as_ref().map(|f| f.selected).unwrap_or(0);
        self.float = Some(FloatMenu::mcp_manager(&self.mcp_rows));
        if let Some(f) = self.float.as_mut() {
            let max = f.filtered_entries().len().saturating_sub(1);
            f.selected = prev_selected.min(max);
        }
        self.clear_notice();
    }

    /// Store import candidates and open the import float.
    pub fn set_mcp_import_rows(&mut self, rows: Vec<(String, String, String, bool)>) {
        self.mcp_import_rows = rows;
    }

    pub fn open_mcp_import_float(&mut self) {
        self.close_float();
        self.clear_select_prompt();
        self.float = Some(FloatMenu::mcp_import(&self.mcp_import_rows));
        self.clear_notice();
    }

    pub fn reopen_mcp_import_float(&mut self) {
        let prev_selected = self.float.as_ref().map(|f| f.selected).unwrap_or(0);
        self.float = Some(FloatMenu::mcp_import(&self.mcp_import_rows));
        if let Some(f) = self.float.as_mut() {
            let max = f.filtered_entries().len().saturating_sub(1);
            f.selected = prev_selected.min(max);
        }
        self.clear_notice();
    }

    /// Start in-float field edit (search bar). Never opens the yellow docked select.
    pub fn start_settings_inline_edit(
        &mut self,
        op: impl Into<String>,
        label: impl Into<String>,
        initial: impl Into<String>,
    ) {
        let op = op.into();
        let label = label.into();
        let initial = initial.into();
        self.settings_inline_op = Some(op);
        if let Some(f) = self.float.as_mut() {
            f.begin_edit(label, initial);
        }
        self.clear_notice();
    }

    pub fn cancel_settings_inline_edit(&mut self) {
        self.settings_inline_op = None;
        if let Some(f) = self.float.as_mut() {
            f.end_edit();
        }
    }

    /// Provider management list (from Settings).
    pub fn open_settings_providers(&mut self, rows: &[(String, String)]) {
        self.float = Some(FloatMenu::settings_providers(rows));
        self.clear_notice();
    }

    /// Models for the focused provider (second level under provider detail).
    pub fn open_settings_models_for_provider(&mut self, provider: &str) {
        self.settings_provider_focus = provider.to_string();
        self.float = Some(FloatMenu::settings_models_for_provider(
            provider,
            &self.settings_model_rows,
        ));
        self.clear_notice();
    }

    pub fn open_settings_provider_detail(&mut self, id: &str, detail: &str) {
        self.settings_provider_focus = id.to_string();
        let fields = self.provider_detail_fields(id);
        self.float = Some(FloatMenu::settings_provider_detail(id, detail, &fields));
        self.clear_notice();
    }

    pub fn open_settings_provider_api(&mut self, id: &str) {
        self.settings_provider_focus = id.to_string();
        self.float = Some(FloatMenu::settings_provider_api(id));
        self.clear_notice();
    }

    pub fn open_settings_thinking_format(&mut self, scope: &str, on_model: bool) {
        self.settings_compat_on_model = on_model;
        self.float = Some(FloatMenu::settings_thinking_format(scope));
        self.clear_notice();
    }

    pub fn open_settings_max_tokens_field(&mut self, scope: &str, on_model: bool) {
        self.settings_compat_on_model = on_model;
        self.float = Some(FloatMenu::settings_max_tokens_field(scope));
        self.clear_notice();
    }

    pub fn open_settings_remote_models(&mut self, provider: &str, rows: Vec<(String, String)>) {
        self.settings_provider_focus = provider.to_string();
        self.float = Some(FloatMenu::settings_remote_models(provider, &rows));
        self.clear_notice();
    }

    pub fn open_settings_model_detail(&mut self, spec: &str, detail: &str) {
        self.settings_model_focus = spec.to_string();
        if let Some((p, _)) = spec.split_once(':') {
            self.settings_provider_focus = p.to_string();
        }
        self.float = Some(FloatMenu::settings_model_detail(spec, detail));
        self.clear_notice();
    }

    /// Re-open provider detail for the focused provider (after edits).
    pub fn reopen_settings_provider_detail(&mut self) {
        let id = self.settings_provider_focus.clone();
        if id.is_empty() {
            self.open_settings_providers(&self.settings_provider_rows.clone());
            return;
        }
        let detail = self
            .settings_provider_rows
            .iter()
            .find(|(k, _)| k == &id)
            .map(|(_, d)| d.clone())
            .unwrap_or_default();
        self.open_settings_provider_detail(&id, &detail);
    }

    /// Open in-float Add model form for the focused provider.
    pub fn open_settings_model_add(&mut self) {
        let provider = self.settings_provider_focus.clone();
        if provider.is_empty() {
            self.set_notice("no provider selected");
            return;
        }
        self.model_draft = Some(ModelDraft::new(&provider));
        self.settings_form_edit = None;
        self.rebuild_settings_model_add_float();
        self.clear_notice();
    }

    pub fn rebuild_settings_model_add_float(&mut self) {
        let Some(draft) = self.model_draft.clone() else {
            return;
        };
        let editing = self.settings_form_edit.clone();
        let mut menu = FloatMenu::settings_model_add(&draft.provider, &draft, editing.as_deref());
        // When editing a field, put current value into search for typing.
        if let Some(key) = &editing {
            menu.begin_edit(key.clone(), draft.field(key));
        }
        self.float = Some(menu);
    }

    /// Navigate one level up in the Settings hierarchy. Returns true if handled.
    pub fn settings_go_back(&mut self) -> bool {
        // Cancel inline ConfigOp field edit first.
        if self.settings_inline_op.is_some() {
            self.cancel_settings_inline_edit();
            return true;
        }
        // Cancel in-form field edit first.
        if self.settings_form_edit.take().is_some() {
            self.rebuild_settings_model_add_float();
            return true;
        }
        let Some(kind) = self.float.as_ref().map(|f| f.kind) else {
            return false;
        };
        match kind {
            FloatKind::Settings => {
                self.close_float();
                true
            }
            FloatKind::SettingsToolOutput => {
                self.open_settings_float();
                true
            }
            FloatKind::SettingsCompaction => {
                self.open_settings_float();
                true
            }
            FloatKind::SettingsProviders => {
                self.open_settings_float();
                true
            }
            FloatKind::SettingsProviderDetail => {
                self.open_settings_providers(&self.settings_provider_rows.clone());
                true
            }
            FloatKind::SettingsProviderApi
            | FloatKind::SettingsRemoteModels
            | FloatKind::SettingsThinkingFormat
            | FloatKind::SettingsMaxTokensField => {
                if self.settings_compat_on_model && !self.settings_model_focus.is_empty() {
                    let spec = self.settings_model_focus.clone();
                    let detail = self
                        .settings_model_rows
                        .iter()
                        .find(|(k, _)| k == &spec)
                        .map(|(_, d)| d.clone())
                        .unwrap_or_default();
                    self.open_settings_model_detail(&spec, &detail);
                } else {
                    self.reopen_settings_provider_detail();
                }
                true
            }
            FloatKind::SettingsModels => {
                self.reopen_settings_provider_detail();
                true
            }
            FloatKind::SettingsModelDetail => {
                let p = self.settings_provider_focus.clone();
                if p.is_empty() {
                    self.open_settings_providers(&self.settings_provider_rows.clone());
                } else {
                    self.open_settings_models_for_provider(&p);
                }
                true
            }
            FloatKind::SettingsModelAdd => {
                self.model_draft = None;
                self.settings_form_edit = None;
                let p = self.settings_provider_focus.clone();
                self.open_settings_models_for_provider(&p);
                true
            }
            FloatKind::Skills => {
                // Same as Thinking: Esc returns to Settings root.
                self.open_settings_float();
                true
            }
            FloatKind::Agents => {
                // From Settings → Agents, Esc goes back to Settings; from /agents just close.
                self.open_settings_float();
                true
            }
            FloatKind::Features => {
                self.open_settings_float();
                true
            }
            FloatKind::Mcp => {
                self.open_settings_float();
                true
            }
            FloatKind::McpImport => {
                // Back to MCP manager (caller may refresh rows).
                self.open_mcp_float();
                true
            }
            FloatKind::BackgroundDetail => {
                // Esc is handled in `handle_float_key` → OpenBackgroundList
                // so the CLI reloads a fresh snapshot (not this cache).
                false
            }
            FloatKind::Background => {
                // Esc closes the panel entirely.
                false
            }
            FloatKind::Thinking => {
                // Opened from Settings — return to root rather than blank.
                self.open_settings_float();
                true
            }
            _ => false,
        }
    }

    /// Feed Settings → Providers / Models lists (from ProviderSet).
    pub fn set_settings_catalog(
        &mut self,
        providers: Vec<(String, String)>,
        models: Vec<(String, String)>,
        provider_fields: Vec<(String, String)>,
    ) {
        self.settings_provider_rows = providers;
        self.settings_model_rows = models;
        self.settings_provider_field_rows = provider_fields;
    }

    fn provider_detail_fields(&self, provider: &str) -> Vec<(String, String)> {
        let prefix = format!("{provider}:");
        self.settings_provider_field_rows
            .iter()
            .filter_map(|(key, value)| {
                key.strip_prefix(&prefix)
                    .map(|field| (field.to_string(), value.clone()))
            })
            .collect()
    }

    fn provider_detail_field_value(&self, provider: &str, key: &str) -> String {
        self.settings_provider_field_rows
            .iter()
            .find(|(k, _)| k == &format!("{provider}:{key}"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    pub(crate) fn float_allows_fetch_models(kind: FloatKind) -> bool {
        matches!(
            kind,
            FloatKind::SettingsModels
                | FloatKind::SettingsProviderDetail
                | FloatKind::SettingsRemoteModels
        )
    }

    /// Emit `ProviderFetchModels` for the focused settings provider.
    /// Emit `ProviderFetchModels` for the focused settings provider.
    pub(crate) fn provider_fetch_models_outcome(&mut self) -> RunOutcome {
        let id = self.settings_provider_focus.clone();
        if id.is_empty() {
            self.set_notice("no provider selected");
            RunOutcome::Noop
        } else {
            RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id })
        }
    }

    /// Save float search into the model draft field being edited.
    pub(crate) fn commit_settings_form_edit(&mut self) -> RunOutcome {
        let Some(key) = self.settings_form_edit.take() else {
            return RunOutcome::Noop;
        };
        let value = self
            .float
            .as_ref()
            .map(|f| f.search.clone())
            .unwrap_or_default();
        if let Some(draft) = self.model_draft.as_mut() {
            draft.set_field(&key, value);
        }
        if let Some(f) = self.float.as_mut() {
            f.end_edit();
        }
        self.rebuild_settings_model_add_float();
        RunOutcome::Noop
    }

    /// Commit in-float ConfigOp edit (provider/model field).
    /// Commit in-float ConfigOp edit (provider/model field).
    pub(crate) fn commit_settings_inline_edit(&mut self) -> RunOutcome {
        let Some(op) = self.settings_inline_op.take() else {
            return RunOutcome::Noop;
        };
        let value = self
            .float
            .as_ref()
            .map(|f| f.search.clone())
            .unwrap_or_default();
        if let Some(f) = self.float.as_mut() {
            f.end_edit();
        }
        match config_op_from_field(&op, &value) {
            Some(cfg) => RunOutcome::ConfigOp(cfg),
            None => {
                self.set_notice("invalid value");
                RunOutcome::Noop
            }
        }
    }

    pub(crate) fn confirm_skills_toggle(&mut self, id: &str) -> RunOutcome {
        if id == "_empty" || id.is_empty() {
            return RunOutcome::Noop;
        }
        RunOutcome::ConfigOp(ConfigOp::SkillToggle {
            path: id.to_string(),
        })
    }

    pub(crate) fn confirm_agents_item(&mut self, id: &str) -> RunOutcome {
        match id {
            "_empty" | "" => RunOutcome::Noop,
            "_dir_project" => {
                let p = self.agents_project_dir.clone();
                self.set_notice(if p.is_empty() {
                    "project agents: <cwd>/.one/agents".into()
                } else {
                    format!("project agents dir · {p}")
                });
                RunOutcome::Noop
            }
            "_dir_user" => {
                let p = self.agents_user_dir.clone();
                self.set_notice(if p.is_empty() {
                    "user agents: ~/.one/agent/agents".into()
                } else {
                    format!("user agents dir · {p}")
                });
                RunOutcome::Noop
            }
            id => {
                // Show path + tools detail; notice also echoes path for copy-friendly UX.
                if let Some(row) = self.agents_rows.iter().find(|r| r.0 == id) {
                    let path = &row.3;
                    self.set_notice(format!("{} · {}", row.1, path));
                }
                self.open_agent_detail_float(id);
                RunOutcome::Noop
            }
        }
    }

    pub(crate) fn confirm_features_toggle(&mut self, id: &str) -> RunOutcome {
        if id == "_empty" || id.is_empty() {
            return RunOutcome::Noop;
        }
        RunOutcome::ConfigOp(ConfigOp::FeatureToggle { id: id.to_string() })
    }

    pub(crate) fn confirm_mcp_action(&mut self, id: &str) -> RunOutcome {
        match id {
            "_empty" | "" => RunOutcome::Noop,
            "_import" => {
                self.close_float();
                RunOutcome::OpenMcpImportPanel
            }
            "_import_all" => {
                self.close_float();
                RunOutcome::ConfigOp(ConfigOp::McpImport {
                    names: Vec::new(),
                    force: false,
                })
            }
            _ => RunOutcome::ConfigOp(ConfigOp::McpToggle {
                name: id.to_string(),
            }),
        }
    }

    pub(crate) fn confirm_mcp_import(&mut self, id: &str) -> RunOutcome {
        if id == "_empty" || id.is_empty() {
            return RunOutcome::Noop;
        }
        // Import one server; force if already owned so user can re-sync.
        let force = self
            .mcp_import_rows
            .iter()
            .find(|(n, _, _, _)| n == id)
            .map(|(_, _, _, owned)| *owned)
            .unwrap_or(false);
        RunOutcome::ConfigOp(ConfigOp::McpImport {
            names: vec![id.to_string()],
            force,
        })
    }

    pub(crate) fn confirm_settings_root(&mut self, id: &str) -> RunOutcome {
        match id {
            "thinking" => {
                self.open_thinking_float();
                RunOutcome::Noop
            }
            "auto_approve" => {
                self.close_float();
                RunOutcome::ConfigOp(ConfigOp::SettingSet {
                    key: "auto_approve".into(),
                    value: "toggle".into(),
                })
            }
            "sandbox" => {
                self.close_float();
                RunOutcome::ConfigOp(ConfigOp::SettingSet {
                    key: "sandbox".into(),
                    value: "cycle".into(),
                })
            }
            "tool_output" => {
                self.open_settings_tool_output();
                RunOutcome::Noop
            }
            "compaction" => {
                self.open_settings_compaction();
                RunOutcome::Noop
            }
            "skills" => {
                self.open_skills_float();
                RunOutcome::Noop
            }
            "agents" => {
                self.open_agents_float();
                RunOutcome::Noop
            }
            "features" => {
                self.open_features_float();
                RunOutcome::Noop
            }
            "mcp" => {
                self.close_float();
                RunOutcome::OpenMcpPanel
            }
            "providers" => {
                self.open_settings_providers(&self.settings_provider_rows.clone());
                RunOutcome::Noop
            }
            "switch_model" => {
                self.open_model_select();
                RunOutcome::Noop
            }
            _ => RunOutcome::Noop,
        }
    }

    pub(crate) fn confirm_settings_tool_output(&mut self, id: &str) -> RunOutcome {
        match id {
            "max_lines" => {
                let initial = self.tool_output_max_lines.to_string();
                self.start_settings_inline_edit(
                    "setting:tool_output.max_lines",
                    "max lines",
                    initial,
                );
                RunOutcome::Noop
            }
            "max_bytes" => {
                let initial = self.tool_output_max_bytes.to_string();
                self.start_settings_inline_edit(
                    "setting:tool_output.max_bytes",
                    "max bytes",
                    initial,
                );
                RunOutcome::Noop
            }
            "hint" => {
                self.set_notice("Over limit → spill to ~/.one/agent/tool-outputs/ (7-day cleanup)");
                RunOutcome::Noop
            }
            _ => RunOutcome::Noop,
        }
    }

    pub(crate) fn confirm_settings_compaction(&mut self, id: &str) -> RunOutcome {
        match id {
            "auto" => {
                self.close_float();
                RunOutcome::ConfigOp(ConfigOp::SettingSet {
                    key: "compaction.auto".into(),
                    value: "toggle".into(),
                })
            }
            "ratio" => {
                let pct = (self.compaction_ratio * 100.0).round() as u32;
                self.start_settings_inline_edit(
                    "setting:compaction.ratio",
                    "threshold % (e.g. 70 or 0.7)",
                    pct.to_string(),
                );
                RunOutcome::Noop
            }
            "threshold" => {
                let initial = self
                    .compaction_threshold
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "auto".into());
                self.start_settings_inline_edit(
                    "setting:compaction.threshold",
                    "token threshold (or auto)",
                    initial,
                );
                RunOutcome::Noop
            }
            "keep_recent" => {
                self.start_settings_inline_edit(
                    "setting:compaction.keep_recent",
                    "keep recent messages",
                    self.compaction_keep_recent.to_string(),
                );
                RunOutcome::Noop
            }
            "prune" => {
                self.close_float();
                RunOutcome::ConfigOp(ConfigOp::SettingSet {
                    key: "compaction.prune".into(),
                    value: "toggle".into(),
                })
            }
            "prune_protect" => {
                self.start_settings_inline_edit(
                    "setting:compaction.prune_protect_tokens",
                    "protect recent tool tokens",
                    self.compaction_prune_protect.to_string(),
                );
                RunOutcome::Noop
            }
            "prune_max_chars" => {
                self.start_settings_inline_edit(
                    "setting:compaction.prune_max_chars",
                    "pruned preview chars",
                    self.compaction_prune_max_chars.to_string(),
                );
                RunOutcome::Noop
            }
            "hint" => {
                self.set_notice(
                    "Main: summarize older turns, keep recent N intact. Prune (off by default): only clears tool bodies outside that tail before summary.",
                );
                RunOutcome::Noop
            }
            _ => RunOutcome::Noop,
        }
    }

    pub(crate) fn confirm_settings_providers(&mut self, id: &str) -> RunOutcome {
        match id {
            "add_provider" => {
                self.start_settings_inline_edit("provider_add_id", "provider id", "");
                RunOutcome::Noop
            }
            id if id.starts_with("p:") => {
                let clean = id.trim_start_matches("p:").to_string();
                let detail = self
                    .settings_provider_rows
                    .iter()
                    .find(|(k, _)| k == &clean)
                    .map(|(_, d)| d.clone())
                    .unwrap_or_default();
                self.open_settings_provider_detail(&clean, &detail);
                RunOutcome::Noop
            }
            _ => RunOutcome::Noop,
        }
    }

    pub(crate) fn confirm_settings_provider_detail(&mut self, id: &str) -> RunOutcome {
        let focus = self.settings_provider_focus.clone();
        match id {
            "models" => {
                self.open_settings_models_for_provider(&focus);
                RunOutcome::Noop
            }
            "fetch_models" => self.provider_fetch_models_outcome(),
            "set_provider_type" | "set_api" => {
                // Fixed protocol enum — select, never free-text.
                self.open_settings_provider_api(&focus);
                RunOutcome::Noop
            }
            "set_base_url" => {
                let initial = self.provider_detail_field_value(&focus, "base_url");
                self.start_settings_inline_edit(
                    format!("provider_set:{focus}:base_url"),
                    "base_url",
                    if initial == "unset" { "" } else { &initial },
                );
                RunOutcome::Noop
            }
            "set_api_key" => {
                let initial = self.provider_detail_field_value(&focus, "api_key");
                self.start_settings_inline_edit(
                    format!("provider_set:{focus}:api_key"),
                    "api_key",
                    if initial == "unset" || initial == "set" {
                        ""
                    } else {
                        &initial
                    },
                );
                RunOutcome::Noop
            }
            "set_default_model" => {
                let initial = self.provider_detail_field_value(&focus, "default_model");
                self.start_settings_inline_edit(
                    format!("provider_set:{focus}:default_model"),
                    "default_model",
                    if initial == "unset" { "" } else { &initial },
                );
                RunOutcome::Noop
            }
            "set_thinking_format" => {
                self.open_settings_thinking_format(&focus, false);
                RunOutcome::Noop
            }
            "set_max_tokens_field" => {
                self.open_settings_max_tokens_field(&focus, false);
                RunOutcome::Noop
            }
            "clear_compat" => RunOutcome::ConfigOp(ConfigOp::ProviderSet {
                id: focus,
                key: "compat".into(),
                value: "clear".into(),
            }),
            id if id.starts_with("cycle_compat:") => {
                let key = id.trim_start_matches("cycle_compat:").to_string();
                // Field rows store camelCase labels: `compat.supportsDeveloperRole`.
                let current = self
                    .provider_detail_fields(&focus)
                    .into_iter()
                    .find(|(k, _)| {
                        let kn = k.trim_start_matches("compat.");
                        kn.eq_ignore_ascii_case(&key)
                            || kn
                                .replace('_', "")
                                .eq_ignore_ascii_case(&key.replace('_', ""))
                    })
                    .map(|(_, v)| v)
                    .unwrap_or_else(|| "auto".into());
                let next = cycle_tri_display(&current);
                RunOutcome::ConfigOp(ConfigOp::ProviderSet {
                    id: focus,
                    key,
                    value: next.to_string(),
                })
            }
            "rm_provider" => RunOutcome::ConfigOp(ConfigOp::ProviderRm { id: focus }),
            _ => RunOutcome::Noop,
        }
    }

    pub(crate) fn confirm_settings_thinking_format(&mut self, id: &str) -> RunOutcome {
        let Some(value) = id.strip_prefix("tf:") else {
            return RunOutcome::Noop;
        };
        if self.settings_compat_on_model {
            let spec = self.settings_model_focus.clone();
            if spec.is_empty() {
                self.set_notice("no model selected");
                return RunOutcome::Noop;
            }
            RunOutcome::ConfigOp(ConfigOp::ModelSet {
                spec,
                key: "thinking_format".into(),
                value: value.to_string(),
            })
        } else {
            let provider = self.settings_provider_focus.clone();
            if provider.is_empty() {
                self.set_notice("no provider selected");
                return RunOutcome::Noop;
            }
            RunOutcome::ConfigOp(ConfigOp::ProviderSet {
                id: provider,
                key: "thinking_format".into(),
                value: value.to_string(),
            })
        }
    }

    pub(crate) fn confirm_settings_max_tokens_field(&mut self, id: &str) -> RunOutcome {
        let Some(value) = id.strip_prefix("mt:") else {
            return RunOutcome::Noop;
        };
        if self.settings_compat_on_model {
            let spec = self.settings_model_focus.clone();
            if spec.is_empty() {
                self.set_notice("no model selected");
                return RunOutcome::Noop;
            }
            RunOutcome::ConfigOp(ConfigOp::ModelSet {
                spec,
                key: "max_tokens_field".into(),
                value: value.to_string(),
            })
        } else {
            let provider = self.settings_provider_focus.clone();
            if provider.is_empty() {
                self.set_notice("no provider selected");
                return RunOutcome::Noop;
            }
            RunOutcome::ConfigOp(ConfigOp::ProviderSet {
                id: provider,
                key: "max_tokens_field".into(),
                value: value.to_string(),
            })
        }
    }

    pub(crate) fn confirm_settings_provider_api(&mut self, id: &str) -> RunOutcome {
        let Some(value) = id.strip_prefix("api:") else {
            return RunOutcome::Noop;
        };
        let provider = self.settings_provider_focus.clone();
        if provider.is_empty() {
            self.set_notice("no provider selected");
            return RunOutcome::Noop;
        }
        // Writes both `api` and `providerType` (canonical protocol string).
        RunOutcome::ConfigOp(ConfigOp::ProviderSet {
            id: provider,
            key: "api".into(),
            value: value.to_string(),
        })
    }

    pub(crate) fn confirm_settings_remote_models(&mut self, id: &str) -> RunOutcome {
        let Some(model_id) = id.strip_prefix("remote_model:") else {
            return RunOutcome::Noop;
        };
        let provider = self.settings_provider_focus.clone();
        if provider.is_empty() || model_id.trim().is_empty() {
            self.set_notice("no remote model selected");
            return RunOutcome::Noop;
        }
        RunOutcome::ConfigOp(ConfigOp::ModelAdd {
            spec: format!("{provider}:{model_id}"),
            name: Some(model_id.to_string()),
            context_window: None,
        })
    }

    pub(crate) fn confirm_settings_models(&mut self, id: &str) -> RunOutcome {
        match id {
            "fetch_models" => self.provider_fetch_models_outcome(),
            "add_model" => {
                // Stay inside Settings float — form with id + optional fields.
                self.open_settings_model_add();
                RunOutcome::Noop
            }
            id if id.starts_with("m:") => {
                let clean = id.trim_start_matches("m:").to_string();
                let detail = self
                    .settings_model_rows
                    .iter()
                    .find(|(k, _)| k == &clean)
                    .map(|(_, d)| d.clone())
                    .unwrap_or_default();
                self.open_settings_model_detail(&clean, &detail);
                RunOutcome::Noop
            }
            _ => RunOutcome::Noop,
        }
    }

    pub(crate) fn confirm_settings_model_add(&mut self, id: &str) -> RunOutcome {
        match id {
            "save" => match self
                .model_draft
                .as_ref()
                .ok_or_else(|| "no draft".to_string())
                .and_then(|d| d.to_config_op())
            {
                Ok(op) => {
                    self.model_draft = None;
                    self.settings_form_edit = None;
                    RunOutcome::ConfigOp(op)
                }
                Err(err) => {
                    self.set_notice(format!("add model: {err}"));
                    RunOutcome::Noop
                }
            },
            "cancel" => {
                self.settings_go_back();
                RunOutcome::Noop
            }
            id if id.starts_with("field:") => {
                let key = id.trim_start_matches("field:").to_string();
                self.settings_form_edit = Some(key);
                self.rebuild_settings_model_add_float();
                RunOutcome::Noop
            }
            _ => RunOutcome::Noop,
        }
    }

    pub(crate) fn confirm_settings_model_detail(&mut self, id: &str) -> RunOutcome {
        let focus = self.settings_model_focus.clone();
        match id {
            "set_name" => {
                self.start_settings_inline_edit(format!("model_set:{focus}:name"), "name", "");
                RunOutcome::Noop
            }
            "set_ctx" => {
                self.start_settings_inline_edit(
                    format!("model_set:{focus}:ctx"),
                    "context_window",
                    "",
                );
                RunOutcome::Noop
            }
            "set_reasoning" => {
                // Cycle unset → true → false → unset via empty/true/false.
                let detail = self
                    .settings_model_rows
                    .iter()
                    .find(|(k, _)| k == &focus)
                    .map(|(_, d)| d.as_str())
                    .unwrap_or("");
                let current = detail
                    .split("reasoning=")
                    .nth(1)
                    .map(|s| s.split_whitespace().next().unwrap_or("unset"))
                    .unwrap_or("unset");
                let next = match current {
                    "true" | "yes" | "1" => "false",
                    "false" | "no" | "0" => "",
                    _ => "true",
                };
                RunOutcome::ConfigOp(ConfigOp::ModelSet {
                    spec: focus,
                    key: "reasoning".into(),
                    value: next.to_string(),
                })
            }
            "set_thinking_level_map" => {
                let detail = self
                    .settings_model_rows
                    .iter()
                    .find(|(k, _)| k == &focus)
                    .map(|(_, d)| d.clone())
                    .unwrap_or_default();
                let initial = detail
                    .split("map=")
                    .nth(1)
                    .map(|s| s.trim())
                    .filter(|s| *s != "(none)" && !s.is_empty())
                    .unwrap_or("");
                self.start_settings_inline_edit(
                    format!("model_set:{focus}:thinking_level_map"),
                    "thinkingLevelMap",
                    initial,
                );
                RunOutcome::Noop
            }
            "set_thinking_format" => {
                self.open_settings_thinking_format(&focus, true);
                RunOutcome::Noop
            }
            "set_max_tokens_field" => {
                self.open_settings_max_tokens_field(&focus, true);
                RunOutcome::Noop
            }
            "clear_compat" => RunOutcome::ConfigOp(ConfigOp::ModelSet {
                spec: focus,
                key: "compat".into(),
                value: "clear".into(),
            }),
            id if id.starts_with("cycle_compat:") => {
                let key = id.trim_start_matches("cycle_compat:").to_string();
                let detail = self
                    .settings_model_rows
                    .iter()
                    .find(|(k, _)| k == &focus)
                    .map(|(_, d)| d.as_str())
                    .unwrap_or("");
                // Best-effort read from detail line for the two common keys.
                let current = if key.contains("developer") {
                    detail
                        .split("devRole=")
                        .nth(1)
                        .map(|s| s.split_whitespace().next().unwrap_or("auto"))
                        .unwrap_or("auto")
                } else if key.contains("reasoning_effort") {
                    detail
                        .split("effort=")
                        .nth(1)
                        .map(|s| s.split_whitespace().next().unwrap_or("auto"))
                        .unwrap_or("auto")
                } else {
                    "auto"
                };
                let next = cycle_tri_display(current);
                RunOutcome::ConfigOp(ConfigOp::ModelSet {
                    spec: focus,
                    key,
                    value: next.to_string(),
                })
            }
            "rm_model" => RunOutcome::ConfigOp(ConfigOp::ModelRm { spec: focus }),
            _ => RunOutcome::Noop,
        }
    }
}

/// Cycle tri-state display: auto → true → false → auto.
fn cycle_tri_display(current: &str) -> &'static str {
    match current.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => "false",
        "false" | "no" | "0" | "off" => "auto",
        _ => "true",
    }
}

/// Parse Settings field-edit op + typed value into a [`ConfigOp`].
fn config_op_from_field(op: &str, value: &str) -> Option<ConfigOp> {
    let value = value.trim();
    // setting:<key> — Settings panel free-text fields (tool_output, …).
    if let Some(key) = op.strip_prefix("setting:") {
        if key.is_empty() || value.is_empty() {
            return None;
        }
        return Some(ConfigOp::SettingSet {
            key: key.to_string(),
            value: value.to_string(),
        });
    }
    if op == "provider_add_id" {
        if value.is_empty() {
            return None;
        }
        return Some(ConfigOp::ProviderAdd {
            id: value.to_string(),
            base_url: None,
        });
    }
    if let Some(rest) = op.strip_prefix("provider_add_base:") {
        return Some(ConfigOp::ProviderAdd {
            id: rest.to_string(),
            base_url: if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            },
        });
    }
    if op == "model_add" {
        if value.is_empty() {
            return None;
        }
        return Some(ConfigOp::ModelAdd {
            spec: value.to_string(),
            name: None,
            context_window: None,
        });
    }
    // model_add:<provider> — value is model id only (legacy docked path)
    if let Some(provider) = op.strip_prefix("model_add:") {
        if value.is_empty() || provider.is_empty() {
            return None;
        }
        return Some(ConfigOp::ModelAdd {
            spec: format!("{provider}:{value}"),
            name: None,
            context_window: None,
        });
    }
    if let Some(rest) = op.strip_prefix("provider_set:") {
        // provider_set:<id>:<key>
        let (id, key) = rest.split_once(':')?;
        if id.is_empty() || key.is_empty() {
            return None;
        }
        return Some(ConfigOp::ProviderSet {
            id: id.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        });
    }
    if let Some(rest) = op.strip_prefix("model_set:") {
        // model_set:<provider:id>:<key> — key is after the last ':'
        let (spec, key) = rest.rsplit_once(':')?;
        if spec.is_empty() || key.is_empty() {
            return None;
        }
        return Some(ConfigOp::ModelSet {
            spec: spec.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        });
    }
    None
}
