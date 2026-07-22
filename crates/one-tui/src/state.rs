//! Shared public types for the interactive TUI.
//!
//! Extracted from `app.rs` so UI, float menus, and `one-cli` can depend on
//! stable data structures without pulling the full `App` implementation.

use std::path::PathBuf;
use std::time::Instant;

use unicode_width::UnicodeWidthChar;

use crate::message::AlertLevel;

/// Map a display column (terminal cells from content left edge) to a caret
/// index into `s` (0..=char_count). Half-open selection uses this caret.
pub fn display_col_to_caret(s: &str, display_col: usize) -> usize {
    let mut w = 0usize;
    for (i, ch) in s.chars().enumerate() {
        if w >= display_col {
            return i;
        }
        w = w.saturating_add(ch.width().unwrap_or(0).max(1));
    }
    s.chars().count()
}

/// Empty-session sample prompts — keys `1`–`3` run these when input is empty.
pub const WELCOME_TRY_PROMPTS: &[&str] = &[
    "list files in this directory",
    "explain how the agent loop works",
    "fix the failing tests",
];

/// How long a toast stays visible unless replaced.
pub(crate) const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(4);

/// One caret in the chat transcript (character-level selection).
///
/// `col` is a **caret** into `chat_line_text[line]` (0..=char_len): selection is
/// the half-open range between ordered anchor and end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectPos {
    pub line: usize,
    pub col: usize,
}

impl SelectPos {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }

    /// Document order: earlier line, then earlier caret.
    pub fn cmp_doc(self, other: Self) -> std::cmp::Ordering {
        self.line
            .cmp(&other.line)
            .then_with(|| self.col.cmp(&other.col))
    }
}

/// Image attachment bound to a prompt placeholder token (`[图片.img]` / `[图片.N.img]`).
///
/// Stored as a **local file path** (Codex-style); base64 is only produced when
/// the provider builds the API request.
///
/// Deleting the token from the input detaches the image (uniform with normal editing).
///
/// While `loading` is true, the chip is already visible but the media file is
/// still being captured (clipboard / PowerShell) — do not submit yet.
#[derive(Debug, Clone)]
pub struct PendingImage {
    pub id: u32,
    pub mime_type: String,
    /// Absolute path under `~/.one/agent/media` or an imported image file.
    /// Empty while `loading`.
    pub path: PathBuf,
    pub name: String,
    /// Clipboard / import still in flight (optimistic chip already shown).
    pub loading: bool,
}

impl PendingImage {
    pub fn token(&self) -> String {
        one_core::image::image_token(self.id)
    }

    pub fn label(&self) -> String {
        if self.loading {
            "[image · … · loading]".to_string()
        } else {
            one_core::image::image_label_path(&self.mime_type, &self.path)
        }
    }
}

/// Long pasted text bound to `[文本.txt]` / `[文本.N.txt]` (same atomic delete UX as images).
#[derive(Debug, Clone)]
pub struct PendingText {
    pub id: u32,
    pub body: String,
}

impl PendingText {
    pub fn token(&self) -> String {
        one_core::image::text_token(self.id)
    }

    pub fn summary(&self) -> String {
        one_core::image::text_blob_summary(&self.body)
    }
}

#[derive(Debug, Clone)]
pub enum RunOutcome {
    Prompt(String),
    FollowUp(String),
    Steer(String),
    /// Cycle agent mode Plan ↔ Build (Shift+Tab / BackTab).
    CycleAgentMode,
    /// Esc Esc on empty input — CLI should open the rewind menu.
    OpenRewind,
    /// Model select confirmed — CLI switches provider/model.
    SwitchModel {
        provider: String,
        model: Option<String>,
    },
    /// Settings UI mutation (providers / models.json).
    ConfigOp(ConfigOp),
    /// Open MCP manager; CLI refreshes live status first.
    OpenMcpPanel,
    /// Open MCP import picker; CLI scans foreign agents first.
    OpenMcpImportPanel,
    Quit,
    Noop,
}

impl RunOutcome {
    pub fn is_actionable(&self) -> bool {
        match self {
            RunOutcome::Noop => false,
            // Empty text is ok for Prompt when images were staged (image-only turn).
            RunOutcome::Prompt(_) => true,
            RunOutcome::FollowUp(t) | RunOutcome::Steer(t) => !t.is_empty(),
            RunOutcome::CycleAgentMode
            | RunOutcome::OpenRewind
            | RunOutcome::OpenMcpPanel
            | RunOutcome::OpenMcpImportPanel
            | RunOutcome::SwitchModel { .. }
            | RunOutcome::ConfigOp(_)
            | RunOutcome::Quit => true,
        }
    }
}

/// Mutations emitted by the Settings center float + field-edit select.
#[derive(Debug, Clone)]
pub enum ConfigOp {
    ProviderAdd {
        id: String,
        base_url: Option<String>,
    },
    ProviderSet {
        id: String,
        key: String,
        value: String,
    },
    ProviderRm {
        id: String,
    },
    /// Fetch OpenAI-compatible remote models for this provider.
    ProviderFetchModels {
        id: String,
    },
    /// Add/upsert model. Connection (`base_url` / `api`) is provider-level only.
    ModelAdd {
        /// `provider:id`
        spec: String,
        name: Option<String>,
        context_window: Option<u32>,
    },
    ModelSet {
        spec: String,
        key: String,
        value: String,
    },
    ModelRm {
        spec: String,
    },
    /// Persist a settings.json key (thinking, sandbox, …).
    SettingSet {
        key: String,
        value: String,
    },
    /// Toggle skill enabled flag (path to SKILL.md).
    SkillToggle {
        path: String,
    },
    /// Toggle MCP server enabled (server name).
    McpToggle {
        name: String,
    },
    /// Import foreign MCP server(s) into One.
    ///
    /// Empty `names` = import all not already owned.
    McpImport {
        names: Vec<String>,
        /// Replace existing One entries with the same name.
        force: bool,
    },
    /// Toggle a settings feature flag (e.g. `subagent`).
    FeatureToggle {
        id: String,
    },
}

/// In-float draft when adding a model (stays inside Settings).
/// Connection (`base_url` / `api`) is provider-level only.
#[derive(Debug, Clone, Default)]
pub struct ModelDraft {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: String,
}

impl ModelDraft {
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            ..Default::default()
        }
    }

    pub fn field(&self, key: &str) -> &str {
        match key {
            "id" => &self.id,
            "name" => &self.name,
            "context_window" | "ctx" => &self.context_window,
            _ => "",
        }
    }

    pub fn set_field(&mut self, key: &str, value: String) {
        match key {
            "id" => self.id = value,
            "name" => self.name = value,
            "context_window" | "ctx" => self.context_window = value,
            _ => {}
        }
    }

    /// Build ConfigOp if id is non-empty.
    pub fn to_config_op(&self) -> Result<ConfigOp, String> {
        let id = self.id.trim();
        if id.is_empty() {
            return Err("model id is required".into());
        }
        if id.chars().any(|c| c.is_whitespace()) {
            return Err("model id must not contain whitespace".into());
        }
        let name = {
            let n = self.name.trim();
            if n.is_empty() {
                None
            } else {
                Some(n.to_string())
            }
        };
        let context_window = {
            let s = self.context_window.trim();
            if s.is_empty() {
                None
            } else {
                Some(
                    s.parse::<u32>()
                        .map_err(|_| format!("context_window must be a number, got `{s}`"))?,
                )
            }
        };
        Ok(ConfigOp::ModelAdd {
            spec: format!("{}:{id}", self.provider),
            name,
            context_window,
        })
    }
}

/// Pending tool-approval prompt (interactive permission gate).
#[derive(Debug, Clone)]
pub struct ApprovalPrompt {
    pub id: u64,
    pub tool: String,
    pub summary: String,
    pub reason: String,
}

/// Why a [`crate::select::SelectPrompt`] is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectKind {
    /// Tool permission gate (`PermissionGate`).
    Approval { id: u64 },
    /// Agent `ask_user` question (sequential multi-question uses one at a time).
    AskUser { id: u64 },
    /// Model switcher docked above the input (Ctrl+L).
    Model,
}

/// User choice for an approval prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalAnswer {
    /// Session-wide auto-approve for remaining process.
    Always,
    /// Allow this single call.
    Once,
    /// Allow matching fingerprint for the rest of the process.
    Session,
    /// Deny; optional free-text feedback is sent back to the model.
    Deny { feedback: Option<String> },
}

/// Ephemeral top-right notice — never agent context.
#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub level: AlertLevel,
    pub created: Instant,
}

impl Toast {
    pub fn alive(&self) -> bool {
        self.created.elapsed() < TOAST_TTL
    }
}
