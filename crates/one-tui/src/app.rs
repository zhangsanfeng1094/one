//! Application state for the interactive chat TUI.

use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::float::{FloatKind, FloatMenu};
use crate::message::{
    summarize_tool_output, truncate_tool_output_for_ui, AlertLevel, Message, MessageRole,
    ToolStatus,
};
use crate::slash::{self, ModelChoice, PopupKind, PopupRow};
use crate::tool_view;

/// Empty-session sample prompts — keys `1`–`3` run these when input is empty.
pub const WELCOME_TRY_PROMPTS: &[&str] = &[
    "list files in this directory",
    "explain how the agent loop works",
    "fix the failing tests",
];

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

/// Background clipboard/image import job (placeholder chip already in the input).
struct ImagePasteJob {
    id: u32,
    report_err: bool,
    rx: std::sync::mpsc::Receiver<Result<(String, PathBuf, String), String>>,
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
    /// Add/upsert model. Connection fields (base_url / api) live on the provider.
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

/// Max interval between Esc presses for empty-input rewind (second tap).
/// Longer than a typical key-repeat delay so a deliberate double-tap is easy.
const ESC_DOUBLE_MS: u128 = 900;

/// Max interval between Ctrl+C presses for confirm-quit (second tap).
/// Same window as Esc double-tap so the muscle memory stays consistent.
const CTRL_C_DOUBLE_MS: u128 = 900;

const STATUS_IDLE: &str = "";
const STATUS_BUSY: &str = "";
/// How long toast stays visible unless replaced.
const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(4);

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

pub struct App {
    pub title: String,
    pub messages: Vec<Message>,
    pub input: String,
    /// Char index of the software caret in `input` (0 = before first char).
    ///
    /// Left/Right move this; insert/backspace/delete operate at this position.
    /// Always clamped to `input.chars().count()` after mutations.
    pub input_cursor: usize,
    pub status: String,
    pub stream_buffer: String,
    /// Streaming thinking / reasoning buffer (separate from assistant text).
    pub thinking_buffer: String,
    /// Default expand policy for **finished** thinking blocks.
    ///
    /// - `false` (default): collapse to `▸ thinking · N chars` after stream ends;
    ///   click / ↵ expands one block; Ctrl+T expands/collapses all.
    /// - `true`: finished blocks stay open showing full body.
    ///
    /// Live streaming always shows a short rolling tail regardless of this flag.
    pub show_thinking: bool,
    pub busy: bool,
    /// Lines scrolled **up from the bottom** of the transcript (0 = stick to end).
    ///
    /// Must be line-based, not message-based: a single long assistant reply can be
    /// taller than the viewport, and Ratatui `List` cannot partial-scroll one item.
    pub chat_scroll: usize,
    pub follow_bottom: bool,
    /// Last drawn chat viewport height (rows). Used for PageUp/PageDown page size.
    pub chat_view_height: usize,
    /// Last drawn transcript line count (after wrap).
    pub chat_total_lines: usize,
    /// Parallel to display lines: which `messages` index owns each transcript line
    /// (for click-to-expand). `None` = spacer / non-interactive.
    pub chat_line_owners: Vec<Option<usize>>,
    /// Top of chat viewport in the full line list (updated each draw).
    pub chat_view_start: usize,
    /// Blank rows painted above short transcripts (bottom-pin). Clicks skip these.
    pub chat_top_pad: usize,
    /// Terminal mouse capture is armed (wheel → chat).
    pub mouse_capture: bool,
    /// In-app transcript selection (absolute display-line indices, inclusive).
    /// App-owned select + OSC 52 copy — does not need native terminal drag-select.
    pub select_anchor: Option<usize>,
    pub select_end: Option<usize>,
    /// True after mouse moved while button down (distinguishes click vs drag).
    pub select_dragging: bool,
    /// Plain text for each display line (parallel to `chat_line_owners`), for copy.
    pub chat_line_text: Vec<String>,
    /// Pending clipboard payload set by UI; terminal session writes OSC 52.
    pub clipboard_pending: Option<String>,
    pub cursor_on: bool,
    /// Compact model label for turn footers (usually just the model id).
    pub mode_label: String,
    /// Agent / mode name shown in turn footer & prompt meta (OpenCode: "Build").
    pub agent_label: String,
    /// Spinner frame index while busy.
    pub spinner_frame: usize,
    /// Selected **row** index in the popup (may point at a header — navigation skips those).
    pub slash_selected: usize,
    /// Models from registry / models.json for `/model` picker.
    pub model_catalog: Vec<ModelChoice>,
    /// Provider rows for Settings → Providers (`id`, detail).
    pub settings_provider_rows: Vec<(String, String)>,
    /// Provider field rows for Settings → Provider detail (`provider:key`, display value).
    pub settings_provider_field_rows: Vec<(String, String)>,
    /// Model rows for Settings → Models (`provider:id`, detail).
    pub settings_model_rows: Vec<(String, String)>,
    /// Skills manager rows: `(path, label, detail, enabled)`.
    pub skills_rows: Vec<(String, String, String, bool)>,
    /// MCP manager rows: `(name, label, detail, enabled)`.
    pub mcp_rows: Vec<(String, String, String, bool)>,
    /// MCP import candidates: `(name, label, detail, already_owned)`.
    pub mcp_import_rows: Vec<(String, String, String, bool)>,
    /// Short MCP summary for Settings root.
    pub mcp_summary: String,
    /// Status-bar / prompt-meta chip, e.g. `MCP 4/5…`. Empty = hidden.
    pub mcp_chip_text: String,
    /// 0=hide 1=loading 2=ok 3=partial 4=error
    pub mcp_chip_kind: u8,
    /// Ephemeral toast (top-right). **Not** chat context, **not** agent messages.
    pub toast: Option<Toast>,
    /// Centered floating secondary menu (Settings, commands, sessions, …).
    pub float: Option<FloatMenu>,
    /// Current provider id (for model picker "current" marker).
    pub current_provider: String,
    /// Current model id.
    pub current_model: String,
    /// Provider id while in Settings → Provider → Models hierarchy.
    pub settings_provider_focus: String,
    /// When true, thinkingFormat / maxTokensField / cycle_compat apply to the focused **model**.
    pub settings_compat_on_model: bool,
    /// Model spec (`provider:id`) while on model detail page.
    pub settings_model_focus: String,
    /// Draft for Settings → Add model form (in-float, never leaves Settings).
    pub model_draft: Option<ModelDraft>,
    /// When set, float search bar edits this model-draft field (`id` / `name` / …).
    pub settings_form_edit: Option<String>,
    /// When set, float search bar edits a ConfigOp field (never opens docked select).
    /// e.g. `provider_set:linuxdo:base_url`, `model_set:p:id:name`, `provider_add_id`.
    pub settings_inline_op: Option<String>,
    /// Thinking level label: off | low | medium | high.
    pub thinking_level: String,
    /// Estimated context tokens (messages) — char/4 heuristic.
    pub usage_tokens: usize,
    /// Provider-reported cumulative input tokens.
    pub usage_input: u64,
    /// Provider-reported cumulative output tokens.
    pub usage_output: u64,
    /// Provider-reported cumulative cache-read tokens (0 = none / unknown).
    pub usage_cache_read: u64,
    /// Provider-reported cumulative cache-write / creation tokens.
    pub usage_cache_write: u64,
    /// Optional rough USD cost estimate (0 = unknown / not shown).
    pub usage_cost_usd: f64,
    /// Optional context window for % display (0 = unknown).
    pub context_window: usize,
    turn_started: Option<Instant>,
    followup_pending: Option<String>,
    steer_pending: Option<String>,
    abort_pending: bool,
    /// Ctrl+C force-quit: leave interactive immediately (not soft cancel).
    force_quit_pending: bool,
    /// Images still referenced by tokens in `input`.
    pub pending_images: Vec<PendingImage>,
    /// In-flight clipboard / import jobs (chip already shown).
    image_jobs: Vec<ImagePasteJob>,
    /// Long text pastes still referenced by `[文本.….txt]` tokens in `input`.
    pub pending_texts: Vec<PendingText>,
    /// Images committed on submit: `(mime, path)` for the agent.
    committed_images: Vec<(String, String)>,
    /// Next image token id (1-based).
    next_image_id: u32,
    /// Next text-chip id (1-based).
    next_text_id: u32,
    /// Submitted prompt history (oldest → newest). Up/Down / Ctrl+P/N navigate.
    prompt_history: Vec<String>,
    /// Index into `prompt_history` while browsing; `None` = live draft.
    history_index: Option<usize>,
    /// Input draft saved when first stepping into history with Up.
    history_draft: String,
    /// Timestamp of the last Esc press (for double-Esc rewind / clear).
    last_esc_at: Option<Instant>,
    /// Timestamp of the last Ctrl+C that armed confirm-quit (double-tap to exit).
    last_ctrl_c_at: Option<Instant>,
    /// Optional on-disk history file (project-scoped). Written on each push.
    history_persist_path: Option<PathBuf>,
    /// Optional callback-less persist via path — CLI sets this after load.
    /// When set, `push_prompt_history` also appends a JSON line.
    history_cwd: Option<PathBuf>,
    /// Interactive tool approval overlay (while busy) — metadata for gate id.
    approval: Option<ApprovalPrompt>,
    /// Choice taken by the user for the current approval.
    approval_answer: Option<ApprovalAnswer>,
    /// Active single/multi-select HITL prompt (permission or ask_user).
    select: Option<crate::select::SelectPrompt>,
    /// Why `select` is open.
    select_kind: Option<SelectKind>,
    /// Completed select result (ask_user path); approval maps into `approval_answer`.
    select_result: Option<(SelectKind, crate::select::SelectResult)>,
}

fn classify_toast_level(text: &str) -> AlertLevel {
    let t = text.trim().to_ascii_lowercase();
    if t.starts_with("error") || t.contains("failed") || t.contains("overflow") {
        AlertLevel::Error
    } else if t.starts_with("warn")
        || t.contains("interrupt")
        || t.contains("overflow")
        || t.starts_with("thinking →")
    {
        AlertLevel::Warn
    } else {
        AlertLevel::Info
    }
}

impl App {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            messages: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            status: STATUS_IDLE.into(),
            stream_buffer: String::new(),
            thinking_buffer: String::new(),
            // Prefer collapsed headers so long reasoning doesn't flood the transcript.
            show_thinking: false,
            busy: false,
            chat_scroll: 0,
            follow_bottom: true,
            chat_view_height: 0,
            chat_total_lines: 0,
            chat_line_owners: Vec::new(),
            chat_view_start: 0,
            chat_top_pad: 0,
            mouse_capture: true,
            select_anchor: None,
            select_end: None,
            select_dragging: false,
            chat_line_text: Vec::new(),
            clipboard_pending: None,
            cursor_on: true,
            mode_label: String::new(),
            agent_label: "Build".into(),
            spinner_frame: 0,
            slash_selected: 0,
            model_catalog: Vec::new(),
            settings_provider_rows: Vec::new(),
            settings_provider_field_rows: Vec::new(),
            settings_model_rows: Vec::new(),
            skills_rows: Vec::new(),
            mcp_rows: Vec::new(),
            mcp_import_rows: Vec::new(),
            mcp_summary: "none".into(),
            mcp_chip_text: String::new(),
            mcp_chip_kind: 0,
            toast: None,
            float: None,
            current_provider: String::new(),
            current_model: String::new(),
            settings_provider_focus: String::new(),
            settings_compat_on_model: false,
            settings_model_focus: String::new(),
            model_draft: None,
            settings_form_edit: None,
            settings_inline_op: None,
            thinking_level: "off".into(),
            usage_tokens: 0,
            usage_input: 0,
            usage_output: 0,
            usage_cache_read: 0,
            usage_cache_write: 0,
            usage_cost_usd: 0.0,
            context_window: 0,
            turn_started: None,
            followup_pending: None,
            steer_pending: None,
            abort_pending: false,
            force_quit_pending: false,
            pending_images: Vec::new(),
            image_jobs: Vec::new(),
            pending_texts: Vec::new(),
            committed_images: Vec::new(),
            next_image_id: 1,
            next_text_id: 1,
            prompt_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            last_esc_at: None,
            last_ctrl_c_at: None,
            history_persist_path: None,
            history_cwd: None,
            approval: None,
            approval_answer: None,
            select: None,
            select_kind: None,
            select_result: None,
        }
    }

    /// Show a tool-approval modal (called from CLI while agent is busy).
    pub fn set_approval_prompt(&mut self, prompt: ApprovalPrompt) {
        // Don't wipe an answer waiting for CLI drain, and don't re-open the same id.
        if self.approval_answer.is_some() {
            return;
        }
        if self.approval.as_ref().map(|p| p.id) == Some(prompt.id) {
            return;
        }
        let select =
            crate::select::SelectPrompt::permission(&prompt.tool, &prompt.summary, &prompt.reason);
        let id = prompt.id;
        self.approval = Some(prompt);
        self.approval_answer = None;
        self.select = Some(select);
        self.select_kind = Some(SelectKind::Approval { id });
        self.select_result = None;
    }

    pub fn clear_approval_prompt(&mut self) {
        self.approval = None;
        if matches!(self.select_kind, Some(SelectKind::Approval { .. })) {
            self.select = None;
            self.select_kind = None;
        }
    }

    pub fn approval_prompt(&self) -> Option<&ApprovalPrompt> {
        self.approval.as_ref()
    }

    /// Active select prompt (permission or ask_user).
    pub fn select_prompt(&self) -> Option<&crate::select::SelectPrompt> {
        self.select.as_ref()
    }

    pub fn select_kind(&self) -> Option<&SelectKind> {
        self.select_kind.as_ref()
    }

    /// Show a generic select prompt (ask_user HITL).
    pub fn set_select_prompt(&mut self, kind: SelectKind, prompt: crate::select::SelectPrompt) {
        // Don't clobber an in-flight dock, or wipe a result waiting for the CLI drain.
        if self.select_result.is_some() {
            return;
        }
        if self.select_kind.as_ref() == Some(&kind) && self.select.is_some() {
            return;
        }
        self.select = Some(prompt);
        self.select_kind = Some(kind);
        self.select_result = None;
    }

    pub fn clear_select_prompt(&mut self) {
        self.select = None;
        self.select_kind = None;
    }

    /// Take a finished select result (ask_user). Approval results go to
    /// [`Self::take_approval_answer`] instead.
    pub fn take_select_result(&mut self) -> Option<(SelectKind, crate::select::SelectResult)> {
        self.select_result.take()
    }

    /// Take the user's answer if any (CLI feeds it into PermissionGate).
    pub fn take_approval_answer(&mut self) -> Option<ApprovalAnswer> {
        self.approval_answer.take()
    }

    /// Apply a finished select; may return a [`RunOutcome`] for CLI to handle.
    fn apply_select_result(&mut self, result: crate::select::SelectResult) -> Option<RunOutcome> {
        let kind = match self.select_kind.take() {
            Some(k) => k,
            None => return None,
        };
        self.select = None;
        match kind {
            SelectKind::Approval { .. } => {
                self.approval = None;
                let answer = match result {
                    crate::select::SelectResult::Cancelled => {
                        ApprovalAnswer::Deny { feedback: None }
                    }
                    crate::select::SelectResult::Confirmed { ids, other } => {
                        match ids.first().map(|s| s.as_str()) {
                            Some("always") => ApprovalAnswer::Always,
                            Some("once") => ApprovalAnswer::Once,
                            Some("session") => ApprovalAnswer::Session,
                            Some("deny") => ApprovalAnswer::Deny { feedback: other },
                            _ => ApprovalAnswer::Deny { feedback: other },
                        }
                    }
                };
                let notice = match &answer {
                    ApprovalAnswer::Always => "always-approve mode",
                    ApprovalAnswer::Once => "approved once",
                    ApprovalAnswer::Session => "approved for session",
                    ApprovalAnswer::Deny { feedback } => {
                        if feedback.as_ref().map(|s| !s.is_empty()).unwrap_or(false) {
                            "denied (with feedback)"
                        } else {
                            "denied"
                        }
                    }
                };
                self.set_notice(notice);
                self.approval_answer = Some(answer);
                None
            }
            SelectKind::AskUser { .. } => {
                self.select_result = Some((kind, result));
                None
            }
            SelectKind::Model => match result {
                crate::select::SelectResult::Cancelled => {
                    self.set_notice("model select cancelled");
                    None
                }
                crate::select::SelectResult::Confirmed { ids, .. } => {
                    let Some(spec) = ids.first() else {
                        return None;
                    };
                    if let Some((p, m)) = spec.split_once(':') {
                        Some(RunOutcome::SwitchModel {
                            provider: p.to_string(),
                            model: Some(m.to_string()),
                        })
                    } else {
                        Some(RunOutcome::SwitchModel {
                            provider: spec.clone(),
                            model: None,
                        })
                    }
                }
            },
        }
    }

    /// Height of the select dock above the prompt (`0` when closed).
    pub fn select_dock_height(&self) -> u16 {
        use crate::select::SelectPhase;
        let Some(prompt) = self.select.as_ref() else {
            return 0;
        };
        let body = prompt.body.lines().filter(|l| !l.is_empty()).count();
        let body = if body == 0 { 0 } else { body + 1 }; // + blank
        let typing = if matches!(prompt.phase, SelectPhase::Typing { .. }) {
            2
        } else {
            0
        };
        // border(2) + title area absorbed in border + body + options + footer
        let content = body + prompt.option_count().min(8) + typing + 1;
        ((content as u16) + 2).clamp(5, 14)
    }

    /// Open model switcher as docked select (Ctrl+L / `/model`).
    pub fn open_model_select(&mut self) {
        use crate::select::{SelectOption, SelectPrompt};
        self.close_float();
        let options: Vec<SelectOption> = self
            .model_catalog
            .iter()
            .map(|m| {
                let id = format!("{}:{}", m.provider, m.id);
                let label = if m.provider == self.current_provider && m.id == self.current_model {
                    format!("{id}  (current)")
                } else {
                    id.clone()
                };
                let desc = if m.name != m.id {
                    m.name.clone()
                } else {
                    String::new()
                };
                SelectOption::new(id, label, desc)
            })
            .collect();
        if options.is_empty() {
            self.set_notice("no models in catalog");
            return;
        }
        let current_spec = format!("{}:{}", self.current_provider, self.current_model);
        let selected = options
            .iter()
            .position(|o| o.id == current_spec)
            .unwrap_or(0);
        let mut prompt = SelectPrompt::single("Model", "Switch provider / model", options);
        prompt.selected = selected;
        prompt.footer_hint = "↑↓ enter switch · esc cancel".into();
        self.select = Some(prompt);
        self.select_kind = Some(SelectKind::Model);
        self.select_result = None;
        self.clear_notice();
    }

    /// Open centered Settings (Ctrl+G / bare `/settings`).
    pub fn open_settings_float(&mut self) {
        self.close_float();
        self.clear_select_prompt();
        self.float = Some(FloatMenu::settings_root_with_mcp(
            &self.thinking_level,
            &self.current_provider,
            &self.current_model,
            &self.mcp_summary,
        ));
        self.clear_notice();
    }

    /// Populate skills manager rows (path, label, detail, enabled).
    pub fn set_skills_rows(&mut self, rows: Vec<(String, String, String, bool)>) {
        self.skills_rows = rows;
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

    /// Open skills enable/disable panel (`/skills`).
    pub fn open_skills_float(&mut self) {
        self.close_float();
        self.clear_select_prompt();
        self.float = Some(FloatMenu::skills_manager(&self.skills_rows));
        self.clear_notice();
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
            FloatKind::Mcp => {
                self.open_settings_float();
                true
            }
            FloatKind::McpImport => {
                // Back to MCP manager (caller may refresh rows).
                self.open_mcp_float();
                true
            }
            FloatKind::Thinking => {
                // Opened from Settings — return to root rather than blank.
                self.open_settings_float();
                true
            }
            _ => false,
        }
    }

    /// Replace in-memory history (e.g. load from disk / past sessions).
    /// Does **not** write back — caller already owns the file contents.
    pub fn load_prompt_history(&mut self, entries: Vec<String>) {
        self.prompt_history = entries;
        self.history_index = None;
        self.history_draft.clear();
    }

    /// Enable project-scoped persistence (Claude: history survives new sessions).
    ///
    /// `cwd` is the project directory used for `~/.one/agent/sessions/--cwd--/prompt_history.jsonl`.
    pub fn enable_prompt_history_persist(&mut self, cwd: impl Into<PathBuf>) {
        let cwd = cwd.into();
        self.history_persist_path = Some(one_session_prompt_history_path(&cwd));
        self.history_cwd = Some(cwd);
    }

    /// Record a prompt into ↑/↓ history (dedupes consecutive identical entries).
    /// When persistence is enabled, also appends to the project history file.
    pub fn push_prompt_history(&mut self, text: impl AsRef<str>) {
        let text = text.as_ref().trim();
        if text.is_empty() {
            return;
        }
        if self.prompt_history.last().map(|s| s.as_str()) == Some(text) {
            return;
        }
        self.prompt_history.push(text.to_string());
        // Cap growth so long sessions stay snappy.
        const MAX: usize = 500;
        if self.prompt_history.len() > MAX {
            let drop_n = self.prompt_history.len() - MAX;
            self.prompt_history.drain(0..drop_n);
        }
        self.history_index = None;
        self.history_draft.clear();

        if let Some(cwd) = &self.history_cwd {
            // Best-effort; history recall must not fail the UI.
            let _ = persist_append_prompt_history(cwd, text);
        }
    }

    pub fn prompt_history_len(&self) -> usize {
        self.prompt_history.len()
    }
}

/// Path helper kept local so one-tui does not hard-depend on one-session types
/// beyond the path layout we already share via the CLI wiring.
fn one_session_prompt_history_path(cwd: &std::path::Path) -> PathBuf {
    // Mirror one_session::paths::session_dir_for_cwd + prompt_history.jsonl
    // so tests / App can show the path without importing session crate in all builds.
    // Actual I/O goes through `persist_append_prompt_history` (CLI-linked).
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let encoded = cwd
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "-");
    home.join(".one/agent/sessions")
        .join(format!("--{encoded}--"))
        .join("prompt_history.jsonl")
}

fn persist_append_prompt_history(cwd: &std::path::Path, text: &str) -> std::io::Result<()> {
    // Inline minimal append so one-tui stays free of one-session if needed.
    // Format matches one_session::prompt_history (JSON string per line).
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    let path = one_session_prompt_history_path(cwd);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(text).unwrap_or_else(|_| text.to_string());
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

// Re-open App impl after free functions.
impl App {
    /// Step older (Up / Ctrl+P). Saves the live draft on first entry.
    pub fn history_prev(&mut self) {
        if self.prompt_history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.history_draft = self.input.clone();
                let i = self.prompt_history.len() - 1;
                self.history_index = Some(i);
                self.input = self.prompt_history[i].clone();
                self.input_cursor_end();
            }
            Some(0) => {
                // Already at oldest — stay put.
            }
            Some(i) => {
                let i = i - 1;
                self.history_index = Some(i);
                self.input = self.prompt_history[i].clone();
                self.input_cursor_end();
            }
        }
        self.pending_images.clear();
        self.pending_texts.clear();
        self.cursor_on = true;
        self.clear_notice();
    }

    /// Step newer (Down / Ctrl+N). Restores the draft past the newest entry.
    pub fn history_next(&mut self) {
        let Some(i) = self.history_index else {
            return;
        };
        if i + 1 < self.prompt_history.len() {
            let i = i + 1;
            self.history_index = Some(i);
            self.input = self.prompt_history[i].clone();
        } else {
            self.history_index = None;
            self.input = std::mem::take(&mut self.history_draft);
        }
        self.input_cursor_end();
        self.pending_images.clear();
        self.pending_texts.clear();
        self.cursor_on = true;
        self.clear_notice();
    }

    /// Leave history browse mode without changing the buffer (after typing, etc.).
    fn leave_history_browse(&mut self) {
        self.history_index = None;
        self.history_draft.clear();
    }

    /// Take and clear images committed on the last submit: `(mime, path)`.
    pub fn take_pending_images(&mut self) -> Vec<(String, String)> {
        if !self.committed_images.is_empty() {
            return std::mem::take(&mut self.committed_images);
        }
        // Fallback: still in the input (e.g. tests calling take without submit).
        self.sync_pending_chips();
        let order = one_core::image::image_token_ids_in(&self.input);
        let mut by_id: std::collections::HashMap<u32, PendingImage> = self
            .pending_images
            .drain(..)
            .map(|img| (img.id, img))
            .collect();
        order
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .map(|img| (img.mime_type, img.path.display().to_string()))
            .collect()
    }

    /// Drop chips whose token was deleted from the input.
    pub fn sync_pending_chips(&mut self) {
        let imgs: std::collections::HashSet<u32> = one_core::image::image_token_ids_in(&self.input)
            .into_iter()
            .collect();
        self.pending_images.retain(|img| imgs.contains(&img.id));
        let texts: std::collections::HashSet<u32> = one_core::image::text_token_ids_in(&self.input)
            .into_iter()
            .collect();
        self.pending_texts.retain(|t| texts.contains(&t.id));
    }

    /// Backward-compatible alias.
    pub fn sync_pending_images(&mut self) {
        self.sync_pending_chips();
    }

    fn clamp_input_cursor(&mut self) {
        let len = self.input.chars().count();
        if self.input_cursor > len {
            self.input_cursor = len;
        }
    }

    /// Move caret to end of `input` (history recall, bulk replace, chip append).
    fn input_cursor_end(&mut self) {
        self.input_cursor = self.input.chars().count();
    }

    fn input_cursor_home(&mut self) {
        self.input_cursor = 0;
    }

    /// Byte index in `input` for the current char cursor.
    fn input_byte_at_cursor(&self) -> usize {
        self.input
            .chars()
            .take(self.input_cursor)
            .map(|c| c.len_utf8())
            .sum()
    }

    /// Left/right caret movement inside the main prompt.
    pub fn move_input_cursor(&mut self, delta: isize) {
        let len = self.input.chars().count() as isize;
        let next = (self.input_cursor as isize + delta).clamp(0, len);
        self.input_cursor = next as usize;
        self.cursor_on = true;
    }

    /// Split `input` at the caret for rendering: (before, after).
    pub fn input_split_at_cursor(&self) -> (&str, &str) {
        let idx = self.input_byte_at_cursor().min(self.input.len());
        let idx = if self.input.is_char_boundary(idx) {
            idx
        } else {
            self.input.len()
        };
        (&self.input[..idx], &self.input[idx..])
    }

    /// Insert a character at the caret.
    fn insert_input_char(&mut self, ch: char) {
        if ch.is_control() && ch != '\n' {
            return;
        }
        self.clamp_input_cursor();
        let idx = self.input_byte_at_cursor();
        self.input.insert(idx, ch);
        self.input_cursor += 1;
        self.cursor_on = true;
    }

    /// Insert a string at the caret (control chars other than `\n` dropped).
    fn insert_input_str(&mut self, text: &str) {
        let cleaned: String = text
            .chars()
            .filter(|c| *c == '\n' || !c.is_control())
            .collect();
        if cleaned.is_empty() {
            return;
        }
        self.clamp_input_cursor();
        let idx = self.input_byte_at_cursor();
        let n = cleaned.chars().count();
        self.input.insert_str(idx, &cleaned);
        self.input_cursor += n;
        self.cursor_on = true;
    }

    fn insert_chip_token(&mut self, token: &str) {
        // Prefer a leading space when inserting mid-buffer after non-whitespace.
        self.clamp_input_cursor();
        let idx = self.input_byte_at_cursor();
        let need_lead = idx > 0
            && !self.input[..idx]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace());
        let mut piece = String::new();
        if need_lead {
            piece.push(' ');
        }
        piece.push_str(token);
        piece.push(' ');
        let n = piece.chars().count();
        self.input.insert_str(idx, &piece);
        self.input_cursor += n;
        self.cursor_on = true;
    }

    /// Attach a ready local image file and insert `[图片.img]` into the input.
    pub fn attach_image_path(&mut self, mime_type: String, path: PathBuf, name: String) {
        let id = self.next_image_id;
        self.next_image_id = self.next_image_id.saturating_add(1).max(2);
        let token = one_core::image::image_token(id);
        let label = one_core::image::image_label_path(&mime_type, &path);
        self.pending_images.push(PendingImage {
            id,
            mime_type,
            path,
            name: name.clone(),
            loading: false,
        });
        self.insert_chip_token(&token);
        self.set_notice(format!("attached {name}  {token}  {label}"));
    }

    /// Attach from base64: write into media store, then path-based attach.
    pub fn attach_image(&mut self, mime_type: String, data: String, name: String) {
        match one_core::image::store_image_base64(&data, Some(&mime_type)) {
            Ok((path, mime)) => self.attach_image_path(mime, path, name),
            Err(err) => self.set_notice(format!("image attach failed: {err}")),
        }
    }

    /// True while any image chip is still loading from clipboard / import.
    pub fn has_loading_images(&self) -> bool {
        self.pending_images.iter().any(|i| i.loading) || !self.image_jobs.is_empty()
    }

    /// Insert an optimistic loading chip immediately (before disk/clipboard work).
    fn begin_loading_image(&mut self, name: &str) -> u32 {
        let id = self.next_image_id;
        self.next_image_id = self.next_image_id.saturating_add(1).max(2);
        let token = one_core::image::image_token(id);
        self.pending_images.push(PendingImage {
            id,
            mime_type: "image/*".into(),
            path: PathBuf::new(),
            name: name.to_string(),
            loading: true,
        });
        self.insert_chip_token(&token);
        self.cursor_on = true;
        self.set_notice(format!("pasting {token}…"));
        id
    }

    /// Remove a chip + pending entry by id (failed paste / user abandoned load).
    fn remove_pending_image(&mut self, id: u32) {
        self.pending_images.retain(|i| i.id != id);
        let token = one_core::image::image_token(id);
        if let Some(pos) = self.input.find(&token) {
            let mut start = pos;
            let mut end = pos + token.len();
            // insert_chip_token writes ` token ` — peel one trailing space.
            if self.input[end..].starts_with(' ') {
                end += 1;
            }
            // And one leading space when not at start.
            if start > 0 && self.input.as_bytes().get(start - 1) == Some(&b' ') {
                start -= 1;
            }
            let removed = self.input[start..end].chars().count();
            let caret_byte = self.input_byte_at_cursor();
            self.input.replace_range(start..end, "");
            if caret_byte >= end {
                self.input_cursor = self.input_cursor.saturating_sub(removed);
            } else if caret_byte > start {
                // Caret was inside the chip — snap to the cut point.
                self.input_cursor = self.input[..start].chars().count();
            }
            self.clamp_input_cursor();
        }
    }

    /// Apply a finished load onto an existing loading chip (or drop on error).
    fn finish_loading_image(
        &mut self,
        id: u32,
        result: Result<(String, PathBuf, String), String>,
        report_err: bool,
    ) {
        match result {
            Ok((mime, path, name)) => {
                if let Some(img) = self.pending_images.iter_mut().find(|i| i.id == id) {
                    img.mime_type = mime;
                    img.path = path;
                    img.name = name.clone();
                    img.loading = false;
                    let label = img.label();
                    let token = img.token();
                    self.set_notice(format!("attached {name}  {token}  {label}"));
                }
            }
            Err(err) => {
                self.remove_pending_image(id);
                if report_err {
                    self.set_notice(format!(
                        "paste failed · {err} · copy a screenshot, or paste a path"
                    ));
                } else {
                    // Quiet probe (e.g. empty bracketed paste) — chip already removed.
                    self.clear_notice();
                }
            }
        }
    }

    /// Poll background image jobs; call every frame from the terminal loop.
    pub fn poll_image_jobs(&mut self) {
        if self.image_jobs.is_empty() {
            return;
        }
        let mut still = Vec::new();
        let jobs = std::mem::take(&mut self.image_jobs);
        for job in jobs {
            match job.rx.try_recv() {
                Ok(result) => {
                    self.finish_loading_image(job.id, result, job.report_err);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    still.push(job);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.finish_loading_image(
                        job.id,
                        Err("image paste cancelled".into()),
                        job.report_err,
                    );
                }
            }
        }
        self.image_jobs = still;
    }

    /// Try host-clipboard bitmap paste (WSL PowerShell / wl-paste / xclip).
    ///
    /// Shows a loading chip **immediately**, then fills path in the background
    /// so the UI never freezes on PowerShell / host tools.
    ///
    /// Returns `true` when a job was started (chip visible). Call
    /// [`Self::poll_image_jobs`] each frame to finalize.
    pub fn try_paste_clipboard_image(&mut self, report_err: bool) -> bool {
        // Don't steal focus from select/float free-text.
        if self.select.is_some() || self.float_open() {
            return false;
        }
        let id = self.begin_loading_image("clipboard.png");
        let (tx, rx) = std::sync::mpsc::channel();
        self.image_jobs.push(ImagePasteJob {
            id,
            report_err,
            rx,
        });
        std::thread::spawn(move || {
            let _ = tx.send(crate::clipboard::paste_image());
        });
        true
    }

    /// Fast check: text looks like an existing image file path (no copy yet).
    fn quick_image_path_candidate(&self, text: &str) -> Option<PathBuf> {
        let path = crate::clipboard::normalize_pasted_path(text)
            .or_else(|| {
                let t = text.trim().trim_matches(|c| c == '"' || c == '\'');
                Some(PathBuf::from(t))
            })?;
        if !one_core::image::is_image_path(&path) {
            return None;
        }
        path.is_file().then_some(path)
    }

    /// Resolve pasted text to an imported media image `(mime, path, name)`.
    fn load_image_from_pasted_path_static(text: &str) -> Option<(String, PathBuf, String)> {
        if let Some(v) = one_core::image::try_load_image_path_paste(text) {
            return Some(v);
        }
        let path = crate::clipboard::normalize_pasted_path(text)?;
        let s = path.to_str()?;
        one_core::image::try_load_image_path_paste(s)
    }

    /// Collapse a long paste into `[文本.txt]` (body kept until submit / delete).
    pub fn attach_text_blob(&mut self, body: String) {
        let id = self.next_text_id;
        self.next_text_id = self.next_text_id.saturating_add(1).max(2);
        let token = one_core::image::text_token(id);
        let summary = one_core::image::text_blob_summary(&body);
        self.pending_texts.push(PendingText { id, body });
        self.insert_chip_token(&token);
        self.set_notice(format!("pasted  {token}  {summary}"));
    }

    /// Backspace: delete one character (or an entire paste chip) before the caret.
    pub fn pop_input(&mut self) {
        if self.input_cursor == 0 || self.input.is_empty() {
            return;
        }
        let byte_end = self.input_byte_at_cursor();
        let before = &self.input[..byte_end];
        if let Some(n) = one_core::image::paste_chip_backspace_len(before) {
            let start = byte_end.saturating_sub(n);
            // How many chars were removed so the caret can step back correctly.
            let removed_chars = self.input[start..byte_end].chars().count();
            self.input.replace_range(start..byte_end, "");
            self.input_cursor = self.input_cursor.saturating_sub(removed_chars);
        } else {
            // Remove the single char immediately before the caret.
            self.input_cursor -= 1;
            let idx = self.input_byte_at_cursor();
            if idx < self.input.len() {
                self.input.remove(idx);
            }
        }
        self.clamp_input_cursor();
        self.sync_pending_chips();
        self.cursor_on = true;
        self.clear_notice();
    }

    /// Delete: remove one character (or paste chip) at/after the caret.
    pub fn delete_input_forward(&mut self) {
        if self.input_cursor >= self.input.chars().count() {
            return;
        }
        let idx = self.input_byte_at_cursor();
        let rest = &self.input[idx..];
        // Atomic chip delete when caret sits at the start of `[图片…]` / `[文本…]`.
        let chip_len = one_core::image::parse_image_token_at(rest)
            .map(|(_, len)| len)
            .or_else(|| one_core::image::parse_text_token_at(rest).map(|(_, len)| len));
        if let Some(len) = chip_len {
            let mut end = idx + len;
            // Peel optional trailing space that insert_chip_token adds.
            if self.input[end..].starts_with(' ') {
                end += 1;
            }
            self.input.replace_range(idx..end, "");
        } else if let Some(ch) = rest.chars().next() {
            self.input.remove(idx);
            let _ = ch;
        }
        self.clamp_input_cursor();
        self.sync_pending_chips();
        self.cursor_on = true;
        self.clear_notice();
    }

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
        self.scroll_to_bottom();
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
    }

    /// Popup rows for current input (commands or models grouped by provider).
    pub fn popup_rows(&self) -> Vec<PopupRow> {
        slash::popup_rows(&self.input, &self.model_catalog)
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
    fn confirm_slash_menu(&mut self) -> RunOutcome {
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

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(Message::user(text));
        self.scroll_to_bottom();
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) {
        let mut msg = Message::assistant(text);
        msg.footer = Some(self.turn_footer(None));
        self.messages.push(msg);
        self.scroll_to_bottom();
    }

    pub fn push_system(&mut self, text: impl Into<String>) {
        self.messages.push(Message::system(text));
        self.scroll_to_bottom();
    }

    pub fn push_tool(&mut self, text: impl Into<String>) {
        // Backward-compatible: `name(args)` or free text → running tool row.
        let text = text.into();
        let (name, detail) = split_tool_text(&text);
        self.messages
            .push(Message::tool(name, detail, ToolStatus::Running));
        self.scroll_to_bottom();
    }

    pub fn push_tool_call(&mut self, name: impl Into<String>, args: impl Into<String>) {
        // Close thinking + assistant bubbles so tool rows sit between
        // completed segments, and the next tool-round thinking starts clean
        // (otherwise deltas keep appending into the same buffer and the next
        // bubble re-shows the previous segment's full text).
        self.seal_stream_segment();
        self.messages
            .push(Message::tool(name, args, ToolStatus::Running));
        self.scroll_to_bottom();
    }

    /// Finalize in-progress thinking / assistant stream bubbles and reset
    /// buffers (used between tool rounds).
    pub fn seal_stream_segment(&mut self) {
        // Thinking must end before tools — interleaved think→tool→think
        // rounds each own a separate bubble with only that round's deltas.
        self.finish_thinking_stream();

        if self.stream_buffer.is_empty() {
            // Seal a trailing empty assistant streaming marker if present.
            if let Some(last) = self.messages.last_mut() {
                if last.role == MessageRole::Assistant && last.streaming {
                    last.streaming = false;
                }
            }
            return;
        }
        self.sync_stream_message();
        if let Some(last) = self.messages.last_mut() {
            if last.role == MessageRole::Assistant && last.streaming {
                last.streaming = false;
            }
        }
        self.stream_buffer.clear();
    }

    /// Mark the latest matching running tool as done / error.
    ///
    /// `output` is optional UI preview (already separate from agent `ToolResult`,
    /// which always carries the full payload for the model).
    pub fn finish_tool(&mut self, name: &str, error: bool) {
        self.finish_tool_with_output(name, error, None);
    }

    pub fn finish_tool_with_output(&mut self, name: &str, error: bool, output: Option<String>) {
        let status = if error {
            ToolStatus::Error
        } else {
            ToolStatus::Done
        };
        let apply = |msg: &mut Message| {
            msg.tool_status = Some(status);
            let args = msg.content.clone();
            let tool_name = msg.tool_name.clone().unwrap_or_else(|| name.to_string());
            if let Some(raw) = output.clone() {
                let mut stored = truncate_tool_output_for_ui(&raw, 4_000);
                let (summary, expand) = if let Some((s, e, better)) =
                    tool_view::summarize_tool_special(&tool_name, &args, &stored, error)
                {
                    if let Some(b) = better {
                        stored = truncate_tool_output_for_ui(&b, 4_000);
                    }
                    (s, e)
                } else {
                    summarize_tool_output(&stored, error)
                };
                msg.tool_output = Some(stored);
                msg.tool_summary = Some(summary);
                msg.tool_expanded = expand;
            } else if error {
                msg.tool_summary = Some("failed".into());
                msg.tool_expanded = true;
            } else if let Some((s, e, better)) =
                tool_view::summarize_tool_special(&tool_name, &args, "", false)
            {
                if let Some(b) = better {
                    msg.tool_output = Some(truncate_tool_output_for_ui(&b, 4_000));
                }
                msg.tool_summary = Some(s);
                msg.tool_expanded = e;
            } else {
                msg.tool_summary = Some("ok".into());
                msg.tool_expanded = false;
            }
        };

        for msg in self.messages.iter_mut().rev() {
            if msg.role == MessageRole::Tool
                && msg.tool_status == Some(ToolStatus::Running)
                && msg.tool_name.as_deref() == Some(name)
            {
                apply(msg);
                return;
            }
        }
        // Fallback: mark any last running tool.
        for msg in self.messages.iter_mut().rev() {
            if msg.role == MessageRole::Tool && msg.tool_status == Some(ToolStatus::Running) {
                apply(msg);
                return;
            }
        }
        if error {
            let mut msg = Message::tool(name, "failed", ToolStatus::Error);
            if let Some(raw) = output {
                let stored = truncate_tool_output_for_ui(&raw, 4_000);
                let (summary, expand) = summarize_tool_output(&stored, true);
                msg.tool_output = Some(stored);
                msg.tool_summary = Some(summary);
                msg.tool_expanded = expand;
            } else {
                msg.tool_summary = Some("failed".into());
                msg.tool_expanded = true;
            }
            self.messages.push(msg);
        }
    }

    /// Toggle last tool body, or expand/collapse a multi-tool group chip.
    pub fn toggle_last_tool_expand(&mut self) {
        if let Some((start, len)) = self.last_tool_streak() {
            if tool_view::streak_can_collapse(&self.messages, start, len) {
                // Chip → individual rows (bodies stay collapsed).
                for msg in &mut self.messages[start..start + len] {
                    msg.tool_ungroup = true;
                }
                return;
            }
            let all_ungrouped = self.messages[start..start + len]
                .iter()
                .all(|m| m.tool_ungroup || m.tool_expanded);
            if len >= tool_view::COLLAPSE_GROUP_MIN
                && all_ungrouped
                && self.messages[start..start + len]
                    .iter()
                    .all(|m| matches!(m.tool_status, Some(ToolStatus::Done)))
            {
                // Individual rows → chip again.
                for msg in &mut self.messages[start..start + len] {
                    msg.tool_ungroup = false;
                    msg.tool_expanded = false;
                }
                return;
            }
        }
        for msg in self.messages.iter_mut().rev() {
            if msg.role == MessageRole::Tool {
                msg.tool_expanded = !msg.tool_expanded;
                return;
            }
        }
    }

    /// Toggle the tool message at `msg_index` (click target).
    pub fn toggle_tool_at(&mut self, msg_index: usize) {
        if self
            .messages
            .get(msg_index)
            .map(|m| m.role != MessageRole::Tool)
            .unwrap_or(true)
        {
            return;
        }
        // Collapsed multi-tool chip → show individual rows.
        if let Some((start, len)) = self.tool_streak_covering(msg_index) {
            if tool_view::streak_can_collapse(&self.messages, start, len) {
                for m in &mut self.messages[start..start + len] {
                    m.tool_ungroup = true;
                }
                return;
            }
        }
        if let Some(msg) = self.messages.get_mut(msg_index) {
            msg.tool_expanded = !msg.tool_expanded;
        }
    }

    /// Toggle a thinking block at `msg_index` (click / enter).
    pub fn toggle_thinking_at(&mut self, msg_index: usize) {
        if let Some(msg) = self.messages.get_mut(msg_index) {
            if msg.role == MessageRole::Thinking && !msg.streaming {
                msg.thinking_expanded = !msg.thinking_expanded;
            }
        }
    }

    /// Map a viewport row (0 = top of chat pane) to absolute transcript line.
    pub fn view_row_to_line(&self, row_in_view: usize) -> Option<usize> {
        if row_in_view < self.chat_top_pad {
            return None;
        }
        let line = self
            .chat_view_start
            .saturating_add(row_in_view - self.chat_top_pad);
        if line < self.chat_total_lines {
            Some(line)
        } else {
            None
        }
    }

    /// Click at row offset within the chat viewport (0 = top visible line).
    pub fn click_chat_row(&mut self, row_in_view: usize) {
        let Some(line) = self.view_row_to_line(row_in_view) else {
            return;
        };
        if let Some(Some(msg_i)) = self.chat_line_owners.get(line).copied() {
            match self.messages.get(msg_i).map(|m| m.role) {
                Some(MessageRole::Thinking) => self.toggle_thinking_at(msg_i),
                Some(MessageRole::Tool) => self.toggle_tool_at(msg_i),
                Some(MessageRole::Alert) => {
                    // dismiss alerts if clickable — leave existing behaviour via tool path no-op
                }
                _ => self.toggle_tool_at(msg_i),
            }
        }
    }

    /// Begin in-app selection at a viewport row (mouse down).
    pub fn select_begin(&mut self, row_in_view: usize) {
        let Some(line) = self.view_row_to_line(row_in_view) else {
            self.clear_selection();
            return;
        };
        self.select_anchor = Some(line);
        self.select_end = Some(line);
        self.select_dragging = false;
    }

    /// Extend selection.
    ///
    /// `from_drag`: true for Drag/Moved while held (always a select gesture).
    /// false for mouse-up release row (only multi-line counts as select).
    pub fn select_update(&mut self, row_in_view: usize, from_drag: bool) {
        let Some(line) = self.view_row_to_line(row_in_view) else {
            return;
        };
        if self.select_anchor.is_none() {
            self.select_anchor = Some(line);
        }
        self.select_end = Some(line);
        if from_drag {
            // Any pointer motion while held → select → auto-copy on release.
            self.select_dragging = true;
        }
        if let (Some(a), Some(b)) = (self.select_anchor, self.select_end) {
            if a != b {
                self.select_dragging = true;
            }
        }
        if self.select_dragging {
            self.follow_bottom = false;
        }
    }

    /// Inclusive absolute line range of the current selection, if any.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let a = self.select_anchor?;
        let b = self.select_end.unwrap_or(a);
        if self.chat_total_lines == 0 {
            return Some((a.min(b), a.max(b)));
        }
        let max = self.chat_total_lines.saturating_sub(1);
        let lo = a.min(b).min(max);
        let hi = a.max(b).min(max);
        Some((lo, hi))
    }

    pub fn clear_selection(&mut self) {
        self.select_anchor = None;
        self.select_end = None;
        self.select_dragging = false;
    }

    /// True when selection spans more than one display line.
    pub fn selection_is_multi_line(&self) -> bool {
        self.selection_range().is_some_and(|(lo, hi)| hi > lo)
    }

    /// Plain text for the selected lines (joined with `\n`).
    pub fn selection_text(&self) -> Option<String> {
        let (lo, hi) = self.selection_range()?;
        if self.chat_line_text.is_empty() {
            return None;
        }
        let hi = hi.min(self.chat_line_text.len().saturating_sub(1));
        let lo = lo.min(hi);
        let mut parts = Vec::new();
        for line in &self.chat_line_text[lo..=hi] {
            parts.push(line.as_str());
        }
        let text = parts.join("\n");
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Queue selection (or last assistant) for clipboard copy (OSC 52 + host fallbacks).
    /// Always auto-copies when there is a real selection — default UX.
    pub fn request_copy_selection(&mut self) -> bool {
        if let Some(text) = self.selection_text() {
            let lines = text.lines().count().max(1);
            let n = text.chars().count();
            self.clipboard_pending = Some(text);
            // Toast updated after terminal flush with ok/err; provisional notice:
            if lines > 1 {
                self.set_notice(format!("copying {lines} lines…"));
            } else {
                self.set_notice(format!("copying {n} chars…"));
            }
            return true;
        }
        // Fallback: last assistant bubble (keybinding with no selection).
        if let Some(msg) = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant && !m.content.is_empty())
        {
            let n = msg.content.chars().count();
            self.clipboard_pending = Some(msg.content.clone());
            self.set_notice(format!("copying last reply ({n} chars)…"));
            return true;
        }
        self.set_notice("nothing to copy");
        false
    }

    /// Finish pointer gesture: drag or multi-line → **auto-copy**;
    /// plain click (no movement) → tool expand.
    pub fn select_finish(&mut self, row_in_view: usize) {
        // Apply release row without forcing drag (click stays click).
        self.select_update(row_in_view, false);
        // Selecting text always copies on release.
        if self.select_dragging || self.selection_is_multi_line() {
            let _ = self.request_copy_selection();
            self.select_dragging = false;
            return;
        }
        // Pure click: clear + expand tools.
        self.clear_selection();
        self.click_chat_row(row_in_view);
    }

    fn last_tool_streak(&self) -> Option<(usize, usize)> {
        let last_tool = self
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::Tool)?;
        // Walk back to streak start.
        let mut start = last_tool;
        while start > 0 && self.messages[start - 1].role == MessageRole::Tool {
            start -= 1;
        }
        let len = last_tool - start + 1;
        Some((start, len))
    }

    fn tool_streak_covering(&self, idx: usize) -> Option<(usize, usize)> {
        if self.messages.get(idx)?.role != MessageRole::Tool {
            return None;
        }
        let mut start = idx;
        while start > 0 && self.messages[start - 1].role == MessageRole::Tool {
            start -= 1;
        }
        let len = tool_view::tool_streak_len(&self.messages, start);
        Some((start, len))
    }

    pub fn append_stream(&mut self, delta: &str) {
        // Text arriving after thinking finalizes the thinking bubble.
        if !self.thinking_buffer.is_empty() {
            self.finish_thinking_stream();
        }
        self.stream_buffer.push_str(delta);
        if self.follow_bottom {
            self.scroll_to_bottom();
        }
    }

    pub fn append_thinking_stream(&mut self, delta: &str) {
        self.thinking_buffer.push_str(delta);
        if self.follow_bottom {
            self.scroll_to_bottom();
        }
    }

    pub fn sync_stream_message(&mut self) {
        self.sync_thinking_message();
        if self.stream_buffer.is_empty() {
            return;
        }

        if let Some(last) = self.messages.last_mut() {
            if last.role == MessageRole::Assistant && last.streaming {
                last.content = self.stream_buffer.clone();
                return;
            }
        }

        self.messages
            .push(Message::streaming_assistant(&self.stream_buffer));
    }

    pub fn sync_thinking_message(&mut self) {
        if self.thinking_buffer.is_empty() {
            return;
        }

        // Update the open streaming thinking bubble even if a later row
        // (e.g. tool) was already inserted — never invent a second bubble
        // that re-dumps the same cumulative buffer.
        if let Some(msg) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Thinking && m.streaming)
        {
            msg.content = self.thinking_buffer.clone();
            msg.thinking_expanded = true;
            return;
        }

        self.messages
            .push(Message::streaming_thinking(&self.thinking_buffer));
    }

    fn finish_thinking_stream(&mut self) {
        if self.thinking_buffer.is_empty() {
            // Still close an orphan streaming thinking bubble (no more deltas).
            if let Some(msg) = self
                .messages
                .iter_mut()
                .rev()
                .find(|m| m.role == MessageRole::Thinking && m.streaming)
            {
                msg.streaming = false;
                msg.thinking_expanded = self.show_thinking;
            }
            return;
        }
        self.sync_thinking_message();
        if let Some(msg) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Thinking && m.streaming)
        {
            msg.streaming = false;
            msg.thinking_expanded = self.show_thinking;
        }
        self.thinking_buffer.clear();
    }

    pub fn finish_stream(&mut self) {
        self.finish_stream_with_interrupted(false);
    }

    pub fn finish_stream_with_interrupted(&mut self, interrupted: bool) {
        self.finish_thinking_stream();
        if self.stream_buffer.is_empty() {
            self.remove_trailing_empty_stream();
            // Still stamp footer on last assistant if any.
            self.attach_turn_footer(interrupted);
            return;
        }

        self.sync_stream_message();
        if let Some(last) = self.messages.last_mut() {
            if last.streaming {
                last.streaming = false;
            }
        }
        self.stream_buffer.clear();
        self.attach_turn_footer(interrupted);
        self.scroll_to_bottom();
    }

    fn attach_turn_footer(&mut self, interrupted: bool) {
        let footer = self.turn_footer(if interrupted { Some(true) } else { None });
        // Attach to the last non-streaming assistant in this turn tail.
        for msg in self.messages.iter_mut().rev() {
            if msg.role == MessageRole::Assistant && !msg.streaming {
                msg.footer = Some(footer);
                break;
            }
            if msg.role == MessageRole::User {
                break;
            }
        }
        // Complete any still-running tools.
        for msg in self.messages.iter_mut() {
            if msg.role == MessageRole::Tool && msg.tool_status == Some(ToolStatus::Running) {
                msg.tool_status = Some(ToolStatus::Done);
            }
        }
    }

    fn turn_footer(&self, interrupted: Option<bool>) -> String {
        let mut parts = vec![self.agent_label.clone()];
        if !self.mode_label.is_empty() {
            parts.push(self.mode_label.clone());
        }
        if let Some(started) = self.turn_started {
            parts.push(format_duration(started.elapsed()));
        }
        if interrupted == Some(true) {
            parts.push("interrupted".into());
        }
        parts.join(" · ")
    }

    pub fn clear_stream(&mut self) {
        self.stream_buffer.clear();
        self.thinking_buffer.clear();
        self.remove_trailing_empty_stream();
    }

    fn remove_trailing_empty_stream(&mut self) {
        if let Some(last) = self.messages.last() {
            if last.streaming && last.content.is_empty() {
                self.messages.pop();
            }
        }
    }

    pub fn begin_busy(&mut self) {
        self.busy = true;
        self.stream_buffer.clear();
        self.thinking_buffer.clear();
        self.remove_trailing_empty_stream();
        self.status = STATUS_BUSY.into();
        self.follow_bottom = true;
        self.turn_started = Some(Instant::now());
        self.spinner_frame = 0;
        self.scroll_to_bottom();
    }

    pub fn end_busy(&mut self) {
        self.busy = false;
        self.status = STATUS_IDLE.into();
    }

    pub fn take_followup(&mut self) -> Option<String> {
        self.followup_pending.take()
    }

    pub fn take_steer(&mut self) -> Option<String> {
        self.steer_pending.take()
    }

    pub fn request_abort(&mut self) {
        self.abort_pending = true;
    }

    pub fn take_abort(&mut self) -> bool {
        std::mem::take(&mut self.abort_pending)
    }

    /// Request immediate interactive exit (Ctrl+C). Distinct from soft abort (`q` / Esc).
    pub fn request_force_quit(&mut self) {
        self.force_quit_pending = true;
        // Also trip abort so in-flight agent work stops if the process is about to leave.
        self.abort_pending = true;
    }

    pub fn take_force_quit(&mut self) -> bool {
        std::mem::take(&mut self.force_quit_pending)
    }

    pub fn force_quit_pending(&self) -> bool {
        self.force_quit_pending
    }

    pub fn scroll_to_bottom(&mut self) {
        self.follow_bottom = true;
        self.chat_scroll = 0;
    }

    pub fn scroll_to_top(&mut self) {
        self.follow_bottom = false;
        let max = self.max_scroll();
        if max == 0 {
            self.follow_bottom = true;
            self.chat_scroll = 0;
        } else {
            self.chat_scroll = max;
        }
    }

    /// How many lines above the bottom can still be revealed.
    pub fn max_scroll(&self) -> usize {
        self.chat_total_lines
            .saturating_sub(self.chat_view_height.max(1))
    }

    /// True when the transcript is taller than the chat viewport.
    pub fn can_scroll(&self) -> bool {
        self.chat_total_lines > self.chat_view_height && self.chat_view_height > 0
    }

    /// Page size for PgUp/PgDn — almost a full viewport, at least 1.
    pub fn page_lines(&self) -> usize {
        self.chat_view_height.saturating_sub(1).max(1)
    }

    /// Scroll the transcript up by `lines` display rows (older content).
    pub fn scroll_up(&mut self, lines: usize) {
        if lines == 0 {
            return;
        }
        self.follow_bottom = false;
        self.chat_scroll = self.chat_scroll.saturating_add(lines);
        // Clamp when metrics are known (updated each draw); otherwise draw clamps.
        if self.chat_total_lines > 0 && self.chat_view_height > 0 {
            self.chat_scroll = self.chat_scroll.min(self.max_scroll());
        }
    }

    /// Scroll the transcript down by `lines` display rows (newer content).
    pub fn scroll_down(&mut self, lines: usize) {
        self.chat_scroll = self.chat_scroll.saturating_sub(lines);
        // Empty welcome is top-anchored; don't re-enter follow-bottom or the
        // next draw will snap back to the title.
        if self.chat_scroll == 0 && !self.messages.is_empty() {
            self.follow_bottom = true;
        }
    }

    /// Mouse wheel / trackpad: positive `delta` = scroll up (older).
    pub fn scroll_by_wheel(&mut self, delta: i32) {
        if delta > 0 {
            self.scroll_up(delta as usize);
        } else if delta < 0 {
            self.scroll_down((-delta) as usize);
        }
    }

    pub fn toggle_cursor(&mut self) {
        self.cursor_on = !self.cursor_on;
        if self.busy {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        // Docked select free-text phase owns paste (never the main prompt).
        if let Some(prompt) = self.select.as_mut() {
            if prompt.handle_paste(text) {
                self.cursor_on = true;
                self.clear_notice();
                return;
            }
            // List phase: swallow paste so it does not leak into the main input.
            return;
        }

        // Center float (Settings field edit / search filter) owns paste.
        if self.float_open() {
            if let Some(f) = self.float.as_mut() {
                f.paste_search(text);
            }
            self.cursor_on = true;
            self.clear_notice();
            return;
        }

        // Empty bracketed paste: terminal cannot deliver bitmaps — try host clipboard
        // (screenshot / browser copy-as-image). Codex does the same via keybind.
        if text.trim().is_empty() {
            // Quiet: no error toast if clipboard has no image (chip removed on fail).
            let _ = self.try_paste_clipboard_image(false);
            self.cursor_on = true;
            return;
        }

        // data-URI → optimistic chip, media write on a worker thread.
        if let Some((mime, data)) = one_core::image::parse_data_uri(text) {
            let id = self.begin_loading_image("paste.png");
            let (tx, rx) = std::sync::mpsc::channel();
            self.image_jobs.push(ImagePasteJob {
                id,
                report_err: true,
                rx,
            });
            std::thread::spawn(move || {
                let r = one_core::image::store_image_base64(&data, Some(&mime)).map(
                    |(path, mime)| (mime, path, "paste.png".into()),
                );
                let _ = tx.send(r);
            });
            return;
        }
        // Path paste: bare / quoted / file:// / Windows→WSL — chip first, import async.
        if let Some(src) = self.quick_image_path_candidate(text) {
            let name = src
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("image")
                .to_string();
            let id = self.begin_loading_image(&name);
            let (tx, rx) = std::sync::mpsc::channel();
            self.image_jobs.push(ImagePasteJob {
                id,
                report_err: true,
                rx,
            });
            let text = text.to_string();
            std::thread::spawn(move || {
                let r = Self::load_image_from_pasted_path_static(&text)
                    .ok_or_else(|| "not an image path".to_string());
                let _ = tx.send(r);
            });
            return;
        }

        // Preserve newlines for multi-line paste (normalize \r\n / \r → \n).
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        // Long paste → `[文本.txt]` chip (same UX as images: compact + atomic delete).
        if one_core::image::should_collapse_paste(&normalized) {
            // Drop other control chars from the stored body.
            let body: String = normalized
                .chars()
                .filter(|c| *c == '\n' || !c.is_control())
                .collect();
            self.attach_text_blob(body);
            self.cursor_on = true;
            return;
        }

        self.insert_input_str(&normalized);
        self.clear_notice();
    }

    /// Main prompt caret is only active when no overlay owns keyboard focus.
    pub fn prompt_focused(&self) -> bool {
        self.select.is_none() && !self.float_open()
    }

    /// How many visual lines the prompt input currently needs (capped).
    pub fn input_line_count(&self) -> usize {
        let n = self.input.split('\n').count().max(1);
        n.min(6)
    }

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

        // Docked select (model / field edit / ask) captures keys before float.
        if self.select.is_some() {
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
            // Ctrl+P / Ctrl+N → prompt history (Claude Code / readline).
            // Command palette remains `/` on empty input.
            KeyCode::Char('p')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.history_prev();
                RunOutcome::Noop
            }
            KeyCode::Char('n')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.history_next();
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
            // Ctrl+O → expand/collapse last tool output body
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_last_tool_expand();
                RunOutcome::Noop
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

    fn is_ctrl_c(key: KeyEvent) -> bool {
        matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && !key.modifiers.contains(KeyModifiers::ALT)
    }

    /// Ctrl+F — fetch remote models. Also accept legacy ASCII ACK (0x06).
    fn is_ctrl_f(key: KeyEvent) -> bool {
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
    fn is_help_key(key: KeyEvent) -> bool {
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

    fn float_allows_fetch_models(kind: FloatKind) -> bool {
        matches!(
            kind,
            FloatKind::SettingsModels
                | FloatKind::SettingsProviderDetail
                | FloatKind::SettingsRemoteModels
        )
    }

    /// Emit `ProviderFetchModels` for the focused settings provider.
    fn provider_fetch_models_outcome(&mut self) -> RunOutcome {
        let id = self.settings_provider_focus.clone();
        if id.is_empty() {
            self.set_notice("no provider selected");
            RunOutcome::Noop
        } else {
            RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id })
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
    fn handle_ctrl_c(&mut self) -> RunOutcome {
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

    fn arm_ctrl_c_quit(&mut self, now: Instant) {
        self.last_ctrl_c_at = Some(now);
    }

    fn confirm_ctrl_c_quit(&mut self) -> RunOutcome {
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
    fn handle_esc(&mut self) -> RunOutcome {
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

    fn handle_float_key(&mut self, key: KeyEvent) -> RunOutcome {
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
            KeyCode::Esc => {
                if !self.settings_go_back() {
                    self.close_float();
                }
                RunOutcome::Noop
            }
            KeyCode::Left if !text_focus => {
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
            KeyCode::Backspace => {
                let empty = self
                    .float
                    .as_ref()
                    .map(|f| f.search.is_empty())
                    .unwrap_or(true);
                if empty && !editing {
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
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
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

    /// Save float search into the model draft field being edited.
    fn commit_settings_form_edit(&mut self) -> RunOutcome {
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
    fn commit_settings_inline_edit(&mut self) -> RunOutcome {
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

    /// Confirm current float selection → nested float or slash Prompt.
    fn confirm_float_selection(&mut self) -> RunOutcome {
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
            FloatKind::Settings => self.confirm_settings_root(&entry.item.id),
            FloatKind::SettingsProviders => self.confirm_settings_providers(&entry.item.id),
            FloatKind::SettingsProviderDetail => {
                self.confirm_settings_provider_detail(&entry.item.id)
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
            FloatKind::Skills => self.confirm_skills_toggle(&entry.item.id),
            FloatKind::Mcp => self.confirm_mcp_action(&entry.item.id),
            FloatKind::McpImport => self.confirm_mcp_import(&entry.item.id),
            FloatKind::Help | FloatKind::Commands | FloatKind::Custom => {
                self.dispatch_command_item(&entry.item.id, &entry.item.hint)
            }
        }
    }

    fn confirm_skills_toggle(&mut self, id: &str) -> RunOutcome {
        if id == "_empty" || id.is_empty() {
            return RunOutcome::Noop;
        }
        RunOutcome::ConfigOp(ConfigOp::SkillToggle {
            path: id.to_string(),
        })
    }

    fn confirm_mcp_action(&mut self, id: &str) -> RunOutcome {
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

    fn confirm_mcp_import(&mut self, id: &str) -> RunOutcome {
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

    fn confirm_settings_root(&mut self, id: &str) -> RunOutcome {
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
            "skills" => {
                self.open_skills_float();
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

    fn confirm_settings_providers(&mut self, id: &str) -> RunOutcome {
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

    fn confirm_settings_provider_detail(&mut self, id: &str) -> RunOutcome {
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
                            || kn.replace('_', "")
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

    fn confirm_settings_thinking_format(&mut self, id: &str) -> RunOutcome {
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

    fn confirm_settings_max_tokens_field(&mut self, id: &str) -> RunOutcome {
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

    fn confirm_settings_provider_api(&mut self, id: &str) -> RunOutcome {
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

    fn confirm_settings_remote_models(&mut self, id: &str) -> RunOutcome {
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

    fn confirm_settings_models(&mut self, id: &str) -> RunOutcome {
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

    fn confirm_settings_model_add(&mut self, id: &str) -> RunOutcome {
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

    fn confirm_settings_model_detail(&mut self, id: &str) -> RunOutcome {
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

    /// Shared handler for command-palette / help rows.
    fn dispatch_command_item(&mut self, id: &str, hint: &str) -> RunOutcome {
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
            "mcp" => {
                self.close_float();
                RunOutcome::OpenMcpPanel
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

        if self.float_open() {
            let _ = self.handle_float_key(key);
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.request_abort();
                self.set_notice("interrupting…");
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = self.submit_steer();
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                let _ = self.submit_followup();
            }
            KeyCode::Backspace => {
                self.pop_input();
            }
            KeyCode::Delete => {
                self.delete_input_forward();
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && !ch.is_control() =>
            {
                self.insert_input_char(ch);
            }
            KeyCode::Left => self.move_input_cursor(-1),
            KeyCode::Right => self.move_input_cursor(1),
            KeyCode::PageUp => self.scroll_up(self.page_lines()),
            KeyCode::PageDown => self.scroll_down(self.page_lines()),
            KeyCode::Home => self.scroll_to_top(),
            KeyCode::End => self.scroll_to_bottom(),
            KeyCode::Up => self.scroll_up(3),
            KeyCode::Down => self.scroll_down(3),
            _ => {}
        }
    }

    /// Complete the path or `@path` token under the cursor (end of input).
    pub fn complete_path_token(&mut self) {
        let Some((prefix, partial)) = path_token_at_end(&self.input) else {
            return;
        };
        let matches = list_path_completions(&partial);
        if matches.is_empty() {
            self.set_notice(format!("no match for `{partial}`"));
            return;
        }
        if matches.len() == 1 {
            let completed = &matches[0];
            self.input = format!("{prefix}{completed}");
            self.input_cursor_end();
            self.cursor_on = true;
            self.clear_notice();
            return;
        }
        // Longest common prefix of all matches.
        let common = longest_common_prefix(&matches);
        if common.len() > partial.len() {
            self.input = format!("{prefix}{common}");
            self.input_cursor_end();
            self.cursor_on = true;
        }
        let preview: Vec<_> = matches.iter().take(8).cloned().collect();
        self.set_notice(format!(
            "{} matches · {}",
            matches.len(),
            preview.join("  ")
        ));
    }

    fn submit_prompt(&mut self) -> RunOutcome {
        // Keep multi-line body; only trim ends.
        self.sync_pending_chips();
        if self.has_loading_images() {
            self.set_notice("still pasting image… · wait a moment");
            return RunOutcome::Noop;
        }
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return RunOutcome::Noop;
        }
        if text == "/quit" || text == "/exit" {
            self.pending_images.clear();
            self.pending_texts.clear();
            self.committed_images.clear();
            self.input.clear();
            return RunOutcome::Quit;
        }
        if text == "/help" {
            self.set_notice(
                "/session /resume /new /model · Ctrl+V image · paste path/[文本.txt] · Ctrl+J nl",
            );
            return RunOutcome::Noop;
        }
        if text == "/clear" {
            self.messages.clear();
            self.pending_images.clear();
            self.pending_texts.clear();
            self.committed_images.clear();
            self.input.clear();
            self.chat_scroll = 0;
            self.follow_bottom = true;
            self.set_notice("chat cleared");
            return RunOutcome::Noop;
        }
        // Bare /model → float picker (secondary menu in the float UI).
        if text == "/model" || text == "/model " {
            self.open_model_picker();
            return RunOutcome::Noop;
        }
        // UI slash commands — handled by one-cli without adding a chat turn.
        if is_ui_slash(&text) {
            self.push_prompt_history(&text);
            self.input.clear();
            return RunOutcome::Prompt(text);
        }

        // Stage image paths for the agent (input will be cleared).
        let img_order = one_core::image::image_token_ids_in(&text);
        let mut img_by_id: std::collections::HashMap<u32, PendingImage> = self
            .pending_images
            .drain(..)
            .map(|img| (img.id, img))
            .collect();
        self.committed_images = img_order
            .into_iter()
            .filter_map(|id| img_by_id.remove(&id))
            .filter(|img| !img.loading && !img.path.as_os_str().is_empty())
            .map(|img| (img.mime_type, img.path.display().to_string()))
            .collect();

        // Expand `[文本.txt]` bodies for the model; strip image tokens (sent as blocks).
        let text_bodies: std::collections::HashMap<u32, String> = self
            .pending_texts
            .drain(..)
            .map(|t| (t.id, t.body))
            .collect();
        let plain = one_core::image::materialize_prompt_text(&text, &text_bodies);
        let expanded = if plain.contains('@') {
            expand_at_files(&plain)
        } else {
            plain
        };

        // Transcript keeps compact chips (`[图片.img]` / `[文本.txt]`), not the full paste.
        self.push_prompt_history(&text);
        self.input.clear();
        self.push_user(&text);
        RunOutcome::Prompt(expanded)
    }

    fn submit_followup(&mut self) -> RunOutcome {
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            return RunOutcome::Noop;
        }
        self.push_user(&text);
        self.followup_pending = Some(text.clone());
        RunOutcome::FollowUp(text)
    }

    fn submit_steer(&mut self) -> RunOutcome {
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            return RunOutcome::Noop;
        }
        self.push_user(&text);
        self.steer_pending = Some(text.clone());
        RunOutcome::Steer(text)
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{}ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let m = (secs / 60.0).floor() as u64;
        let s = secs % 60.0;
        format!("{m}m{s:.0}s")
    }
}

/// Split input into (prefix, path_token) when the last token looks path-like or is `@…`.
fn path_token_at_end(input: &str) -> Option<(String, String)> {
    let trimmed_end = input.trim_end_matches(|c: char| c == ' ' || c == '\n');
    if trimmed_end.is_empty() {
        return None;
    }
    // Find last whitespace-separated token.
    let start = trimmed_end
        .rfind(|c: char| c.is_whitespace())
        .map(|i| i + 1)
        .unwrap_or(0);
    let token = &trimmed_end[start..];
    if token.is_empty() {
        return None;
    }
    let is_at = token.starts_with('@');
    let path_part = if is_at { &token[1..] } else { token };
    // Only complete when path-ish or @reference.
    if !is_at
        && !path_part.contains('/')
        && !path_part.starts_with('.')
        && !path_part.starts_with('~')
    {
        return None;
    }
    let prefix = input[..input.len() - token.len()].to_string();
    let partial = if is_at {
        format!("@{path_part}")
    } else {
        path_part.to_string()
    };
    Some((prefix, partial))
}

fn list_path_completions(partial: &str) -> Vec<String> {
    let at = partial.starts_with('@');
    let raw = if at { &partial[1..] } else { partial };
    let expanded = expand_tilde(raw);
    let (dir, file_prefix) = if expanded.ends_with('/') || expanded.is_empty() {
        (
            if expanded.is_empty() {
                ".".into()
            } else {
                expanded.clone()
            },
            String::new(),
        )
    } else {
        let path = std::path::Path::new(&expanded);
        match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => (
                if parent.as_os_str().is_empty() {
                    ".".into()
                } else {
                    parent.to_string_lossy().into_owned()
                },
                name.to_string_lossy().into_owned(),
            ),
            _ => (".".into(), expanded.clone()),
        }
    };

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !file_prefix.starts_with('.') {
            continue;
        }
        if !name.starts_with(&file_prefix) {
            continue;
        }
        let mut rendered = if dir == "." {
            name.clone()
        } else if dir.ends_with('/') {
            format!("{dir}{name}")
        } else {
            format!("{dir}/{name}")
        };
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            rendered.push('/');
        }
        if at {
            out.push(format!("@{rendered}"));
        } else {
            out.push(rendered);
        }
    }
    out.sort();
    out
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return home;
        }
    }
    path.to_string()
}

fn longest_common_prefix(items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut prefix = items[0].as_str();
    for s in &items[1..] {
        while !s.starts_with(prefix) {
            if prefix.is_empty() {
                return String::new();
            }
            prefix = &prefix[..prefix.len() - 1];
        }
    }
    prefix.to_string()
}

/// Expand `@path` tokens into fenced file bodies for the model.
pub fn expand_at_files(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        // Token until whitespace.
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        let path_raw = &rest[..end];
        rest = &rest[end..];
        if path_raw.is_empty() {
            out.push('@');
            continue;
        }
        let path = expand_tilde(path_raw);
        match std::fs::read_to_string(&path) {
            Ok(body) => {
                let name = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path_raw);
                out.push_str(&format!(
                    "\n\n--- file: {path_raw} ---\n```\n{body}\n```\n--- end {name} ---\n"
                ));
            }
            Err(_) => {
                // Keep original token if unreadable.
                out.push('@');
                out.push_str(path_raw);
            }
        }
    }
    out.push_str(rest);
    out
}

fn split_tool_text(content: &str) -> (String, String) {
    let content = content.trim();
    if let Some(open) = content.find('(') {
        if content.ends_with(')') && open > 0 {
            let name = content[..open].trim().to_string();
            let inner = content[open + 1..content.len() - 1].trim().to_string();
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return (name, inner);
            }
        }
    }
    let mut parts = content.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("tool").to_string();
    let detail = parts.next().unwrap_or("").to_string();
    (name, detail)
}

pub type InteractiveApp = App;

/// Slash commands handled by the CLI as UI ops (not agent user turns).
fn is_ui_slash(text: &str) -> bool {
    let cmd = text
        .split_whitespace()
        .next()
        .unwrap_or(text)
        .split(':')
        .next()
        .unwrap_or(text);
    matches!(
        cmd,
        "/session"
            | "/resume"
            | "/new"
            | "/name"
            | "/model"
            | "/thinking"
            | "/compact"
            | "/settings"
            | "/skills"
            | "/mcp"
            | "/tree"
            | "/rewind"
            | "/export"
            | "/reload"
            | "/clear"
            | "/help"
            | "/quit"
            | "/exit"
            | "/plan"
            | "/act"
            | "/build"
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        }
    }

    #[test]
    fn enter_submits_prompt() {
        let mut app = App::new("test");
        app.input = "hello".into();
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            RunOutcome::Prompt(t) => assert_eq!(t, "hello"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(app.messages.last().unwrap().content, "hello");
        assert_eq!(app.prompt_history, vec!["hello".to_string()]);
    }

    #[test]
    fn ctrl_g_opens_settings_float() {
        let mut app = App::new("test");
        let out = app.handle_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert!(matches!(out, RunOutcome::Noop));
        let f = app.float.as_ref().expect("settings float open");
        assert_eq!(f.kind, FloatKind::Settings);
        assert!(!f.filtered_entries().is_empty());
    }

    #[test]
    fn settings_models_ctrl_f_fetches_remote_models() {
        let mut app = App::new("test");
        app.settings_provider_focus = "proxy".into();
        app.open_settings_models_for_provider("proxy");

        let out = app.handle_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL));

        assert!(matches!(
            out,
            RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id }) if id == "proxy"
        ));
    }

    #[test]
    fn settings_provider_detail_ctrl_f_fetches_remote_models() {
        let mut app = App::new("test");
        app.open_settings_provider_detail("proxy", "1 model");

        let out = app.handle_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL));

        assert!(matches!(
            out,
            RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id }) if id == "proxy"
        ));
    }

    #[test]
    fn settings_models_ctrl_shift_f_and_legacy_ack_fetch() {
        let mut app = App::new("test");
        app.open_settings_models_for_provider("proxy");

        // Uppercase F + CONTROL (some terminals / Caps Lock).
        let out = app.handle_key(key(KeyCode::Char('F'), KeyModifiers::CONTROL));
        assert!(matches!(
            out,
            RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id }) if id == "proxy"
        ));

        // Legacy Ctrl+F as ASCII ACK (0x06).
        let out = app.handle_key(key(KeyCode::Char('\u{06}'), KeyModifiers::NONE));
        assert!(matches!(
            out,
            RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id }) if id == "proxy"
        ));
    }

    #[test]
    fn settings_models_enter_on_fetch_row() {
        let mut app = App::new("test");
        app.open_settings_models_for_provider("proxy");
        // First row is "Fetch remote models".
        let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            out,
            RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id }) if id == "proxy"
        ));
    }

    #[test]
    fn provider_detail_rows_show_configured_values() {
        let mut app = App::new("test");
        app.set_settings_catalog(
            vec![("proxy".into(), "1 model".into())],
            vec![],
            vec![
                ("proxy:provider_type".into(), "openai-compatible".into()),
                ("proxy:base_url".into(), "https://proxy.example/v1".into()),
                ("proxy:api".into(), "openai-completions".into()),
                ("proxy:api_key".into(), "$PROXY_KEY".into()),
                ("proxy:default_model".into(), "m1".into()),
            ],
        );

        app.open_settings_provider_detail("proxy", "1 model");
        let entries = app.float.as_ref().unwrap().filtered_entries();

        assert!(entries
            .iter()
            .any(|e| e.item.id == "set_provider_type" && e.item.detail == "openai-compatible"));
        assert!(entries
            .iter()
            .any(|e| e.item.id == "set_base_url" && e.item.detail == "https://proxy.example/v1"));
        // api is merged into the protocol select row (no separate set_api row).
        assert!(!entries.iter().any(|e| e.item.id == "set_api"));
    }

    #[test]
    fn settings_remote_model_list_filters_and_adds_model() {
        let mut app = App::new("test");
        app.open_settings_provider_detail("proxy", "1 model");
        app.open_settings_remote_models(
            "proxy",
            vec![
                ("gpt-4.1".into(), "remote".into()),
                ("o3".into(), "remote".into()),
            ],
        );

        let f = app.float.as_mut().expect("remote models float");
        assert_eq!(f.kind, FloatKind::SettingsRemoteModels);
        f.search = "o3".into();
        let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            out,
            RunOutcome::ConfigOp(ConfigOp::ModelAdd {
                spec,
                name,
                context_window: None,
            }) if spec == "proxy:o3" && name.as_deref() == Some("o3")
        ));
    }

    #[test]
    fn provider_api_uses_enum_picker() {
        let mut app = App::new("test");
        app.open_settings_provider_detail("proxy", "1 model");
        let f = app.float.as_mut().expect("provider detail");
        let api_index = f
            .filtered_entries()
            .iter()
            .position(|e| e.item.id == "set_provider_type")
            .unwrap();
        f.selected = api_index;

        let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(out, RunOutcome::Noop));
        let f = app.float.as_ref().expect("api picker");
        assert_eq!(f.kind, FloatKind::SettingsProviderApi);
        assert!(f
            .filtered_entries()
            .iter()
            .any(|e| e.item.id == "api:openai-responses"));
        assert!(f
            .filtered_entries()
            .iter()
            .any(|e| e.item.id == "api:openai-completions"));
        assert!(f
            .filtered_entries()
            .iter()
            .any(|e| e.item.id == "api:anthropic-messages"));
        assert!(f
            .filtered_entries()
            .iter()
            .any(|e| e.item.id == "api:gemini-generate-content"));
    }

    #[test]
    fn provider_api_picker_saves_fixed_values_and_unset() {
        let mut app = App::new("test");
        app.open_settings_provider_detail("proxy", "1 model");
        app.open_settings_provider_api("proxy");
        let f = app.float.as_mut().expect("api picker");
        f.selected = f
            .filtered_entries()
            .iter()
            .position(|e| e.item.id == "api:openai-responses")
            .unwrap();

        let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            out,
            RunOutcome::ConfigOp(ConfigOp::ProviderSet { id, key, value })
                if id == "proxy" && key == "api" && value == "openai-responses"
        ));

        app.open_settings_provider_api("proxy");
        let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            out,
            RunOutcome::ConfigOp(ConfigOp::ProviderSet { id, key, value })
                if id == "proxy" && key == "api" && value.is_empty()
        ));
    }

    #[test]
    fn slash_settings_enter_opens_settings_float() {
        let mut app = App::new("test");
        app.input = "/settings".into();
        app.clamp_slash_selection();
        // Highlight /settings if filtered list has it.
        let rows = app.popup_rows();
        if let Some(i) = rows
            .iter()
            .position(|r| matches!(r, PopupRow::Command(c) if c.name == "/settings"))
        {
            app.slash_selected = i;
        }
        let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(out, RunOutcome::Noop), "got {out:?}");
        let f = app.float.as_ref().expect("settings float after /settings");
        assert_eq!(f.kind, FloatKind::Settings);
    }

    #[test]
    fn up_down_navigates_prompt_history() {
        let mut app = App::new("test");
        app.push_prompt_history("first");
        app.push_prompt_history("second");
        app.input = "draft".into();

        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input, "second");
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input, "first");
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input, "second");
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input, "draft");

        // Ctrl+P / Ctrl+N same as Up/Down.
        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "second");
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn single_esc_clears_draft_into_history() {
        let mut app = App::new("test");
        app.input = "unsent draft".into();
        // One Esc clears immediately (must always feel responsive).
        assert!(matches!(
            app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            RunOutcome::Noop
        ));
        assert!(app.input.is_empty());
        assert_eq!(
            app.prompt_history.last().map(String::as_str),
            Some("unsent draft")
        );
        app.history_prev();
        assert_eq!(app.input, "unsent draft");
    }

    #[test]
    fn double_esc_empty_opens_rewind() {
        let mut app = App::new("test");
        assert!(app.input.is_empty());
        assert!(matches!(
            app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            RunOutcome::Noop
        ));
        assert!(matches!(
            app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            RunOutcome::OpenRewind
        ));
    }

    #[test]
    fn single_esc_empty_does_not_open_rewind() {
        let mut app = App::new("test");
        assert!(matches!(
            app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            RunOutcome::Noop
        ));
        // No second Esc — must not open rewind on a lonely press.
        assert!(!matches!(
            app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            RunOutcome::OpenRewind
        ));
    }

    #[test]
    fn load_prompt_history_enables_cross_session_recall() {
        let mut app = App::new("test");
        // Simulate startup load from previous sessions / disk.
        app.load_prompt_history(vec![
            "from last session".into(),
            "another old prompt".into(),
        ]);
        assert_eq!(app.prompt_history_len(), 2);
        app.input.clear();
        app.history_prev();
        assert_eq!(app.input, "another old prompt");
        app.history_prev();
        assert_eq!(app.input, "from last session");
    }

    #[test]
    fn multi_line_selection_range_and_text() {
        let mut app = App::new("t");
        app.chat_total_lines = 5;
        app.chat_line_text = vec![
            "line-0".into(),
            "line-1".into(),
            "line-2".into(),
            "line-3".into(),
            "line-4".into(),
        ];
        app.select_anchor = Some(1);
        app.select_end = Some(3);
        assert!(app.selection_is_multi_line());
        assert_eq!(app.selection_range(), Some((1, 3)));
        let text = app.selection_text().unwrap();
        assert_eq!(text, "line-1\nline-2\nline-3");
        assert!(app.request_copy_selection());
        assert_eq!(
            app.clipboard_pending.as_deref(),
            Some("line-1\nline-2\nline-3")
        );
    }

    #[test]
    fn page_up_disables_follow() {
        let mut app = App::new("test");
        for i in 0..20 {
            app.push_system(format!("m{i}"));
        }
        assert!(app.follow_bottom);
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(!app.follow_bottom);
        app.scroll_to_bottom();
        assert!(app.follow_bottom);
    }

    #[test]
    fn paste_preserves_newlines() {
        let mut app = App::new("test");
        app.handle_paste("foo\nbar");
        assert_eq!(app.input, "foo\nbar");
    }

    #[test]
    fn alt_h_opens_help_even_with_draft() {
        let mut app = App::new("test");
        app.input = "draft".into();
        app.input_cursor = app.input.chars().count();
        app.handle_key(key(KeyCode::Char('h'), KeyModifiers::ALT));
        assert!(app.float_open());
        assert_eq!(
            app.float.as_ref().map(|f| f.kind),
            Some(crate::float::FloatKind::Help)
        );
        assert_eq!(app.input, "draft", "Alt+H must not mutate the draft");
    }

    #[test]
    fn alt_h_uppercase_opens_help() {
        let mut app = App::new("test");
        app.handle_key(key(KeyCode::Char('H'), KeyModifiers::ALT));
        assert!(app.float_open());
        assert_eq!(
            app.float.as_ref().map(|f| f.kind),
            Some(crate::float::FloatKind::Help)
        );
    }

    #[test]
    fn help_fallback_chords_still_open_help() {
        // Silent fallbacks (still work, not shown on status strip).
        let cases = [
            (KeyCode::Char('k'), KeyModifiers::CONTROL),
            (KeyCode::Char('K'), KeyModifiers::CONTROL),
            (KeyCode::Char('\u{0b}'), KeyModifiers::NONE),
            (KeyCode::F(1), KeyModifiers::NONE),
            (KeyCode::Char('/'), KeyModifiers::CONTROL),
            (KeyCode::Char('_'), KeyModifiers::CONTROL),
            (KeyCode::Char('\u{1f}'), KeyModifiers::NONE),
        ];
        for (code, mods) in cases {
            let mut app = App::new("test");
            app.handle_key(key(code, mods));
            assert!(app.float_open(), "expected help for {code:?} {mods:?}");
            assert_eq!(
                app.float.as_ref().map(|f| f.kind),
                Some(crate::float::FloatKind::Help)
            );
        }
    }

    #[test]
    fn question_mark_is_plain_text() {
        let mut app = App::new("test");
        app.handle_key(key(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(!app.float_open());
        assert_eq!(app.input, "?");
    }

    #[test]
    fn bare_slash_still_opens_slash_menu() {
        let mut app = App::new("test");
        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(!app.float_open());
        assert_eq!(app.input, "/");
        assert!(app.slash_menu_visible());
    }

    #[test]
    fn paste_into_float_edit_does_not_touch_main_input() {
        let mut app = App::new("test");
        app.input = "draft".into();
        app.float = Some(FloatMenu::settings_provider_detail(
            "linuxdo",
            "custom",
            &[("base_url".into(), "unset".into())],
        ));
        app.start_settings_inline_edit("provider_set:linuxdo:base_url", "base_url", "");
        assert!(app.float_open());
        assert!(!app.prompt_focused());

        app.handle_paste("https://api.example.com/v1\n");
        assert_eq!(app.input, "draft", "main prompt must stay untouched");
        let search = app.float.as_ref().map(|f| f.search.clone()).unwrap();
        assert_eq!(search, "https://api.example.com/v1");
        assert!(app.float.as_ref().is_some_and(|f| f.edit_mode));
    }

    #[test]
    fn main_input_left_right_moves_cursor_and_inserts_mid() {
        let mut app = App::new("test");
        for ch in "hello".chars() {
            app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(app.input, "hello");
        assert_eq!(app.input_cursor, 5);

        // ←←← → caret before "llo"
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, 2);

        app.handle_key(key(KeyCode::Char('X'), KeyModifiers::NONE));
        assert_eq!(app.input, "heXllo");
        assert_eq!(app.input_cursor, 3);

        // Backspace deletes before caret.
        app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "hello");
        assert_eq!(app.input_cursor, 2);

        // Delete removes after caret.
        app.handle_key(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(app.input, "helo");
        assert_eq!(app.input_cursor, 2);

        // Right to end, then right stays at end.
        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, app.input.chars().count());
        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, app.input.chars().count());
    }

    #[test]
    fn float_edit_left_right_moves_cursor_not_back() {
        let mut app = App::new("test");
        app.float = Some(FloatMenu::settings_provider_detail(
            "linuxdo",
            "custom",
            &[],
        ));
        app.start_settings_inline_edit(
            "provider_set:linuxdo:base_url",
            "base_url",
            "https://api.example.com",
        );
        let end = app.float.as_ref().unwrap().search_cursor;
        assert_eq!(end, "https://api.example.com".chars().count());

        // Left must move caret, not leave edit mode / pop the float.
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        assert!(app.float_open());
        assert!(app.settings_inline_op.is_some());
        assert!(app.float.as_ref().is_some_and(|f| f.edit_mode));
        assert_eq!(app.float.as_ref().unwrap().search_cursor, end - 3);

        // Insert in the middle (caret is 3 chars before end → before "com").
        app.handle_key(key(KeyCode::Char('X'), KeyModifiers::NONE));
        assert_eq!(
            app.float.as_ref().map(|f| f.search.as_str()),
            Some("https://api.example.Xcom")
        );

        // Home / End
        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.float.as_ref().unwrap().search_cursor, 0);
        app.handle_key(key(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(
            app.float.as_ref().unwrap().search_cursor,
            app.float.as_ref().unwrap().search.chars().count()
        );

        // Esc still cancels edit (not left).
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.settings_inline_op.is_none());
        assert!(app.float.as_ref().is_some_and(|f| !f.edit_mode));
    }

    #[test]
    fn paste_into_float_filter_does_not_touch_main_input() {
        let mut app = App::new("test");
        app.input = "hello".into();
        app.open_settings_providers(&[("linuxdo".into(), "ok".into())]);
        app.handle_paste("linu");
        assert_eq!(app.input, "hello");
        assert_eq!(app.float.as_ref().map(|f| f.search.as_str()), Some("linu"));
    }

    fn drain_image_jobs(app: &mut App) {
        for _ in 0..200 {
            app.poll_image_jobs();
            if !app.has_loading_images() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("image job did not finish");
    }

    #[test]
    fn paste_data_uri_inserts_image_token() {
        let mut app = App::new("test");
        let uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        app.handle_paste(uri);
        // Chip appears immediately (loading).
        assert!(
            app.input.contains(one_core::image::IMAGE_TOKEN),
            "input={}",
            app.input
        );
        drain_image_jobs(&mut app);
        assert_eq!(app.pending_images.len(), 1);
        assert_eq!(app.pending_images[0].mime_type, "image/png");
        assert!(!app.pending_images[0].loading);
        let taken = app.take_pending_images();
        assert_eq!(taken.len(), 1);
    }

    #[test]
    fn set_input_for_edit_with_images_restores_chips() {
        let mut app = App::new("test");
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let (path, mime) = one_core::image::store_image_base64(b64, Some("image/png")).unwrap();
        let token = one_core::image::image_token(1);
        app.set_input_for_edit_with_images(
            format!("这个是什么 {token} "),
            vec![(mime.clone(), path.display().to_string())],
        );
        assert!(app.input.contains(&token));
        assert_eq!(app.pending_images.len(), 1);
        assert_eq!(app.pending_images[0].mime_type, "image/png");
        let taken = app.take_pending_images();
        assert_eq!(taken.len(), 1);
        // Simulate submit path: chips present → committed on submit_prompt.
        app.set_input_for_edit_with_images(
            format!("再看 {token}"),
            vec![(mime, path.display().to_string())],
        );
        let outcome = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match outcome {
            RunOutcome::Prompt(p) => {
                assert!(p.contains("再看"), "{p}");
                // Image tokens stripped for agent text; bytes go via take_pending_images.
                assert!(!p.contains("[image ·"), "{p}");
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
        let imgs = app.take_pending_images();
        assert_eq!(imgs.len(), 1);
    }

    #[test]
    fn paste_image_path_file_attaches() {
        let dir = std::env::temp_dir().join(format!("one-tui-img-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("dot.png");
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let bytes = one_core::image::decode_base64(b64).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        let mut app = App::new("test");
        app.handle_paste(path.to_str().unwrap());
        assert!(app.input.contains(one_core::image::IMAGE_TOKEN));
        drain_image_jobs(&mut app);
        assert_eq!(app.pending_images.len(), 1);
        assert_eq!(app.pending_images[0].mime_type, "image/png");
        assert!(!app.pending_images[0].loading);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctrl_v_image_key_shows_placeholder_immediately() {
        let mut app = App::new("test");
        let key = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        let _ = app.handle_key(key);
        // Optimistic chip appears before clipboard work finishes (do not wait
        // for PowerShell — it can hang for seconds under WSL).
        assert!(
            app.input.contains(one_core::image::IMAGE_TOKEN),
            "expected loading chip in input, got {}",
            app.input
        );
        assert!(
            app.pending_images.iter().any(|i| i.loading),
            "expected loading pending image"
        );
        assert!(app.has_loading_images());
        let toast = app.toast.as_ref().map(|t| t.text.as_str()).unwrap_or("");
        assert!(
            toast.contains("pasting"),
            "expected pasting toast, got {toast:?}"
        );
        // Abandon in-flight job (dropping app closes the channel).
    }

    #[test]
    fn submit_blocked_while_image_loading() {
        let mut app = App::new("test");
        let _ = app.begin_loading_image("x.png");
        assert!(app.has_loading_images());
        let outcome = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(outcome, RunOutcome::Noop));
        let toast = app.toast.as_ref().map(|t| t.text.as_str()).unwrap_or("");
        assert!(toast.contains("pasting") || toast.contains("still"), "{toast}");
    }

    #[test]
    fn deleting_image_token_detaches() {
        let mut app = App::new("test");
        let tiny = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        app.attach_image("image/png".into(), tiny.into(), "shot.png".into());
        assert_eq!(app.pending_images.len(), 1);
        // User deletes the whole token from input.
        app.input = "hello only".into();
        app.sync_pending_images();
        assert!(app.pending_images.is_empty());
    }

    #[test]
    fn backspace_removes_image_token_atomically() {
        let mut app = App::new("test");
        app.input = "hello".into();
        let tiny = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        app.attach_image("image/png".into(), tiny.into(), "shot.png".into());
        // input is "hello [图片.img] "
        assert!(app.input.contains(one_core::image::IMAGE_TOKEN));
        assert_eq!(app.pending_images.len(), 1);
        // One Backspace wipes the whole token (+ spaces), not char-by-char.
        app.pop_input();
        assert_eq!(app.input, "hello");
        assert!(app.pending_images.is_empty());
    }

    #[test]
    fn long_paste_becomes_text_chip() {
        let mut app = App::new("test");
        let long = "line\n".repeat(30);
        app.handle_paste(&long);
        assert!(
            app.input.contains(one_core::image::TEXT_TOKEN),
            "input={}",
            app.input
        );
        assert!(!app.input.contains("line\nline"));
        assert_eq!(app.pending_texts.len(), 1);
        assert!(app.pending_texts[0].body.contains("line"));

        // Atomic backspace clears chip + body.
        app.pop_input();
        assert!(app.input.is_empty() || !app.input.contains("文本"));
        assert!(app.pending_texts.is_empty());
    }

    #[test]
    fn submit_expands_text_chip_for_agent() {
        let mut app = App::new("test");
        app.attach_text_blob("SECRET_BODY_XYZ\nsecond".into());
        // Chip already in input with trailing space; append instruction.
        app.input.push_str("summarize");
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            RunOutcome::Prompt(t) => {
                assert!(t.contains("SECRET_BODY_XYZ"), "agent text={t}");
                assert!(t.contains("summarize"), "agent text={t}");
                assert!(!t.contains("文本"), "chip should expand, got {t}");
            }
            other => panic!("unexpected {other:?}"),
        }
        // Transcript stays compact.
        let shown = &app.messages.last().unwrap().content;
        assert!(shown.contains(one_core::image::TEXT_TOKEN), "{shown}");
        assert!(!shown.contains("SECRET_BODY_XYZ"), "{shown}");
    }

    #[test]
    fn submit_image_only_prompt() {
        let mut app = App::new("test");
        let tiny = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        app.attach_image("image/png".into(), tiny.into(), "shot.png".into());
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            RunOutcome::Prompt(t) => assert!(t.is_empty(), "token should be stripped, got {t}"),
            other => panic!("unexpected {other:?}"),
        }
        // Staged for CLI take.
        let taken = app.take_pending_images();
        assert_eq!(taken.len(), 1);
        assert!(app
            .messages
            .last()
            .unwrap()
            .content
            .contains(one_core::image::IMAGE_TOKEN));
    }

    #[test]
    fn ctrl_j_inserts_newline() {
        let mut app = App::new("test");
        app.input = "a".into();
        app.input_cursor = 1;
        app.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "a\n");
    }

    #[test]
    fn busy_esc_aborts_ctrl_c_force_quits() {
        let mut app = App::new("test");
        app.begin_busy();

        // Soft cancel: Esc only (`q` is a normal character).
        app.handle_busy_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.take_abort());
        assert!(!app.force_quit_pending());

        // Bare `q` is steer/follow-up text, not abort.
        app.handle_busy_key(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(app.input, "q");
        assert!(!app.take_abort());
        assert!(!app.force_quit_pending());

        // First Ctrl+C clears steer draft (does not force-quit).
        app.handle_busy_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty());
        assert!(!app.force_quit_pending());

        // Second Ctrl+C force-quits — never soft-cancel only.
        app.handle_busy_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.force_quit_pending());
        assert!(app.take_force_quit());
        // request_force_quit also trips abort so in-flight work stops.
        assert!(app.take_abort());
    }

    #[test]
    fn ask_user_enter_keeps_result_against_prompt_reopen() {
        use crate::select::{SelectOption, SelectPrompt, SelectResult};

        let mut app = App::new("test");
        app.begin_busy();
        let mut prompt = SelectPrompt::single(
            "颜色选择",
            "你想选择哪种颜色?",
            vec![
                SelectOption::new("红色", "红色", ""),
                SelectOption::new("绿色", "绿色", ""),
                SelectOption::new("蓝色", "蓝色", ""),
            ],
        );
        prompt.allow_other = true;
        app.set_select_prompt(SelectKind::AskUser { id: 1 }, prompt);
        assert!(app.select_prompt().is_some());

        // Confirm first option (Enter).
        app.handle_busy_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.select_prompt().is_none(), "dock should close after confirm");

        // Simulate the old buggy drain order: re-surface pending HITL before
        // taking the answer. Must not wipe select_result.
        let mut reopen = SelectPrompt::single(
            "颜色选择",
            "你想选择哪种颜色?",
            vec![
                SelectOption::new("红色", "红色", ""),
                SelectOption::new("绿色", "绿色", ""),
                SelectOption::new("蓝色", "蓝色", ""),
            ],
        );
        reopen.allow_other = true;
        app.set_select_prompt(SelectKind::AskUser { id: 1 }, reopen);

        let (kind, result) = app.take_select_result().expect("result must survive reopen");
        assert!(matches!(kind, SelectKind::AskUser { id: 1 }));
        assert_eq!(
            result,
            SelectResult::Confirmed {
                ids: vec!["红色".into()],
                other: None,
            }
        );
    }

    #[test]
    fn ask_user_tab_enters_other_typing() {
        use crate::select::{SelectOption, SelectPrompt, SelectPhase, SelectResult};

        let mut app = App::new("test");
        app.begin_busy();
        let mut prompt = SelectPrompt::single(
            "颜色选择",
            "你想选择哪种颜色?",
            vec![
                SelectOption::new("红色", "红色", ""),
                SelectOption::new("绿色", "绿色", ""),
            ],
        );
        prompt.allow_other = true;
        app.set_select_prompt(SelectKind::AskUser { id: 7 }, prompt);

        app.handle_busy_key(key(KeyCode::Tab, KeyModifiers::NONE));
        let p = app.select_prompt().expect("still open for typing");
        assert!(matches!(p.phase, SelectPhase::Typing { .. }));
        assert!(p.is_other_row(p.selected));

        app.handle_busy_key(key(KeyCode::Char('紫'), KeyModifiers::NONE));
        app.handle_busy_key(key(KeyCode::Enter, KeyModifiers::NONE));
        let (_, result) = app.take_select_result().unwrap();
        assert_eq!(
            result,
            SelectResult::Confirmed {
                ids: vec![],
                other: Some("紫".into()),
            }
        );
    }

    #[test]
    fn idle_ctrl_c_requires_double_tap_to_quit() {
        let mut app = App::new("test");
        match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
            RunOutcome::Noop => {}
            other => panic!("expected Noop (arm quit), got {other:?}"),
        }
        match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
            RunOutcome::Quit => {}
            other => panic!("expected Quit on second Ctrl+C, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_closes_settings_then_quits() {
        let mut app = App::new("test");
        app.open_settings_float();
        assert!(app.float_open());

        match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
            RunOutcome::Noop => {}
            other => panic!("expected Noop (close float), got {other:?}"),
        }
        assert!(!app.float_open());

        match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
            RunOutcome::Quit => {}
            other => panic!("expected Quit on second Ctrl+C, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_clears_input_then_quits() {
        let mut app = App::new("test");
        app.input = "draft text".into();

        match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
            RunOutcome::Noop => {}
            other => panic!("expected Noop (clear input), got {other:?}"),
        }
        assert!(app.input.is_empty());

        match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
            RunOutcome::Quit => {}
            other => panic!("expected Quit on second Ctrl+C, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_quit_arm_disarmed_by_other_key() {
        let mut app = App::new("test");
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            RunOutcome::Noop
        ));
        // Typing cancels the pending quit arm.
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            RunOutcome::Noop
        ));
        // Next Ctrl+C clears the typed char (does not quit — arm was disarmed).
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            RunOutcome::Noop
        ));
        assert!(app.input.is_empty());
        // Clearing re-arms: one more Ctrl+C quits.
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            RunOutcome::Quit
        ));
    }

    #[test]
    fn expand_at_files_inlines_content() {
        let dir = std::env::temp_dir().join(format!("one-at-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("note.txt");
        std::fs::write(&path, "hello-at-file").unwrap();
        let input = format!("review @{}", path.display());
        let expanded = expand_at_files(&input);
        assert!(expanded.contains("hello-at-file"), "{expanded}");
        assert!(expanded.contains("file:"), "{expanded}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_token_detects_at_and_slash() {
        assert!(path_token_at_end("see @src/").is_some());
        assert!(path_token_at_end("open ./foo").is_some());
        assert!(path_token_at_end("just words").is_none());
    }

    #[test]
    fn stream_sync_marks_assistant_streaming() {
        let mut app = App::new("test");
        app.begin_busy();
        app.append_stream("hi");
        app.sync_stream_message();
        let last = app.messages.last().unwrap();
        assert!(last.streaming);
        assert_eq!(last.content, "hi");
        app.finish_stream();
        assert!(!app.messages.last().unwrap().streaming);
        assert!(app.messages.last().unwrap().footer.is_some());
    }

    #[test]
    fn thinking_stream_then_text() {
        let mut app = App::new("test");
        app.begin_busy();
        app.append_thinking_stream("ponder");
        app.sync_stream_message();
        assert_eq!(app.messages.last().unwrap().role, MessageRole::Thinking);
        assert!(app.messages.last().unwrap().streaming);
        app.append_stream("answer");
        app.sync_stream_message();
        // Thinking finalized, assistant streaming.
        assert_eq!(app.messages.len(), 2);
        assert_eq!(app.messages[0].role, MessageRole::Thinking);
        assert!(!app.messages[0].streaming);
        assert_eq!(app.messages[1].role, MessageRole::Assistant);
        assert_eq!(app.messages[1].content, "answer");
        app.finish_stream();
        assert!(!app.messages[1].streaming);
    }

    #[test]
    fn thinking_tool_thinking_are_separate_segments() {
        // Interleaved think → tool → think must not accumulate prior text
        // into the second bubble (regression: seal forgot to finish thinking).
        let mut app = App::new("test");
        app.begin_busy();
        app.append_thinking_stream("first round plan. ");
        app.sync_stream_message();
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].content, "first round plan. ");

        app.push_tool_call("web_search", "query");
        assert!(!app.messages[0].streaming, "thinking sealed before tool");
        assert!(
            app.thinking_buffer.is_empty(),
            "buffer cleared so next round starts clean"
        );
        assert_eq!(app.messages[0].content, "first round plan. ");
        // Default policy: collapse finished thinking so tool rows stay scannable.
        assert!(
            !app.messages[0].thinking_expanded,
            "finished thinking collapses by default"
        );
        assert_eq!(app.messages.last().unwrap().role, MessageRole::Tool);

        app.append_thinking_stream("second round only.");
        app.sync_stream_message();
        app.finish_stream();

        let thinking: Vec<_> = app
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Thinking)
            .collect();
        assert_eq!(thinking.len(), 2, "one bubble per thinking round");
        assert_eq!(thinking[0].content, "first round plan. ");
        assert_eq!(
            thinking[1].content, "second round only.",
            "second bubble must not re-include first round text"
        );
        assert!(!thinking[1].content.contains("first round"));
        assert!(
            thinking
                .iter()
                .all(|m| !m.thinking_expanded && !m.streaming),
            "both finished segments stay collapsed by default"
        );
    }

    #[test]
    fn finished_thinking_collapses_by_default() {
        let mut app = App::new("test");
        app.begin_busy();
        app.append_thinking_stream("long chain of thought…");
        app.sync_stream_message();
        assert!(app.messages[0].streaming);
        assert!(app.messages[0].thinking_expanded); // live tail while streaming
        app.finish_stream();
        assert!(!app.messages[0].streaming);
        assert!(
            !app.messages[0].thinking_expanded,
            "after stream ends, default is ▸ collapsed header"
        );
        assert!(!app.show_thinking);
    }

    #[test]
    fn ctrl_t_toggles_thinking_visibility() {
        let mut app = App::new("test");
        app.messages.push(Message::thinking("secret plan"));
        // Default: collapsed.
        assert!(!app.show_thinking);
        assert!(!app.messages[0].thinking_expanded);
        match app.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL)) {
            RunOutcome::Noop => {}
            other => panic!("expected Noop, got {other:?}"),
        }
        assert!(app.show_thinking);
        assert!(app.messages[0].thinking_expanded);
        // Toggle back collapses all.
        match app.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL)) {
            RunOutcome::Noop => {}
            other => panic!("expected Noop, got {other:?}"),
        }
        assert!(!app.show_thinking);
        assert!(!app.messages[0].thinking_expanded);
    }

    #[test]
    fn shift_tab_cycles_agent_mode_space_does_not() {
        let mut app = App::new("test");
        // Empty-input Space used to cycle modes — now it types a space.
        assert!(matches!(
            app.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE)),
            RunOutcome::Noop
        ));
        assert_eq!(app.input, " ");
        app.input.clear();

        // Crossterm reports Shift+Tab as BackTab.
        assert!(matches!(
            app.handle_key(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            RunOutcome::CycleAgentMode
        ));
        // Some terminals send Tab+SHIFT instead.
        assert!(matches!(
            app.handle_key(key(KeyCode::Tab, KeyModifiers::SHIFT)),
            RunOutcome::CycleAgentMode
        ));
        // Plain Tab is still completion, not mode cycle.
        assert!(matches!(
            app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE)),
            RunOutcome::Noop
        ));
    }

    #[test]
    fn tool_lifecycle() {
        let mut app = App::new("test");
        app.push_tool_call("bash", "ls");
        assert_eq!(
            app.messages.last().unwrap().tool_status,
            Some(ToolStatus::Running)
        );
        app.finish_tool("bash", false);
        assert_eq!(
            app.messages.last().unwrap().tool_status,
            Some(ToolStatus::Done)
        );
    }

    #[test]
    fn tool_error_auto_expands_output() {
        let mut app = App::new("test");
        app.push_tool_call("bash", "cargo test");
        app.finish_tool_with_output(
            "bash",
            true,
            Some("error: could not compile `one`\n  --> src/lib.rs:1".into()),
        );
        let last = app.messages.last().unwrap();
        assert_eq!(last.tool_status, Some(ToolStatus::Error));
        assert!(last.tool_expanded);
        assert!(last
            .tool_output
            .as_ref()
            .unwrap()
            .contains("could not compile"));
        assert!(last.tool_summary.as_ref().unwrap().contains("error"));
    }

    #[test]
    fn alert_is_ui_only_role() {
        let mut app = App::new("test");
        app.push_error_alert("provider timeout");
        let last = app.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::Alert);
        assert_eq!(last.alert_level, Some(AlertLevel::Error));
    }

    #[test]
    fn edit_tool_gets_diff_summary() {
        let mut app = App::new("test");
        app.push_tool_call(
            "edit",
            r#"{"path":"src/a.rs","old_string":"fn a(){}","new_string":"fn a(){\n  1\n}"}"#,
        );
        app.finish_tool_with_output("edit", false, Some("Updated src/a.rs".into()));
        let last = app.messages.last().unwrap();
        assert_eq!(last.tool_status, Some(ToolStatus::Done));
        let summary = last.tool_summary.as_deref().unwrap_or("");
        assert!(
            summary.contains("edited") || summary.contains("a.rs"),
            "{summary}"
        );
        let out = last.tool_output.as_deref().unwrap_or("");
        assert!(out.contains('+') || out.contains("Updated"), "{out}");
    }

    #[test]
    fn welcome_try_keys_submit_sample_prompts() {
        let mut app = App::new("test");
        assert!(app.messages.is_empty());
        assert!(app.input.is_empty());

        let out = app.handle_key(key(KeyCode::Char('1'), KeyModifiers::NONE));
        match out {
            RunOutcome::Prompt(p) => {
                assert_eq!(p, WELCOME_TRY_PROMPTS[0]);
            }
            other => panic!("expected Prompt from try key, got {other:?}"),
        }
        assert!(app.input.is_empty(), "submit should clear input");
        // submit_prompt already pushed the user turn — digits no longer shortcut.
        assert!(!app.messages.is_empty());

        let out2 = app.handle_key(key(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(matches!(out2, RunOutcome::Noop));
        assert_eq!(app.input, "2");
    }

    #[test]
    fn toast_expires_and_classifies_error() {
        let mut app = App::new("test");
        app.set_notice("error: boom");
        let t = app.toast_active().unwrap();
        assert_eq!(t.level, AlertLevel::Error);
        assert!(t.text.contains("boom"));
        // Force expiry.
        if let Some(toast) = app.toast.as_mut() {
            toast.created = Instant::now() - TOAST_TTL - std::time::Duration::from_secs(1);
        }
        app.tick_toast();
        assert!(app.toast_active().is_none());
    }

    #[test]
    fn three_done_tools_form_collapsible_group() {
        let mut app = App::new("test");
        for (name, args) in [
            ("read", r#"{"path":"a.rs"}"#),
            ("bash", r#"{"command":"ls"}"#),
            ("grep", r#"{"pattern":"x"}"#),
        ] {
            app.push_tool_call(name, args);
            app.finish_tool_with_output(name, false, Some("ok\nline2".into()));
            // Force collapsed body so group can form.
            if let Some(last) = app.messages.last_mut() {
                last.tool_expanded = false;
                last.tool_ungroup = false;
            }
        }
        assert!(tool_view::streak_can_collapse(&app.messages, 0, 3));
        app.toggle_last_tool_expand();
        assert!(app.messages.iter().all(|m| m.tool_ungroup));
        assert!(!tool_view::streak_can_collapse(&app.messages, 0, 3));
    }
}
