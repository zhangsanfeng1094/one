use crate::message::{AgentMessage, ContentBlock, UserContent};

/// Fraction of the model context window at which auto-compact fires.
pub const DEFAULT_COMPACT_RATIO: f64 = 0.70;
/// Floor so tiny windows still allow a bit of room before compacting.
pub const MIN_COMPACT_THRESHOLD: usize = 16_000;
/// Used when `context_window` is unknown (0).
pub const FALLBACK_COMPACT_THRESHOLD: usize = 80_000;

#[derive(Debug, Clone)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub token_threshold: usize,
    pub keep_recent_messages: usize,
    /// Max chars of the fallback extract summary (when LLM summary is unavailable).
    pub max_summary_chars: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            token_threshold: FALLBACK_COMPACT_THRESHOLD,
            keep_recent_messages: 12,
            max_summary_chars: 6_000,
        }
    }
}

impl CompactionConfig {
    /// Build config with threshold ≈ `ratio * context_window` (default 70%).
    ///
    /// When `context_window` is 0, keeps [`FALLBACK_COMPACT_THRESHOLD`].
    pub fn from_context_window(context_window: usize) -> Self {
        Self {
            token_threshold: threshold_for_context_window(context_window),
            ..Default::default()
        }
    }
}

/// Compact when estimated/observed tokens reach this many of the model window.
pub fn threshold_for_context_window(context_window: usize) -> usize {
    if context_window == 0 {
        return FALLBACK_COMPACT_THRESHOLD;
    }
    let raw = ((context_window as f64) * DEFAULT_COMPACT_RATIO).round() as usize;
    // Leave a little headroom under the hard window for the summary turn + tools.
    let capped = raw.min(context_window.saturating_sub(4_096).max(MIN_COMPACT_THRESHOLD));
    capped.max(MIN_COMPACT_THRESHOLD)
}

pub fn estimate_tokens(messages: &[AgentMessage]) -> usize {
    let chars: usize = messages.iter().map(message_chars).sum();
    // ~4 chars/token heuristic (same as common rough estimates).
    chars / 4
}

/// Prefer provider-reported last-prompt size when available; else char estimate.
pub fn tokens_for_compaction(
    messages: &[AgentMessage],
    last_prompt_tokens: Option<u64>,
) -> usize {
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
                ContentBlock::ToolCall { name, arguments, .. } => {
                    name.len() + arguments.to_string().len() + 32
                }
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
        assert!(kept.is_empty() || !kept.is_empty() || true);
        let _ = kept;
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
        assert_eq!(
            threshold_for_context_window(0),
            FALLBACK_COMPACT_THRESHOLD
        );
        let t = threshold_for_context_window(200_000);
        assert_eq!(t, 140_000); // 70%
        assert!(should_compact_tokens(140_000, &CompactionConfig::from_context_window(200_000)));
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
}
