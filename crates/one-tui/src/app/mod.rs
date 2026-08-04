//! Application state and core event handling for the interactive chat TUI.
//!
//! Split by concern into child modules (same pattern as [`crate::settings`]):
//! each file continues `impl App` so private fields stay accessible.
//!
//! Public types live in [`crate::state`]; Settings / MCP / skills float
//! navigation lives in [`crate::settings`].

mod chrome;
mod helpers;
mod history;
mod hitl;
mod input;
mod busy_keys;
mod float_keys;
mod keys;
mod submit;
mod transcript;
mod view;

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use crate::float::FloatMenu;
use crate::message::{AlertLevel, ChatLineTarget, Message};
use crate::slash::ModelChoice;
use crate::state::{
    ApprovalAnswer, ApprovalPrompt, ModelDraft, PendingImage, PendingText, RunOutcome, SelectKind,
    SelectPos, SettingsDeleteTarget, Toast,
};

pub use helpers::expand_at_files;

/// Background clipboard/image import job (placeholder chip already in the input).
pub(crate) struct ImagePasteJob {
    pub(crate) id: u32,
    pub(crate) report_err: bool,
    pub(crate) rx: std::sync::mpsc::Receiver<Result<(String, PathBuf, String), String>>,
}

/// Max interval between Esc presses for empty-input rewind (second tap).
/// Longer than a typical key-repeat delay so a deliberate double-tap is easy.
pub(crate) const ESC_DOUBLE_MS: u128 = 900;

/// Max interval between Ctrl+C presses for confirm-quit (second tap).
/// Same window as Esc double-tap so the muscle memory stays consistent.
pub(crate) const CTRL_C_DOUBLE_MS: u128 = 900;

pub(crate) const STATUS_IDLE: &str = "";
pub(crate) const STATUS_BUSY: &str = "";

#[derive(Debug, Clone)]
pub(crate) struct RetryWait {
    pub(crate) retry: usize,
    pub(crate) max_retries: usize,
    pub(crate) ready_at: Instant,
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
    /// Absolute first visible display row while browsing transcript history.
    ///
    /// Must be line-based, not message-based: a single long assistant reply can be
    /// taller than the viewport, and Ratatui `List` cannot partial-scroll one item.
    /// Ignored while `follow_bottom` is true, when the renderer derives the
    /// latest start row from the transcript height.
    pub chat_scroll: usize,
    pub follow_bottom: bool,
    /// Last drawn chat viewport height (rows). Used for PageUp/PageDown page size.
    pub chat_view_height: usize,
    /// Last drawn transcript line count (after wrap).
    pub chat_total_lines: usize,
    /// Parallel to display lines: click target for each transcript line.
    /// `None` = spacer / non-interactive.
    pub chat_line_owners: Vec<Option<ChatLineTarget>>,
    /// Keyboard/mouse focus on a transcript message index (tool / thinking).
    /// Painted as a left rail; navigable with j/k when the prompt is empty.
    pub chat_focus: Option<usize>,
    /// Top of chat viewport in the full line list (updated each draw).
    pub chat_view_start: usize,
    /// Blank rows painted above short transcripts (bottom-pin). Clicks skip these.
    pub chat_top_pad: usize,
    /// Terminal mouse capture is armed (wheel → chat).
    pub mouse_capture: bool,
    /// Left column of chat text content (terminal x). Mouse col − this → display col.
    pub chat_content_x: u16,
    /// In-app transcript selection (character carets on absolute display lines).
    /// App-owned select + OSC 52 copy — does not need native terminal drag-select.
    pub select_anchor: Option<SelectPos>,
    pub select_end: Option<SelectPos>,
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
    /// Pending provider retry, rendered as a countdown while the backoff runs.
    retry_wait: Option<RetryWait>,
    /// Selected **row** index in the popup (may point at a header — navigation skips those).
    pub slash_selected: usize,
    /// Models from registry / models.json for `/model` picker.
    pub model_catalog: Vec<ModelChoice>,
    /// Specs (`provider:id`) that appear in Ctrl+L. `None` / empty = all models.
    pub enabled_models: Option<Vec<String>>,
    /// Provider rows for Settings → Providers (`id`, detail).
    pub settings_provider_rows: Vec<(String, String)>,
    /// Provider field rows for Settings → Provider detail (`provider:key`, display value).
    pub settings_provider_field_rows: Vec<(String, String)>,
    /// Model rows for Settings → Models (`provider:id`, detail).
    pub settings_model_rows: Vec<(String, String)>,
    /// Skills manager rows: `(path, label, detail, enabled)`.
    pub skills_rows: Vec<(String, String, String, bool)>,
    /// Agents catalog rows: `(id, label, detail, path, source)`.
    pub agents_rows: Vec<(String, String, String, String, String)>,
    /// Project / user agent directory paths for the agents panel header.
    pub agents_project_dir: String,
    pub agents_user_dir: String,
    /// Features manager rows: `(id, label, detail, enabled, affects_context)`.
    pub features_rows: Vec<(String, String, String, bool, bool)>,
    /// MCP manager rows: `(name, label, detail, enabled)`.
    pub mcp_rows: Vec<(String, String, String, bool)>,
    /// MCP import candidates: `(name, label, detail, already_owned)`.
    pub mcp_import_rows: Vec<(String, String, String, bool)>,
    /// Short MCP summary for Settings root.
    pub mcp_summary: String,
    /// Effective tool_output max_lines (for Settings UI).
    pub tool_output_max_lines: usize,
    /// Effective tool_output max_bytes (for Settings UI).
    pub tool_output_max_bytes: usize,
    /// Compaction strategy display state (Settings UI).
    pub compaction_auto: bool,
    pub compaction_ratio: f64,
    pub compaction_threshold: Option<usize>,
    pub compaction_keep_recent: usize,
    pub compaction_prune: bool,
    pub compaction_prune_protect: usize,
    pub compaction_prune_max_chars: usize,
    /// Persisted broad permission choice shown by Settings. This is intentionally
    /// separate from the live `PermissionGate`, which is built at session start.
    pub settings_saved_auto_approve: bool,
    /// Persisted path-sandbox choice shown by Settings; applied when a new runtime starts.
    pub settings_saved_sandbox: String,
    /// Status-bar / prompt-meta chip, e.g. `MCP 4/5…`. Empty = hidden.
    pub mcp_chip_text: String,
    /// 0=hide 1=loading 2=ok 3=partial 4=error
    pub mcp_chip_kind: u8,
    /// Status-bar chip for **bash** background only, e.g. `bg:1`. Empty = hidden.
    pub bg_chip_text: String,
    /// 0=hide 1=running 2=idle/done 3=failed mixed 4=error
    pub bg_chip_kind: u8,
    /// Status-bar chip for **subagents** (`task`), e.g. `task:1`. Empty = hidden.
    pub task_chip_text: String,
    /// Same kind codes as `bg_chip_kind`.
    pub task_chip_kind: u8,
    /// Cached `/ps` bash list rows for Esc-back from detail.
    pub bg_ps_list: Vec<(String, String, String, String)>,
    /// Bash task id currently shown in BackgroundDetail.
    pub bg_ps_detail_id: Option<String>,
    /// Cached `/tasks` subagent list rows for Esc-back from detail.
    pub task_list: Vec<(String, String, String, String)>,
    /// Subagent job id currently shown in SubagentDetail.
    pub task_detail_id: Option<String>,
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
    /// Pending destructive Settings action; Enter only executes after its target is typed.
    pub(crate) settings_delete_target: Option<SettingsDeleteTarget>,
    /// Thinking level label: off | low | medium | high.
    pub thinking_level: String,
    /// Context tokens for window %: last provider prompt size when known,
    /// else char/4 message estimate (see `usage_tokens_estimated`).
    pub usage_tokens: usize,
    /// True when `usage_tokens` is a local char/4 estimate (not last API usage).
    pub usage_tokens_estimated: bool,
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
    /// UI outcomes queued while streaming (e.g. `/ps`) for the CLI busy tick.
    busy_ui_queue: VecDeque<RunOutcome>,
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
    /// Submitted prompt history (oldest → newest). Up/Down / Ctrl+P navigate.
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
    /// Workspace cwd used for prompt-history paths and relative path display.
    pub(crate) history_cwd: Option<PathBuf>,
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
            chat_focus: None,
            chat_view_start: 0,
            chat_top_pad: 0,
            mouse_capture: true,
            chat_content_x: 0,
            select_anchor: None,
            select_end: None,
            select_dragging: false,
            chat_line_text: Vec::new(),
            clipboard_pending: None,
            cursor_on: true,
            mode_label: String::new(),
            agent_label: "Build".into(),
            spinner_frame: 0,
            retry_wait: None,
            slash_selected: 0,
            model_catalog: Vec::new(),
            enabled_models: None,
            settings_provider_rows: Vec::new(),
            settings_provider_field_rows: Vec::new(),
            settings_model_rows: Vec::new(),
            skills_rows: Vec::new(),
            agents_rows: Vec::new(),
            agents_project_dir: String::new(),
            agents_user_dir: String::new(),
            features_rows: Vec::new(),
            mcp_rows: Vec::new(),
            mcp_import_rows: Vec::new(),
            mcp_summary: "none".into(),
            tool_output_max_lines: 2000,
            tool_output_max_bytes: 50 * 1024,
            compaction_auto: true,
            compaction_ratio: 0.70,
            compaction_threshold: None,
            compaction_keep_recent: 12,
            compaction_prune: false,
            compaction_prune_protect: 40_000,
            compaction_prune_max_chars: 2_000,
            settings_saved_auto_approve: false,
            settings_saved_sandbox: "workspace-write".into(),
            mcp_chip_text: String::new(),
            mcp_chip_kind: 0,
            bg_chip_text: String::new(),
            bg_chip_kind: 0,
            task_chip_text: String::new(),
            task_chip_kind: 0,
            bg_ps_list: Vec::new(),
            bg_ps_detail_id: None,
            task_list: Vec::new(),
            task_detail_id: None,
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
            settings_delete_target: None,
            thinking_level: "off".into(),
            usage_tokens: 0,
            usage_tokens_estimated: true,
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
            busy_ui_queue: VecDeque::new(),
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
}

pub type InteractiveApp = App;
