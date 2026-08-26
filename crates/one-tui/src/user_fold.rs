//! User-message and turn fold rules: long-paste preview, and Q&A collapse.

use crate::message::{Message, MessageRole};

pub const FOLD_LINE_THRESHOLD: usize = 6;
pub const FOLD_CHAR_THRESHOLD: usize = 400;
pub const FOLD_CODE_LINE_THRESHOLD: usize = 10;
pub const PREVIEW_LINES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeBlock {
    /// Line index of the opening fence.
    pub open: usize,
    /// Line index of the closing fence, or the last line if unclosed.
    pub close: usize,
    /// Body lines between fences (excludes the fence lines themselves).
    pub body_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collapse {
    /// Keep the first `keep` source lines; `hidden` source lines follow.
    Text {
        keep: usize,
        hidden: usize,
        total: usize,
        chars: usize,
    },
    /// Keep source lines before the fence; fold the whole code block.
    Code {
        keep_before: usize,
        body_lines: usize,
        hidden: usize,
        total: usize,
    },
}

pub fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

pub fn fence_lang(line: &str) -> &str {
    let t = line.trim_start();
    let rest = if let Some(r) = t.strip_prefix("```") {
        r
    } else if let Some(r) = t.strip_prefix("~~~") {
        r
    } else {
        ""
    };
    rest.trim()
}

pub fn code_blocks(lines: &[&str]) -> Vec<CodeBlock> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if is_fence(lines[i]) {
            let open = i;
            i += 1;
            while i < lines.len() && !is_fence(lines[i]) {
                i += 1;
            }
            let closed = i < lines.len();
            let close = if closed {
                i
            } else {
                lines.len().saturating_sub(1)
            };
            let body_end = if closed { i } else { lines.len() };
            let body_lines = body_end.saturating_sub(open + 1);
            out.push(CodeBlock {
                open,
                close,
                body_lines,
            });
            i = if closed { i + 1 } else { lines.len() };
            continue;
        }
        i += 1;
    }
    out
}

/// Drop injected reminder blocks so fold math matches what the bubble paints.
pub fn visible_user_text(content: &str) -> String {
    let without_sys = strip_tag_blocks(content, "<system-reminder>", "</system-reminder>");
    strip_tag_blocks(&without_sys, "<reminder>", "</reminder>")
        .trim()
        .to_string()
}

fn strip_tag_blocks(text: &str, open: &str, close: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        if let Some(end) = after.find(close) {
            rest = &after[end + close.len()..];
        } else {
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

pub fn should_fold(content: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() > FOLD_LINE_THRESHOLD {
        return true;
    }
    if content.chars().count() > FOLD_CHAR_THRESHOLD {
        return true;
    }
    code_blocks(&lines)
        .iter()
        .any(|b| b.body_lines > FOLD_CODE_LINE_THRESHOLD)
}

/// `user_expanded`: `None` = auto, `Some(true)` = remember open, `Some(false)` = remember shut.
///
/// Current-turn auto path is always expanded. A manual collapse still wins.
pub fn is_folded(user_expanded: Option<bool>, is_current: bool, content: &str) -> bool {
    if !should_fold(content) {
        return false;
    }
    match user_expanded {
        Some(true) => false,
        Some(false) => true,
        None => !is_current,
    }
}

/// A visible user question (not an injected reminder-only row).
pub fn is_real_user(msg: &Message) -> bool {
    msg.role == MessageRole::User && !visible_user_text(&msg.content).is_empty()
}

/// Exclusive end of the turn that starts at `user_idx` (next real user, or len).
pub fn turn_end(messages: &[Message], user_idx: usize) -> usize {
    messages
        .iter()
        .enumerate()
        .skip(user_idx.saturating_add(1))
        .find(|(_, m)| is_real_user(m))
        .map(|(i, _)| i)
        .unwrap_or(messages.len())
}

pub fn turn_has_followup(messages: &[Message], user_idx: usize) -> bool {
    user_idx < messages.len() && turn_end(messages, user_idx) > user_idx + 1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnStats {
    pub tools: usize,
    pub replies: usize,
    pub thinking: usize,
}

pub fn turn_stats(messages: &[Message], user_idx: usize) -> TurnStats {
    let end = turn_end(messages, user_idx);
    let mut stats = TurnStats::default();
    for msg in messages.iter().take(end).skip(user_idx + 1) {
        match msg.role {
            MessageRole::Tool => stats.tools += 1,
            MessageRole::Assistant => stats.replies += 1,
            MessageRole::Thinking => stats.thinking += 1,
            _ => {}
        }
    }
    stats
}

pub fn format_turn_summary(stats: TurnStats) -> String {
    let mut parts: Vec<String> = Vec::new();
    if stats.tools > 0 {
        parts.push(format!("{} 工具", stats.tools));
    }
    if stats.thinking > 0 {
        parts.push("thinking".into());
    }
    if stats.replies > 0 {
        parts.push("回复".into());
    }
    if parts.is_empty() {
        "本轮".into()
    } else {
        parts.join(" · ")
    }
}

/// First non-empty source line — compact title while the turn is collapsed.
pub fn turn_preview(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// `turn_expanded`: `None` = auto (history folded, current open), `Some` = session override.
pub fn is_turn_folded(turn_expanded: Option<bool>, is_current: bool, has_followup: bool) -> bool {
    if !has_followup {
        return false;
    }
    match turn_expanded {
        Some(true) => false,
        Some(false) => true,
        None => !is_current,
    }
}

pub fn collapse_plan(content: &str) -> Option<Collapse> {
    if !should_fold(content) {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len().max(1);
    let chars = content.chars().count();
    if lines.is_empty() {
        return Some(Collapse::Text {
            keep: 0,
            hidden: 0,
            total: 1,
            chars,
        });
    }
    let blocks = code_blocks(&lines);
    let take = PREVIEW_LINES.min(lines.len());
    if let Some(b) = blocks.iter().find(|b| b.open < take && b.close + 1 > take) {
        let keep_before = b.open;
        let hidden = total.saturating_sub(keep_before);
        return Some(Collapse::Code {
            keep_before,
            body_lines: b.body_lines,
            hidden,
            total,
        });
    }
    Some(Collapse::Text {
        keep: take,
        hidden: total.saturating_sub(take),
        total,
        chars,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_does_not_fold() {
        assert!(!should_fold("hello"));
        assert!(!should_fold("a\nb\nc\nd\ne\nf"));
        assert!(collapse_plan("hello").is_none());
    }

    #[test]
    fn seven_lines_fold() {
        let s = (0..7)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(should_fold(&s));
        match collapse_plan(&s) {
            Some(Collapse::Text {
                keep,
                hidden,
                total,
                ..
            }) => {
                assert_eq!(keep, 3);
                assert_eq!(hidden, 4);
                assert_eq!(total, 7);
            }
            other => panic!("expected text collapse, got {other:?}"),
        }
    }

    #[test]
    fn long_chars_fold() {
        let s = "x".repeat(401);
        assert!(should_fold(&s));
        match collapse_plan(&s) {
            Some(Collapse::Text {
                keep,
                hidden,
                chars,
                ..
            }) => {
                assert_eq!(keep, 1);
                assert_eq!(hidden, 0);
                assert_eq!(chars, 401);
            }
            other => panic!("expected text collapse, got {other:?}"),
        }
    }

    #[test]
    fn code_block_over_ten_folds_as_unit() {
        let mut body = String::from("look at this:\n```\n");
        for i in 0..14 {
            body.push_str(&format!("e{i}\n"));
        }
        body.push_str("```\nplease");
        assert!(should_fold(&body));
        match collapse_plan(&body) {
            Some(Collapse::Code {
                keep_before,
                body_lines,
                ..
            }) => {
                assert_eq!(keep_before, 1);
                assert_eq!(body_lines, 14);
            }
            other => panic!("expected code collapse, got {other:?}"),
        }
    }

    #[test]
    fn does_not_cut_inside_fence() {
        let s = "one\ntwo\n```\na\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n```";
        match collapse_plan(s) {
            Some(Collapse::Code {
                keep_before,
                body_lines,
                ..
            }) => {
                assert_eq!(keep_before, 2);
                assert_eq!(body_lines, 11);
            }
            other => panic!("expected rewind to fence, got {other:?}"),
        }
    }

    #[test]
    fn current_turn_auto_expands() {
        let s = (0..10)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!is_folded(None, true, &s));
        assert!(is_folded(None, false, &s));
        assert!(!is_folded(Some(true), false, &s));
        assert!(is_folded(Some(false), true, &s));
    }

    #[test]
    fn short_never_folds_even_if_overridden() {
        assert!(!is_folded(Some(false), false, "hi"));
    }

    #[test]
    fn historical_turns_auto_fold_when_they_have_followup() {
        assert!(!is_turn_folded(None, true, true));
        assert!(is_turn_folded(None, false, true));
        assert!(!is_turn_folded(None, false, false));
        assert!(!is_turn_folded(Some(true), false, true));
        assert!(is_turn_folded(Some(false), true, true));
    }

    #[test]
    fn turn_range_and_summary() {
        let messages = vec![
            Message::user("q1"),
            Message::tool("bash", "ls", crate::message::ToolStatus::Done),
            Message::assistant("ok"),
            Message::user("q2"),
        ];
        assert!(is_real_user(&messages[0]));
        assert_eq!(turn_end(&messages, 0), 3);
        assert!(turn_has_followup(&messages, 0));
        assert!(!turn_has_followup(&messages, 3));
        let stats = turn_stats(&messages, 0);
        assert_eq!(stats.tools, 1);
        assert_eq!(stats.replies, 1);
        assert_eq!(format_turn_summary(stats), "1 工具 · 回复");
        assert_eq!(turn_preview("  \nFix the leak\nmore"), "Fix the leak");
    }
}
