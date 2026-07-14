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
    /// Model picker (`/model`).
    Models,
    /// Help catalog (`/help`) — selecting a row drills into that command.
    Help,
    /// Resume session list (`/resume`).
    Sessions,
    /// Thinking level picker (`/thinking`).
    Thinking,
    /// Session branch tree (`/tree`).
    Tree,
    /// Read-only info panel (session summary, …). Enter closes.
    Info,
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
}

/// Flat view of a selectable entry (for navigation / confirm).
#[derive(Debug, Clone)]
pub struct FloatEntry {
    pub section_title: String,
    pub item: FloatItem,
}

impl FloatMenu {
    pub fn commands_palette() -> Self {
        Self {
            kind: FloatKind::Commands,
            title: "Commands".into(),
            search: String::new(),
            sections: default_command_sections(),
            selected: 0,
        }
    }

    /// `/help` — browse commands; confirm drills into secondary UI or runs.
    pub fn help_menu() -> Self {
        Self {
            kind: FloatKind::Help,
            title: "Help".into(),
            search: String::new(),
            // Reuse the same catalog as the command palette (grouped).
            sections: default_command_sections(),
            selected: 0,
        }
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
        }
    }

    /// `/resume` session list. `id` is the resume key (path or session id).
    pub fn sessions_picker(
        sessions: &[(String, String, String, String)],
    ) -> Self {
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
                sections.push(FloatSection {
                    title: prov,
                    items,
                });
            }
        }
        Self {
            kind: FloatKind::Models,
            title: "Models".into(),
            search: String::new(),
            sections,
            selected: 0,
        }
    }

    /// Flatten filtered selectable items.
    pub fn filtered_entries(&self) -> Vec<FloatEntry> {
        let q = self.search.trim().to_ascii_lowercase();
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
                item("session", "Session Info", "path · name · messages", "/session"),
                item("resume", "Resume Session", "list / open past sessions", "/resume"),
                item("new", "New Session", "start a fresh session", "/new"),
                item("name", "Name Session", "set display name", "/name "),
                item("tree", "Session Tree", "list or switch branch", "/tree"),
                item("export", "Export Session", "write HTML export", "/export"),
                item("clear", "Clear Chat", "clear on-screen history", "/clear"),
            ],
        },
        FloatSection {
            title: "Model & Context".into(),
            items: vec![
                item("model", "Switch Model", "pick model by provider", "/model"),
                item("thinking", "Thinking Level", "off/low/medium/high", "/thinking"),
                item("compact", "Compact Context", "summarize older turns", "/compact"),
                item(
                    "skill",
                    "Force-load Skill",
                    "optional; agent auto-loads via read",
                    "/skill:",
                ),
                item("reload", "Reload", "extensions · skills · prompts", "/reload"),
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
        let rows = vec![
            (
                "/tmp/a.jsonl".into(),
                "demo".into(),
                "today".into(),
                "abc".into(),
            ),
        ];
        let m = FloatMenu::sessions_picker(&rows);
        assert_eq!(m.kind, FloatKind::Sessions);
        assert_eq!(m.filtered_entries().len(), 1);
    }
}
