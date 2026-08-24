use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::message::{AgentMessage, ContentBlock, TextOrImage, UserContent};
use crate::reminder::{append_system_reminder, system_reminder};
use serde::{Deserialize, Serialize};

/// Mode of conversation compaction (aligns with `grok-build` CompactionMode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    /// LLM or extractive summary of previous messages (default).
    Summary,
    /// Keep raw transcript pointer with pruned tool outputs.
    Transcript,
    /// Split conversation into discrete persisted segments.
    Segments,
}

impl Default for CompactionMode {
    fn default() -> Self {
        Self::Summary
    }
}

/// Why compact ran. Maps to PreCompact/PostCompact hook matcher `manual` | `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    #[default]
    Auto,
    Manual,
    Overflow,
    ModelSwitch,
}

impl CompactTrigger {
    /// Manual `/compact` and overflow recovery always proceed.
    pub fn force(self) -> bool {
        matches!(self, Self::Manual | Self::Overflow)
    }

    /// Auto-compact suppression does not apply to these triggers.
    pub fn ignore_suppression(self) -> bool {
        matches!(self, Self::Manual | Self::Overflow | Self::ModelSwitch)
    }

    /// hooks.json matcher (`manual` or `auto`), matching Grok.
    pub fn hook_matcher(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto | Self::Overflow | Self::ModelSwitch => "auto",
        }
    }
}

/// Request for a compact pass. `instructions` is `/compact [context]`.
#[derive(Debug, Clone, Default)]
pub struct CompactRequest {
    pub trigger: CompactTrigger,
    pub instructions: Option<String>,
}

impl CompactRequest {
    pub fn auto() -> Self {
        Self {
            trigger: CompactTrigger::Auto,
            instructions: None,
        }
    }

    pub fn manual(instructions: Option<String>) -> Self {
        Self {
            trigger: CompactTrigger::Manual,
            instructions,
        }
    }

    pub fn overflow() -> Self {
        Self {
            trigger: CompactTrigger::Overflow,
            instructions: None,
        }
    }

    pub fn model_switch() -> Self {
        Self {
            trigger: CompactTrigger::ModelSwitch,
            instructions: None,
        }
    }

    pub fn force(&self) -> bool {
        self.trigger.force()
    }
}

/// Suppression states to avoid repeated compaction attempts or infinite loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionSuppression {
    /// No suppression; compaction will fire normally when threshold is reached.
    #[default]
    None,
    /// Suppress compaction for the remainder of the current turn only.
    Turn,
    /// Suppress compaction across turns until explicitly cleared or threshold changes.
    Sticky,
    /// Suppress compaction until next successful LLM sampling turn.
    StickyUntilSuccess,
    /// Suppress due to authentication or provider error.
    Auth,
}

/// Checkpoint saved during compaction to support safe cross-compaction rewinds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionCheckpoint {
    pub prompt_index_at_compaction: usize,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub summary: String,
    #[serde(default)]
    pub mode: CompactionMode,
}

/// Outcome of a background prefire run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefireOutcome {
    Triggered,
    AlreadySatisfied,
    Skipped,
    Suppressed,
}

/// Holds pre-computed Pass-1 NOTE₁ generated during background prefire.
#[derive(Debug, Clone, Default)]
pub struct PrefireCandidate {
    pub prefix_len: usize,
    pub prefix_fingerprint: u64,
    pub prefix_tokens: usize,
    pub candidate_summary: String,
}

/// Session-state snippet injected after a compact (Grok `CompactionStateContext` mini).
#[derive(Debug, Clone, Default)]
pub struct CompactionStateContext {
    pub cwd: String,
    pub plan_active: bool,
    pub plan_path: Option<String>,
    pub edited_paths: Vec<String>,
}

/// Fraction of the model context window at which auto-compact fires (Grok default 85%).
pub const DEFAULT_COMPACT_RATIO: f64 = 0.85;
/// Floor so tiny windows still allow a bit of room before compacting.
pub const MIN_COMPACT_THRESHOLD: usize = 16_000;
/// Used when `context_window` is unknown (0).
pub const FALLBACK_COMPACT_THRESHOLD: usize = 80_000;
/// Recent tool-output tokens kept intact when pruning older tool results (legacy).
pub const DEFAULT_PRUNE_PROTECT_TOKENS: usize = 40_000;
/// Max chars kept on a pruned tool result body (legacy / hard-clear preview).
pub const DEFAULT_PRUNE_MAX_CHARS: usize = 2_000;
/// Marker left in place of cleared tool output (idempotent for re-prune).
pub const PRUNED_TOOL_PLACEHOLDER: &str = "[Old tool result content cleared]";
/// Soft-trim ellipsis between kept head and tail.
pub const SOFT_TRIM_MARKER: &str = "\n…\n";
/// Lead below the auto-compact limit at which two-pass Pass-1 may prefire (Grok 10%).
pub const DEFAULT_PREFIRE_LEAD_RATIO: f64 = 0.10;
/// Deprecated alias: old meaning was "fraction of threshold". Prefer lead ratio.
pub const DEFAULT_PREFIRE_RATIO: f64 = 0.85;
/// Recent user turns whose tool results are never pruned (Grok `keep_last_n_turns`).
pub const DEFAULT_PRUNE_KEEP_LAST_N_TURNS: usize = 3;
/// Character threshold above which old tool results are soft-trimmed.
pub const DEFAULT_PRUNE_SOFT_TRIM_THRESHOLD: usize = 4_000;
pub const DEFAULT_PRUNE_SOFT_TRIM_HEAD: usize = 1_500;
pub const DEFAULT_PRUNE_SOFT_TRIM_TAIL: usize = 1_500;
/// User-turn age after which tool results are replaced with a placeholder.
pub const DEFAULT_PRUNE_HARD_CLEAR_AGE_TURNS: usize = 10;

#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// When false, auto-compact (threshold path) is a no-op; force still works if caller allows.
    pub enabled: bool,
    /// Compaction mode (Summary, Transcript, or Segments).
    pub mode: CompactionMode,
    /// Suppression policy to prevent infinite compaction loops.
    pub suppression: CompactionSuppression,
    /// Fire auto-compact when observed/estimated tokens ≥ this.
    pub token_threshold: usize,
    /// Model context window used to compute prefire lead (0 = unknown).
    pub context_window: usize,
    /// Messages kept verbatim after summary (tail of the transcript).
    pub keep_recent_messages: usize,
    /// Max chars of the fallback extract summary (when LLM summary is unavailable).
    pub max_summary_chars: usize,
    /// When true, prune old tool bodies by user-turn age every compact check.
    pub prune: bool,
    /// Within the older (pre-tail) region only: keep about this many tokens of
    /// the newest old tool outputs before clearing older ones (char/4). Legacy.
    pub prune_protect_tokens: usize,
    /// Max chars retained on a pruned tool result (plus placeholder). Legacy.
    pub prune_max_chars: usize,
    /// Recent user turns whose tool results are never pruned.
    pub prune_keep_last_n_turns: usize,
    pub prune_soft_trim_threshold: usize,
    pub prune_soft_trim_head: usize,
    pub prune_soft_trim_tail: usize,
    pub prune_hard_clear_age_turns: usize,
    /// Opt-in two-pass summarization (Pass1 NOTE₁ + Pass2 final).
    pub two_pass: bool,
    /// Fraction of `context_window` below `token_threshold` at which Pass-1 prefires.
    pub prefire_lead_ratio: f64,
    /// Deprecated: fraction of `token_threshold` at which prune-only prefire ran.
    pub prefire_ratio: f64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: CompactionMode::Summary,
            suppression: CompactionSuppression::None,
            token_threshold: FALLBACK_COMPACT_THRESHOLD,
            context_window: 0,
            keep_recent_messages: 12,
            max_summary_chars: 6_000,
            prune: true,
            prune_protect_tokens: DEFAULT_PRUNE_PROTECT_TOKENS,
            prune_max_chars: DEFAULT_PRUNE_MAX_CHARS,
            prune_keep_last_n_turns: DEFAULT_PRUNE_KEEP_LAST_N_TURNS,
            prune_soft_trim_threshold: DEFAULT_PRUNE_SOFT_TRIM_THRESHOLD,
            prune_soft_trim_head: DEFAULT_PRUNE_SOFT_TRIM_HEAD,
            prune_soft_trim_tail: DEFAULT_PRUNE_SOFT_TRIM_TAIL,
            prune_hard_clear_age_turns: DEFAULT_PRUNE_HARD_CLEAR_AGE_TURNS,
            two_pass: false,
            prefire_lead_ratio: DEFAULT_PREFIRE_LEAD_RATIO,
            prefire_ratio: DEFAULT_PREFIRE_RATIO,
        }
    }
}

impl CompactionConfig {
    /// Build config with threshold ≈ `ratio * context_window` (default 85%).
    ///
    /// When `context_window` is 0, keeps [`FALLBACK_COMPACT_THRESHOLD`].
    pub fn from_context_window(context_window: usize) -> Self {
        Self::from_window_and_ratio(context_window, DEFAULT_COMPACT_RATIO)
    }

    /// Threshold from window × ratio (clamped). Absolute `token_threshold` override
    /// should be applied by the caller after this helper when settings provide one.
    pub fn from_window_and_ratio(context_window: usize, ratio: f64) -> Self {
        Self {
            token_threshold: threshold_for_context_window_ratio(context_window, ratio),
            context_window,
            ..Default::default()
        }
    }
}

/// Compact when estimated/observed tokens reach this many of the model window.
pub fn threshold_for_context_window(context_window: usize) -> usize {
    threshold_for_context_window_ratio(context_window, DEFAULT_COMPACT_RATIO)
}

/// Like [`threshold_for_context_window`] with a custom ratio in `(0, 1]`.
///
/// Invalid ratios fall back to [`DEFAULT_COMPACT_RATIO`].
pub fn threshold_for_context_window_ratio(context_window: usize, ratio: f64) -> usize {
    if context_window == 0 {
        return FALLBACK_COMPACT_THRESHOLD;
    }
    let r = if ratio.is_finite() && ratio > 0.0 && ratio <= 1.0 {
        ratio
    } else {
        DEFAULT_COMPACT_RATIO
    };
    let raw = ((context_window as f64) * r).round() as usize;
    // Leave a little headroom under the hard window for the summary turn + tools.
    let capped = raw.min(
        context_window
            .saturating_sub(4_096)
            .max(MIN_COMPACT_THRESHOLD),
    );
    capped.max(MIN_COMPACT_THRESHOLD)
}

pub fn estimate_tokens(messages: &[AgentMessage]) -> usize {
    let chars: usize = messages.iter().map(message_chars).sum();
    // ~4 chars/token heuristic (same as common rough estimates).
    chars / 4
}

/// Prefer provider-reported last-prompt size when available; else char estimate.
pub fn tokens_for_compaction(messages: &[AgentMessage], last_prompt_tokens: Option<u64>) -> usize {
    match last_prompt_tokens {
        Some(n) if n > 0 => n as usize,
        _ => estimate_tokens(messages),
    }
}

fn message_chars(message: &AgentMessage) -> usize {
    match message {
        AgentMessage::User(user) => match &user.content {
            UserContent::Text(text) => text.len(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .map(|block| match block {
                    crate::message::TextOrImage::Text { text } => text.len(),
                    crate::message::TextOrImage::Image { .. } => 256,
                })
                .sum(),
        },
        AgentMessage::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::Thinking { thinking, .. } => thinking.len(),
                ContentBlock::ToolCall {
                    name, arguments, ..
                } => name.len() + arguments.to_string().len() + 32,
            })
            .sum(),
        AgentMessage::ToolResult(result) => result
            .content
            .iter()
            .map(|block| match block {
                crate::message::TextOrImage::Text { text } => text.len(),
                crate::message::TextOrImage::Image { .. } => 256,
            })
            .sum(),
    }
}

pub fn should_compact(messages: &[AgentMessage], config: &CompactionConfig) -> bool {
    should_compact_tokens(estimate_tokens(messages), config)
}

/// Same as [`should_compact`] but with an already-resolved token count
/// (e.g. from [`tokens_for_compaction`]).
pub fn should_compact_tokens(tokens: usize, config: &CompactionConfig) -> bool {
    if !config.enabled || config.suppression != CompactionSuppression::None {
        return false;
    }
    tokens >= config.token_threshold
}

/// Token count at which two-pass Pass-1 may prefire (below full compact threshold).
///
/// Grok: auto-compact limit minus `lead_percent` of the context window (default 10%).
pub fn prefire_threshold(config: &CompactionConfig) -> usize {
    let lead_r = if config.prefire_lead_ratio.is_finite()
        && config.prefire_lead_ratio > 0.0
        && config.prefire_lead_ratio < 1.0
    {
        config.prefire_lead_ratio
    } else {
        DEFAULT_PREFIRE_LEAD_RATIO
    };
    let lead_tokens = if config.context_window > 0 {
        ((config.context_window as f64) * lead_r).round() as usize
    } else {
        ((config.token_threshold as f64) * lead_r).round() as usize
    };
    config
        .token_threshold
        .saturating_sub(lead_tokens)
        .min(config.token_threshold.saturating_sub(1))
}

/// True when tokens are high enough for prune-only work but not yet full compact.
///
/// Prune now runs every turn when enabled; this flag remains for callers that
/// still want the "prefire band" (legacy).
pub fn should_prefire_prune(tokens: usize, config: &CompactionConfig) -> bool {
    config.enabled
        && config.prune
        && tokens >= prefire_threshold(config)
        && tokens < config.token_threshold
}

/// True when two-pass Pass-1 should start in the background.
pub fn should_prefire_two_pass(tokens: usize, config: &CompactionConfig) -> bool {
    config.enabled
        && config.two_pass
        && config.suppression == CompactionSuppression::None
        && tokens >= prefire_threshold(config)
        && tokens < config.token_threshold
}

fn user_turns_after(messages: &[AgentMessage], index: usize) -> usize {
    messages
        .get(index + 1..)
        .map(|tail| {
            tail.iter()
                .filter(|m| matches!(m, AgentMessage::User(_)))
                .count()
        })
        .unwrap_or(0)
}

fn tool_result_text(result: &crate::message::ToolResultMessage) -> String {
    result
        .content
        .iter()
        .filter_map(|b| match b {
            TextOrImage::Text { text } => Some(text.as_str()),
            TextOrImage::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_hard_cleared(result: &crate::message::ToolResultMessage) -> bool {
    result
        .content
        .iter()
        .all(|b| matches!(b, TextOrImage::Text { text } if text.contains(PRUNED_TOOL_PLACEHOLDER)))
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn take_chars_end(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}

/// Prune old tool result bodies by **user-turn age** (Grok `[compaction.pruning]`).
///
/// - Last [`CompactionConfig::prune_keep_last_n_turns`] user turns: never pruned.
/// - Older than that and over `prune_soft_trim_threshold` chars: keep head + tail.
/// - Age ≥ `prune_hard_clear_age_turns`: replace with [`PRUNED_TOOL_PLACEHOLDER`].
///
/// Returns the number of tool results that were modified.
pub fn prune_old_tool_outputs(messages: &mut [AgentMessage], config: &CompactionConfig) -> usize {
    if !config.prune {
        return 0;
    }
    let keep_turns = config.prune_keep_last_n_turns.max(1);
    let hard_age = config.prune_hard_clear_age_turns.max(keep_turns);
    let soft_threshold = config.prune_soft_trim_threshold;
    let head_n = config.prune_soft_trim_head;
    let tail_n = config.prune_soft_trim_tail;
    let mut pruned = 0usize;

    for i in 0..messages.len() {
        let age = user_turns_after(messages, i);
        let Some(AgentMessage::ToolResult(result)) = messages.get_mut(i) else {
            continue;
        };
        if is_hard_cleared(result) {
            continue;
        }
        if age < keep_turns {
            continue;
        }
        let text = tool_result_text(result);
        if age >= hard_age {
            result.content = vec![TextOrImage::Text {
                text: PRUNED_TOOL_PLACEHOLDER.to_string(),
            }];
            pruned += 1;
            continue;
        }
        let chars = text.chars().count();
        if soft_threshold > 0 && chars > soft_threshold {
            let head = take_chars(&text, head_n);
            let tail = take_chars_end(&text, tail_n);
            let body = format!("{head}{SOFT_TRIM_MARKER}{tail}");
            result.content = vec![TextOrImage::Text { text: body }];
            pruned += 1;
        }
    }
    pruned
}

/// Split messages into (older to summarize, recent to keep).
pub fn split_for_compaction<'a>(
    messages: &'a [AgentMessage],
    config: &CompactionConfig,
) -> Option<(&'a [AgentMessage], &'a [AgentMessage])> {
    if messages.len() <= config.keep_recent_messages {
        return None;
    }
    let split = messages.len() - config.keep_recent_messages;
    // Never split in the middle of a tool-call / tool-result pair: walk back so
    // the first kept message is not an orphan toolResult.
    let mut split = split;
    while split > 0 {
        if matches!(messages.get(split), Some(AgentMessage::ToolResult(_))) {
            split -= 1;
            continue;
        }
        break;
    }
    if split == 0 {
        return None;
    }
    Some(messages.split_at(split))
}

/// Local extractive summary used when LLM summarization is unavailable.
pub fn extractive_summary(older: &[AgentMessage], max_chars: usize) -> String {
    let mut lines = Vec::new();
    for message in older {
        let line = match message {
            AgentMessage::User(user) => {
                let text = user_text(user);
                format!("User: {}", truncate(&text, 400))
            }
            AgentMessage::Assistant(assistant) => {
                let text = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let tools: Vec<_> = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall { name, .. } => Some(name.as_str()),
                        _ => None,
                    })
                    .collect();
                if tools.is_empty() {
                    format!("Assistant: {}", truncate(&text, 400))
                } else {
                    format!(
                        "Assistant (tools: {}): {}",
                        tools.join(", "),
                        truncate(&text, 200)
                    )
                }
            }
            AgentMessage::ToolResult(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        crate::message::TextOrImage::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                format!(
                    "ToolResult[{}{}]: {}",
                    result.tool_name,
                    if result.is_error { " ERROR" } else { "" },
                    truncate(&text, 200)
                )
            }
        };
        if !line.trim().is_empty() {
            lines.push(line);
        }
    }

    let body = lines.join("\n");
    format!(
        "Earlier conversation summary ({} messages):\n{}",
        older.len(),
        truncate(&body, max_chars)
    )
}

/// Format conversation turns for the summarizer (not an extractive dump).
///
/// Thinking blocks are dropped; images become `[image]`; tool calls flatten
/// to `[Called tools: …]` like Grok's compacted-history prep.
pub fn format_transcript(messages: &[AgentMessage], max_chars: usize) -> String {
    let mut lines = Vec::new();
    for message in messages {
        match message {
            AgentMessage::User(user) => {
                let text = user_text_with_images(user);
                if !text.trim().is_empty() {
                    lines.push(format!("User: {text}"));
                }
            }
            AgentMessage::Assistant(assistant) => {
                let tools: Vec<&str> = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall { name, .. } => Some(name.as_str()),
                        _ => None,
                    })
                    .collect();
                let text = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                if !tools.is_empty() {
                    lines.push(format!("[Called tools: {}]", tools.join(", ")));
                }
                if !text.trim().is_empty() {
                    lines.push(format!("Assistant: {text}"));
                }
            }
            AgentMessage::ToolResult(result) => {
                let text = tool_result_text(result);
                let err = if result.is_error { " ERROR" } else { "" };
                lines.push(format!(
                    "ToolResult[{}{err}]: {}",
                    result.tool_name,
                    truncate(&text, 400)
                ));
            }
        }
    }
    truncate(&lines.join("\n"), max_chars)
}

/// Fingerprint a prefix so a cached NOTE₁ can be reused only if it still matches.
pub fn prefix_fingerprint(messages: &[AgentMessage]) -> u64 {
    let mut hasher = DefaultHasher::new();
    messages.len().hash(&mut hasher);
    for message in messages {
        match message {
            AgentMessage::User(user) => {
                0u8.hash(&mut hasher);
                user_text(user).hash(&mut hasher);
            }
            AgentMessage::Assistant(assistant) => {
                1u8.hash(&mut hasher);
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text } => {
                            0u8.hash(&mut hasher);
                            text.hash(&mut hasher);
                        }
                        ContentBlock::ToolCall {
                            name, arguments, ..
                        } => {
                            1u8.hash(&mut hasher);
                            name.hash(&mut hasher);
                            arguments.to_string().hash(&mut hasher);
                        }
                        ContentBlock::Thinking { .. } => {}
                    }
                }
            }
            AgentMessage::ToolResult(result) => {
                2u8.hash(&mut hasher);
                result.tool_name.hash(&mut hasher);
                result.is_error.hash(&mut hasher);
                tool_result_text(result).len().hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// Paths touched by `write` / `edit` / `search_replace` in this transcript.
pub fn edited_paths_from_messages(messages: &[AgentMessage]) -> Vec<String> {
    let mut paths = Vec::new();
    for message in messages {
        let AgentMessage::Assistant(assistant) = message else {
            continue;
        };
        for block in &assistant.content {
            let ContentBlock::ToolCall {
                name, arguments, ..
            } = block
            else {
                continue;
            };
            if !matches!(name.as_str(), "write" | "edit" | "search_replace") {
                continue;
            }
            let path = arguments
                .get("path")
                .or_else(|| arguments.get("file_path"))
                .or_else(|| arguments.get("filePath"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if let Some(path) = path {
                if !paths.iter().any(|p| p == &path) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

/// System-reminder body attached to a compaction summary.
pub fn format_compaction_reminder(ctx: &CompactionStateContext) -> String {
    let mut lines = vec![
        "This session was compacted. The summary above replaces older turns; continue from the recent messages after it.".to_string(),
    ];
    if !ctx.cwd.is_empty() {
        lines.push(format!("cwd: {}", ctx.cwd));
    }
    if ctx.plan_active {
        match &ctx.plan_path {
            Some(p) if !p.is_empty() => {
                lines.push(format!("Plan mode is active. Plan file: {p}"));
            }
            _ => lines.push("Plan mode is active.".into()),
        }
    }
    if !ctx.edited_paths.is_empty() {
        let shown: Vec<&str> = ctx
            .edited_paths
            .iter()
            .map(String::as_str)
            .take(24)
            .collect();
        let extra = ctx.edited_paths.len().saturating_sub(shown.len());
        let list = shown.join(", ");
        if extra > 0 {
            lines.push(format!("Files edited this session: {list} (+{extra} more)"));
        } else {
            lines.push(format!("Files edited this session: {list}"));
        }
    }
    lines.join("\n")
}

/// Wrap summary + reminder the same way session resume rebuilds context.
pub fn compacted_live_messages(summary: &str, kept: Vec<AgentMessage>) -> Vec<AgentMessage> {
    let mut out = Vec::with_capacity(kept.len() + 1);
    out.push(AgentMessage::assistant_text(
        "system",
        "compaction",
        format!("[Compaction summary]\n{summary}"),
    ));
    out.extend(kept);
    out
}

pub fn attach_compaction_reminder(summary: &str, ctx: &CompactionStateContext) -> String {
    let body = format_compaction_reminder(ctx);
    if summary.trim().is_empty() {
        return system_reminder(body);
    }
    append_system_reminder(summary, body)
}

/// Split older messages into a Prefix (Pass 1) and Suffix (Pass 2) for two-pass compaction.
///
/// Pass 1 condenses the oldest turns into a structured note `NOTE₁`.
/// Pass 2 combines `NOTE₁` with recent intermediate context for the final compact summary.
pub fn split_two_pass(older: &[AgentMessage]) -> Option<(&[AgentMessage], &[AgentMessage])> {
    if older.len() < 4 {
        return None;
    }
    let mid = older.len() / 2;
    // Align split away from orphan tool results
    let mut split = mid;
    while split > 0 && split < older.len() {
        if matches!(older.get(split), Some(AgentMessage::ToolResult(_))) {
            split += 1;
            continue;
        }
        break;
    }
    if split == 0 || split >= older.len() {
        return None;
    }
    Some(older.split_at(split))
}

/// Compact messages: returns (summary text, kept recent messages).
/// Summary is extractive (no LLM). Prefer `summarize_messages` prompt + provider for quality.
pub fn compact_messages(
    messages: &[AgentMessage],
    config: &CompactionConfig,
) -> (String, Vec<AgentMessage>) {
    let Some((older, recent)) = split_for_compaction(messages, config) else {
        return (String::new(), messages.to_vec());
    };
    let summary = extractive_summary(older, config.max_summary_chars);
    (summary, recent.to_vec())
}

const DEFAULT_SUMMARY_INSTRUCTIONS: &str =
    "Preserve decisions, file paths, commands run, errors, and unfinished work. Be concise.";

/// Build a one-shot user prompt asking the model to summarize older turns.
///
/// Feeds a real transcript (not an extractive dump). `custom_instructions`
/// come from `/compact [context]`.
pub fn summarization_prompt(older: &[AgentMessage], custom_instructions: Option<&str>) -> String {
    let transcript = format_transcript(older, 24_000);
    let extra = custom_instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SUMMARY_INSTRUCTIONS);
    let user_ctx = custom_instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("\n\nUser-provided context for this compaction:\n{s}\n"))
        .unwrap_or_default();
    format!(
        "You are summarizing an earlier portion of a coding-agent conversation for context compaction.\n\
         Write a dense summary that a future assistant can use to continue the work.\n\
         {extra}{user_ctx}\n\
         --- conversation ---\n\
         {transcript}\n\
         --- end ---\n\n\
         Reply with ONLY the summary, no preamble."
    )
}

/// Pass-1 prompt: oldest prefix → NOTE₁ (used by two-pass + background prefire).
pub fn two_pass_pass1_prompt(prefix: &[AgentMessage]) -> String {
    let transcript = format_transcript(prefix, 16_000);
    format!(
        "You are condensing the OLDEST portion of a coding-agent conversation into NOTE₁.\n\
         Capture decisions, file paths, commands, errors, and unfinished work. Be dense.\n\n\
         --- oldest conversation ---\n\
         {transcript}\n\
         --- end ---\n\n\
         Reply with ONLY NOTE₁, no preamble."
    )
}

/// Pass-2 prompt: NOTE₁ + intermediate suffix → final compact summary.
pub fn two_pass_pass2_prompt(
    note1: &str,
    suffix: &[AgentMessage],
    custom_instructions: Option<&str>,
) -> String {
    let transcript = format_transcript(suffix, 16_000);
    let extra = custom_instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("\n\nUser-provided context for this compaction:\n{s}\n"))
        .unwrap_or_default();
    format!(
        "You are writing the FINAL compaction summary for a coding-agent conversation.\n\
         Combine NOTE₁ (oldest portion, already condensed) with the more recent intermediate turns.\n\
         Preserve decisions, file paths, commands run, errors, and unfinished work. Be concise.{extra}\n\
         --- NOTE₁ ---\n\
         {note1}\n\
         --- end NOTE₁ ---\n\n\
         --- intermediate conversation ---\n\
         {transcript}\n\
         --- end ---\n\n\
         Reply with ONLY the final summary, no preamble."
    )
}

/// Detect provider/API errors that indicate context window overflow.
pub fn is_context_overflow_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "context length",
        "context_length",
        "maximum context",
        "max context",
        "token limit",
        "too many tokens",
        "context window",
        "prompt is too long",
        "prompt too long",
        "exceeds the model",
        "exceeds model",
        "context_length_exceeded",
        "max_tokens",
        "request too large",
        "payload too large",
    ];
    NEEDLES.iter().any(|n| lower.contains(n))
}

fn user_text(user: &crate::message::UserMessage) -> String {
    match &user.content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                crate::message::TextOrImage::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn user_text_with_images(user: &crate::message::UserMessage) -> String {
    match &user.content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                crate::message::TextOrImage::Text { text } => text.clone(),
                crate::message::TextOrImage::Image { .. } => "[image]".into(),
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::AgentMessage;

    #[test]
    fn extractive_not_debug_dump() {
        let messages = vec![
            AgentMessage::user_text("hello world"),
            AgentMessage::assistant_text("mock", "m", "hi there"),
        ];
        let (summary, kept) = compact_messages(
            &messages,
            &CompactionConfig {
                keep_recent_messages: 0,
                ..Default::default()
            },
        );
        // keep_recent 0 → split at len, all older
        assert!(summary.contains("User:"));
        assert!(!summary.contains("UserMessage"));
        assert!(kept.is_empty());
    }

    #[test]
    fn overflow_detection() {
        assert!(is_context_overflow_error(
            "anthropic 400: prompt is too long: 200000 tokens"
        ));
        assert!(is_context_overflow_error("context_length_exceeded"));
        assert!(!is_context_overflow_error("rate limit exceeded"));
    }

    #[test]
    fn does_not_orphan_tool_result() {
        let messages = vec![
            AgentMessage::user_text("a"),
            AgentMessage::assistant_text("p", "m", "ok"),
            AgentMessage::user_text("b"),
            // pretend tool result without pairing care — just ensure split doesn't land on ToolResult
            AgentMessage::user_text("c"),
            AgentMessage::user_text("d"),
        ];
        let config = CompactionConfig {
            keep_recent_messages: 2,
            ..Default::default()
        };
        let (older, recent) = split_for_compaction(&messages, &config).unwrap();
        assert_eq!(recent.len(), 2);
        assert!(!older.is_empty());
    }

    #[test]
    fn threshold_from_context_window() {
        assert_eq!(threshold_for_context_window(0), FALLBACK_COMPACT_THRESHOLD);
        let t = threshold_for_context_window(200_000);
        assert_eq!(t, 170_000); // 85%
        assert!(should_compact_tokens(
            170_000,
            &CompactionConfig::from_context_window(200_000)
        ));
        assert!(!should_compact_tokens(
            100_000,
            &CompactionConfig::from_context_window(200_000)
        ));
    }

    #[test]
    fn prefers_observed_prompt_tokens() {
        let messages = vec![AgentMessage::user_text("short")];
        // Char estimate is tiny; observed says we're already huge.
        let tokens = tokens_for_compaction(&messages, Some(90_000));
        assert_eq!(tokens, 90_000);
        assert!(should_compact_tokens(
            tokens,
            &CompactionConfig {
                token_threshold: 80_000,
                ..Default::default()
            }
        ));
        // Zero observed → fall back to estimate.
        let est = tokens_for_compaction(&messages, Some(0));
        assert_eq!(est, estimate_tokens(&messages));
    }

    #[test]
    fn threshold_custom_ratio() {
        let t = threshold_for_context_window_ratio(100_000, 0.5);
        assert_eq!(t, 50_000);
        // Invalid ratio → default 85%.
        assert_eq!(
            threshold_for_context_window_ratio(100_000, 0.0),
            threshold_for_context_window(100_000)
        );
    }

    fn tool_result(name: &str, body: &str) -> AgentMessage {
        AgentMessage::ToolResult(crate::message::ToolResultMessage {
            tool_call_id: format!("c-{name}"),
            tool_name: name.into(),
            content: vec![TextOrImage::Text { text: body.into() }],
            is_error: false,
            timestamp: 0,
        })
    }

    #[test]
    fn prune_by_user_turn_age() {
        let big = "x".repeat(8_000);
        // user0, tool_old, user1, tool_mid, user2, tool_recent
        // keep_last_n_turns=1 → only tool_recent (age 0) protected.
        let mut messages = vec![
            AgentMessage::user_text("u0"),
            tool_result("old", &big),
            AgentMessage::user_text("u1"),
            tool_result("mid", &big),
            AgentMessage::user_text("u2"),
            tool_result("recent", &big),
        ];
        let config = CompactionConfig {
            prune: true,
            prune_keep_last_n_turns: 1,
            prune_hard_clear_age_turns: 2,
            prune_soft_trim_threshold: 4_000,
            prune_soft_trim_head: 20,
            prune_soft_trim_tail: 20,
            ..Default::default()
        };
        let n = prune_old_tool_outputs(&mut messages, &config);
        assert!(n >= 2, "expected old+mid pruned, got {n}");
        // recent (age 0) stays full
        if let AgentMessage::ToolResult(r) = &messages[5] {
            let t = match &r.content[0] {
                TextOrImage::Text { text } => text.as_str(),
                _ => "",
            };
            assert!(!t.contains(PRUNED_TOOL_PLACEHOLDER));
            assert_eq!(t.len(), big.len());
        } else {
            panic!("expected tool result");
        }
        // old (age 2) hard-cleared
        if let AgentMessage::ToolResult(r) = &messages[1] {
            let t = match &r.content[0] {
                TextOrImage::Text { text } => text.as_str(),
                _ => "",
            };
            assert!(t.contains(PRUNED_TOOL_PLACEHOLDER));
        } else {
            panic!("expected tool result");
        }
        // mid (age 1) soft-trimmed
        if let AgentMessage::ToolResult(r) = &messages[3] {
            let t = match &r.content[0] {
                TextOrImage::Text { text } => text.as_str(),
                _ => "",
            };
            assert!(t.contains(SOFT_TRIM_MARKER));
            assert!(!t.contains(PRUNED_TOOL_PLACEHOLDER));
        } else {
            panic!("expected tool result");
        }
        // Second prune is idempotent on hard-cleared; mid already under threshold.
        assert_eq!(prune_old_tool_outputs(&mut messages, &config), 0);
    }

    #[test]
    fn prune_never_touches_recent_turns() {
        let big = "x".repeat(8_000);
        let mut messages = vec![
            AgentMessage::user_text("a"),
            tool_result("t1", &big),
            AgentMessage::user_text("b"),
            tool_result("t2", &big),
            AgentMessage::user_text("c"),
            tool_result("t3", &big),
        ];
        let config = CompactionConfig {
            prune: true,
            prune_keep_last_n_turns: 3,
            prune_hard_clear_age_turns: 10,
            prune_soft_trim_threshold: 100,
            ..Default::default()
        };
        assert_eq!(prune_old_tool_outputs(&mut messages, &config), 0);
        for m in &messages {
            if let AgentMessage::ToolResult(r) = m {
                let t = match &r.content[0] {
                    TextOrImage::Text { text } => text.as_str(),
                    _ => "",
                };
                assert!(!t.contains(PRUNED_TOOL_PLACEHOLDER));
                assert!(!t.contains(SOFT_TRIM_MARKER));
            }
        }
    }

    #[test]
    fn prune_disabled_is_noop() {
        let mut messages = vec![
            AgentMessage::user_text("u"),
            tool_result("t", &"y".repeat(4_000)),
            AgentMessage::user_text("v"),
        ];
        let config = CompactionConfig {
            prune: false,
            prune_keep_last_n_turns: 1,
            prune_hard_clear_age_turns: 1,
            ..Default::default()
        };
        assert_eq!(prune_old_tool_outputs(&mut messages, &config), 0);
    }

    #[test]
    fn default_enables_prune() {
        assert!(CompactionConfig::default().prune);
        assert!(CompactionConfig::from_context_window(200_000).prune);
        assert!(!CompactionConfig::default().two_pass);
    }

    #[test]
    fn prefire_lead_below_full_threshold() {
        let config = CompactionConfig {
            token_threshold: 170_000,
            context_window: 200_000,
            prefire_lead_ratio: 0.10,
            two_pass: true,
            prune: true,
            enabled: true,
            ..Default::default()
        };
        let pre = prefire_threshold(&config);
        assert_eq!(pre, 150_000); // 170k - 10% of 200k
        assert!(should_prefire_two_pass(150_000, &config));
        assert!(should_prefire_two_pass(169_999, &config));
        assert!(!should_prefire_two_pass(149_999, &config));
        assert!(!should_prefire_two_pass(170_000, &config)); // full compact, not prefire
        assert!(should_compact_tokens(170_000, &config));

        let no_two = CompactionConfig {
            two_pass: false,
            ..config.clone()
        };
        assert!(!should_prefire_two_pass(160_000, &no_two));

        let suppressed = CompactionConfig {
            suppression: CompactionSuppression::StickyUntilSuccess,
            ..config
        };
        assert!(!should_prefire_two_pass(160_000, &suppressed));
        assert!(!should_compact_tokens(170_000, &suppressed));
    }

    #[test]
    fn transcript_skips_thinking_and_flattens_tools() {
        let messages = vec![
            AgentMessage::user_text("fix the bug"),
            AgentMessage::Assistant(crate::message::AssistantMessage {
                content: vec![
                    ContentBlock::thinking("secret chain of thought"),
                    ContentBlock::ToolCall {
                        id: "1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "a.rs"}),
                    },
                    ContentBlock::text("I will read the file"),
                ],
                provider: "p".into(),
                model: "m".into(),
                stop_reason: crate::message::StopReason::ToolUse,
                citations: vec![],
                timestamp: 0,
            }),
        ];
        let t = format_transcript(&messages, 4_000);
        assert!(t.contains("User: fix the bug"));
        assert!(t.contains("[Called tools: read]"));
        assert!(t.contains("I will read the file"));
        assert!(!t.contains("secret chain of thought"));
    }

    #[test]
    fn summarization_prompt_includes_instructions_and_transcript() {
        let older = vec![AgentMessage::user_text("implement auth")];
        let p = summarization_prompt(&older, Some("keep the auth implementation details"));
        assert!(p.contains("implement auth"));
        assert!(p.contains("keep the auth implementation details"));
        assert!(p.contains("User-provided context"));
        assert!(!p.contains("UserMessage"));
    }

    #[test]
    fn reminder_and_live_shape() {
        let ctx = CompactionStateContext {
            cwd: "/tmp/proj".into(),
            plan_active: true,
            plan_path: Some("/tmp/proj/plan.md".into()),
            edited_paths: vec!["src/lib.rs".into()],
        };
        let body = format_compaction_reminder(&ctx);
        assert!(body.contains("cwd: /tmp/proj"));
        assert!(body.contains("Plan mode"));
        assert!(body.contains("src/lib.rs"));
        let summary = attach_compaction_reminder("did auth", &ctx);
        assert!(summary.contains("did auth"));
        assert!(summary.contains("<system-reminder>"));
        let live = compacted_live_messages(&summary, vec![AgentMessage::user_text("next")]);
        assert_eq!(live.len(), 2);
        match &live[0] {
            AgentMessage::Assistant(a) => {
                let t = match &a.content[0] {
                    ContentBlock::Text { text } => text.as_str(),
                    _ => "",
                };
                assert!(t.starts_with("[Compaction summary]"));
            }
            _ => panic!("expected assistant summary"),
        }
    }

    #[test]
    fn edited_paths_from_write_calls() {
        let msg = AgentMessage::Assistant(crate::message::AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "1".into(),
                name: "write".into(),
                arguments: serde_json::json!({"path": "foo.rs", "contents": "x"}),
            }],
            provider: "p".into(),
            model: "m".into(),
            stop_reason: crate::message::StopReason::ToolUse,
            citations: vec![],
            timestamp: 0,
        });
        assert_eq!(edited_paths_from_messages(&[msg]), vec!["foo.rs"]);
    }

    #[test]
    fn prefix_fingerprint_changes_with_content() {
        let a = vec![AgentMessage::user_text("one")];
        let b = vec![AgentMessage::user_text("two")];
        assert_ne!(prefix_fingerprint(&a), prefix_fingerprint(&b));
        assert_eq!(prefix_fingerprint(&a), prefix_fingerprint(&a));
    }

    #[test]
    fn compact_trigger_matcher() {
        assert_eq!(CompactTrigger::Manual.hook_matcher(), "manual");
        assert_eq!(CompactTrigger::Auto.hook_matcher(), "auto");
        assert_eq!(CompactTrigger::Overflow.hook_matcher(), "auto");
        assert!(CompactTrigger::Manual.force());
        assert!(!CompactTrigger::Auto.force());
        assert!(CompactTrigger::ModelSwitch.ignore_suppression());
    }
}
