//! Tool transcript helpers: grouping, edit/write previews, diff line paint.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::message::{Message, MessageRole, ToolStatus};
use crate::ui::text::expand_tabs;

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
    messages[start..start + len].iter().all(tool_groupable_base)
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
        && messages[start..start + len].iter().any(|m| m.tool_ungroup)
}

/// Resolve effective tool display name (e.g. `use_tool` unpacking inner target).
pub fn tool_display_name(tool_name: &str, args: &str) -> String {
    if tool_name == "use_tool" {
        if let Some(target) = json_field(args, "tool_name") {
            let target = target.trim();
            if !target.is_empty() {
                return target.to_string();
            }
        }
    }
    tool_name.to_string()
}

/// Short label for a tool in a group header: `bash` / `edit:path` / `linear__save_issue`.
pub fn tool_short_label(msg: &Message) -> String {
    let raw_name = msg.tool_name.as_deref().unwrap_or("tool");
    let name = tool_display_name(raw_name, &msg.content);
    let detail = pretty_path_or_cmd(&msg.content, None);
    if detail.is_empty() {
        name
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

/// Aggregate tool names for collapsed chips: `[todo_write] [grep x2] [read x2]`.
pub fn aggregate_tool_names(names: &[String]) -> String {
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for n in names {
        let key = if n.is_empty() {
            "tool".to_string()
        } else {
            n.clone()
        };
        if !counts.contains_key(&key) {
            order.push(key.clone());
        }
        *counts.entry(key).or_insert(0) += 1;
    }
    order
        .iter()
        .map(|n| {
            let c = counts.get(n).copied().unwrap_or(1);
            if c > 1 {
                format!("[{n} ×{c}]")
            } else {
                format!("[{n}]")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Collapse multi-line / escaped newlines into a single-line preview (`↵` separators).
///
/// Does **not** aggressively end-truncate long paths — callers should apply
/// [`truncate_middle`] / display-width middle truncate so filenames survive.
pub fn single_line_preview(s: &str, max_chars: usize) -> String {
    let flat = s
        .split(|c| c == '\n' || c == '\r')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ↵ ");
    // Also flatten literal `\n` that leaked from unparsed JSON.
    let flat = flat.replace("\\n", " ↵ ");
    truncate_middle(&flat, max_chars)
}

/// Middle-truncate by **char count**, keeping head + tail (filenames / destinations).
///
/// Prefer this over end-ellipsis when the important token is at the end of a path
/// or command line.
pub fn truncate_middle(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    if max_chars <= 1 {
        return "…".into();
    }
    if max_chars <= 3 {
        let t: String = chars
            .into_iter()
            .take(max_chars.saturating_sub(1))
            .collect();
        return format!("{t}…");
    }
    // ~40% head / ~60% tail so destinations & file names win over argv0.
    let inner = max_chars - 1; // room for …
    let head_n = (inner * 2) / 5;
    let tail_n = inner - head_n;
    let head: String = chars.iter().take(head_n).collect();
    let tail: String = chars[chars.len() - tail_n..].iter().collect();
    format!("{head}…{tail}")
}

/// Shorten an absolute path for transcript display.
///
/// - Under `cwd` → `./relative/path`
/// - Under `$HOME` → `~/…`
/// - Otherwise unchanged
pub fn shorten_display_path(path: &str, cwd: Option<&Path>) -> String {
    let path = path.trim();
    if path.is_empty() {
        return String::new();
    }
    // Prefer real Path strip_prefix, then string prefix (symlink / non-canonical cwd).
    if let Some(cwd) = cwd {
        let p = Path::new(path);
        if let Ok(rel) = p.strip_prefix(cwd) {
            let s = rel.to_string_lossy();
            return if s.is_empty() {
                "./".into()
            } else {
                format!("./{s}")
            };
        }
        let cwd_s = cwd.to_string_lossy();
        let cwd_trim = cwd_s.trim_end_matches('/');
        if path == cwd_trim {
            return "./".into();
        }
        let prefix = format!("{cwd_trim}/");
        if let Some(rest) = path.strip_prefix(&prefix) {
            return format!("./{rest}");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let home = home.trim_end_matches('/');
        if path == home {
            return "~/".into();
        }
        let prefix = format!("{home}/");
        if let Some(rest) = path.strip_prefix(&prefix) {
            return format!("~/{rest}");
        }
    }
    path.to_string()
}

/// Replace absolute workspace / home prefixes inside free-form text (bash cmds, etc.).
///
/// `cd /home/…/tools/one/benches/foo` → `cd ./benches/foo`
pub fn shorten_paths_in_text(s: &str, cwd: Option<&Path>) -> String {
    let trimmed = s.trim();
    if looks_like_path(trimmed) {
        return shorten_display_path(trimmed, cwd);
    }
    let mut out = s.to_string();
    if let Some(cwd) = cwd {
        let cwd_s = cwd.to_string_lossy();
        let cwd_trim = cwd_s.trim_end_matches('/');
        if !cwd_trim.is_empty() && out.contains(cwd_trim) {
            // Prefer `./rest` over `./rest` double-slash: replace `cwd/` first.
            let with_slash = format!("{cwd_trim}/");
            if out.contains(&with_slash) {
                out = out.replace(&with_slash, "./");
            }
            // Bare exact path token remaining (e.g. `cd /proj` with no trailing slash use).
            if out.contains(cwd_trim) {
                out = out.replace(cwd_trim, ".");
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let home = home.trim_end_matches('/');
        if !home.is_empty() && out.contains(home) {
            let with_slash = format!("{home}/");
            if out.contains(&with_slash) {
                out = out.replace(&with_slash, "~/");
            }
            if out.contains(home) {
                out = out.replace(home, "~");
            }
        }
    }
    out
}

/// Split a display path into `(dir_with_slash, file_name)` for dim/highlight paint.
pub fn path_dir_and_name(path: &str) -> (String, String) {
    if let Some(pos) = path.rfind('/') {
        (path[..=pos].to_string(), path[pos + 1..].to_string())
    } else {
        (String::new(), path.to_string())
    }
}

/// Whether `s` looks like a filesystem path (not a shell command / URL query).
pub fn looks_like_path(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.contains(' ') || s.contains('\n') {
        return false;
    }
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("~/")
        || s.starts_with("../")
        || (s.contains('/') && !s.contains("://"))
}

fn format_json_args_preview(val: &Value, cwd: Option<&Path>) -> String {
    let obj = match val {
        Value::Object(map) => map,
        Value::String(s) => return single_line_preview(&shorten_paths_in_text(s, cwd), 240),
        _ => return String::new(),
    };

    // If this is a use_tool wrapper, unwrap tool_input
    if let Some(inner) = obj.get("tool_input") {
        if let Value::Object(_) = inner {
            return format_json_args_preview(inner, cwd);
        } else if let Value::String(s) = inner {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                return format_json_args_preview(&parsed, cwd);
            }
            return single_line_preview(&shorten_paths_in_text(s, cwd), 240);
        }
    }

    // 1. Question / Query / Prompt
    let query_val = obj
        .get("question")
        .or_else(|| obj.get("query"))
        .or_else(|| obj.get("prompt"))
        .and_then(|v| v.as_str());

    let repo_val = obj
        .get("repoName")
        .or_else(|| obj.get("repo_name"))
        .or_else(|| obj.get("repo"))
        .and_then(|v| v.as_str());

    let path_val = obj
        .get("path")
        .or_else(|| obj.get("file_path"))
        .or_else(|| obj.get("filePath"))
        .and_then(|v| v.as_str());

    let cmd_val = obj
        .get("command")
        .or_else(|| obj.get("cmd"))
        .and_then(|v| v.as_str());

    let pattern_val = obj
        .get("pattern")
        .or_else(|| obj.get("regex"))
        .and_then(|v| v.as_str());

    let url_val = obj
        .get("url")
        .or_else(|| obj.get("uri"))
        .and_then(|v| v.as_str());

    let title_val = obj
        .get("title")
        .or_else(|| obj.get("message"))
        .or_else(|| obj.get("text"))
        .or_else(|| obj.get("description"))
        .or_else(|| obj.get("name"))
        .and_then(|v| v.as_str());

    if let Some(q) = query_val {
        let q_short = single_line_preview(q, 160);
        if let Some(repo) = repo_val {
            return format!("{repo} · \"{q_short}\"");
        }
        if let Some(p) = path_val {
            let p_short = shorten_display_path(p, cwd);
            return format!("{p_short} · \"{q_short}\"");
        }
        return format!("\"{q_short}\"");
    }

    if let Some(cmd) = cmd_val {
        let short = shorten_paths_in_text(cmd, cwd);
        return single_line_preview(&short, 240);
    }

    if let Some(p) = path_val {
        if let Some(pat) = pattern_val {
            let p_short = shorten_display_path(p, cwd);
            return format!("{pat} · {p_short}");
        }
        return shorten_display_path(p, cwd);
    }

    if let Some(pat) = pattern_val {
        return single_line_preview(pat, 120);
    }

    if let Some(url) = url_val {
        return single_line_preview(url, 96);
    }

    if let Some(title) = title_val {
        let t_short = single_line_preview(title, 120);
        return format!("\"{t_short}\"");
    }

    // Generic key=val pairs (up to 3 fields)
    let mut pairs = Vec::new();
    for (k, v) in obj {
        if k == "tool_name" {
            continue;
        }
        match v {
            Value::String(s) => pairs.push(format!("{k}=\"{}\"", single_line_preview(s, 40))),
            Value::Number(n) => pairs.push(format!("{k}={n}")),
            Value::Bool(b) => pairs.push(format!("{k}={b}")),
            _ => {}
        }
        if pairs.len() >= 3 {
            break;
        }
    }
    if !pairs.is_empty() {
        return pairs.join(" ");
    }

    String::new()
}

fn pretty_path_or_cmd(args: &str, cwd: Option<&Path>) -> String {
    let t = args.trim();
    if t.starts_with('{') && t.ends_with('}') {
        if let Ok(val) = serde_json::from_str::<Value>(t) {
            let res = format_json_args_preview(&val, cwd);
            if !res.is_empty() {
                return res;
            }
        }
    }
    for key in [
        "path",
        "file_path",
        "filePath",
        "command",
        "pattern",
        "query",
        "url",
    ] {
        if let Some(v) = json_field(t, key) {
            if key == "command" || key == "pattern" || key == "query" {
                // Paths inside shell commands → relative; leave room for UI middle-trunc.
                let short = shorten_paths_in_text(&v, cwd);
                return single_line_preview(&short, 240);
            }
            if key == "url" {
                return single_line_preview(&v, 96);
            }
            return shorten_display_path(&v, cwd);
        }
    }
    if !(t.starts_with('{') && t.ends_with('}')) {
        return single_line_preview(&shorten_paths_in_text(t, cwd), 240);
    }
    String::new()
}

/// Pretty one-line tool args for headers (path shortened, newlines → `↵`).
pub fn pretty_tool_detail(args: &str, cwd: Option<&Path>) -> String {
    pretty_path_or_cmd(args, cwd)
}

/// Full multi-line tool args for expanded body — paths shortened, **no char cap**.
///
/// Collapsed headers middle-truncate long bash/heredocs; expand must recover the
/// middle so transcript history is not permanently cropped.
pub fn pretty_tool_detail_full(args: &str, cwd: Option<&Path>) -> String {
    let t = args.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.starts_with('{') && t.ends_with('}') {
        if let Ok(val) = serde_json::from_str::<Value>(t) {
            // If it is use_tool, pretty print the inner tool_input
            if let Some(inner) = val.get("tool_input") {
                if let Ok(pretty) = serde_json::to_string_pretty(inner) {
                    return pretty;
                }
            }
            for key in ["command", "pattern", "query"] {
                if let Some(v) = val.get(key).and_then(|v| v.as_str()) {
                    return shorten_paths_in_text(v, cwd);
                }
            }
            for key in ["path", "file_path", "filePath"] {
                if let Some(v) = val.get(key).and_then(|v| v.as_str()) {
                    return shorten_display_path(v, cwd);
                }
            }
            if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                return pretty;
            }
        }
    }
    shorten_paths_in_text(t, cwd)
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

/// Detect if output looks like a unified / line-based diff (edit/write patches).
///
/// Prefer real unified-diff headers. Bare `+`/`-` counting alone is too aggressive
/// for ordinary text (e.g. Markdown bullet lists under `read`) and must not drive
/// the IDE diff UI outside edit/write tools.
pub fn looks_like_diff(text: &str) -> bool {
    let mut plus = 0;
    let mut minus = 0;
    let mut saw_header = false;
    for line in text.lines().take(80) {
        if line.starts_with("+++ ")
            || line.starts_with("--- ")
            || line.starts_with("@@ ")
            || line.starts_with("diff --git ")
            || line.starts_with("Updated ")
            || line.starts_with("Wrote ")
        {
            saw_header = true;
            // Header alone is enough for synthetic write previews / real patches.
            if line.starts_with("+++ ")
                || line.starts_with("--- ")
                || line.starts_with("@@ ")
                || line.starts_with("diff --git ")
            {
                return true;
            }
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            plus += 1;
        }
        if line.starts_with('-') && !line.starts_with("---") {
            minus += 1;
        }
    }
    // "Wrote …" / "Updated …" previews: need both body markers.
    saw_header && plus + minus >= 1
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
            let text = expand_tabs(&line[1..], 4);
            rows.push(IdeDiffRow {
                kind: DiffLineKind::Add,
                line_no: Some(new_ln),
                text,
            });
            new_ln = new_ln.saturating_add(1);
            in_hunk = true;
        } else if line.starts_with('-') && !line.starts_with("---") {
            let text = expand_tabs(&line[1..], 4);
            rows.push(IdeDiffRow {
                kind: DiffLineKind::Del,
                line_no: Some(old_ln),
                text,
            });
            old_ln = old_ln.saturating_add(1);
            in_hunk = true;
        } else if line.starts_with(' ') || (in_hunk && !line.is_empty() && !line.starts_with('@')) {
            // Context: leading space in unified diff, or bare context after a hunk.
            let raw = if line.starts_with(' ') {
                &line[1..]
            } else {
                line
            };
            let text = expand_tabs(raw, 4);
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

/// Tokenize for word-level inline diff (words + single separators).
pub fn diff_tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let mut chars = rest.char_indices();
        let Some((_, c0)) = chars.next() else {
            break;
        };
        if c0.is_alphanumeric() || c0 == '_' {
            let end = chars
                .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            out.push(&rest[..end]);
            rest = &rest[end..];
        } else {
            let end = c0.len_utf8();
            out.push(&rest[..end]);
            rest = &rest[end..];
        }
    }
    out
}

/// Word-level highlight ranges for a del/add pair.
///
/// Returns parallel segment lists `(text, emphasized)` for old and new lines.
/// When lines are identical or too large, returns a single non-emphasized segment each.
pub fn inline_diff_segments(old: &str, new: &str) -> (Vec<(String, bool)>, Vec<(String, bool)>) {
    const MAX: usize = 400;
    if old == new || old.len() > MAX || new.len() > MAX {
        return (
            vec![(old.to_string(), false)],
            vec![(new.to_string(), false)],
        );
    }
    let a = diff_tokens(old);
    let b = diff_tokens(new);
    if a.is_empty() && b.is_empty() {
        return (vec![(String::new(), false)], vec![(String::new(), false)]);
    }
    // Cap token count for O(n*m) LCS.
    if a.len() > 120 || b.len() > 120 {
        return (
            vec![(old.to_string(), false)],
            vec![(new.to_string(), false)],
        );
    }

    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0u16; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            dp[i + 1][j + 1] = if a[i] == b[j] {
                dp[i][j].saturating_add(1)
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    // Backtrack → common-token mask
    let mut common_a = vec![false; n];
    let mut common_b = vec![false; m];
    let mut i = n;
    let mut j = m;
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            common_a[i - 1] = true;
            common_b[j - 1] = true;
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }

    let merge = |toks: &[&str], common: &[bool]| -> Vec<(String, bool)> {
        let mut segs: Vec<(String, bool)> = Vec::new();
        for (t, &is_common) in toks.iter().zip(common.iter()) {
            let emp = !is_common;
            if let Some(last) = segs.last_mut() {
                if last.1 == emp {
                    last.0.push_str(t);
                    continue;
                }
            }
            segs.push(((*t).to_string(), emp));
        }
        if segs.is_empty() {
            segs.push((String::new(), false));
        }
        // If everything is emphasized, drop word-level paint (whole line already colored).
        if segs.iter().all(|(_, e)| *e) {
            return vec![(toks.concat(), false)];
        }
        segs
    };

    (merge(&a, &common_a), merge(&b, &common_b))
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
    // task / mcp / search: still summarize so the main row shows status + info.
    if is_error
        && name != "bash"
        && name != "shell"
        && name != "task"
        && name != "use_tool"
        && !name.contains("__")
        && name != "search_tool"
        && name != "mcp_status"
    {
        return None;
    }
    match name {
        "task" => {
            // Sole source for main-row presentation is tool_result text (not job
            // state). One-liner = status · description; expand when findings body
            // exists so the main transcript matches what `/tasks` shows.
            let mut status = if is_error { "error" } else { "done" };
            let mut body_lines = 0usize;
            let mut first_body = String::new();
            for line in output.lines() {
                let t = line.trim();
                if t.starts_with('[') && t.contains("task") {
                    if let Some(idx) = t.find("status=") {
                        let rest = &t[idx + 7..];
                        let token = rest
                            .split(|c: char| c == ' ' || c == '·' || c == ']')
                            .next()
                            .unwrap_or("")
                            .trim();
                        if !token.is_empty() {
                            status = match token {
                                s if s.starts_with("success") => "success",
                                s if s.starts_with("started") => "started",
                                s if s.starts_with("aborted") => "aborted",
                                s if s.starts_with("runtime_error") => "error",
                                s if s.starts_with("timeout") || s.starts_with("timed_out") => {
                                    "timeout"
                                }
                                s if s.starts_with("max_turns") => "max_turns",
                                s if s.starts_with("incomplete") => "incomplete",
                                other => other,
                            };
                        }
                    }
                    continue;
                }
                if t.is_empty() || t.starts_with("log_path:") {
                    continue;
                }
                body_lines += 1;
                if first_body.is_empty()
                    && !t.starts_with("Background job started")
                    && !t.starts_with("Result arrives")
                {
                    first_body = truncate(t, 48);
                }
            }
            let desc = json_field(args, "description")
                .or_else(|| json_field(args, "agent"))
                .or_else(|| json_field(args, "mode"))
                .unwrap_or_else(|| "explore".into());
            let summary = if first_body.is_empty() {
                format!("{status} · {desc}")
            } else {
                format!("{status} · {desc} · {first_body}")
            };
            let bg_started = status == "started";
            let backgrounded = output.contains("handed off to background")
                || json_field(args, "backgrounded").as_deref() == Some("true");
            let summary = if backgrounded && bg_started {
                format!("started · auto-bg · {desc}")
            } else {
                summary
            };
            let expand = is_error || (!bg_started && body_lines > 0);
            Some((summary, expand, None))
        }
        "edit" => {
            if is_error {
                return None;
            }
            let better = if looks_like_diff(output) {
                None
            } else {
                edit_diff_from_args(args)
            };
            let body = better.as_deref().unwrap_or(output);
            let (adds, dels) = count_diff_stats(body);
            // Path lives on the header; summary is stats only (less noise).
            let summary = if adds + dels > 0 {
                format!("+{adds} −{dels}")
            } else {
                "edited".into()
            };
            // Auto-expand small edits so the diff is visible.
            let expand = adds + dels > 0 && adds + dels <= 24;
            Some((summary, expand, better))
        }
        "write" => {
            let better = if looks_like_diff(output) {
                None
            } else {
                write_preview_from_args(args)
            };
            let bytes = json_field(args, "content").map(|c| c.len()).unwrap_or(0);
            // Path on header; summary is size only.
            let summary = format!("wrote {bytes} B");
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
            // Success: metrics only (no redundant "exit 0" — ✓ already means ok).
            // Failure: keep exit code front-and-center.
            let summary = match code {
                Some(0) if !is_error && body_lines == 0 => String::new(),
                Some(0) if !is_error && body_lines == 1 => truncate(body.trim(), 40),
                Some(0) if !is_error => format!("{body_lines} lines"),
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
                None if body_lines == 0 => String::new(),
                None if body_lines == 1 => truncate(output.trim(), 40),
                None => format!("{body_lines} lines"),
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
            // Path is on the tool header — summary only carries result metrics.
            if output.contains("[image") {
                return Some(("image".into(), true, None));
            }
            let lines = output.lines().count();
            Some((format!("{lines} lines"), false, None))
        }
        "grep" | "find" | "ls" => {
            if is_error {
                return None;
            }
            let lines = output.lines().filter(|l| !l.trim().is_empty()).count();
            if lines == 0 {
                Some(("no matches".into(), false, None))
            } else {
                Some((format!("{lines} lines"), false, None))
            }
        }
        "search_tool" => {
            if is_error {
                return Some(("error".into(), true, None));
            }
            if let Ok(val) = serde_json::from_str::<Value>(output.trim()) {
                let status = val
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("ready");
                let count = val
                    .get("results")
                    .and_then(|r| r.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let summary = if count == 0 {
                    format!("{status} · no tools found")
                } else if count == 1 {
                    format!("{status} · 1 tool found")
                } else {
                    format!("{status} · {count} tools found")
                };

                let mut better = String::new();
                if let Some(results) = val.get("results").and_then(|r| r.as_array()) {
                    if !results.is_empty() {
                        better.push_str(&format!("Found {} MCP tool(s):\n", results.len()));
                        for r in results.iter().take(20) {
                            let tool_name = r
                                .get("tool_name")
                                .and_then(|t| t.as_str())
                                .unwrap_or("tool");
                            let desc = r.get("description").and_then(|d| d.as_str()).unwrap_or("");
                            let first_desc = desc.lines().next().unwrap_or("").trim();
                            if first_desc.is_empty() {
                                better.push_str(&format!("  • {tool_name}\n"));
                            } else {
                                better.push_str(&format!(
                                    "  • {tool_name}: {}\n",
                                    truncate(first_desc, 60)
                                ));
                            }
                        }
                        if results.len() > 20 {
                            better.push_str(&format!("  … +{} more tools\n", results.len() - 20));
                        }
                    }
                }
                let better_opt = if better.is_empty() {
                    None
                } else {
                    Some(better.trim_end().to_string())
                };
                Some((summary, false, better_opt))
            } else {
                let lines = output.lines().filter(|l| !l.trim().is_empty()).count();
                Some((format!("{lines} lines"), false, None))
            }
        }
        "mcp_status" => {
            if is_error {
                return Some(("error".into(), true, None));
            }
            if let Ok(val) = serde_json::from_str::<Value>(output.trim()) {
                let connected = val
                    .get("connected")
                    .and_then(|c| c.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let failed = val
                    .get("failed")
                    .and_then(|f| f.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let summary = if failed > 0 {
                    format!("{connected} connected, {failed} failed")
                } else {
                    format!("{connected} connected")
                };
                Some((summary, false, None))
            } else {
                Some(("status".into(), false, None))
            }
        }
        tool if tool == "use_tool" || tool.contains("__") => {
            if is_error {
                let err_msg = if let Ok(val) = serde_json::from_str::<Value>(output.trim()) {
                    val.get("error")
                        .or_else(|| val.get("message"))
                        .and_then(|m| m.as_str())
                        .map(|s| truncate(s, 48))
                } else {
                    output
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .map(|l| truncate(l.trim(), 48))
                };
                let summary = match err_msg {
                    Some(msg) => format!("error · {msg}"),
                    None => "error".into(),
                };
                return Some((summary, true, None));
            }

            let trimmed = output.trim();
            if trimmed.contains("structuredContent:") {
                let lines = output.lines().filter(|l| !l.trim().is_empty()).count();
                return Some((format!("{lines} lines · structured"), false, None));
            }

            if (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'))
            {
                if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                    let summary = match &val {
                        Value::Array(arr) => {
                            if arr.is_empty() {
                                "0 items".into()
                            } else if arr.len() == 1 {
                                "1 item".into()
                            } else {
                                format!("{} items", arr.len())
                            }
                        }
                        Value::Object(obj) => {
                            if let Some(items) = obj
                                .get("items")
                                .or_else(|| obj.get("results"))
                                .and_then(|v| v.as_array())
                            {
                                format!("{} items", items.len())
                            } else if let Some(count) = obj
                                .get("count")
                                .or_else(|| obj.get("total"))
                                .and_then(|v| v.as_i64())
                            {
                                format!("{count} items")
                            } else if let Some(status) = obj.get("status").and_then(|v| v.as_str())
                            {
                                format!("status={status}")
                            } else if let Some(msg) = obj
                                .get("message")
                                .or_else(|| obj.get("title"))
                                .and_then(|v| v.as_str())
                            {
                                truncate(msg, 40)
                            } else if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
                                format!("id={id}")
                            } else {
                                format!("{} fields", obj.len())
                            }
                        }
                        _ => "ok".into(),
                    };
                    let better = if !trimmed.contains('\n')
                        && (trimmed.len() > 20 || trimmed.contains(','))
                    {
                        serde_json::to_string_pretty(&val).ok()
                    } else {
                        None
                    };
                    return Some((summary, false, better));
                }
            }

            let non_empty: Vec<&str> = output
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            let summary = if non_empty.is_empty() {
                "ok".into()
            } else if non_empty.len() == 1 {
                truncate(non_empty[0], 40)
            } else {
                format!("{} lines", non_empty.len())
            };
            Some((summary, false, None))
        }
        _ => {
            let trimmed = output.trim();
            if (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'))
            {
                if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                    let summary = match &val {
                        Value::Array(arr) => format!("json · {} items", arr.len()),
                        Value::Object(obj) => {
                            if let Some(status) = obj.get("status").and_then(|s| s.as_str()) {
                                format!("status={status}")
                            } else {
                                format!("json · {} keys", obj.len())
                            }
                        }
                        _ => "json".into(),
                    };
                    let better = if !trimmed.contains('\n')
                        && (trimmed.len() > 20 || trimmed.contains(','))
                    {
                        serde_json::to_string_pretty(&val).ok()
                    } else {
                        None
                    };
                    return Some((summary, is_error, better));
                }
            }
            None
        }
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
        assert!(
            rows.len() >= 8,
            "expected per-line ide rows, got {}",
            rows.len()
        );
        assert!(rows
            .iter()
            .any(|r| r.kind == DiffLineKind::Del && r.text == "fn a() {}"));
        assert_eq!(rows[0].line_no, Some(1));
    }

    #[test]
    fn edit_diff_from_args_accepts_aliases() {
        let args = r#"{"filePath":"b.txt","oldString":"x\ny","newString":"z"}"#;
        let d = edit_diff_from_args(args).unwrap();
        assert!(d.contains("Updated b.txt"), "{d}");
        assert!(
            d.contains("-x") && d.contains("-y") && d.contains("+z"),
            "{d}"
        );
    }

    #[test]
    fn shorten_display_path_relative_and_home() {
        let cwd = Path::new("/home/fxh/tools/one");
        assert_eq!(
            shorten_display_path(
                "/home/fxh/tools/one/crates/one-tools/src/bash.rs",
                Some(cwd)
            ),
            "./crates/one-tools/src/bash.rs"
        );
        assert_eq!(shorten_display_path("/home/fxh/tools/one", Some(cwd)), "./");
        // Outside cwd still may become ~/ when HOME matches.
        std::env::set_var("HOME", "/home/fxh");
        assert_eq!(
            shorten_display_path("/home/fxh/.config/foo", Some(cwd)),
            "~/.config/foo"
        );
    }

    #[test]
    fn single_line_preview_flattens_newlines() {
        assert_eq!(single_line_preview("a\nb\nc", 40), "a ↵ b ↵ c");
        assert_eq!(single_line_preview("bg id\\n# how", 40), "bg id ↵ # how");
    }

    #[test]
    fn truncate_middle_keeps_tail() {
        let s = "cd /home/fxh/tools/one/benches/out/tb-regex-checker";
        let t = truncate_middle(s, 28);
        assert!(t.contains('…'), "{t}");
        assert!(
            t.ends_with("checker") || t.contains("regex"),
            "tail (filename) must survive: {t}"
        );
        assert!(!t.ends_with('…'), "must not end-ellipsis only: {t}");
    }

    #[test]
    fn shorten_paths_in_text_rewrites_cwd_inside_command() {
        let cwd = Path::new("/home/fxh/tools/one");
        let cmd = "cd /home/fxh/tools/one/benches/out/tb-regex-checker && ls";
        let s = shorten_paths_in_text(cmd, Some(cwd));
        assert!(s.contains("./benches/out/tb-regex-checker"), "{s}");
        assert!(!s.contains("/home/fxh/tools/one/benches"), "{s}");
    }

    #[test]
    fn pretty_tool_detail_shortens_bash_command_paths() {
        let cwd = Path::new("/home/fxh/tools/one");
        let args = r#"{"command":"cp /home/fxh/tools/one/benches/out/tb-regex-checker/a /tmp/b"}"#;
        let d = pretty_tool_detail(args, Some(cwd));
        assert!(d.contains("./benches/out/tb-regex-checker"), "{d}");
        assert!(!d.contains("/home/fxh/tools/one/"), "{d}");
    }

    #[test]
    fn aggregate_tool_names_counts_dupes() {
        let names = vec![
            "todo_write".into(),
            "grep".into(),
            "grep".into(),
            "read".into(),
            "read".into(),
        ];
        assert_eq!(
            aggregate_tool_names(&names),
            "[todo_write] [grep ×2] [read ×2]"
        );
    }

    #[test]
    fn read_summary_is_metrics_only() {
        let (s, expand, better) =
            summarize_tool_special("read", r#"{"path":"/abs/foo.rs"}"#, "line1\nline2\n", false)
                .unwrap();
        assert_eq!(s, "2 lines");
        assert!(!s.contains('/'), "{s}");
        assert!(!expand);
        assert!(better.is_none());
    }

    #[test]
    fn looks_like_diff_rejects_plain_markdown_and_read_bodies() {
        // Markdown bullets must never drive the IDE edit UI for read output.
        let md = "\
# One\n\
\n\
## Features\n\
- **minimal core**\n\
- **built-in tools**\n\
- item three\n";
        assert!(
            !looks_like_diff(md),
            "plain markdown should not look like a diff:\n{md}"
        );

        let numbered = "1|# One\n2|\n3|- bullet\n4|+ plusish\n";
        assert!(
            !looks_like_diff(numbered),
            "read-style numbered body is not a patch:\n{numbered}"
        );
    }

    #[test]
    fn looks_like_diff_accepts_unified_and_write_preview() {
        let unified = "\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1,2 +1,2 @@\n\
-old\n\
+new\n";
        assert!(looks_like_diff(unified));

        let write_preview = "\
Wrote 12 bytes → foo.txt (2 lines)\n\
+++ b/foo.txt\n\
+hello\n\
+world\n";
        assert!(looks_like_diff(write_preview));
    }

    #[test]
    fn task_findings_auto_expand() {
        let args = r#"{"description":"Research MCP","agent":"explore"}"#;
        let out = "\
[task · explore · Research MCP · status=success · id=job_ab12_1]
## 结论
当前 McpManager 已有状态快照。
";
        let (s, expand, _) = summarize_tool_special("task", args, out, false).unwrap();
        assert!(s.contains("success"), "{s}");
        assert!(s.contains("Research MCP"), "{s}");
        assert!(s.contains("结论") || s.contains("McpManager"), "{s}");
        assert!(expand, "findings body must expand on main transcript");

        let started = "\
[task · explore · long job · status=started · id=job_x]
Background job started. Continue other work.
";
        let (s2, expand2, _) = summarize_tool_special("task", args, started, false).unwrap();
        assert!(s2.contains("started"), "{s2}");
        assert!(!expand2, "background start stays compact");
    }

    #[test]
    fn bash_exit_summary() {
        let (s, expand, _) =
            summarize_tool_special("bash", r#"{"command":"false"}"#, "exit 1\nboom", true).unwrap();
        assert!(s.contains("exit 1"), "{s}");
        assert!(expand);

        // Success: no redundant "exit 0" — ✓ already signals ok.
        let (s0, expand0, _) =
            summarize_tool_special("bash", r#"{"command":"true"}"#, "exit 0", false).unwrap();
        assert!(!s0.contains("exit 0"), "{s0}");
        assert!(s0.is_empty(), "empty metrics when no output: {s0}");
        assert!(!expand0);

        let (s_lines, _, _) =
            summarize_tool_special("bash", r#"{"command":"ls"}"#, "exit 0\na\nb\nc\n", false)
                .unwrap();
        assert_eq!(s_lines, "3 lines");
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

    #[test]
    fn inline_diff_highlights_changed_words() {
        let (old, new) = inline_diff_segments(
            "let text = tool_output_for_ui(output);",
            "let text = tool_output_for_ui(&output);",
        );
        let emp_old: String = old
            .iter()
            .filter(|(_, e)| *e)
            .map(|(s, _)| s.as_str())
            .collect();
        let emp_new: String = new
            .iter()
            .filter(|(_, e)| *e)
            .map(|(s, _)| s.as_str())
            .collect();
        // Only the `&` insertion should be emphasized on the new side; old may be empty emp.
        assert!(
            emp_new.contains('&') || new.iter().any(|(s, e)| *e && s.contains('&')),
            "expected & highlighted in new, segs={new:?}"
        );
        assert!(
            !emp_old.contains("tool_output_for_ui"),
            "shared identifier should not be fully emphasized: {old:?}"
        );
    }

    #[test]
    fn inline_diff_identical_is_plain() {
        let (old, new) = inline_diff_segments("same line", "same line");
        assert_eq!(old, vec![("same line".into(), false)]);
        assert_eq!(new, vec![("same line".into(), false)]);
    }

    #[test]
    fn diff_tokens_keeps_separators() {
        let t = diff_tokens("a.b(c)");
        assert_eq!(t, vec!["a", ".", "b", "(", "c", ")"]);
    }

    #[test]
    fn use_tool_resolves_target_name_and_detail() {
        let raw_name = "use_tool";
        let args = r#"{"tool_name":"deepwiki__ask_question","tool_input":{"question":"How does One work?","repoName":"facebook/react"}}"#;
        let display = tool_display_name(raw_name, args);
        assert_eq!(display, "deepwiki__ask_question");

        let detail = pretty_tool_detail(args, None);
        assert!(detail.contains("facebook/react"), "detail={detail}");
        assert!(detail.contains("How does One work?"), "detail={detail}");

        let full = pretty_tool_detail_full(args, None);
        assert!(
            full.contains("\"question\": \"How does One work?\"")
                || full.contains("How does One work?"),
            "full={full}"
        );
    }

    #[test]
    fn search_tool_summarizes_and_formats_better_output() {
        let args = r#"{"query":"linear"}"#;
        let out = r#"{"status":"ready","results":[{"tool_name":"linear__create_issue","description":"Create a new issue in Linear\nSupports labels and team."}]}"#;
        let (s, expand, better) = summarize_tool_special("search_tool", args, out, false).unwrap();
        assert_eq!(s, "ready · 1 tool found");
        assert!(!expand);
        let better_text = better.unwrap();
        assert!(
            better_text.contains("Found 1 MCP tool(s):"),
            "better={better_text}"
        );
        assert!(
            better_text.contains("• linear__create_issue: Create a new issue in Linear"),
            "better={better_text}"
        );
    }

    #[test]
    fn mcp_status_summarizes_correctly() {
        let out = r#"{"connected":["deepwiki","linear"],"failed":["broken_server"]}"#;
        let (s, expand, _) = summarize_tool_special("mcp_status", "{}", out, false).unwrap();
        assert_eq!(s, "2 connected, 1 failed");
        assert!(!expand);
    }

    #[test]
    fn mcp_use_tool_summarizes_json_and_error() {
        // Success JSON array
        let out_arr = r#"[{"id":1},{"id":2},{"id":3}]"#;
        let (s_arr, _, better_arr) =
            summarize_tool_special("linear__list_issues", "{}", out_arr, false).unwrap();
        assert_eq!(s_arr, "3 items");
        assert!(better_arr.is_some());

        // Error JSON
        let err_json = r#"{"error":"Resource not found","code":404}"#;
        let (s_err, expand_err, _) =
            summarize_tool_special("deepwiki__ask_question", "{}", err_json, true).unwrap();
        assert!(s_err.contains("Resource not found"), "s_err={s_err}");
        assert!(expand_err);
    }

    #[test]
    fn generic_json_output_pretty_prints_and_summarizes() {
        let compact = r#"{"count":42,"status":"active","description":"a compact json response"}"#;
        let (s, expand, better) =
            summarize_tool_special("custom_api", "{}", compact, false).unwrap();
        assert_eq!(s, "status=active");
        assert!(!expand);
        assert!(
            better.is_some(),
            "compact json should generate pretty printed better view"
        );
        let formatted = better.unwrap();
        assert!(formatted.contains('\n'), "formatted={formatted}");
        assert!(formatted.contains("\"count\": 42"), "formatted={formatted}");
    }

    #[test]
    fn ide_diff_rows_expands_tabs_and_strips_cr() {
        let text = "\
Updated parser/parser_test.go
--- a/parser/parser_test.go
+++ b/parser/parser_test.go
@@ -1740,3 +1740,3 @@
 \tif indexExp.End != nil {\r
-\t\ttestPrefixExpression(t, indexExp.Step, \"-\", 1)\r
+\t\tprefixExp, ok := indexExp.Step.(*ast.PrefixExpression)\r
";
        let rows = parse_ide_diff_rows(text);
        assert_eq!(rows.len(), 3);
        // Ensure tabs are expanded to 4 spaces per tab level
        assert_eq!(rows[0].text, "    if indexExp.End != nil {");
        assert_eq!(
            rows[1].text,
            "        testPrefixExpression(t, indexExp.Step, \"-\", 1)"
        );
        assert_eq!(
            rows[2].text,
            "        prefixExp, ok := indexExp.Step.(*ast.PrefixExpression)"
        );
        // Ensure carriage returns are eliminated
        assert!(!rows[0].text.contains('\r'));
        assert!(!rows[1].text.contains('\r'));
        assert!(!rows[2].text.contains('\r'));
    }
}
