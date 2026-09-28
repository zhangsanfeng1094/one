//! Lightweight `<system-reminder>` / `<user_query>` helpers (Grok Build style).
//!
//! Runtime notices are tagged user messages, not extra system-prompt prefixes,
//! so they do not bust the cached role + tools prefix. Human turns are wrapped
//! in `<user_query>` so notices sitting on the same role stay distinguishable.

use crate::message::{TextOrImage, UserContent};

/// Open tag (lowercase, hyphenated — matches common agent harnesses).
pub const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";
/// Close tag.
pub const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

/// Human-turn open tag (Grok Build `wrap_user_query`).
pub const USER_QUERY_OPEN: &str = "<user_query>";
/// Human-turn close tag.
pub const USER_QUERY_CLOSE: &str = "</user_query>";

/// Wrap `body` in a system-reminder block. Idempotent if already wrapped.
pub fn system_reminder(body: impl AsRef<str>) -> String {
    let body = body.as_ref().trim();
    if body.is_empty() {
        return format!("{SYSTEM_REMINDER_OPEN}\n{SYSTEM_REMINDER_CLOSE}");
    }
    if body.starts_with(SYSTEM_REMINDER_OPEN) && body.ends_with(SYSTEM_REMINDER_CLOSE) {
        return body.to_string();
    }
    format!("{SYSTEM_REMINDER_OPEN}\n{body}\n{SYSTEM_REMINDER_CLOSE}")
}

/// True if `text` already contains a system-reminder block.
pub fn has_system_reminder(text: &str) -> bool {
    text.contains(SYSTEM_REMINDER_OPEN)
}

/// Append a reminder block under existing content (blank line separator).
pub fn append_system_reminder(content: &str, body: impl AsRef<str>) -> String {
    let body = body.as_ref();
    let reminder = system_reminder(body);
    if content.trim().is_empty() {
        return reminder;
    }
    if has_system_reminder(content) && content.contains(body.trim()) {
        return content.to_string();
    }
    format!("{}\n\n{reminder}", content.trim_end())
}

/// Wrap human text in `<user_query>`. Empty and already-wrapped input are unchanged.
pub fn wrap_user_query(text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    if text.trim().is_empty() || has_user_query(text) || is_system_notice_text(text) {
        return text.to_string();
    }
    format!("{USER_QUERY_OPEN}\n{text}\n{USER_QUERY_CLOSE}")
}

/// True if `text` already contains a user-query block.
pub fn has_user_query(text: &str) -> bool {
    text.contains(USER_QUERY_OPEN)
}

/// Prefer inner `<user_query>` text; otherwise return the trimmed original.
pub fn extract_user_query(text: &str) -> String {
    if let Some(start) = text.find(USER_QUERY_OPEN) {
        let content_start = start + USER_QUERY_OPEN.len();
        if let Some(rel_end) = text[content_start..].find(USER_QUERY_CLOSE) {
            return text[content_start..content_start + rel_end]
                .trim()
                .to_string();
        }
    }
    text.trim().to_string()
}

/// Wrap the first text part of a human user message. Image parts stay as-is.
pub fn wrap_user_content_query(content: &mut UserContent) {
    match content {
        UserContent::Text(text) => {
            *text = wrap_user_query(text.as_str());
        }
        UserContent::Blocks(blocks) => {
            for block in blocks {
                if let TextOrImage::Text { text } = block {
                    *text = wrap_user_query(text.as_str());
                    break;
                }
            }
        }
    }
}

/// Engine-injected user-role notice, not a human prompt.
pub fn is_system_notice_text(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with(SYSTEM_REMINDER_OPEN)
        || trimmed.starts_with(SYSTEM_REMINDER_CLOSE)
        || trimmed.starts_with("[Background task completed]")
        || trimmed.starts_with("[job completed]")
        || trimmed.starts_with("[Monitor stopped]")
        || trimmed.starts_with("[System reminder]")
        || trimmed.starts_with("<env>")
        || trimmed.starts_with("<context>")
        || trimmed.starts_with("<memory-catalog>")
        || trimmed.starts_with("### Learned Tool Intent")
        || trimmed.starts_with("### Graph Intent Guidance")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_is_idempotent() {
        let r = system_reminder("file is empty");
        assert!(r.starts_with(SYSTEM_REMINDER_OPEN));
        assert!(r.ends_with(SYSTEM_REMINDER_CLOSE));
        assert!(r.contains("file is empty"));
        assert_eq!(system_reminder(&r), r);
    }

    #[test]
    fn append_under_content() {
        let out = append_system_reminder("hello", "note");
        assert!(out.starts_with("hello"));
        assert!(has_system_reminder(&out));
        assert!(out.contains("note"));
    }

    #[test]
    fn wrap_user_query_is_idempotent() {
        let wrapped = wrap_user_query("fix the header");
        assert_eq!(wrapped, "<user_query>\nfix the header\n</user_query>");
        assert_eq!(wrap_user_query(&wrapped), wrapped);
        assert_eq!(extract_user_query(&wrapped), "fix the header");
        assert_eq!(extract_user_query("plain"), "plain");
        assert_eq!(wrap_user_query(""), "");
        let notice = system_reminder("MCP servers connected:");
        assert_eq!(wrap_user_query(&notice), notice);
    }
}
