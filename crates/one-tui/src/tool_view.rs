//! Tool transcript helpers: grouping, edit/write previews, diff line paint.

use crate::message::{Message, MessageRole, ToolStatus};

/// Max tools shown as a single collapsed “N tools” chip before forcing expand.
pub const COLLAPSE_GROUP_MIN: usize = 3;

/// Base eligibility for multi-tool grouping (ignores expand / ungroup flags).
///
/// Background bash lifecycle tools stay out of chips so start / wait / kill stay visible.
pub fn tool_groupable_base(msg: &Message) -> bool {
    if msg.role != MessageRole::Tool || !matches!(msg.tool_status, Some(ToolStatus::Done)) {
        return false;
    }
    let name = msg.tool_name.as_deref().unwrap_or("");
    if matches!(name, "bash_output" | "bash_kill") {
        return false;
    }
    if name == "bash" || name == "shell" {
        if msg
            .tool_output
            .as_deref()
            .is_some_and(|o| o.contains("Background task started"))
        {
            return false;
        }
        if msg
            .tool_summary
            .as_deref()
            .is_some_and(|s| s.starts_with("bg "))
        {
            return false;
        }
    }
    true
}

/// Whether this tool row can hide inside a collapsed multi-tool group.
pub fn tool_collapsible(msg: &Message) -> bool {
    !msg.tool_expanded && !msg.tool_ungroup && tool_groupable_base(msg)
}

/// Consecutive tool messages starting at `start`.
pub fn tool_streak_len(messages: &[Message], start: usize) -> usize {
    let mut n = 0;
    while start + n < messages.len() && messages[start + n].role == MessageRole::Tool {
        n += 1;
    }
    n
}

/// True when the streak is long enough and every tool is base-groupable.
pub fn streak_group_eligible(messages: &[Message], start: usize, len: usize) -> bool {
    if len < COLLAPSE_GROUP_MIN {
        return false;
    }
    messages[start..start + len]
        .iter()
        .all(tool_groupable_base)
}

/// True when the whole streak is done successes and none expanded → show group chip.
pub fn streak_can_collapse(messages: &[Message], start: usize, len: usize) -> bool {
    if len < COLLAPSE_GROUP_MIN {
        return false;
    }
    messages[start..start + len].iter().all(tool_collapsible)
}

/// Ungrouped multi-tool stack that should show a clickable `▾ N tools` header.
pub fn streak_shows_group_header(messages: &[Message], start: usize, len: usize) -> bool {
    streak_group_eligible(messages, start, len)
        && !streak_can_collapse(messages, start, len)
        && messages[start..start + len]
            .iter()
            .any(|m| m.tool_ungroup)
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

/// Decode a JSON string literal starting at `s` (`"..."`), including escapes.
///
/// Important: a naive `\\` → next-char copy turns `\n` into the letter `n`,
/// which collapses multi-line edit/write args into one giant red/green row.
fn json_string_value(s: &str) -> Option<String> {
    let s = s.trim();
    if !s.starts_with('"') {
        return None;
    }
    // Slice the quoted literal (respecting escapes), then let serde decode it.
    let bytes = s.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i = i.saturating_add(2);
            }
            b'"' => {
                let literal = s.get(..=i)?;
                return serde_json::from_str(literal).ok();
            }
            _ => i += 1,
        }
    }
    None
}

/// Build a synthetic unified diff from edit tool args when output lacks one.
pub fn edit_diff_from_args(args: &str) -> Option<String> {
    let path = json_field(args, "path")
        .or_else(|| json_field(args, "file_path"))
        .or_else(|| json_field(args, "filePath"))?;
    let old = json_field(args, "old_string")
        .or_else(|| json_field(args, "oldString"))
        .or_else(|| json_field(args, "oldText"))?;
    let new = json_field(args, "new_string")
        .or_else(|| json_field(args, "newString"))
        .or_else(|| json_field(args, "newText"))?;
    Some(format_edit_diff(&path, &old, &new))
}

pub fn format_edit_diff(path: &str, old: &str, new: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("Updated {path}\n"));
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    // Proper unified-diff header so IDE gutter numbers start at 1, not at line count.
    out.push_str(&format!(
        "@@ -1,{} +1,{} @@\n",
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
    let n = content
        .lines()
        .count()
        .max(if content.is_empty() { 0 } else { 1 });
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

/// One visual row in an IDE-style edit/write diff (line number + code, no `+/-` chrome).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdeDiffRow {
    pub kind: DiffLineKind,
    /// 1-based file line number to show in the gutter (`None` for meta / unknown).
    pub line_no: Option<u32>,
    /// Code text without unified-diff prefix.
    pub text: String,
}

/// Parse unified / line-based tool output into IDE-style rows with line numbers.
///
/// Skips `Updated` / `---` / `+++` / `@@` headers so the transcript looks like a
/// Cursor/VS Code inline diff: gutter numbers + red/green body.
pub fn parse_ide_diff_rows(text: &str) -> Vec<IdeDiffRow> {
    let mut rows = Vec::new();
    let mut old_ln: u32 = 1;
    let mut new_ln: u32 = 1;
    let mut in_hunk = false;

    for line in text.lines() {
        if let Some((o, n)) = parse_hunk_header(line) {
            old_ln = o;
            new_ln = n;
            in_hunk = true;
            continue;
        }
        if line.starts_with("+++ ")
            || line.starts_with("--- ")
            || line.starts_with("Updated ")
            || line.starts_with("Wrote ")
            || line.starts_with("diff --git ")
            || line.starts_with("index ")
        {
            continue;
        }

        if line.starts_with('+') && !line.starts_with("+++") {
            let text = line[1..].to_string();
            rows.push(IdeDiffRow {
                kind: DiffLineKind::Add,
                line_no: Some(new_ln),
                text,
            });
            new_ln = new_ln.saturating_add(1);
            in_hunk = true;
        } else if line.starts_with('-') && !line.starts_with("---") {
            let text = line[1..].to_string();
            rows.push(IdeDiffRow {
                kind: DiffLineKind::Del,
                line_no: Some(old_ln),
                text,
            });
            old_ln = old_ln.saturating_add(1);
            in_hunk = true;
        } else if line.starts_with(' ') || (in_hunk && !line.is_empty() && !line.starts_with('@')) {
            // Context: leading space in unified diff, or bare context after a hunk.
            let text = if line.starts_with(' ') {
                line[1..].to_string()
            } else {
                line.to_string()
            };
            rows.push(IdeDiffRow {
                kind: DiffLineKind::Context,
                line_no: Some(old_ln),
                text,
            });
            old_ln = old_ln.saturating_add(1);
            new_ln = new_ln.saturating_add(1);
        }
        // ignore blank/unknown outside hunks
    }
    rows
}

/// `@@ -old_start,old_count +new_start,new_count @@` → (old_start, new_start).
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@")?;
    let rest = rest.trim_start();
    // Expect `-N` or `-N,M`
    let rest = rest.strip_prefix('-')?;
    let (old_tok, rest) = split_hunk_token(rest)?;
    let rest = rest.trim_start().strip_prefix('+')?;
    let (new_tok, _) = split_hunk_token(rest)?;
    let old = old_tok.parse::<u32>().ok()?;
    let new = new_tok.parse::<u32>().ok()?;
    Some((old.max(1), new.max(1)))
}

fn split_hunk_token(s: &str) -> Option<(&str, &str)> {
    let end = s
        .find(|c: char| c == ',' || c == ' ' || c == '@')
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let tok = &s[..end];
    let rest = s[end..].trim_start_matches(|c: char| c == ',' || c.is_ascii_digit());
    Some((tok, rest))
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
            // Background start: show task_id prominently (Claude-style), keep expanded
            // so it is not buried inside a collapsed "N tools" chip.
            if output.contains("Background task started") {
                let task_id = output
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("task_id:"))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("?");
                let cmd = json_field(args, "command").unwrap_or_default();
                let cmd_bit = if cmd.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", truncate(&cmd, 28))
                };
                return Some((
                    format!("bg {task_id}{cmd_bit}"),
                    true, // auto-expand so user sees the start notice
                    None,
                ));
            }

            let (code, body) = parse_bash_exit(output);
            let body_lines = body.lines().filter(|l| !l.is_empty()).count();
            let failed = is_error
                || matches!(code, Some(c) if c != 0)
                || code.is_none() && output.starts_with("exit signal");
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
        "bash_output" => {
            let status = output
                .lines()
                .find_map(|l| l.trim().strip_prefix("status:"))
                .map(str::trim)
                .unwrap_or("?");
            let task_id = output
                .lines()
                .find_map(|l| l.trim().strip_prefix("task_id:"))
                .map(str::trim)
                .unwrap_or("?");
            let running = status == "running";
            let failed = is_error || matches!(status, "timed_out" | "killed" | "failed");
            let last_log = output
                .lines()
                .map(str::trim)
                .filter(|l| {
                    !l.is_empty()
                        && !l.starts_with("task_id:")
                        && !l.starts_with("command:")
                        && !l.starts_with("status:")
                        && !l.starts_with("exit:")
                        && !l.starts_with("elapsed")
                        && !l.starts_with("--- ")
                        && *l != "(no output yet)"
                })
                .last();
            let summary =
                if output.starts_with("Background tasks:") || output.starts_with("No background") {
                    format!(
                        "list · {}",
                        truncate(output.lines().next().unwrap_or("ps"), 40)
                    )
                } else if let Some(line) = last_log {
                    format!("{status} · {}", truncate(line, 42))
                } else {
                    format!("{status} · {task_id}")
                };
            // Expand finished / failed; keep running compact but show last log line.
            Some((summary, !running || failed, None))
        }
        "bash_kill" => {
            let task_id = output
                .lines()
                .find_map(|l| l.trim().strip_prefix("task_id:"))
                .map(str::trim)
                .unwrap_or("?");
            Some((format!("killed · {task_id}"), true, None))
        }
        "read" => {
            let path = json_field(args, "path")
                .or_else(|| json_field(args, "file_path"))
                .unwrap_or_else(|| "file".into());
            if output.contains("[image") {
                return Some((format!("read {path} · image"), true, None));
            }
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
        assert!(!streak_shows_group_header(&msgs, 0, 3));
    }

    #[test]
    fn ungrouped_streak_shows_header() {
        let mut msgs = vec![
            Message::tool("read", r#"{"path":"a"}"#, ToolStatus::Done),
            Message::tool("bash", r#"{"command":"ls"}"#, ToolStatus::Done),
            Message::tool("edit", r#"{"path":"b"}"#, ToolStatus::Done),
        ];
        for m in &mut msgs {
            m.tool_ungroup = true;
        }
        assert!(!streak_can_collapse(&msgs, 0, 3));
        assert!(streak_shows_group_header(&msgs, 0, 3));
        assert!(streak_group_eligible(&msgs, 0, 3));
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
    fn json_field_unescapes_newlines_and_tabs() {
        let args = r#"{"path":"x.rs","old_string":"a\nb","new_string":"a\n\tb\nc"}"#;
        assert_eq!(json_field(args, "old_string").as_deref(), Some("a\nb"));
        assert_eq!(json_field(args, "new_string").as_deref(), Some("a\n\tb\nc"));
    }

    #[test]
    fn edit_diff_from_args_splits_multiline_bodies() {
        // Regression: bad JSON unescape glued multi-line edits into one red/green row
        // (literal `textn//` instead of line breaks), which made edit UI unreadable.
        let args = r#"{"path":"ui.rs","old_string":"// chip\n// text\nfn a() {}","new_string":"// chip\n// text\nfn a() {\n  1\n}"}"#;
        let d = edit_diff_from_args(args).unwrap();
        assert!(
            d.contains("@@ -1,3 +1,5 @@"),
            "expected 1-based hunk header, got:\n{d}"
        );
        let del_lines: Vec<&str> = d
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .collect();
        let add_lines: Vec<&str> = d
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .collect();
        assert_eq!(del_lines.len(), 3, "{d}");
        assert_eq!(add_lines.len(), 5, "{d}");
        assert!(del_lines.iter().any(|l| *l == "-fn a() {}"), "{d}");
        assert!(add_lines.iter().any(|l| *l == "+fn a() {"), "{d}");

        let rows = parse_ide_diff_rows(&d);
        assert!(rows.len() >= 8, "expected per-line ide rows, got {}", rows.len());
        assert!(rows.iter().any(|r| r.kind == DiffLineKind::Del && r.text == "fn a() {}"));
        assert_eq!(rows[0].line_no, Some(1));
    }

    #[test]
    fn edit_diff_from_args_accepts_aliases() {
        let args = r#"{"filePath":"b.txt","oldString":"x\ny","newString":"z"}"#;
        let d = edit_diff_from_args(args).unwrap();
        assert!(d.contains("Updated b.txt"), "{d}");
        assert!(d.contains("-x") && d.contains("-y") && d.contains("+z"), "{d}");
    }

    #[test]
    fn bash_exit_summary() {
        let (s, expand, _) =
            summarize_tool_special("bash", r#"{"command":"false"}"#, "exit 1\nboom", true).unwrap();
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

    #[test]
    fn ide_diff_rows_track_line_numbers() {
        let text = "\
Updated src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -49,4 +49,6 @@
 limit: Optional[int] = Field(
     default=None,
     description=(
-        \"Optional. Maximum rows returned. Defaults to the server limit; keep \"
-        \"small for exploration.\"
+        \"Optional. Maximum rows returned. Prefer always
+setting this: exploration \"
+        \"LIMIT ≤ 50, filtered detail checks LIMIT ≤ 100.
+Do not omit for broad MATCH \"
+        \"that could return large node lists; prefer
+server-side aggregation instead.\"
     ),
 )
";
        let rows = parse_ide_diff_rows(text);
        assert!(!rows.is_empty(), "expected ide rows");
        // Headers skipped
        assert!(rows.iter().all(|r| r.kind != DiffLineKind::Meta));
        // First context starts at 49
        assert_eq!(rows[0].line_no, Some(49));
        assert_eq!(rows[0].kind, DiffLineKind::Context);
        // Find first del/add
        let del = rows.iter().find(|r| r.kind == DiffLineKind::Del).unwrap();
        let add = rows.iter().find(|r| r.kind == DiffLineKind::Add).unwrap();
        assert!(del.text.contains("Defaults to the server"));
        assert!(add.text.contains("Prefer always"));
        assert_eq!(del.line_no, Some(52));
        assert_eq!(add.line_no, Some(52));
    }
}
