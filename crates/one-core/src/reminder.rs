//! Lightweight `<system-reminder>` injection helpers (Grok / Claude style).
//!
//! Reminders are plain text the model can see (tool results or user notices).
//! They are **not** system-prompt prefixes — so they do not bust prompt cache
//! of the fixed role + tools section. Use for empty reads, truncated spills,
//! background task completions, and other runtime corrections.

/// Open tag (lowercase, hyphenated — matches common agent harnesses).
pub const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";
/// Close tag.
pub const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

/// Wrap `body` in a system-reminder block. Idempotent if already wrapped.
pub fn system_reminder(body: impl AsRef<str>) -> String {
    let body = body.as_ref().trim();
    if body.is_empty() {
        return format!("{SYSTEM_REMINDER_OPEN}\n{SYSTEM_REMINDER_CLOSE}");
    }
    if body.contains(SYSTEM_REMINDER_OPEN) {
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
}
