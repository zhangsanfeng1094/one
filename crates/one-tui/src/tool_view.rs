//! Tool transcript helpers: grouping, edit/write previews, diff line paint.

use crate::message::{ToolStatus, Message, MessageRole};

/// Max tools shown as a single collapsed “N tools” chip before forcing expand.
pub const COLLAPSE_GROUP_MIN: usize = 3;

/// Whether this tool row can hide inside a collapsed multi-tool group.
pub fn tool_collapsible(msg: &Message) -> bool {
    msg.role == MessageRole::Tool
        && matches!(msg.tool_status, Some(ToolStatus::Done))
        && !msg.tool_expanded
        && !msg.tool_ungroup
}

/// Consecutive tool messages starting at `start`.
pub fn tool_streak_len(messages: &[Message], start: usize) -> usize {
    let mut n = 0;
    while start + n < messages.len() && messages[start + n].role == MessageRole::Tool {
        n += 1;
    }
    n
}

/// True when the whole streak is done successes and none expanded → show group chip.
pub fn streak_can_collapse(messages: &[Message], start: usize, len: usize) -> bool {
    if len < COLLAPSE_GROUP_MIN {
        return false;
    }
    messages[start..start + len].iter().all(tool_collapsible)
}

/// Short label for a tool in a group header: `bash` / `edit:path`.
pub fn tool_short_label(msg: &Message) -> String {
    let name = msg.tool_name.as_deref().unwrap_or("tool");
    let detail = pretty_path_or_cmd(&msg.content);
    if detail.is_empty() {
        name.to_string()
    } else {
        // Keep group headers skim-friendly.
        let d = if detail.chars().count() > 24 {
            let t: String = detail.chars().take(23).collect();
            format!("{t}…")
        } else {
            detail
        };
        format!("{name}:{d}")
    }
}

fn pretty_path_or_cmd(args: &str) -> String {
    let t = args.trim();
    if !(t.starts_with('{') && t.ends_with('}')) {
        return t.chars().take(40).collect();
    }
    for key in ["path", "file_path", "command", "pattern", "query", "url"] {
        if let Some(v) = json_field(t, key) {
            return v;
        }
    }
    String::new()
}

/// Extract a JSON string field without full serde (args may be partial).
pub fn json_field(obj: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let idx = obj.find(&needle)?;
    let after = &obj[idx + needle.len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim();
    json_string_value(rest)
}

fn json_string_value(s: &str) -> Option<String> {
    let s = s.trim();
    if !s.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = s[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '"' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

/// Build a synthetic unified diff from edit tool args when output lacks one.
pub fn edit_diff_from_args(args: &str) -> Option<String> {
    let path = json_field(args, "path")?;
    let old = json_field(args, "old_string")?;
    let new = json_field(args, "new_string")?;
    Some(format_edit_diff(&path, &old, &new))
}

pub fn format_edit_diff(path: &str, old: &str, new: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("Updated {path}\n"));
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    out.push_str(&format!(
        "@@ -{} +{} @@\n",
        old_lines.len().max(1),
        new_lines.len().max(1)
    ));
    for line in old_lines {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in new_lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Write tool: short content preview from args.
pub fn write_preview_from_args(args: &str) -> Option<String> {
    let path = json_field(args, "path")?;
    let content = json_field(args, "content").unwrap_or_default();
    let n = content.lines().count().max(if content.is_empty() { 0 } else { 1 });
    let bytes = content.len();
    let mut out = format!("Wrote {bytes} bytes → {path} ({n} lines)\n");
    // Preview first few lines as + adds (new file body).
    for (i, line) in content.lines().take(12).enumerate() {
        if i == 0 {
            out.push_str(&format!("+++ b/{path}\n"));
        }
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    let total = content.lines().count();
    if total > 12 {
        out.push_str(&format!("… +{} more lines\n", total - 12));
    }
    Some(out.trim_end().to_string())
}

/// Detect if output looks like a unified / line-based diff.
pub fn looks_like_diff(text: &str) -> bool {
    let mut plus = 0;
    let mut minus = 0;
    for line in text.lines().take(40) {
        if line.starts_with("+++ ") || line.starts_with("--- ") || line.starts_with("@@ ") {
            return true;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            plus += 1;
        }
        if line.starts_with('-') && !line.starts_with("---") {
            minus += 1;
        }
    }
    plus + minus >= 2
}

/// Classify a single output line for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Meta,
    Add,
    Del,
    Context,
    Plain,
}

pub fn classify_diff_line(line: &str) -> DiffLineKind {
    if line.starts_with("+++ ")
        || line.starts_with("--- ")
        || line.starts_with("@@ ")
        || line.starts_with("Updated ")
        || line.starts_with("Wrote ")
    {
        DiffLineKind::Meta
    } else if line.starts_with('+') {
        DiffLineKind::Add
    } else if line.starts_with('-') {
        DiffLineKind::Del
    } else if line.starts_with(' ') {
        DiffLineKind::Context
    } else {
        DiffLineKind::Plain
    }
}

/// Parse `exit N` / `exit signal` prefix from bash tool output.
pub fn parse_bash_exit(output: &str) -> (Option<i64>, &str) {
    let trimmed = output.trim_start();
    if let Some(rest) = trimmed.strip_prefix("exit ") {
        let mut parts = rest.splitn(2, |c: char| c == '\n' || c == '\r');
        let code_tok = parts.next().unwrap_or("").trim();
        let body = parts.next().unwrap_or("").trim_start();
        if code_tok == "signal" {
            return (None, body);
        }
        if let Ok(n) = code_tok.parse::<i64>() {
            return (Some(n), body);
        }
    }
    (None, output)
}

/// Richer summary for edit/write/bash.
///
/// Returns `(summary, auto_expand, optional_rewritten_output)`.
pub fn summarize_tool_special(
    name: &str,
    args: &str,
    output: &str,
    is_error: bool,
) -> Option<(String, bool, Option<String>)> {
    // bash synthesizes its own summary even when is_error (exit ≠ 0).
    if is_error && name != "bash" && name != "shell" {
        return None;
    }
    match name {
        "edit" => {
            if is_error {
                return None;
            }
            let path = json_field(args, "path").unwrap_or_else(|| "file".into());
            let better = if looks_like_diff(output) {
                None
            } else {
                edit_diff_from_args(args)
            };
            let body = better.as_deref().unwrap_or(output);
            let (adds, dels) = count_diff_stats(body);
            let summary = if adds + dels > 0 {
                format!("edited {path} · +{adds} −{dels}")
            } else {
                format!("edited {path}")
            };
            // Auto-expand small edits so the diff is visible.
            let expand = adds + dels > 0 && adds + dels <= 24;
            Some((summary, expand, better))
        }
        "write" => {
            let path = json_field(args, "path").unwrap_or_else(|| "file".into());
            let better = if looks_like_diff(output) {
                None
            } else {
                write_preview_from_args(args)
            };
            let bytes = json_field(args, "content").map(|c| c.len()).unwrap_or(0);
            let summary = format!("wrote {path} · {bytes} B");
            Some((summary, false, better))
        }
        "bash" | "shell" => {
            let (code, body) = parse_bash_exit(output);
            let body_lines = body.lines().filter(|l| !l.is_empty()).count();
            let failed = is_error || matches!(code, Some(c) if c != 0) || code.is_none() && output.starts_with("exit signal");
            let summary = match code {
                Some(0) if !is_error && body_lines == 0 => "exit 0".into(),
                Some(0) if !is_error && body_lines == 1 => {
                    format!("exit 0 · {}", truncate(body.trim(), 40))
                }
                Some(0) if !is_error => format!("exit 0 · {body_lines} lines"),
                Some(c) if body_lines == 0 => format!("exit {c}"),
                Some(c) => format!("exit {c} · {body_lines} lines"),
                None if failed => {
                    let first = body
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("failed");
                    format!("error · {}", truncate(first, 48))
                }
                None if body_lines <= 1 => {
                    format!("ok · {}", truncate(output.trim(), 40))
                }
                None => format!("ok · {body_lines} lines"),
            };
            // Failures auto-expand so stderr is visible mid-transcript.
            let _ = args;
            Some((summary, failed, None))
        }
        "read" => {
            let path = json_field(args, "path")
                .or_else(|| json_field(args, "file_path"))
                .unwrap_or_else(|| "file".into());
            let lines = output.lines().count();
            Some((format!("read {path} · {lines} lines"), false, None))
        }
        _ => None,
    }
}

fn count_diff_stats(text: &str) -> (usize, usize) {
    let mut adds = 0;
    let mut dels = 0;
    for line in text.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            adds += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            dels += 1;
        }
    }
    (adds, dels)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, ToolStatus};

    #[test]
    fn collapse_three_done_tools() {
        let msgs = vec![
            Message::tool("read", r#"{"path":"a"}"#, ToolStatus::Done),
            Message::tool("bash", r#"{"command":"ls"}"#, ToolStatus::Done),
            Message::tool("edit", r#"{"path":"b"}"#, ToolStatus::Done),
        ];
        assert!(streak_can_collapse(&msgs, 0, 3));
    }

    #[test]
    fn no_collapse_with_error() {
        let mut msgs = vec![
            Message::tool("read", "{}", ToolStatus::Done),
            Message::tool("bash", "{}", ToolStatus::Error),
            Message::tool("edit", "{}", ToolStatus::Done),
        ];
        msgs[0].tool_expanded = false;
        assert!(!streak_can_collapse(&msgs, 0, 3));
    }

    #[test]
    fn edit_diff_from_args_works() {
        let args = r#"{"path":"x.rs","old_string":"a","new_string":"b"}"#;
        let d = edit_diff_from_args(args).unwrap();
        assert!(d.contains("-a"));
        assert!(d.contains("+b"));
    }

    #[test]
    fn bash_exit_summary() {
        let (s, expand, _) =
            summarize_tool_special("bash", r#"{"command":"false"}"#, "exit 1\nboom", true)
                .unwrap();
        assert!(s.contains("exit 1"), "{s}");
        assert!(expand);

        let (s0, expand0, _) =
            summarize_tool_special("bash", r#"{"command":"true"}"#, "exit 0", false).unwrap();
        assert!(s0.contains("exit 0"), "{s0}");
        assert!(!expand0);
    }

    #[test]
    fn parse_bash_exit_line() {
        let (c, body) = parse_bash_exit("exit 2\nstderr here");
        assert_eq!(c, Some(2));
        assert_eq!(body, "stderr here");
    }
}
