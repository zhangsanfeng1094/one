//! Floating modal menu — centered overlay for secondary UIs
//! (command palette, model picker, future settings, …).
//!
//! Visual language matches a typical agent TUI command palette:
//! title · search · section headers · items with right-aligned hints · footer.

use crate::slash::ModelChoice;

/// Visual role of a float row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FloatItemStyle {
    /// Ordinary list row (settings fields, servers, models, …).
    #[default]
    Normal,
    /// Primary action (import, fetch models, …) — accent label + chip.
    Action,
}

/// One selectable row inside a section.
#[derive(Debug, Clone)]
pub struct FloatItem {
    pub id: String,
    pub label: String,
    /// Secondary text (description).
    pub detail: String,
    /// Right-aligned hint (shortcut / path).
    pub hint: String,
    pub style: FloatItemStyle,
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
    /// Provider/model thinkingFormat picker.
    SettingsThinkingFormat,
    /// Provider/model maxTokensField picker.
    SettingsMaxTokensField,
    /// Remote model ids fetched from the provider.
    SettingsRemoteModels,
    /// Model list under Settings.
    SettingsModels,
    /// Actions for one model.
    SettingsModelDetail,
    /// Add-model form under a provider (stays in Settings float).
    SettingsModelAdd,
    /// Skills manager — list + enable/disable (Codex-style).
    Skills,
    /// MCP servers — status + enable/disable.
    Mcp,
    /// Import MCP servers from Claude / Codex / Cursor.
    McpImport,
    /// Subscription / OAuth login provider picker (`/login`).
    Login,
    /// Logout provider picker (`/logout`).
    Logout,
    /// Generic custom list.
    Custom,
}

/// Centered floating menu state.
#[derive(Debug, Clone)]
pub struct FloatMenu {
    pub kind: FloatKind,
    pub title: String,
    pub search: String,
    /// Char-index caret into `search` (0..=search.chars().count()).
    pub search_cursor: usize,
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
            search_cursor: 0,
        }
    }

    pub fn commands_palette() -> Self {
        Self::with_sections(FloatKind::Commands, "Commands", default_command_sections())
    }

    /// `/help` — browse commands; confirm drills into secondary UI or runs.
    pub fn help_menu() -> Self {
        Self::with_sections(FloatKind::Help, "Help", default_command_sections())
    }

    /// `/login` — pick Codex / OpenCode Zen / Go (etc.) inside the TUI.
    ///
    /// Each row: `(id, label, detail, logged_in)`.
    pub fn login_picker(rows: &[(String, String, String, bool)]) -> Self {
        let items = if rows.is_empty() {
            vec![item(
                "_empty",
                "(no providers)",
                "catalog empty",
                "",
            )]
        } else {
            rows.iter()
                .map(|(id, label, detail, logged_in)| {
                    let hint = if *logged_in { "signed in" } else { "sign in" };
                    FloatItem {
                        id: id.clone(),
                        label: label.clone(),
                        detail: detail.clone(),
                        hint: hint.into(),
                        style: if *logged_in {
                            FloatItemStyle::Normal
                        } else {
                            FloatItemStyle::Action
                        },
                    }
                })
                .collect()
        };
        Self::with_sections(
            FloatKind::Login,
            "Login · select provider",
            vec![FloatSection {
                title: "Subscription / OAuth".into(),
                items,
            }],
        )
    }

    /// `/logout` — pick a stored credential (or all).
    ///
    /// Each row: `(id, label, detail)`.
    pub fn logout_picker(rows: &[(String, String, String)]) -> Self {
        let mut items: Vec<FloatItem> = rows
            .iter()
            .map(|(id, label, detail)| FloatItem {
                id: id.clone(),
                label: label.clone(),
                detail: detail.clone(),
                hint: "logout".into(),
                style: FloatItemStyle::Normal,
            })
            .collect();
        if items.is_empty() {
            items.push(item("_empty", "(no credentials)", "nothing stored", ""));
        } else {
            items.push(action_item(
                "all",
                "Log out all",
                "clear every entry in auth.json",
                "all",
            ));
        }
        Self::with_sections(
            FloatKind::Logout,
            "Logout · select provider",
            vec![FloatSection {
                title: "Stored credentials".into(),
                items,
            }],
        )
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
                    style: FloatItemStyle::Normal,
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
            search_cursor: 0,
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
                style: FloatItemStyle::Normal,
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
            search_cursor: 0,
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
                style: FloatItemStyle::Normal,
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
            search_cursor: 0,
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
                style: FloatItemStyle::Normal,
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
            search_cursor: 0,
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
                style: FloatItemStyle::Normal,
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
            search_cursor: 0,
        }
    }

    /// Settings root — separate from `/` command palette.
    ///
    /// Hierarchy: Settings → Providers → Provider → Models → Model
    /// (no standalone global Models entry).
    pub fn settings_root(thinking: &str, provider: &str, model: &str) -> Self {
        Self::settings_root_with_mcp(thinking, provider, model, "open /mcp")
    }

    /// Settings root with live MCP status line.
    pub fn settings_root_with_mcp(
        thinking: &str,
        provider: &str,
        model: &str,
        mcp_summary: &str,
    ) -> Self {
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
                        item(
                            "skills",
                            "Skills",
                            "enable / disable discovered skills",
                            "/skills",
                        ),
                        item(
                            "mcp",
                            "MCP",
                            mcp_summary,
                            "/mcp",
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
            search_cursor: 0,
        }
    }

    /// MCP manager: rows are `(name, label, detail, enabled)`.
    ///
    /// Keep labels coarse — no connection URLs, transports, or raw errors.
    pub fn mcp_manager(rows: &[(String, String, String, bool)]) -> Self {
        let mut on_items = Vec::new();
        let mut off_items = Vec::new();
        for (id, label, detail, on) in rows {
            let item = FloatItem {
                id: id.clone(),
                label: label.clone(),
                detail: detail.clone(),
                hint: if *on {
                    "on".into()
                } else {
                    "off".into()
                },
                style: FloatItemStyle::Normal,
            };
            if *on {
                on_items.push(item);
            } else {
                off_items.push(item);
            }
        }
        let on_n = on_items.len();
        let off_n = off_items.len();
        let mut sections = Vec::new();
        sections.push(FloatSection {
            title: "Actions".into(),
            items: vec![
                action_item(
                    "_import",
                    "Import from other agents",
                    "Claude · Codex · Cursor · project .mcp.json → One",
                    "import",
                ),
                action_item(
                    "_import_all",
                    "Import all available",
                    "copy every foreign server not already in One",
                    "all",
                ),
            ],
        });
        if on_n > 0 {
            sections.push(FloatSection {
                title: format!("On ({on_n})"),
                items: on_items,
            });
        }
        if off_n > 0 {
            sections.push(FloatSection {
                title: format!("Off ({off_n})"),
                items: off_items,
            });
        }
        if on_n == 0 && off_n == 0 {
            sections.push(FloatSection {
                title: "Servers".into(),
                items: vec![item(
                    "_empty",
                    "(none in One)",
                    "Import above, or `one mcp add …`",
                    "",
                )],
            });
        }
        Self {
            kind: FloatKind::Mcp,
            title: "MCP".into(),
            search: String::new(),
            sections,
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
            search_cursor: 0,
        }
    }

    /// Import picker: rows are `(name, label, detail, already_owned)`.
    pub fn mcp_import(rows: &[(String, String, String, bool)]) -> Self {
        let mut items = Vec::new();
        for (id, label, detail, owned) in rows {
            items.push(FloatItem {
                id: id.clone(),
                label: label.clone(),
                detail: detail.clone(),
                hint: if *owned { "owned".into() } else { "import".into() },
                style: if *owned {
                    FloatItemStyle::Normal
                } else {
                    FloatItemStyle::Action
                },
            });
        }
        if items.is_empty() {
            items.push(item(
                "_empty",
                "(nothing found)",
                "No Claude / Codex / Cursor MCP configs detected",
                "",
            ));
        }
        Self {
            kind: FloatKind::McpImport,
            title: "Import MCP".into(),
            search: String::new(),
            sections: vec![FloatSection {
                title: format!("Candidates ({})", rows.len()),
                items,
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
            search_cursor: 0,
        }
    }

    /// Skills manager: rows are `(path, label, detail, enabled)`.
    pub fn skills_manager(rows: &[(String, String, String, bool)]) -> Self {
        let mut enabled_items = Vec::new();
        let mut disabled_items = Vec::new();
        for (path, label, detail, on) in rows {
            let item = FloatItem {
                id: path.clone(),
                label: label.clone(),
                detail: detail.clone(),
                hint: if *on {
                    "on · Enter off".into()
                } else {
                    "off · Enter on".into()
                },
                style: FloatItemStyle::Normal,
};
            if *on {
                enabled_items.push(item);
            } else {
                disabled_items.push(item);
            }
        }
        let mut sections = Vec::new();
        if !enabled_items.is_empty() {
            sections.push(FloatSection {
                title: format!("Enabled ({})", enabled_items.len()),
                items: enabled_items,
            });
        }
        if !disabled_items.is_empty() {
            sections.push(FloatSection {
                title: format!("Disabled ({})", disabled_items.len()),
                items: disabled_items,
            });
        }
        if sections.is_empty() {
            sections.push(FloatSection {
                title: "Skills".into(),
                items: vec![item(
                    "_empty",
                    "(no skills discovered)",
                    "add SKILL.md under .agents/skills or ~/.one/agent/skills",
                    "",
                )],
            });
        }
        Self {
            kind: FloatKind::Skills,
            title: "Skills".into(),
            search: String::new(),
            sections,
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
            search_cursor: 0,
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
                style: FloatItemStyle::Normal,
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
            search_cursor: 0,
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
        // `provider_type` and `api` are the same fixed protocol field (mirrored).
        let provider_type = {
            let t = field_value("provider_type", "default/unset");
            if t == "default/unset" {
                field_value("api", "default/unset")
            } else {
                t
            }
        };
        let base_url = field_value("base_url", "unset");
        let api_key = field_value("api_key", "unset");
        let default_model = field_value("default_model", "unset");
        let thinking_format = field_value("thinking_format", "auto");
        let max_tokens_field = field_value("max_tokens_field", "auto");
        let compat_summary = field_value("compat", "auto (detect)");
        // Models is a top-level nav row (no section chrome); Connection / Danger
        // are section headers — three-column key · value · [action] in the list.
        let models_detail = if detail.trim().is_empty() {
            "list · add · edit".to_string()
        } else {
            detail.to_string()
        };

        let mut compat_items = vec![
            item(
                "set_thinking_format",
                "thinkingFormat",
                &thinking_format,
                "select",
            ),
            item(
                "set_max_tokens_field",
                "maxTokensField",
                &max_tokens_field,
                "select",
            ),
        ];
        // Common Pi compat bools — Enter cycles auto → true → false.
        for (label, key) in [
            ("supportsDeveloperRole", "supports_developer_role"),
            ("supportsReasoningEffort", "supports_reasoning_effort"),
            ("supportsUsageInStreaming", "supports_usage_in_streaming"),
            ("supportsStrictMode", "supports_strict_mode"),
            ("requiresToolResultName", "requires_tool_result_name"),
            (
                "requiresAssistantAfterToolResult",
                "requires_assistant_after_tool_result",
            ),
            ("requiresThinkingAsText", "requires_thinking_as_text"),
            (
                "requiresReasoningContent",
                "requires_reasoning_content_on_assistant_messages",
            ),
            ("forceAdaptiveThinking", "force_adaptive_thinking"),
            ("allowEmptySignature", "allow_empty_signature"),
        ] {
            let display = fields
                .iter()
                .find(|(k, _)| k == &format!("compat.{label}") || k.ends_with(&format!(":{key}")) || k.ends_with(&format!("compat.{label}")))
                .map(|(_, v)| v.clone())
                .or_else(|| {
                    // provider_field_rows uses `id:compat.Label`
                    fields
                        .iter()
                        .find(|(k, _)| k.contains(&format!("compat.{label}")))
                        .map(|(_, v)| v.clone())
                })
                .unwrap_or_else(|| "auto".into());
            compat_items.push(item(
                &format!("cycle_compat:{key}"),
                label,
                &display,
                "cycle",
            ));
        }
        compat_items.push(item(
            "clear_compat",
            "Clear compat overrides",
            &compat_summary,
            "reset",
        ));

        Self {
            kind: FloatKind::SettingsProviderDetail,
            title: format!("Provider · {id}"),
            search: String::new(),
            sections: vec![
                FloatSection {
                    title: String::new(),
                    items: vec![
                        item("models", "Models", &models_detail, "→"),
                        action_item(
                            "fetch_models",
                            "Fetch & import remote models",
                            "GET /models → batch add · Ctrl+F",
                            "fetch",
                        ),
                    ],
                },
                FloatSection {
                    title: "Connection".into(),
                    items: vec![
                        item("set_provider_type", "protocol", &provider_type, "select"),
                        item("set_base_url", "base_url", &base_url, "edit"),
                        item("set_api_key", "api_key", &api_key, "edit"),
                        item("set_default_model", "default_model", &default_model, "edit"),
                    ],
                },
                FloatSection {
                    title: "Compat (Pi)".into(),
                    items: compat_items,
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
            search_cursor: 0,
        }
    }

    /// Thinking format picker (provider or model scope — caller handles confirm).
    pub fn settings_thinking_format(scope: &str) -> Self {
        Self {
            kind: FloatKind::SettingsThinkingFormat,
            title: format!("thinkingFormat · {scope}"),
            search: String::new(),
            sections: vec![FloatSection {
                title: "How reasoning/thinking is encoded on the wire".into(),
                items: vec![
                    item("tf:auto", "auto", "detect from provider / baseUrl", ""),
                    item("tf:openai", "openai", "reasoning_effort", ""),
                    item("tf:openrouter", "openrouter", "reasoning: { effort }", ""),
                    item("tf:deepseek", "deepseek", "thinking: { type } + effort", ""),
                    item("tf:together", "together", "reasoning: { enabled }", ""),
                    item("tf:zai", "zai", "thinking enabled/disabled", ""),
                    item("tf:qwen", "qwen", "enable_thinking bool", ""),
                    item(
                        "tf:qwen-chat-template",
                        "qwen-chat-template",
                        "chat_template_kwargs",
                        "",
                    ),
                    item("tf:chat-template", "chat-template", "custom kwargs", ""),
                    item("tf:string-thinking", "string-thinking", "thinking: string", ""),
                    item("tf:ant-ling", "ant-ling", "reasoning: { effort }", ""),
                ],
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
            search_cursor: 0,
        }
    }

    pub fn settings_max_tokens_field(scope: &str) -> Self {
        Self {
            kind: FloatKind::SettingsMaxTokensField,
            title: format!("maxTokensField · {scope}"),
            search: String::new(),
            sections: vec![FloatSection {
                title: "Field name for max output tokens".into(),
                items: vec![
                    item("mt:auto", "auto", "detect from provider / URL", ""),
                    item(
                        "mt:max_completion_tokens",
                        "max_completion_tokens",
                        "OpenAI modern",
                        "",
                    ),
                    item("mt:max_tokens", "max_tokens", "legacy / local proxies", ""),
                ],
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
            search_cursor: 0,
        }
    }

    /// Fixed protocol picker — drives LLM request/response codecs.
    pub fn settings_provider_api(provider: &str) -> Self {
        Self {
            kind: FloatKind::SettingsProviderApi,
            title: format!("Protocol · {provider}"),
            search: String::new(),
            sections: vec![FloatSection {
                title: "Wire protocol (select, not free-text)".into(),
                items: vec![
                    item(
                        "api:",
                        "default/unset",
                        "inherit built-in default for this provider",
                        "",
                    ),
                    item(
                        "api:openai-completions",
                        "openai-completions",
                        "OpenAI Chat Completions · widest compatible",
                        "",
                    ),
                    item(
                        "api:openai-responses",
                        "openai-responses",
                        "OpenAI Responses API · first-party OpenAI",
                        "",
                    ),
                    item(
                        "api:anthropic-messages",
                        "anthropic-messages",
                        "Anthropic Messages API · Claude",
                        "",
                    ),
                    item(
                        "api:gemini-generate-content",
                        "gemini-generate-content",
                        "Gemini native generateContent / streamGenerateContent",
                        "",
                    ),
                ],
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
            search_cursor: 0,
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
                style: FloatItemStyle::Normal,
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
            search_cursor: 0,
        }
    }

    /// Models for **one** provider (second level under Provider detail).
    pub fn settings_models_for_provider(provider: &str, rows: &[(String, String)]) -> Self {
        let prefix = format!("{provider}:");
        let mut items: Vec<FloatItem> = vec![action_item(
            "fetch_models",
            "Fetch & import remote models",
            "GET /models → batch write models.json",
            "fetch",
        )];
        let model_items: Vec<FloatItem> = rows
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
                    style: FloatItemStyle::Normal,
}
            })
            .collect();
        let n = model_items.len();
        items.extend(model_items);
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
                title: format!("{n} · Ctrl+F import all · Esc/← back"),
                items,
            }],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
            search_cursor: 0,
        }
    }

    pub fn settings_model_detail(spec: &str, detail: &str) -> Self {
        let short = spec.split_once(':').map(|(_, id)| id).unwrap_or(spec);
        // detail may be multi-line "name · ctx · reasoning=… · map=…"
        let field = |prefix: &str, default: &str| {
            detail
                .split(prefix)
                .nth(1)
                .map(|s| s.split_whitespace().next().unwrap_or(default))
                .unwrap_or(default)
                .to_string()
        };
        let reasoning = field("reasoning=", "unset");
        let tlm = {
            let raw = detail.split("map=").nth(1).map(str::trim).unwrap_or("");
            if raw.is_empty() {
                "(none)".to_string()
            } else {
                // map value may contain commas; take until next known token or end.
                raw.split(" devRole=")
                    .next()
                    .unwrap_or(raw)
                    .trim()
                    .to_string()
            }
        };
        let thinking_format = field("format=", "auto");
        let dev_role = field("devRole=", "auto");
        let effort = field("effort=", "auto");
        Self {
            kind: FloatKind::SettingsModelDetail,
            title: format!("Model · {short}"),
            search: String::new(),
            sections: vec![
                FloatSection {
                    title: detail.into(),
                    items: vec![
                        item("set_name", "name", short, "edit"),
                        item("set_ctx", "context_window", "e.g. 128000", "edit"),
                        item("set_reasoning", "reasoning", &reasoning, "cycle"),
                        item(
                            "set_thinking_level_map",
                            "thinkingLevelMap",
                            &tlm,
                            "edit",
                        ),
                        item(
                            "set_thinking_format",
                            "thinkingFormat",
                            &thinking_format,
                            "select",
                        ),
                        item(
                            "set_max_tokens_field",
                            "maxTokensField",
                            "auto / override",
                            "select",
                        ),
                    ],
                },
                FloatSection {
                    title: "Compat overrides (model > provider)".into(),
                    items: vec![
                        item(
                            "cycle_compat:supports_developer_role",
                            "supportsDeveloperRole",
                            &dev_role,
                            "cycle",
                        ),
                        item(
                            "cycle_compat:supports_reasoning_effort",
                            "supportsReasoningEffort",
                            &effort,
                            "cycle",
                        ),
                        item(
                            "cycle_compat:requires_reasoning_content_on_assistant_messages",
                            "requiresReasoningContent",
                            "auto",
                            "cycle",
                        ),
                        item(
                            "clear_compat",
                            "Clear model compat",
                            "inherit provider",
                            "reset",
                        ),
                    ],
                },
                FloatSection {
                    title: "Danger".into(),
                    items: vec![item(
                        "rm_model",
                        "Delete model",
                        "remove from models.json",
                        "rm",
                    )],
                },
            ],
            selected: 0,
            edit_mode: false,
            edit_label: String::new(),
            search_cursor: 0,
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
                style: FloatItemStyle::Normal,
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
            search_cursor: 0,
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
                        style: FloatItemStyle::Normal,
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
            search_cursor: 0,
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
        self.search_cursor = self.search.chars().count();
    }

    pub fn end_edit(&mut self) {
        self.edit_mode = false;
        self.edit_label.clear();
        self.search.clear();
        self.search_cursor = 0;
    }

    /// Rows for rendering: headers injected when section changes.
    /// Empty section titles are skipped (top-level nav rows like Models).
    pub fn render_rows(&self) -> Vec<FloatRenderRow> {
        let entries = self.filtered_entries();
        let mut rows = Vec::new();
        let mut last_section: Option<String> = None;
        for (i, e) in entries.iter().enumerate() {
            let sec = e.section_title.as_str();
            let section_changed = match &last_section {
                None => true,
                Some(prev) => prev != sec,
            };
            if section_changed {
                if !sec.is_empty() {
                    rows.push(FloatRenderRow::Header(e.section_title.clone()));
                }
                last_section = Some(e.section_title.clone());
            }
            rows.push(FloatRenderRow::Item {
                entry_index: i,
                label: e.item.label.clone(),
                detail: e.item.detail.clone(),
                hint: e.item.hint.clone(),
                style: e.item.style,
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

    fn clamp_search_cursor(&mut self) {
        let len = self.search.chars().count();
        if self.search_cursor > len {
            self.search_cursor = len;
        }
    }

    /// Byte index in `search` for the current char cursor.
    fn search_byte_at_cursor(&self) -> usize {
        self.search
            .chars()
            .take(self.search_cursor)
            .map(|c| c.len_utf8())
            .sum()
    }

    /// Left/right caret movement inside the search/edit bar.
    pub fn move_search_cursor(&mut self, delta: isize) {
        let len = self.search.chars().count() as isize;
        let next = (self.search_cursor as isize + delta).clamp(0, len);
        self.search_cursor = next as usize;
    }

    pub fn search_cursor_home(&mut self) {
        self.search_cursor = 0;
    }

    pub fn search_cursor_end(&mut self) {
        self.search_cursor = self.search.chars().count();
    }

    /// Split `search` at the caret for rendering: (before, after).
    pub fn search_split_at_cursor(&self) -> (&str, &str) {
        let idx = self.search_byte_at_cursor().min(self.search.len());
        // Ensure char boundary (cursor is char-based, so this should already be).
        let idx = if self.search.is_char_boundary(idx) {
            idx
        } else {
            self.search.len()
        };
        (&self.search[..idx], &self.search[idx..])
    }

    pub fn push_search(&mut self, ch: char) {
        if ch.is_control() {
            return;
        }
        let idx = self.search_byte_at_cursor();
        self.search.insert(idx, ch);
        self.search_cursor += 1;
        if !self.edit_mode {
            self.selected = 0;
            self.clamp_selected();
        }
    }

    /// Insert pasted text at the caret (control chars dropped).
    pub fn paste_search(&mut self, text: &str) {
        let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
        if cleaned.is_empty() {
            return;
        }
        let idx = self.search_byte_at_cursor();
        let n = cleaned.chars().count();
        self.search.insert_str(idx, &cleaned);
        self.search_cursor += n;
        if !self.edit_mode {
            self.selected = 0;
            self.clamp_selected();
        }
    }

    /// Backspace: delete the character before the caret.
    pub fn pop_search(&mut self) {
        if self.search_cursor == 0 {
            return;
        }
        self.search_cursor -= 1;
        let idx = self.search_byte_at_cursor();
        if idx < self.search.len() {
            self.search.remove(idx);
        }
        if !self.edit_mode {
            self.selected = 0;
            self.clamp_selected();
        }
    }

    /// Delete: remove the character under/after the caret.
    pub fn delete_search_forward(&mut self) {
        if self.search_cursor >= self.search.chars().count() {
            return;
        }
        let idx = self.search_byte_at_cursor();
        if idx < self.search.len() {
            self.search.remove(idx);
        }
        self.clamp_search_cursor();
        if !self.edit_mode {
            self.selected = 0;
            self.clamp_selected();
        }
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
        style: FloatItemStyle,
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
                    "skills",
                    "Manage Skills",
                    "list · enable · disable",
                    "/skills",
                ),
                item(
                    "mcp",
                    "MCP Servers",
                    "status · enable · disable",
                    "/mcp",
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
            title: "Account".into(),
            items: vec![
                item(
                    "login",
                    "Login",
                    "Codex · OpenCode Zen / Go",
                    "/login",
                ),
                item(
                    "logout",
                    "Logout",
                    "clear stored credentials",
                    "/logout",
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
        style: FloatItemStyle::Normal,
    }
}

/// Accent row for primary actions (import, fetch, …).
fn action_item(id: &str, label: &str, detail: &str, hint: &str) -> FloatItem {
    FloatItem {
        id: id.into(),
        label: label.into(),
        detail: detail.into(),
        hint: hint.into(),
        style: FloatItemStyle::Action,
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
