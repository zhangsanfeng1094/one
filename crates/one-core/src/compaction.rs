use crate::message::{AgentMessage, ContentBlock, TextOrImage, UserContent};

/// Fraction of the model context window at which auto-compact fires.
pub const DEFAULT_COMPACT_RATIO: f64 = 0.70;
/// Floor so tiny windows still allow a bit of room before compacting.
pub const MIN_COMPACT_THRESHOLD: usize = 16_000;
/// Used when `context_window` is unknown (0).
pub const FALLBACK_COMPACT_THRESHOLD: usize = 80_000;
/// Recent tool-output tokens kept intact when pruning older tool results.
pub const DEFAULT_PRUNE_PROTECT_TOKENS: usize = 40_000;
/// Max chars kept on a pruned tool result body.
pub const DEFAULT_PRUNE_MAX_CHARS: usize = 2_000;
/// Marker left in place of cleared tool output (idempotent for re-prune).
pub const PRUNED_TOOL_PLACEHOLDER: &str = "[Old tool result content cleared]";

#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// When false, auto-compact (threshold path) is a no-op; force still works if caller allows.
    pub enabled: bool,
    /// Fire auto-compact when observed/estimated tokens ≥ this.
    pub token_threshold: usize,
    /// Messages kept verbatim after summary (tail of the transcript).
    pub keep_recent_messages: usize,
    /// Max chars of the fallback extract summary (when LLM summary is unavailable).
    pub max_summary_chars: usize,
    /// When true, as a cheap pre-pass before LLM summary: clear tool *bodies*
    /// that sit **outside** the keep_recent tail (recent turns are never pruned).
    /// Default false — most users only need threshold + keep_recent.
    pub prune: bool,
    /// Within the older (pre-tail) region only: keep about this many tokens of
    /// the newest old tool outputs before clearing older ones (char/4).
    pub prune_protect_tokens: usize,
    /// Max chars retained on a pruned tool result (plus placeholder).
    pub prune_max_chars: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            token_threshold: FALLBACK_COMPACT_THRESHOLD,
            keep_recent_messages: 12,
            max_summary_chars: 6_000,
            prune: false,
            prune_protect_tokens: DEFAULT_PRUNE_PROTECT_TOKENS,
            prune_max_chars: DEFAULT_PRUNE_MAX_CHARS,
        }
    }
}

impl CompactionConfig {
    /// Build config with threshold ≈ `ratio * context_window` (default 70%).
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
    config.enabled && tokens >= config.token_threshold
}

/// Cheap pre-pass before LLM summary (Hermes/OpenCode-style).
///
/// **When:** only if `config.prune` is true (default off) and the session still
/// holds older messages beyond [`CompactionConfig::keep_recent_messages`].
///
/// **What:** clear tool *result bodies* in the **pre-tail** region (everything
/// before the last `keep_recent` messages). The keep_recent **tail is never
/// pruned** — those turns keep full tool outputs.
///
/// Within the pre-tail only, the newest ~[`CompactionConfig::prune_protect_tokens`]
/// of tool output can stay intact so a mid-history tool dump is not wiped if it
/// is still relatively recent among the old messages.
///
/// Returns the number of tool results that were pruned.
pub fn prune_old_tool_outputs(messages: &mut [AgentMessage], config: &CompactionConfig) -> usize {
    if !config.prune {
        return 0;
    }
    let n = messages.len();
    if n == 0 {
        return 0;
    }
    // Hard floor: last keep_recent messages (and any tool results inside them)
    // are never pruned — same boundary as summarization tail.
    let keep = config.keep_recent_messages.max(1);
    let tail_start = n.saturating_sub(keep);
    if tail_start == 0 {
        return 0; // entire buffer is the protected tail
    }

    let protect = config.prune_protect_tokens;
    let max_chars = config.prune_max_chars;
    let mut protected = 0usize;
    let mut pruned = 0usize;

    // Pre-tail tool results only, newest → oldest.
    let mut indices: Vec<usize> = messages[..tail_start]
        .iter()
        .enumerate()
        .filter_map(|(i, m)| matches!(m, AgentMessage::ToolResult(_)).then_some(i))
        .collect();
    indices.reverse();

    for i in indices {
        let Some(AgentMessage::ToolResult(result)) = messages.get_mut(i) else {
            continue;
        };
        let text_len: usize = result
            .content
            .iter()
            .map(|b| match b {
                TextOrImage::Text { text } => text.len(),
                TextOrImage::Image { .. } => 256,
            })
            .sum();
        // Already pruned placeholders cost almost nothing and stay as-is.
        if result.content.iter().all(
            |b| matches!(b, TextOrImage::Text { text } if text.contains(PRUNED_TOOL_PLACEHOLDER)),
        ) {
            continue;
        }
        let est = text_len / 4;
        // Soft budget inside pre-tail: keep the newest old tool dumps first.
        if protected < protect {
            protected = protected.saturating_add(est);
            continue;
        }
        // Prune this older tool result.
        let head = result
            .content
            .iter()
            .find_map(|b| match b {
                TextOrImage::Text { text } if !text.is_empty() => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");
        let preview = if max_chars == 0 || head.is_empty() {
            String::new()
        } else {
            let take = max_chars.min(head.chars().count());
            let s: String = head.chars().take(take).collect();
            if head.chars().count() > take {
                format!("{s}…\n")
            } else {
                format!("{s}\n")
            }
        };
        let body = format!("{preview}{PRUNED_TOOL_PLACEHOLDER}");
        result.content = vec![TextOrImage::Text { text: body }];
        pruned += 1;
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

/// Build a one-shot user prompt asking the model to summarize older turns.
pub fn summarization_prompt(older: &[AgentMessage], custom_instructions: Option<&str>) -> String {
    let extract = extractive_summary(older, 12_000);
    let extra = custom_instructions.unwrap_or(
        "Preserve decisions, file paths, commands run, errors, and unfinished work. Be concise.",
    );
    format!(
        "You are summarizing an earlier portion of a coding-agent conversation for context compaction.\n\
         Write a dense summary that a future assistant can use to continue the work.\n\
         {extra}\n\n\
         --- conversation extract ---\n\
         {extract}\n\
         --- end extract ---\n\n\
         Reply with ONLY the summary, no preamble."
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
        assert_eq!(t, 140_000); // 70%
        assert!(should_compact_tokens(
            140_000,
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
        // Invalid ratio → default 70%.
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
    fn prune_clears_old_tool_outputs() {
        let big = "x".repeat(8_000); // ~2000 tokens
                                     // Layout: [old tool] [filler…] [recent tool in tail]
                                     // keep_recent=2 → last two messages never pruned.
        let mut messages = vec![
            AgentMessage::user_text("start"),
            tool_result("old", &big),
            AgentMessage::user_text("mid"),
            tool_result("recent", &big),
        ];
        let config = CompactionConfig {
            prune: true,
            keep_recent_messages: 2, // protects "mid" + "recent" tool
            prune_protect_tokens: 0, // clear all pre-tail tools
            prune_max_chars: 32,
            ..Default::default()
        };
        let n = prune_old_tool_outputs(&mut messages, &config);
        assert!(n >= 1, "expected at least one pruned tool result");
        // Tail tool result must stay full (keep_recent hard floor).
        if let AgentMessage::ToolResult(r) = &messages[3] {
            let t = match &r.content[0] {
                TextOrImage::Text { text } => text.as_str(),
                _ => "",
            };
            assert!(!t.contains(PRUNED_TOOL_PLACEHOLDER));
            assert_eq!(t.len(), big.len());
        } else {
            panic!("expected tool result");
        }
        // Older (pre-tail) should be pruned.
        if let AgentMessage::ToolResult(r) = &messages[1] {
            let t = match &r.content[0] {
                TextOrImage::Text { text } => text.as_str(),
                _ => "",
            };
            assert!(t.contains(PRUNED_TOOL_PLACEHOLDER));
        } else {
            panic!("expected tool result");
        }
        // Second prune is idempotent.
        assert_eq!(prune_old_tool_outputs(&mut messages, &config), 0);
    }

    #[test]
    fn prune_never_touches_keep_recent_tail() {
        let big = "x".repeat(4_000);
        let mut messages = vec![
            tool_result("a", &big),
            tool_result("b", &big),
            tool_result("c", &big),
        ];
        let config = CompactionConfig {
            prune: true,
            keep_recent_messages: 3, // entire buffer is tail
            prune_protect_tokens: 0,
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
            }
        }
    }

    #[test]
    fn prune_disabled_is_noop() {
        let mut messages = vec![tool_result("t", &"y".repeat(4_000))];
        let config = CompactionConfig {
            prune: false,
            prune_protect_tokens: 0,
            keep_recent_messages: 1,
            ..Default::default()
        };
        assert_eq!(prune_old_tool_outputs(&mut messages, &config), 0);
    }
}
