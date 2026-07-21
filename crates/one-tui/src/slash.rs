//! Slash-command + model picker (grouped by provider).

/// One slash command shown in the `/` completion popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
}

/// Built-in interactive commands (keep in sync with one-cli slash handlers).
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        usage: "/help",
        description: "show available commands",
    },
    SlashCommand {
        name: "/session",
        usage: "/session",
        description: "show session path, name, message count",
    },
    SlashCommand {
        name: "/resume",
        usage: "/resume [id|name|file]",
        description: "list or open a past session",
    },
    SlashCommand {
        name: "/new",
        usage: "/new",
        description: "start a new session",
    },
    SlashCommand {
        name: "/name",
        usage: "/name <title>",
        description: "set session display name",
    },
    SlashCommand {
        name: "/model",
        usage: "/model [provider[:model]]",
        description: "switch model (bare = select above input · Ctrl+L)",
    },
    SlashCommand {
        name: "/login",
        usage: "/login [provider]",
        description: "login · bare opens TUI picker (Codex / OpenCode …)",
    },
    SlashCommand {
        name: "/logout",
        usage: "/logout [provider|all]",
        description: "logout · bare opens TUI picker of stored creds",
    },
    SlashCommand {
        name: "/settings",
        usage: "/settings [key value]",
        description: "settings panel (Ctrl+G) or set key value",
    },
    SlashCommand {
        name: "/thinking",
        usage: "/thinking [off|low|medium|high]",
        description: "set thinking level (or Ctrl+G Settings)",
    },
    SlashCommand {
        name: "/plan",
        usage: "/plan",
        description: "enter plan mode (read-only + plan file)",
    },
    SlashCommand {
        name: "/act",
        usage: "/act",
        description: "approve plan and implement (Build mode)",
    },
    SlashCommand {
        name: "/build",
        usage: "/build",
        description: "alias for /act",
    },
    SlashCommand {
        name: "/compact",
        usage: "/compact [instructions]",
        description: "manually compact context",
    },
    SlashCommand {
        name: "/skills",
        usage: "/skills [enable|disable <name>]",
        description: "manage skills · enable/disable (bare = panel)",
    },
    SlashCommand {
        name: "/agents",
        usage: "/agents [list|path <name>|inspect <name>]",
        description: "agent presets · JSON paths · tools (bare = panel)",
    },
    SlashCommand {
        name: "/mcp",
        usage: "/mcp [import|enable|disable <name>]",
        description: "MCP servers · status · enable/disable (bare = panel)",
    },
    SlashCommand {
        name: "/skill",
        usage: "/skill:name [args]",
        description: "force-load skill (else agent auto-reads when relevant)",
    },
    SlashCommand {
        name: "/tree",
        usage: "/tree [id]",
        description: "list or switch session branch",
    },
    SlashCommand {
        name: "/rewind",
        usage: "/rewind [id]",
        description: "rewind to a previous prompt (Esc Esc)",
    },
    SlashCommand {
        name: "/export",
        usage: "/export [path]",
        description: "export session to HTML",
    },
    SlashCommand {
        name: "/reload",
        usage: "/reload",
        description: "hot-reload extensions & resources",
    },
    SlashCommand {
        name: "/clear",
        usage: "/clear",
        description: "clear on-screen chat history",
    },
    SlashCommand {
        name: "/quit",
        usage: "/quit",
        description: "exit interactive mode",
    },
    SlashCommand {
        name: "/exit",
        usage: "/exit",
        description: "exit interactive mode",
    },
];

/// A model available for `/model` completion (fed from models.json / registry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub provider: String,
    pub id: String,
    pub name: String,
}

impl ModelChoice {
    pub fn spec(&self) -> String {
        format!("{}:{}", self.provider, self.id)
    }
}

/// A single row in the popup list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupRow {
    /// Non-selectable group header (provider name).
    Header(String),
    /// Selectable slash command.
    Command(&'static SlashCommand),
    /// Selectable model under a provider group.
    Model(ModelChoice),
}

impl PopupRow {
    pub fn selectable(&self) -> bool {
        !matches!(self, PopupRow::Header(_))
    }

    pub fn label(&self) -> String {
        match self {
            PopupRow::Header(p) => p.clone(),
            PopupRow::Command(c) => c.name.to_string(),
            PopupRow::Model(m) => m.id.clone(),
        }
    }

    pub fn description(&self) -> String {
        match self {
            PopupRow::Header(_) => String::new(),
            PopupRow::Command(c) => c.description.to_string(),
            PopupRow::Model(m) => {
                if m.name != m.id {
                    m.name.clone()
                } else {
                    format!("{} · {}", m.provider, m.id)
                }
            }
        }
    }
}

/// Popup mode derived from current input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupKind {
    /// Typing `/…` command name (no space yet, and not deep into /model args).
    Commands,
    /// Typing `/model …` — show models grouped by provider.
    Models,
}

/// What the input is doing for completion purposes.
pub fn popup_kind(input: &str) -> Option<PopupKind> {
    let s = input;
    if !s.starts_with('/') {
        return None;
    }
    // Exact /model or /model + args → model picker
    if s == "/model" || s.starts_with("/model ") || s.starts_with("/model\t") {
        return Some(PopupKind::Models);
    }
    // Prefix of "/model" still in command mode (e.g. "/mo")
    if "/model".starts_with(&s.to_ascii_lowercase()) || s.eq_ignore_ascii_case("/model") {
        // "/m", "/mo", "/mod" … show commands (including /model)
        // but if it's exactly "/model" handled above
    }
    // Any other slash with a space: command token done, no popup
    if s[1..].contains(' ') || s[1..].contains('\t') {
        return None;
    }
    Some(PopupKind::Commands)
}

/// Filter command catalog by typed prefix.
pub fn filter_commands(prefix: &str) -> Vec<&'static SlashCommand> {
    let q = prefix.to_ascii_lowercase();
    SLASH_COMMANDS
        .iter()
        .filter(|cmd| {
            if q == "/" {
                return true;
            }
            cmd.name.to_ascii_lowercase().starts_with(&q)
                || cmd
                    .name
                    .trim_start_matches('/')
                    .to_ascii_lowercase()
                    .starts_with(q.trim_start_matches('/'))
        })
        .collect()
}

/// Build model rows grouped by provider, filtered by the text after `/model`.
///
/// Query examples:
/// - `""` or whitespace → all models
/// - `openai` → providers/ids matching openai
/// - `openai:gpt` → models under openai matching gpt
/// - `deepseek` → any provider/id/name match
pub fn filter_models_grouped(catalog: &[ModelChoice], query: &str) -> Vec<PopupRow> {
    let q = query.trim().to_ascii_lowercase();
    let (prov_filter, model_filter) = if let Some((p, m)) = q.split_once(':') {
        (p.to_string(), m.to_string())
    } else {
        (q.clone(), q.clone())
    };

    // Preserve provider order as first-seen in catalog.
    let mut provider_order: Vec<String> = Vec::new();
    for m in catalog {
        if !provider_order.iter().any(|p| p == &m.provider) {
            provider_order.push(m.provider.clone());
        }
    }

    let mut rows = Vec::new();
    for prov in provider_order {
        let models: Vec<&ModelChoice> = catalog
            .iter()
            .filter(|m| m.provider == prov)
            .filter(|m| {
                if q.is_empty() {
                    return true;
                }
                // `provider:model` form
                if q.contains(':') {
                    let p_ok = m.provider.to_ascii_lowercase().starts_with(&prov_filter)
                        || prov_filter.is_empty();
                    let m_ok = model_filter.is_empty()
                        || m.id.to_ascii_lowercase().contains(&model_filter)
                        || m.name.to_ascii_lowercase().contains(&model_filter);
                    return p_ok && m_ok;
                }
                // free text: match provider OR id OR name
                m.provider.to_ascii_lowercase().contains(&q)
                    || m.id.to_ascii_lowercase().contains(&q)
                    || m.name.to_ascii_lowercase().contains(&q)
            })
            .collect();

        if models.is_empty() {
            continue;
        }
        rows.push(PopupRow::Header(prov.clone()));
        for m in models {
            rows.push(PopupRow::Model(m.clone()));
        }
    }
    rows
}

/// Command-mode rows (all selectable).
pub fn command_rows(prefix: &str) -> Vec<PopupRow> {
    filter_commands(prefix)
        .into_iter()
        .map(PopupRow::Command)
        .collect()
}

/// Full popup rows for current input + catalog.
pub fn popup_rows(input: &str, catalog: &[ModelChoice]) -> Vec<PopupRow> {
    match popup_kind(input) {
        Some(PopupKind::Commands) => command_rows(input),
        Some(PopupKind::Models) => {
            let query = input.strip_prefix("/model").unwrap_or("").trim_start();
            filter_models_grouped(catalog, query)
        }
        None => Vec::new(),
    }
}

/// Selectable indices only (skip headers).
pub fn selectable_indices(rows: &[PopupRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.selectable())
        .map(|(i, _)| i)
        .collect()
}

/// Text to put into the input when completing a row.
pub fn completion_for_row(row: &PopupRow) -> Option<String> {
    match row {
        PopupRow::Header(_) => None,
        PopupRow::Command(c) => {
            // Only required args (`<…>`) keep a trailing space so the user can type.
            // Optional `[…]` alone (e.g. `/settings`) must not block Enter from running.
            let needs_args = c.usage.contains('<');
            Some(if needs_args {
                format!("{} ", c.name)
            } else {
                c.name.to_string()
            })
        }
        PopupRow::Model(m) => Some(format!("/model {}", m.spec())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_catalog() -> Vec<ModelChoice> {
        vec![
            ModelChoice {
                provider: "openai".into(),
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
            },
            ModelChoice {
                provider: "openai".into(),
                id: "gpt-4o-mini".into(),
                name: "GPT-4o mini".into(),
            },
            ModelChoice {
                provider: "opencode".into(),
                id: "deepseek-v4-flash".into(),
                name: "deepseek-v4-flash".into(),
            },
        ]
    }

    #[test]
    fn kind_commands_vs_models() {
        assert_eq!(popup_kind("/"), Some(PopupKind::Commands));
        assert_eq!(popup_kind("/mo"), Some(PopupKind::Commands));
        assert_eq!(popup_kind("/model"), Some(PopupKind::Models));
        assert_eq!(popup_kind("/model "), Some(PopupKind::Models));
        assert_eq!(popup_kind("/model openai"), Some(PopupKind::Models));
        assert_eq!(popup_kind("/help x"), None);
    }

    #[test]
    fn models_grouped_by_provider() {
        let rows = filter_models_grouped(&sample_catalog(), "");
        let headers: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                PopupRow::Header(p) => Some(p.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec!["openai", "opencode"]);
        assert!(rows
            .iter()
            .any(|r| matches!(r, PopupRow::Model(m) if m.id == "gpt-4o")));
    }

    #[test]
    fn filter_by_provider_prefix() {
        let rows = filter_models_grouped(&sample_catalog(), "open");
        // both openai and opencode match "open"
        assert!(rows
            .iter()
            .any(|r| matches!(r, PopupRow::Header(p) if p == "openai")));
        assert!(rows
            .iter()
            .any(|r| matches!(r, PopupRow::Header(p) if p == "opencode")));
    }

    #[test]
    fn filter_provider_colon_model() {
        let rows = filter_models_grouped(&sample_catalog(), "openai:mini");
        let models: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                PopupRow::Model(m) => Some(m.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(models, vec!["gpt-4o-mini"]);
    }

    #[test]
    fn completion_model_line() {
        let m = ModelChoice {
            provider: "opencode".into(),
            id: "deepseek-v4-flash".into(),
            name: "ds".into(),
        };
        let row = PopupRow::Model(m);
        assert_eq!(
            completion_for_row(&row).as_deref(),
            Some("/model opencode:deepseek-v4-flash")
        );
    }

    #[test]
    fn settings_completion_has_no_trailing_space() {
        let cmd = SLASH_COMMANDS
            .iter()
            .find(|c| c.name == "/settings")
            .expect("settings command");
        let row = PopupRow::Command(cmd);
        assert_eq!(completion_for_row(&row).as_deref(), Some("/settings"));
    }

    #[test]
    fn name_completion_keeps_trailing_space_for_required_arg() {
        let cmd = SLASH_COMMANDS
            .iter()
            .find(|c| c.name == "/name")
            .expect("name command");
        let row = PopupRow::Command(cmd);
        assert_eq!(completion_for_row(&row).as_deref(), Some("/name "));
    }
}
