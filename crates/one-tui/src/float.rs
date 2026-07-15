//! Floating modal menu — centered overlay for secondary UIs
//! (command palette, model picker, future settings, …).
//!
//! Visual language matches a typical agent TUI command palette:
//! title · search · section headers · items with right-aligned hints · footer.

use crate::slash::ModelChoice;

/// One selectable row inside a section.
#[derive(Debug, Clone)]
pub struct FloatItem {
    pub id: String,
    pub label: String,
    /// Secondary text (description).
    pub detail: String,
    /// Right-aligned hint (shortcut / path).
    pub hint: String,
}

/// Named group of items (e.g. provider name, "Session", "Tools").
#[derive(Debug, Clone)]
pub struct FloatSection {
    pub title: String,
    pub items: Vec<FloatItem>,
}

/// Kind of float — affects title / confirm behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatKind {
    /// Root command palette (`/`).
    Commands,
    /// Model picker (legacy center; prefer docked select).
    Models,
    /// Help catalog (`/help`) — selecting a row drills into that command.
    Help,
    /// Resume session list (`/resume`).
    Sessions,
    /// Thinking level picker (`/thinking`).
    Thinking,
    /// Session branch tree (`/tree`).
    Tree,
    /// Rewind to a previous user prompt (Esc Esc / `/rewind`).
    Rewind,
    /// Read-only info panel (session summary, …). Enter closes.
    Info,
    /// Settings root (Ctrl+G).
    Settings,
    /// Provider list under Settings.
    SettingsProviders,
    /// Actions for one provider.
    SettingsProviderDetail,
    /// Provider-level API enum picker.
    SettingsProviderApi,
    /// Remote model ids fetched from the provider.
    SettingsRemoteModels,
    /// Model list under Settings.
    SettingsModels,
    /// Actions for one model.
    SettingsModelDetail,
    /// Add-model form under a provider (stays in Settings float).
    SettingsModelAdd,
    /// Generic custom list.
    Custom,
}

/// Centered floating menu state.
#[derive(Debug, Clone)]
pub struct FloatMenu {
    pub kind: FloatKind,
    pub title: String,
    pub search: String,
    pub sections: Vec<FloatSection>,
    /// Index into flattened **selectable** items (not headers).
    pub selected: usize,
    /// When true, `search` is an in-float value editor (not a filter).
    /// Used by Settings so we never leave the float for a docked select.
    pub edit_mode: bool,
    /// Label shown instead of "search:" while `edit_mode` (e.g. `base_url`).
    pub edit_label: String,
}

/// Flat view of a selectable entry (for navigation / confirm).
#[derive(Debug, Clone)]
pub struct FloatEntry {
    pub section_title: String,
    pub item: FloatItem,
}

impl FloatMenu {
    fn with_sections(
        kind: FloatKind,
        title: impl Into<String>,
        sections: Vec<FloatSection>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            search: String::new(),
            sections,
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    pub fn commands_palette() -> Self {
        Self::with_sections(FloatKind::Commands, "Commands", default_command_sections())
    }

    /// `/help` — browse commands; confirm drills into secondary UI or runs.
    pub fn help_menu() -> Self {
        Self::with_sections(FloatKind::Help, "Help", default_command_sections())
    }

    /// `/thinking` level picker.
    pub fn thinking_picker(current: &str) -> Self {
        let levels = ["off", "low", "medium", "high"];
        let items = levels
            .iter()
            .map(|l| {
                let label = if *l == current {
                    format!("{l}  (current)")
                } else {
                    (*l).to_string()
                };
                FloatItem {
                    id: (*l).into(),
                    label,
                    detail: match *l {
                        "off" => "no extended thinking",
                        "low" => "light reasoning",
                        "medium" => "balanced",
                        "high" => "deep reasoning",
                        _ => "",
                    }
                    .into(),
                    hint: format!("/thinking {l}"),
                }
            })
            .collect();
        Self {
            kind: FloatKind::Thinking,
            title: "Thinking".into(),
            search: String::new(),
            sections: vec![FloatSection {
                title: "Level".into(),
                items,
            }],
            selected: levels.iter().position(|l| *l == current).unwrap_or(0),
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    /// `/resume` session list. `id` is the resume key (path or session id).
    pub fn sessions_picker(sessions: &[(String, String, String, String)]) -> Self {
        // (id, label, detail, hint)
        let items: Vec<FloatItem> = sessions
            .iter()
            .map(|(id, label, detail, hint)| FloatItem {
                id: id.clone(),
                label: label.clone(),
                detail: detail.clone(),
                hint: hint.clone(),
            })
            .collect();
        Self {
            kind: FloatKind::Sessions,
            title: "Resume Session".into(),
            search: String::new(),
            sections: vec![FloatSection {
                title: if items.is_empty() {
                    "No sessions".into()
                } else {
                    format!("{} recent", items.len())
                },
                items,
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    /// `/tree` branch list. Each item id is an entry id.
    pub fn tree_picker(entries: &[(String, String, String)]) -> Self {
        // (id, label, detail)
        let items: Vec<FloatItem> = entries
            .iter()
            .map(|(id, label, detail)| FloatItem {
                id: id.clone(),
                label: label.clone(),
                detail: detail.clone(),
                hint: id.chars().take(8).collect(),
            })
            .collect();
        Self {
            kind: FloatKind::Tree,
            title: "Session Tree".into(),
            search: String::new(),
            sections: vec![FloatSection {
                title: "Branches".into(),
                items,
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    /// Esc Esc / `/rewind` — pick a prior user prompt to re-edit.
    /// `prompts` is `(entry_id, preview)` newest first.
    pub fn rewind_picker(prompts: &[(String, String)]) -> Self {
        let items: Vec<FloatItem> = prompts
            .iter()
            .enumerate()
            .map(|(i, (id, preview))| FloatItem {
                id: id.clone(),
                label: preview.clone(),
                detail: "restore conversation before this · edit prompt".into(),
                hint: format!("#{}", i + 1),
            })
            .collect();
        Self {
            kind: FloatKind::Rewind,
            title: "Rewind".into(),
            search: String::new(),
            sections: vec![FloatSection {
                title: if items.is_empty() {
                    "No prompts yet".into()
                } else {
                    format!("{} prompts · conversation only", items.len())
                },
                items,
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    /// Read-only key/value style info (session summary).
    pub fn info_panel(title: impl Into<String>, rows: &[(String, String)]) -> Self {
        let items: Vec<FloatItem> = rows
            .iter()
            .map(|(k, v)| FloatItem {
                id: k.clone(),
                label: k.clone(),
                detail: v.clone(),
                hint: String::new(),
            })
            .collect();
        Self {
            kind: FloatKind::Info,
            title: title.into(),
            search: String::new(),
            sections: vec![FloatSection {
                title: "Details".into(),
                items,
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    /// Settings root — separate from `/` command palette.
    ///
    /// Hierarchy: Settings → Providers → Provider → Models → Model
    /// (no standalone global Models entry).
    pub fn settings_root(thinking: &str, provider: &str, model: &str) -> Self {
        Self {
            kind: FloatKind::Settings,
            title: "Settings".into(),
            search: String::new(),
            sections: vec![
                FloatSection {
                    title: "General".into(),
                    items: vec![
                        item(
                            "thinking",
                            "Thinking",
                            &format!("current: {thinking}"),
                            "levels",
                        ),
                        item(
                            "auto_approve",
                            "Auto-approve",
                            "toggle bash danger prompts",
                            "toggle",
                        ),
                        item(
                            "sandbox",
                            "Sandbox",
                            "workspace-write / full-access",
                            "cycle",
                        ),
                    ],
                },
                FloatSection {
                    title: "Providers".into(),
                    items: vec![
                        item(
                            "providers",
                            "Manage providers",
                            "base_url · api · models · keys",
                            "models.json",
                        ),
                        item(
                            "switch_model",
                            "Switch active model",
                            &format!("now {provider}:{model}"),
                            "Ctrl+L",
                        ),
                    ],
                },
            ],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    pub fn settings_providers(rows: &[(String, String)]) -> Self {
        let mut items: Vec<FloatItem> = rows
            .iter()
            .map(|(id, detail)| FloatItem {
                id: format!("p:{id}"),
                label: id.clone(),
                detail: detail.clone(),
                hint: "→".into(),
            })
            .collect();
        items.push(item(
            "add_provider",
            "+ Add provider",
            "id + base_url → models.json",
            "new",
        ));
        Self {
            kind: FloatKind::SettingsProviders,
            title: "Providers".into(),
            search: String::new(),
            sections: vec![FloatSection {
                title: format!("{} · Esc/← back", rows.len()),
                items,
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    pub fn settings_provider_detail(id: &str, detail: &str, fields: &[(String, String)]) -> Self {
        let field_value = |key: &str, fallback: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| fallback.to_string())
        };
        let provider_type = field_value("provider_type", "default/unset");
        let base_url = field_value("base_url", "unset");
        let api = field_value("api", "default/unset");
        let api_key = field_value("api_key", "unset");
        let default_model = field_value("default_model", "unset");
        Self {
            kind: FloatKind::SettingsProviderDetail,
            title: format!("Provider · {id}"),
            search: String::new(),
            sections: vec![
                FloatSection {
                    title: detail.into(),
                    items: vec![item(
                        "models",
                        "Models",
                        "list · add · edit under this provider",
                        "→",
                    )],
                },
                FloatSection {
                    title: "Connection".into(),
                    items: vec![
                        item("set_provider_type", "providerType", &provider_type, "edit"),
                        item("set_base_url", "base_url", &base_url, "edit"),
                        item("set_api", "api", &api, "select"),
                        item("set_api_key", "api_key", &api_key, "edit"),
                        item("set_default_model", "default_model", &default_model, "edit"),
                    ],
                },
                FloatSection {
                    title: "Danger".into(),
                    items: vec![item(
                        "rm_provider",
                        "Delete provider",
                        "removes provider and its models",
                        "rm",
                    )],
                },
            ],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    pub fn settings_provider_api(provider: &str) -> Self {
        Self {
            kind: FloatKind::SettingsProviderApi,
            title: format!("Provider API · {provider}"),
            search: String::new(),
            sections: vec![FloatSection {
                title: "Wire API".into(),
                items: vec![
                    item("api:", "default/unset", "clear provider-level api", ""),
                    item(
                        "api:openai-responses",
                        "openai-responses",
                        "OpenAI Responses API",
                        "",
                    ),
                    item(
                        "api:openai-completions",
                        "openai-completions",
                        "Chat Completions compatible",
                        "",
                    ),
                ],
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    pub fn settings_remote_models(provider: &str, rows: &[(String, String)]) -> Self {
        let items: Vec<FloatItem> = rows
            .iter()
            .map(|(id, detail)| FloatItem {
                id: format!("remote_model:{id}"),
                label: id.clone(),
                detail: detail.clone(),
                hint: "add".into(),
            })
            .collect();
        Self {
            kind: FloatKind::SettingsRemoteModels,
            title: format!("Remote models · {provider}"),
            search: String::new(),
            sections: vec![FloatSection {
                title: format!("{} fetched · Enter add · Esc back", items.len()),
                items,
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    /// Models for **one** provider (second level under Provider detail).
    pub fn settings_models_for_provider(provider: &str, rows: &[(String, String)]) -> Self {
        let prefix = format!("{provider}:");
        let mut items: Vec<FloatItem> = rows
            .iter()
            .filter(|(spec, _)| spec == provider || spec.starts_with(&prefix))
            .map(|(spec, detail)| {
                let label = spec
                    .strip_prefix(&prefix)
                    .unwrap_or(spec.as_str())
                    .to_string();
                FloatItem {
                    id: format!("m:{spec}"),
                    label,
                    detail: detail.clone(),
                    hint: "→".into(),
                }
            })
            .collect();
        let n = items.len();
        items.push(item(
            "add_model",
            "+ Add model",
            &format!("adds under {provider}"),
            "new",
        ));
        Self {
            kind: FloatKind::SettingsModels,
            title: format!("Models · {provider}"),
            search: String::new(),
            sections: vec![FloatSection {
                title: format!("{n} · Esc/← back to provider"),
                items,
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    pub fn settings_model_detail(spec: &str, detail: &str) -> Self {
        let short = spec.split_once(':').map(|(_, id)| id).unwrap_or(spec);
        Self {
            kind: FloatKind::SettingsModelDetail,
            title: format!("Model · {short}"),
            search: String::new(),
            sections: vec![FloatSection {
                title: detail.into(),
                items: vec![
                    item("set_name", "name", short, "edit"),
                    item("set_ctx", "context_window", "e.g. 128000", "edit"),
                    item("rm_model", "Delete model", "remove from models.json", "rm"),
                ],
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    /// In-float add-model form. Connection (`base_url` / `api`) is provider-level only.
    pub fn settings_model_add(
        provider: &str,
        draft: &crate::app::ModelDraft,
        editing: Option<&str>,
    ) -> Self {
        let mark = |key: &str, label: &str, value: &str, hint: &str| -> FloatItem {
            let editing_here = editing == Some(key);
            let display = if value.is_empty() {
                "(empty)".to_string()
            } else {
                value.to_string()
            };
            let label = if editing_here {
                format!("▸ {label}")
            } else {
                label.to_string()
            };
            FloatItem {
                id: format!("field:{key}"),
                label,
                detail: display,
                hint: if editing_here {
                    "typing…".into()
                } else {
                    hint.into()
                },
            }
        };
        Self {
            kind: FloatKind::SettingsModelAdd,
            title: format!("Add model · {provider}"),
            search: String::new(),
            sections: vec![
                FloatSection {
                    title: if editing.is_some() {
                        "Type value in search · Enter save field · Esc cancel edit".into()
                    } else {
                        "id required · name/ctx optional · base_url/api on provider".into()
                    },
                    items: vec![
                        mark("id", "id *", &draft.id, "required"),
                        mark("name", "name", &draft.name, "optional"),
                        mark(
                            "context_window",
                            "context_window",
                            &draft.context_window,
                            "optional",
                        ),
                    ],
                },
                FloatSection {
                    title: "Actions".into(),
                    items: vec![
                        item("save", "Save model", "write to models.json", "enter"),
                        item("cancel", "Cancel", "back to model list", "esc"),
                    ],
                },
            ],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    /// Model picker: one section per provider.
    pub fn model_picker(catalog: &[ModelChoice], current: Option<(&str, &str)>) -> Self {
        let mut order: Vec<String> = Vec::new();
        for m in catalog {
            if !order.iter().any(|p| p == &m.provider) {
                order.push(m.provider.clone());
            }
        }
        let mut sections = Vec::new();
        for prov in order {
            let items: Vec<FloatItem> = catalog
                .iter()
                .filter(|m| m.provider == prov)
                .map(|m| {
                    let is_current = current
                        .map(|(p, id)| p == m.provider && id == m.id)
                        .unwrap_or(false);
                    let label = if is_current {
                        format!("{} (current)", m.id)
                    } else {
                        m.id.clone()
                    };
                    let detail = if m.name != m.id {
                        m.name.clone()
                    } else {
                        String::new()
                    };
                    FloatItem {
                        id: format!("{}:{}", m.provider, m.id),
                        label,
                        detail,
                        hint: m.provider.clone(),
                    }
                })
                .collect();
            if !items.is_empty() {
                sections.push(FloatSection { title: prov, items });
            }
        }
        Self {
            kind: FloatKind::Models,
            title: "Models".into(),
            search: String::new(),
            sections,
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
        }
    }

    /// Flatten filtered selectable items.
    pub fn filtered_entries(&self) -> Vec<FloatEntry> {
        // In edit mode search holds the value being typed — never filter the list.
        let q = if self.edit_mode {
            String::new()
        } else {
            self.search.trim().to_ascii_lowercase()
        };
        let mut out = Vec::new();
        for sec in &self.sections {
            for item in &sec.items {
                if q.is_empty()
                    || item.label.to_ascii_lowercase().contains(&q)
                    || item.detail.to_ascii_lowercase().contains(&q)
                    || item.id.to_ascii_lowercase().contains(&q)
                    || sec.title.to_ascii_lowercase().contains(&q)
                    || item.hint.to_ascii_lowercase().contains(&q)
                {
                    out.push(FloatEntry {
                        section_title: sec.title.clone(),
                        item: item.clone(),
                    });
                }
            }
        }
        out
    }

    /// Enter in-float value edit (search bar becomes the editor).
    pub fn begin_edit(&mut self, label: impl Into<String>, initial: impl Into<String>) {
        self.edit_mode = true;
        self.edit_label = label.into();
        self.search = initial.into();
    }

    pub fn end_edit(&mut self) {
        self.edit_mode = false;
        self.edit_label.clear();
        self.search.clear();
    }

    /// Rows for rendering: headers injected when section changes.
    pub fn render_rows(&self) -> Vec<FloatRenderRow> {
        let entries = self.filtered_entries();
        let mut rows = Vec::new();
        let mut last_section = String::new();
        for (i, e) in entries.iter().enumerate() {
            if e.section_title != last_section {
                rows.push(FloatRenderRow::Header(e.section_title.clone()));
                last_section = e.section_title.clone();
            }
            rows.push(FloatRenderRow::Item {
                entry_index: i,
                label: e.item.label.clone(),
                detail: e.item.detail.clone(),
                hint: e.item.hint.clone(),
            });
        }
        rows
    }

    pub fn clamp_selected(&mut self) {
        let n = self.filtered_entries().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let n = self.filtered_entries().len();
        if n == 0 {
            return;
        }
        if delta < 0 {
            self.selected = self.selected.saturating_sub((-delta) as usize);
        } else {
            self.selected = (self.selected + delta as usize).min(n - 1);
        }
    }

    pub fn selected_entry(&self) -> Option<FloatEntry> {
        let entries = self.filtered_entries();
        entries.get(self.selected).cloned()
    }

    pub fn push_search(&mut self, ch: char) {
        if !ch.is_control() {
            self.search.push(ch);
            self.selected = 0;
            self.clamp_selected();
        }
    }

    pub fn pop_search(&mut self) {
        self.search.pop();
        self.selected = 0;
        self.clamp_selected();
    }
}

#[derive(Debug, Clone)]
pub enum FloatRenderRow {
    Header(String),
    Item {
        /// Index into filtered_entries (for selection highlight).
        entry_index: usize,
        label: String,
        detail: String,
        hint: String,
    },
}

fn default_command_sections() -> Vec<FloatSection> {
    // Mirrors every interactive slash command — all secondary UIs go through the float.
    vec![
        FloatSection {
            title: "Session".into(),
            items: vec![
                item(
                    "session",
                    "Session Info",
                    "path · name · messages",
                    "/session",
                ),
                item(
                    "resume",
                    "Resume Session",
                    "list / open past sessions",
                    "/resume",
                ),
                item("new", "New Session", "start a fresh session", "/new"),
                item("name", "Name Session", "set display name", "/name "),
                item("tree", "Session Tree", "list or switch branch", "/tree"),
                item(
                    "rewind",
                    "Rewind",
                    "edit a previous prompt · Esc Esc",
                    "/rewind",
                ),
                item("export", "Export Session", "write HTML export", "/export"),
                item("clear", "Clear Chat", "clear on-screen history", "/clear"),
            ],
        },
        FloatSection {
            title: "Model & Context".into(),
            items: vec![
                item("model", "Switch Model", "select above input", "Ctrl+L"),
                item(
                    "settings",
                    "Settings",
                    "thinking · providers · models",
                    "Ctrl+G",
                ),
                item(
                    "thinking",
                    "Thinking Level",
                    "off/low/medium/high",
                    "/thinking",
                ),
                item("plan", "Plan Mode", "explore + write plan only", "/plan"),
                item("act", "Act / Build", "approve plan and implement", "/act"),
                item(
                    "compact",
                    "Compact Context",
                    "summarize older turns",
                    "/compact",
                ),
                item(
                    "skill",
                    "Force-load Skill",
                    "optional; agent auto-loads via read",
                    "/skill:",
                ),
                item(
                    "reload",
                    "Reload",
                    "extensions · skills · prompts",
                    "/reload",
                ),
            ],
        },
        FloatSection {
            title: "Other".into(),
            items: vec![
                item("help", "Help", "list slash commands", "/help"),
                item("quit", "Quit", "exit interactive mode", "/quit"),
                item("exit", "Exit", "exit interactive mode", "/exit"),
            ],
        },
    ]
}

fn item(id: &str, label: &str, detail: &str, hint: &str) -> FloatItem {
    FloatItem {
        id: id.into(),
        label: label.into(),
        detail: detail.into(),
        hint: hint.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_picker_groups_providers() {
        let cat = vec![
            ModelChoice {
                provider: "openai".into(),
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
            },
            ModelChoice {
                provider: "opencode".into(),
                id: "deepseek-v4-flash".into(),
                name: "ds".into(),
            },
        ];
        let menu = FloatMenu::model_picker(&cat, Some(("openai", "gpt-4o")));
        assert_eq!(menu.sections.len(), 2);
        assert!(menu.sections[0].items[0].label.contains("current"));
        let rows = menu.render_rows();
        assert!(matches!(rows[0], FloatRenderRow::Header(_)));
    }

    #[test]
    fn search_filters() {
        let mut menu = FloatMenu::commands_palette();
        menu.search = "quit".into();
        let e = menu.filtered_entries();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].item.id, "quit");
    }

    #[test]
    fn help_and_thinking_kinds() {
        let h = FloatMenu::help_menu();
        assert_eq!(h.kind, FloatKind::Help);
        assert!(!h.filtered_entries().is_empty());
        let t = FloatMenu::thinking_picker("medium");
        assert_eq!(t.kind, FloatKind::Thinking);
        assert_eq!(t.selected, 2);
    }

    #[test]
    fn sessions_picker_builds() {
        let rows = vec![(
            "/tmp/a.jsonl".into(),
            "demo".into(),
            "today".into(),
            "abc".into(),
        )];
        let m = FloatMenu::sessions_picker(&rows);
        assert_eq!(m.kind, FloatKind::Sessions);
        assert_eq!(m.filtered_entries().len(), 1);
    }
}
