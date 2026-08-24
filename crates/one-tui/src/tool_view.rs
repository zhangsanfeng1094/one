//! Tool transcript helpers: grouping, edit/write previews, diff line paint.

use std::collections::HashMap;
use std::path::Path;

use ratatui::text::Span;
use serde_json::Value;

use crate::message::{Message, MessageRole, ToolStatus};
use crate::theme::Theme;
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
///
/// `use_tool` is formatted as labeled fields (Grok-style), never as a JSON dump.
pub fn pretty_tool_detail_full(args: &str, cwd: Option<&Path>) -> String {
    let t = args.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.starts_with('{') && t.ends_with('}') {
        if let Ok(val) = serde_json::from_str::<Value>(t) {
            if val.get("tool_name").is_some() && val.get("tool_input").is_some() {
                return format_use_tool_args_view(t).unwrap_or_default();
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
            if let Some(fields) = format_object_fields(&val, 0) {
                return fields;
            }
        }
    }
    shorten_paths_in_text(t, cwd)
}

/// If a string is valid JSON (object or array), format with standard indentation (Grok style).
pub fn maybe_pretty_json(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            return serde_json::to_string_pretty(&val).ok();
        }
    }
    None
}

/// Extract use_tool arguments into flat key-value pairs (Grok Build style).
/// Nested objects/arrays are compact JSON representations.
pub fn extract_use_tool_args(args: &str) -> Vec<(String, String)> {
    let Some(obj) = use_tool_input_object(args) else {
        return Vec::new();
    };
    obj.into_iter()
        .map(|(k, v)| {
            let repr = match &v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => "null".to_string(),
                Value::Array(_) | Value::Object(_) => serde_json::to_string(&v).unwrap_or_default(),
            };
            (k, repr)
        })
        .collect()
}

fn use_tool_input_object(args: &str) -> Option<serde_json::Map<String, Value>> {
    let val: Value = serde_json::from_str(args.trim()).ok()?;
    if let Some(inner) = val.get("tool_input") {
        return match inner {
            Value::Object(m) => Some(m.clone()),
            Value::String(s) => match serde_json::from_str(s) {
                Ok(Value::Object(m)) => Some(m),
                _ => None,
            },
            _ => None,
        };
    }
    match val {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// Human-readable `use_tool` argument block: labeled fields, never a JSON dump.
pub fn format_use_tool_args_view(args: &str) -> Option<String> {
    let obj = use_tool_input_object(args)?;
    if obj.is_empty() {
        return None;
    }
    format_object_fields(&Value::Object(obj), 0)
}

fn format_object_fields(value: &Value, indent: usize) -> Option<String> {
    let obj = value.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (key, val) in obj {
        push_field_line(&mut out, key, val, indent);
    }
    let text = out.trim_end().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn push_field_line(out: &mut String, key: &str, value: &Value, indent: usize) {
    let pad = "  ".repeat(indent);
    match value {
        Value::String(s) => {
            if s.contains('\n') {
                out.push_str(&format!("{pad}- {key}:\n"));
                for line in s.lines() {
                    out.push_str(&format!("{pad}    {line}\n"));
                }
            } else {
                out.push_str(&format!("{pad}- {key}: {s}\n"));
            }
        }
        Value::Number(n) => out.push_str(&format!("{pad}- {key}: {n}\n")),
        Value::Bool(b) => out.push_str(&format!("{pad}- {key}: {b}\n")),
        Value::Null => out.push_str(&format!("{pad}- {key}: null\n")),
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str(&format!("{pad}- {key}: (empty)\n"));
            } else {
                out.push_str(&format!("{pad}- {key}:\n"));
                for (k, v) in map {
                    push_field_line(out, k, v, indent + 1);
                }
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str(&format!("{pad}- {key}: (empty list)\n"));
                return;
            }
            let scalars = arr.iter().all(|item| {
                matches!(
                    item,
                    Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null
                )
            });
            if scalars {
                let items: Vec<String> = arr.iter().map(|v| scalar_preview(v, 80)).collect();
                let joined = items.join(", ");
                if joined.chars().count() <= 90 {
                    out.push_str(&format!("{pad}- {key}: {joined}\n"));
                } else {
                    out.push_str(&format!("{pad}- {key}:\n"));
                    for item in items {
                        out.push_str(&format!("{pad}    - {item}\n"));
                    }
                }
            } else {
                out.push_str(&format!("{pad}- {key}:\n"));
                for (idx, item) in arr.iter().enumerate() {
                    match item {
                        Value::Object(map) => {
                            out.push_str(&format!("{pad}    - #{idx}\n"));
                            for (k, v) in map {
                                push_field_line(out, k, v, indent + 2);
                            }
                        }
                        Value::Array(_) => {
                            out.push_str(&format!("{pad}    - #{idx}\n"));
                            push_json_outline_value(out, item, indent + 2, 5);
                        }
                        _ => out.push_str(&format!("{pad}    - {}\n", scalar_preview(item, 100))),
                    }
                }
            }
        }
    }
}

fn is_field_key(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// True when a line looks like formatted/indented JSON or JSON token.
pub fn is_json_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if t.starts_with('{') || t.starts_with('}') || t.starts_with('[') || t.starts_with(']') {
        return true;
    }
    if t.starts_with('"') {
        if t.contains("\":") || t.ends_with("\",") || t.ends_with('"') {
            return true;
        }
    }
    if t.starts_with("true") || t.starts_with("false") || t.starts_with("null") {
        return true;
    }
    if t.chars()
        .next()
        .map(|c| c.is_ascii_digit() || c == '-')
        .unwrap_or(false)
    {
        let first_word = t
            .split(|c: char| c == ',' || c.is_whitespace())
            .next()
            .unwrap_or("");
        if first_word.parse::<f64>().is_ok() || first_word.parse::<i64>().is_ok() {
            return true;
        }
    }
    false
}

/// Tokenize and syntax-highlight a single line of formatted JSON (VS Code / Grok style).
pub fn highlight_json_line(line: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let trimmed = line.trim_start();
    let indent_len = line.len() - trimmed.len();
    if indent_len > 0 {
        spans.push(Span::raw(line[..indent_len].to_string()));
    }
    if trimmed.is_empty() {
        return spans;
    }

    let mut rest = trimmed;
    while !rest.is_empty() {
        let cur = rest.trim_start();
        let ws = rest.len() - cur.len();
        if ws > 0 {
            spans.push(Span::raw(rest[..ws].to_string()));
        }
        if cur.is_empty() {
            break;
        }

        // 1. Quoted string (JSON key or JSON string value)
        if cur.starts_with('"') {
            if let Some(q_end) = find_matching_quote(cur) {
                let quote_str = &cur[..=q_end];
                let after = &cur[q_end + 1..];
                let after_trimmed = after.trim_start();

                if after_trimmed.starts_with(':') {
                    // It's a JSON key
                    spans.push(Span::styled(quote_str.to_string(), Theme::json_key()));
                    let space_len = after.len() - after_trimmed.len();
                    if space_len > 0 {
                        spans.push(Span::raw(after[..space_len].to_string()));
                    }
                    let colon_and_space = if after_trimmed.starts_with(": ") {
                        ": "
                    } else {
                        ":"
                    };
                    spans.push(Span::styled(
                        colon_and_space.to_string(),
                        Theme::json_punct(),
                    ));
                    rest = &after_trimmed[colon_and_space.len()..];
                    continue;
                } else {
                    // String value
                    spans.push(Span::styled(quote_str.to_string(), Theme::json_string()));
                    rest = after;
                    continue;
                }
            } else {
                // Unterminated quote
                spans.push(Span::styled(cur.to_string(), Theme::json_string()));
                break;
            }
        }

        // 2. Structural punctuation
        if cur.starts_with('{')
            || cur.starts_with('}')
            || cur.starts_with('[')
            || cur.starts_with(']')
            || cur.starts_with(',')
        {
            spans.push(Span::styled(cur[..1].to_string(), Theme::json_punct()));
            rest = &cur[1..];
            continue;
        }

        // 3. Keywords: true / false / null
        if let Some(after) = cur.strip_prefix("true") {
            spans.push(Span::styled("true".to_string(), Theme::json_bool()));
            rest = after;
            continue;
        }
        if let Some(after) = cur.strip_prefix("false") {
            spans.push(Span::styled("false".to_string(), Theme::json_bool()));
            rest = after;
            continue;
        }
        if let Some(after) = cur.strip_prefix("null") {
            spans.push(Span::styled("null".to_string(), Theme::json_null()));
            rest = after;
            continue;
        }

        // 4. Numbers
        let num_len = cur
            .find(|c: char| {
                !(c.is_ascii_digit() || c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E')
            })
            .unwrap_or(cur.len());
        if num_len > 0 {
            let num_candidate = &cur[..num_len];
            if num_candidate.parse::<f64>().is_ok() || num_candidate.parse::<i64>().is_ok() {
                spans.push(Span::styled(
                    num_candidate.to_string(),
                    Theme::json_number(),
                ));
                rest = &cur[num_len..];
                continue;
            }
        }

        // 5. Fallback for unclassified tokens
        let token_len = cur
            .find(|c: char| c.is_whitespace() || c == ',' || c == '}' || c == ']' || c == ':')
            .unwrap_or(cur.len())
            .max(1);
        spans.push(Span::raw(cur[..token_len].to_string()));
        rest = &cur[token_len..];
    }

    spans
}

/// Highlight human-readable structured tool outputs (like search_tool / mcp_status / schema listings).
pub fn highlight_tool_output_line(line: &str) -> Option<Vec<Span<'static>>> {
    let trimmed = line.trim_start();
    let indent_len = line.len() - trimmed.len();
    let indent = &line[..indent_len];

    // 1. Server section header: `[server_name]`
    if trimmed.starts_with('[') && trimmed.ends_with(']') && !trimmed.contains(' ') {
        let name = &trimmed[1..trimmed.len() - 1];
        return Some(vec![
            Span::raw(indent.to_string()),
            Span::styled("[", Theme::json_punct()),
            Span::styled(name.to_string(), Theme::heading_sub()),
            Span::styled("]", Theme::json_punct()),
        ]);
    }

    // 2. Human-readable MCP result/status headers.
    if (trimmed.starts_with("Found ") && trimmed.contains("MCP tool(s)"))
        || (trimmed.starts_with("MCP Servers (") && trimmed.ends_with(':'))
        || trimmed.starts_with("MCP result")
    {
        return Some(vec![
            Span::raw(indent.to_string()),
            Span::styled(trimmed.to_string(), Theme::tool_group_title()),
        ]);
    }

    // 3. Tool signature line: `• tool_name(params...)` or `  • tool_name(params...)`.
    // Plain outline bullets (`• key: value`) fall through to the next branch.
    if let Some(rest) = trimmed.strip_prefix("• ") {
        if let Some(paren_idx) = rest.find('(') {
            let mut spans = vec![
                Span::raw(indent.to_string()),
                Span::styled("• ", Theme::json_punct()),
            ];
            let func_name = &rest[..paren_idx];
            let after_paren = &rest[paren_idx..];
            spans.push(Span::styled(func_name.to_string(), Theme::tool_kind("mcp")));
            if let Some(close_idx) = after_paren.rfind(')') {
                let inside = &after_paren[1..close_idx];
                spans.push(Span::styled("(", Theme::json_punct()));
                // Format parameter tokens inside parentheses
                let mut first_p = true;
                for part in inside.split(", ") {
                    if part.is_empty() {
                        continue;
                    }
                    if !first_p {
                        spans.push(Span::styled(", ", Theme::json_punct()));
                    }
                    first_p = false;
                    if let Some((p_name, p_type)) = part.split_once(": ") {
                        spans.push(Span::styled(p_name.to_string(), Theme::tool_detail_done()));
                        spans.push(Span::styled(": ", Theme::json_punct()));
                        spans.push(Span::styled(p_type.to_string(), Theme::json_string()));
                    } else {
                        spans.push(Span::styled(part.to_string(), Theme::tool_detail_done()));
                    }
                }
                spans.push(Span::styled(")", Theme::json_punct()));
                if close_idx + 1 < after_paren.len() {
                    spans.push(Span::styled(
                        after_paren[close_idx + 1..].to_string(),
                        Theme::tool_detail_done(),
                    ));
                }
            } else {
                spans.push(Span::styled(
                    after_paren.to_string(),
                    Theme::tool_detail_done(),
                ));
            }
            return Some(spans);
        }
    }

    // 4. Parameter/detail bullet line: `- param_name (type, req): desc` or `• key: value`.
    if let Some(rest) = trimmed.strip_prefix("• ") {
        if !rest.contains('(') {
            let mut spans = vec![
                Span::raw(indent.to_string()),
                Span::styled("• ", Theme::json_punct()),
            ];
            if let Some((key, value)) = rest.split_once(": ") {
                spans.push(Span::styled(key.to_string(), Theme::json_key()));
                spans.push(Span::styled(": ", Theme::json_punct()));
                spans.push(Span::styled(value.to_string(), Theme::tool_detail_done()));
            } else {
                spans.push(Span::styled(rest.to_string(), Theme::tool_detail_done()));
            }
            return Some(spans);
        }
    }

    if let Some(rest) = trimmed.strip_prefix("- ") {
        let mut spans = vec![
            Span::raw(indent.to_string()),
            Span::styled("- ", Theme::meta()),
        ];
        if let Some((field_part, desc_part)) = rest.split_once("): ") {
            if let Some(paren_idx) = field_part.find(" (") {
                let field_name = &field_part[..paren_idx];
                let type_info = &field_part[paren_idx + 2..];
                spans.push(Span::styled(field_name.to_string(), Theme::json_key()));
                spans.push(Span::styled(" (", Theme::json_punct()));
                spans.push(Span::styled(type_info.to_string(), Theme::meta()));
                spans.push(Span::styled("): ", Theme::json_punct()));
                spans.push(Span::styled(
                    desc_part.to_string(),
                    Theme::tool_detail_done(),
                ));
                return Some(spans);
            }
        } else if let Some((field_part, _)) = rest.split_once(')') {
            if let Some(paren_idx) = field_part.find(" (") {
                let field_name = &field_part[..paren_idx];
                let type_info = &field_part[paren_idx + 2..];
                spans.push(Span::styled(field_name.to_string(), Theme::json_key()));
                spans.push(Span::styled(" (", Theme::json_punct()));
                spans.push(Span::styled(type_info.to_string(), Theme::meta()));
                spans.push(Span::styled(")", Theme::json_punct()));
                return Some(spans);
            }
        }
        if let Some(spans) = highlight_labeled_field(indent, "- ", rest) {
            return Some(spans);
        }
        spans.push(Span::styled(rest.to_string(), Theme::tool_detail_done()));
        return Some(spans);
    }

    // 5. Bare labeled fields from expanded use_tool args: `key: value`
    if let Some(spans) = highlight_labeled_field(indent, "", trimmed) {
        return Some(spans);
    }

    None
}

fn highlight_labeled_field(indent: &str, bullet: &str, rest: &str) -> Option<Vec<Span<'static>>> {
    let rest = rest.trim_end();
    if let Some(key) = rest.strip_suffix(':') {
        if is_field_key(key) {
            return Some(vec![
                Span::raw(indent.to_string()),
                Span::styled(bullet.to_string(), Theme::meta()),
                Span::styled(key.to_string(), Theme::json_key()),
                Span::styled(":", Theme::json_punct()),
            ]);
        }
        return None;
    }
    let (key, value) = rest.split_once(": ")?;
    if !is_field_key(key) {
        return None;
    }
    Some(vec![
        Span::raw(indent.to_string()),
        Span::styled(bullet.to_string(), Theme::meta()),
        Span::styled(key.to_string(), Theme::json_key()),
        Span::styled(": ", Theme::json_punct()),
        Span::styled(value.to_string(), Theme::tool_detail_done()),
    ])
}

fn find_matching_quote(s: &str) -> Option<usize> {
    if !s.starts_with('"') {
        return None;
    }
    let bytes = s.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i = i.saturating_add(2);
            }
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
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

/// Paint-time rewrite: if stored output is still raw JSON, format it for the TUI.
///
/// Already-rewritten bodies (search_tool signatures, MCP outlines) pass through.
pub fn display_tool_output(name: &str, args: &str, output: &str, is_error: bool) -> String {
    if !output_looks_like_json(output) {
        return output.to_string();
    }
    match summarize_tool_special(name, args, output, is_error) {
        Some((_, _, Some(better))) => better,
        _ => output.to_string(),
    }
}

fn output_looks_like_json(output: &str) -> bool {
    let t = output.trim();
    if t.contains("structuredContent:") {
        return true;
    }
    (t.starts_with('{') && t.contains('}')) || (t.starts_with('[') && t.contains(']'))
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
                let (summary, better_opt) = format_search_tool_view(&val);
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
                let ready = val
                    .get("ready")
                    .and_then(|r| r.as_u64())
                    .or_else(|| {
                        val.get("connected")
                            .and_then(|c| c.as_array())
                            .map(|a| a.len() as u64)
                    })
                    .unwrap_or(0);
                let failed = val
                    .get("unavailable")
                    .or_else(|| val.get("failed"))
                    .and_then(|f| f.as_u64().or_else(|| f.as_array().map(|a| a.len() as u64)))
                    .unwrap_or(0);
                let connecting = val.get("connecting").and_then(|c| c.as_u64()).unwrap_or(0);
                let total_tools = val.get("total_tools").and_then(|t| t.as_u64()).unwrap_or(0);

                let summary = if failed > 0 {
                    format!("{ready} ready, {failed} failed · {total_tools} tools")
                } else if connecting > 0 {
                    format!("{ready} ready, {connecting} connecting · {total_tools} tools")
                } else if total_tools > 0 {
                    format!("{ready} ready · {total_tools} tools")
                } else {
                    format!("{ready} ready")
                };
                let better = format_mcp_status_view(&val);
                Some((summary, false, better))
            } else {
                Some(("status".into(), false, None))
            }
        }
        tool if tool == "use_tool" || tool.contains("__") => {
            if is_error {
                let err_msg = mcp_error_summary(output).map(|s| truncate(&s, 48));
                let summary = match err_msg {
                    Some(msg) => format!("error · {msg}"),
                    None => "error".into(),
                };
                return Some((summary, true, format_mcp_tool_output_view(args, output)));
            }

            let trimmed = output.trim();
            if let Some((_, structured)) = parse_structured_content_block(trimmed) {
                let summary = summarize_mcp_json_value(&structured);
                return Some((summary, false, format_mcp_tool_output_view(args, output)));
            }

            if (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'))
            {
                if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                    let summary = summarize_mcp_json_value(&val);
                    let better = format_mcp_tool_output_view(args, output)
                        .or_else(|| serde_json::to_string_pretty(&val).ok());
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

fn summarize_mcp_json_value(val: &Value) -> String {
    match val {
        Value::Array(arr) => match arr.len() {
            0 => "0 items".into(),
            1 => "1 item".into(),
            n => format!("{n} items"),
        },
        Value::Object(obj) => {
            if let Some(items) = obj
                .get("items")
                .or_else(|| obj.get("results"))
                .or_else(|| obj.get("data"))
                .and_then(|v| v.as_array())
            {
                format!("{} items", items.len())
            } else if let Some(count) = obj
                .get("count")
                .or_else(|| obj.get("total"))
                .and_then(|v| v.as_i64())
            {
                format!("{count} items")
            } else if let Some(status) = obj.get("status").and_then(|v| v.as_str()) {
                format!("status={status}")
            } else if let Some(msg) = obj
                .get("message")
                .or_else(|| obj.get("title"))
                .and_then(|v| v.as_str())
            {
                truncate(msg, 40)
            } else if let Some(result) = obj.get("result").and_then(|v| v.as_str()) {
                truncate(result, 40)
            } else if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
                format!("id={id}")
            } else {
                format!("{} fields", obj.len())
            }
        }
        _ => "ok".into(),
    }
}

fn parse_structured_content_block(output: &str) -> Option<(&str, Value)> {
    let marker = "structuredContent:";
    let idx = output.find(marker)?;
    let text_part = output[..idx].trim_end();
    let json_part = output[idx + marker.len()..].trim();
    let structured = serde_json::from_str::<Value>(json_part).ok()?;
    Some((text_part, structured))
}

fn mcp_error_summary(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
        return val
            .get("error")
            .or_else(|| val.get("message"))
            .or_else(|| val.get("result"))
            .and_then(|m| m.as_str())
            .map(str::to_string);
    }
    if let Some((text_part, structured)) = parse_structured_content_block(trimmed) {
        if let Some(msg) = structured
            .get("error")
            .or_else(|| structured.get("message"))
            .or_else(|| structured.get("result"))
            .and_then(|m| m.as_str())
        {
            return Some(msg.to_string());
        }
        if let Some(first) = text_part
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && *l != "structuredContent:")
        {
            return Some(first.to_string());
        }
    }
    trimmed
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && *l != "structuredContent:")
        .map(str::to_string)
}

fn format_mcp_tool_output_view(args: &str, output: &str) -> Option<String> {
    let trimmed = output.trim();
    let (plain_text, value) =
        if let Some((text_part, structured)) = parse_structured_content_block(trimmed) {
            (text_part.trim(), structured)
        } else if (trimmed.starts_with('{') && trimmed.ends_with('}'))
            || (trimmed.starts_with('[') && trimmed.ends_with(']'))
        {
            ("", serde_json::from_str::<Value>(trimmed).ok()?)
        } else {
            return None;
        };

    let mut out = String::new();
    if let Some(target) = json_field(args, "tool_name").filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!("MCP result · {}\n", target.trim()));
    } else {
        out.push_str("MCP result\n");
    }

    if !plain_text.is_empty() {
        out.push('\n');
        for line in plain_text
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.trim().is_empty())
        {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }

    let body = format_json_value_as_outline(&value, 0, 8);
    if !body.is_empty() {
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&body);
    }

    Some(out.trim_end().to_string())
}

fn format_json_value_as_outline(value: &Value, indent: usize, max_items: usize) -> String {
    let mut out = String::new();
    push_json_outline_value(&mut out, value, indent, max_items);
    out.trim_end().to_string()
}

fn push_json_outline_value(out: &mut String, value: &Value, indent: usize, max_items: usize) {
    let pad = "  ".repeat(indent);
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str(&format!("{pad}∅ empty object\n"));
                return;
            }
            for (idx, (key, val)) in map.iter().enumerate() {
                if idx >= max_items {
                    out.push_str(&format!("{pad}… +{} fields\n", map.len() - idx));
                    break;
                }
                push_json_outline_entry(out, key, val, indent, max_items);
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str(&format!("{pad}∅ empty list\n"));
                return;
            }
            for (idx, item) in arr.iter().enumerate() {
                if idx >= max_items {
                    out.push_str(&format!("{pad}… +{} items\n", arr.len() - idx));
                    break;
                }
                match item {
                    Value::Object(_) | Value::Array(_) => {
                        out.push_str(&format!("{pad}• #{idx}\n"));
                        push_json_outline_value(out, item, indent + 1, max_items.min(6));
                    }
                    _ => out.push_str(&format!("{pad}• {}\n", scalar_preview(item, 120))),
                }
            }
        }
        _ => out.push_str(&format!("{pad}{}\n", scalar_preview(value, 160))),
    }
}

fn push_json_outline_entry(
    out: &mut String,
    key: &str,
    value: &Value,
    indent: usize,
    max_items: usize,
) {
    let pad = "  ".repeat(indent);
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str(&format!("{pad}• {key}: {{}}\n"));
            } else {
                out.push_str(&format!("{pad}• {key}\n"));
                push_json_outline_value(out, value, indent + 1, max_items.min(6));
            }
        }
        Value::Array(arr) => {
            out.push_str(&format!("{pad}• {key}: {} item(s)\n", arr.len()));
            for (idx, item) in arr.iter().enumerate().take(max_items.min(5)) {
                match item {
                    Value::Object(_) | Value::Array(_) => {
                        out.push_str(&format!("{pad}  • #{idx}\n"));
                        push_json_outline_value(out, item, indent + 2, 5);
                    }
                    _ => out.push_str(&format!("{pad}  • {}\n", scalar_preview(item, 100))),
                }
            }
            if arr.len() > max_items.min(5) {
                out.push_str(&format!(
                    "{pad}  … +{} items\n",
                    arr.len().saturating_sub(max_items.min(5))
                ));
            }
        }
        _ => out.push_str(&format!("{pad}• {key}: {}\n", scalar_preview(value, 140))),
    }
}

fn scalar_preview(value: &Value, max: usize) -> String {
    match value {
        Value::String(s) => truncate(&single_line_preview(s, max), max),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        Value::Object(map) => format!("{} fields", map.len()),
        Value::Array(arr) => format!("{} item(s)", arr.len()),
    }
}

struct DiscoveredTool {
    tool_name: String,
    description: String,
    input_schema: Option<Value>,
}

struct DiscoveredServerGroup {
    server_name: String,
    tools: Vec<DiscoveredTool>,
}

fn parse_discovered_tools(
    val: &Value,
) -> (
    Vec<DiscoveredServerGroup>,
    usize,
    String,
    Option<usize>,
    Option<String>,
) {
    let status = val
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("ready")
        .to_string();
    let total_tools = val
        .get("total_tools")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let note = val
        .get("note")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());

    let mut groups: Vec<DiscoveredServerGroup> = Vec::new();

    if let Some(results) = val.get("results").and_then(|r| r.as_array()) {
        for item in results {
            if let Some(tools_arr) = item.get("tools").and_then(|t| t.as_array()) {
                let server_name = item
                    .get("server")
                    .and_then(|s| s.as_str())
                    .unwrap_or("tools")
                    .to_string();
                let mut tools = Vec::new();
                for t in tools_arr {
                    let tool_name = t
                        .get("tool_name")
                        .and_then(|s| s.as_str())
                        .unwrap_or("tool")
                        .to_string();
                    let description = t
                        .get("description")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input_schema = t.get("input_schema").cloned();
                    tools.push(DiscoveredTool {
                        tool_name,
                        description,
                        input_schema,
                    });
                }
                if !tools.is_empty() {
                    groups.push(DiscoveredServerGroup { server_name, tools });
                }
            } else if let Some(tool_name_str) = item.get("tool_name").and_then(|s| s.as_str()) {
                let tool_name = tool_name_str.to_string();
                let server_name = item
                    .get("server_name")
                    .or_else(|| item.get("server"))
                    .and_then(|s| s.as_str())
                    .or_else(|| tool_name.split("__").next())
                    .unwrap_or("tools")
                    .to_string();
                let description = item
                    .get("description")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let input_schema = item.get("input_schema").cloned();

                if let Some(group) = groups.iter_mut().find(|g| g.server_name == server_name) {
                    group.tools.push(DiscoveredTool {
                        tool_name,
                        description,
                        input_schema,
                    });
                } else {
                    groups.push(DiscoveredServerGroup {
                        server_name,
                        tools: vec![DiscoveredTool {
                            tool_name,
                            description,
                            input_schema,
                        }],
                    });
                }
            }
        }
    }

    let total_found: usize = groups.iter().map(|g| g.tools.len()).sum();
    (groups, total_found, status, total_tools, note)
}

fn format_tool_signature_and_params(tool: &DiscoveredTool) -> (String, Vec<String>) {
    let mut param_items: Vec<(bool, String, String, String)> = Vec::new();

    if let Some(ref schema) = tool.input_schema {
        let required_set: std::collections::HashSet<&str> = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
            for (name, prop) in props {
                let is_req = required_set.contains(name.as_str());
                let type_str = if let Some(enums) = prop.get("enum").and_then(|e| e.as_array()) {
                    if enums.len() <= 3 && enums.iter().all(|v| v.is_string()) {
                        enums
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| format!("\"{s}\""))
                            .collect::<Vec<_>>()
                            .join("|")
                    } else {
                        prop.get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("string")
                            .to_string()
                    }
                } else if let Some(t) = prop.get("type").and_then(|t| t.as_str()) {
                    if t == "array" {
                        if let Some(item_type) = prop
                            .get("items")
                            .and_then(|i| i.get("type"))
                            .and_then(|t| t.as_str())
                        {
                            format!("{item_type}[]")
                        } else {
                            "array".to_string()
                        }
                    } else {
                        t.to_string()
                    }
                } else {
                    "any".to_string()
                };

                let req_label = if is_req { "required" } else { "optional" };
                let default_str = if let Some(def) = prop.get("default") {
                    format!(", default: {def}")
                } else {
                    String::new()
                };

                let desc_clean = prop
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(|d| d.lines().next().unwrap_or("").trim())
                    .filter(|s| !s.is_empty());

                let detail_line = if let Some(desc) = desc_clean {
                    format!("    - {name} ({type_str}, {req_label}{default_str}): {desc}")
                } else {
                    format!("    - {name} ({type_str}, {req_label}{default_str})")
                };

                param_items.push((is_req, name.clone(), type_str, detail_line));
            }
        }
    }

    // Required parameters first, then by name for stable ordering
    param_items.sort_by(|a, b| match b.0.cmp(&a.0) {
        std::cmp::Ordering::Equal => a.1.cmp(&b.1),
        other => other,
    });

    let sig_params: Vec<String> = param_items
        .iter()
        .map(|(is_req, name, type_str, _)| {
            if *is_req {
                format!("{name}: {type_str}")
            } else {
                format!("{name}?: {type_str}")
            }
        })
        .collect();

    let param_lines: Vec<String> = param_items
        .into_iter()
        .map(|(_, _, _, line)| line)
        .collect();

    let sig = if sig_params.is_empty() {
        format!("  • {}()", tool.tool_name)
    } else {
        format!("  • {}({})", tool.tool_name, sig_params.join(", "))
    };

    (sig, param_lines)
}

fn format_search_tool_view(val: &Value) -> (String, Option<String>) {
    let (groups, total_found, status, total_tools, note) = parse_discovered_tools(val);

    let summary = if total_found == 0 {
        format!("{status} · no tools found")
    } else if total_found == 1 {
        let sname = groups
            .first()
            .map(|g| g.server_name.as_str())
            .unwrap_or("tool");
        format!("{status} · 1 tool ({sname})")
    } else if groups.len() == 1 {
        let sname = &groups[0].server_name;
        format!("{status} · {total_found} tools ({sname})")
    } else {
        format!("{status} · {total_found} tools ({} servers)", groups.len())
    };

    if total_found == 0 {
        let mut text = "No MCP tools found.".to_string();
        if let Some(n) = note {
            text.push_str(&format!("\n\nNote: {n}"));
        }
        return (summary, Some(text));
    }

    let mut out = String::new();
    let total_in_catalog = total_tools.unwrap_or(total_found);
    let more_note = if total_in_catalog > total_found {
        format!(" ({} total in catalog)", total_in_catalog)
    } else {
        String::new()
    };

    if groups.len() == 1 {
        let sname = &groups[0].server_name;
        out.push_str(&format!(
            "Found {total_found} MCP tool(s) from {sname}{more_note}:\n"
        ));
    } else {
        out.push_str(&format!(
            "Found {total_found} MCP tool(s) across {} servers{more_note}:\n",
            groups.len()
        ));
    }

    for (gi, group) in groups.iter().enumerate() {
        if groups.len() > 1 || !out.contains(&format!("from {}", group.server_name)) {
            if gi > 0 {
                out.push('\n');
            }
            out.push_str(&format!("\n[{}]\n", group.server_name));
        }

        for (ti, tool) in group.tools.iter().enumerate() {
            if ti > 0 {
                out.push('\n');
            }
            let (sig, params) = format_tool_signature_and_params(tool);
            out.push_str(&sig);
            out.push('\n');

            let desc_first = tool.description.lines().next().unwrap_or("").trim();
            if !desc_first.is_empty() {
                out.push_str(&format!("    {}\n", truncate(desc_first, 90)));
            }

            for p in params {
                out.push_str(&p);
                out.push('\n');
            }
        }
    }

    if let Some(n) = note {
        out.push_str(&format!("\nNote: {n}\n"));
    }

    (summary, Some(out.trim_end().to_string()))
}

fn format_mcp_status_view(val: &Value) -> Option<String> {
    let servers = val.get("servers").and_then(|s| s.as_array())?;
    if servers.is_empty() {
        return None;
    }
    let mut out = String::new();
    let total_tools = val.get("total_tools").and_then(|t| t.as_u64()).unwrap_or(0);
    let ready = val.get("ready").and_then(|r| r.as_u64()).unwrap_or(0);
    out.push_str(&format!(
        "MCP Servers ({ready} ready · {total_tools} tools):\n\n"
    ));
    for s in servers {
        let name = s.get("server").and_then(|v| v.as_str()).unwrap_or("server");
        let status = s
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let count = s.get("tool_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let desc = s
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        out.push_str(&format!("  • {name}: {status} ({count} tools)\n"));
        if !desc.is_empty() {
            out.push_str(&format!("    {}\n", truncate(desc, 80)));
        }
    }
    Some(out.trim_end().to_string())
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
            full.contains("- question: How does One work?"),
            "full={full}"
        );
        assert!(full.contains("- repoName: facebook/react"), "full={full}");
        assert!(
            !full.contains('{') && !full.contains('"'),
            "expanded use_tool args must not dump JSON: {full}"
        );
    }

    #[test]
    fn use_tool_args_format_nested_fields_without_json() {
        let args = r#"{"tool_name":"linear__create_issue","tool_input":{"title":"Bug","nested":{"a":1,"b":"two"},"tags":["ui","tui"],"note":"line1\nline2"}}"#;
        let text = format_use_tool_args_view(args).unwrap();
        assert!(text.contains("- title: Bug"), "text={text}");
        assert!(text.contains("- nested:"), "text={text}");
        assert!(text.contains("- a: 1"), "text={text}");
        assert!(text.contains("- b: two"), "text={text}");
        assert!(text.contains("- tags: ui, tui"), "text={text}");
        assert!(text.contains("- note:"), "text={text}");
        assert!(text.contains("    line1"), "text={text}");
        assert!(!text.contains('{'), "text={text}");
        assert!(!text.contains("\"title\""), "text={text}");
    }

    #[test]
    fn search_tool_summarizes_and_formats_better_output() {
        let args = r#"{"query":"linear"}"#;
        let out = r#"{"status":"ready","results":[{"server":"linear","tools":[{"tool_name":"linear__create_issue","description":"Create a new issue in Linear\nSupports labels and team.","input_schema":{"type":"object","properties":{"title":{"type":"string","description":"Issue title"},"priority":{"type":"integer","description":"1-5"}},"required":["title"]}}]}],"total_tools":5}"#;
        let (s, expand, better) = summarize_tool_special("search_tool", args, out, false).unwrap();
        assert_eq!(s, "ready · 1 tool (linear)");
        assert!(!expand);
        let better_text = better.unwrap();
        assert!(
            better_text.contains("Found 1 MCP tool(s) from linear (5 total in catalog):"),
            "better={better_text}"
        );
        assert!(
            better_text.contains("• linear__create_issue(title: string, priority?: integer)"),
            "better={better_text}"
        );
        assert!(
            better_text.contains("Create a new issue in Linear"),
            "better={better_text}"
        );
        assert!(
            better_text.contains("- title (string, required): Issue title"),
            "better={better_text}"
        );
        assert!(
            better_text.contains("- priority (integer, optional): 1-5"),
            "better={better_text}"
        );
    }

    #[test]
    fn search_tool_screenshot_payload_formats_grok_style() {
        let args = r#"{"query":"search"}"#;
        let out = r#"{
  "note": null,
  "results": [
    {
      "server": "agy",
      "tools": [
        {
          "description": "[MCP:agy] Search the live web using the Antigravity/agy Google session (Cloud Code googleSearch). Use for news,\ncurrent events, and facts that need citations.",
          "input_schema": {
            "properties": {
              "query": {
                "description": "Search query, including dates or locale when relevant.",
                "type": "string"
              }
            },
            "required": [
              "query"
            ],
            "type": "object"
          },
          "score": 2.817005157470703,
          "tool_name": "agy__search_web"
        }
      ]
    },
    {
      "server": "context-mode",
      "tools": [
        {
          "description": "[MCP:context-mode] Search a unified knowledge base with a multi-strategy ranking pipeline.",
          "tool_name": "context-mode__search"
        }
      ]
    }
  ]
}"#;
        let (s, expand, better) = summarize_tool_special("search_tool", args, out, false).unwrap();
        assert_eq!(s, "ready · 2 tools (2 servers)");
        assert!(!expand);
        let text = better.unwrap();
        assert!(
            text.contains("Found 2 MCP tool(s) across 2 servers:"),
            "text={text}"
        );
        assert!(text.contains("[agy]"), "text={text}");
        assert!(
            text.contains("• agy__search_web(query: string)"),
            "text={text}"
        );
        assert!(text.contains("- query (string, required): Search query, including dates or locale when relevant."), "text={text}");
        assert!(text.contains("[context-mode]"), "text={text}");
        assert!(text.contains("• context-mode__search()"), "text={text}");
    }

    #[test]
    fn search_tool_large_pretty_json_formats_from_full_payload() {
        let huge = "knowledge ".repeat(400);
        let out = format!(
            r#"{{
  "note": null,
  "results": [
    {{
      "server": "agy",
      "tools": [
        {{
          "description": "[MCP:agy] Search the live web. {huge}",
          "input_schema": {{
            "properties": {{
              "query": {{"description": "Search query", "type": "string"}}
            }},
            "required": ["query"],
            "type": "object"
          }},
          "tool_name": "agy__search_web"
        }}
      ]
    }}
  ],
  "status": "ready",
  "total_tools": 16
}}"#
        );
        assert!(
            out.len() > 4_000,
            "fixture must exceed the TUI store cap, got {}",
            out.len()
        );

        let truncated = crate::message::truncate_tool_output_for_ui(&out, 4_000);
        let old = summarize_tool_special("search_tool", r#"{"query":"search"}"#, &truncated, false);
        // Truncated pretty JSON does not parse, so the old finish path stored a dump.
        if let Some((_, _, better)) = old {
            assert!(
                better.is_none()
                    || !better
                        .as_deref()
                        .unwrap_or("")
                        .contains("agy__search_web(query: string)"),
                "truncated JSON must not be a reliable formatter input"
            );
        }

        let (s, _, better) =
            summarize_tool_special("search_tool", r#"{"query":"search"}"#, &out, false).unwrap();
        let text = better.expect("full payload should format");
        assert!(s.contains("1 tool"), "s={s}");
        assert!(
            text.contains("• agy__search_web(query: string)"),
            "text={text}"
        );
        assert!(!text.contains("\"input_schema\""), "text={text}");
        assert!(!text.contains("\"tool_name\""), "text={text}");

        let painted = display_tool_output("search_tool", r#"{"query":"search"}"#, &out, false);
        assert!(painted.contains("• agy__search_web(query: string)"));
        assert!(!painted.contains("\"input_schema\""));
    }

    #[test]
    fn search_tool_flat_results_formatted_properly() {
        let args = r#"{"query":"deepwiki"}"#;
        let out = r#"{"status":"ready","results":[{"tool_name":"deepwiki__ask_question","description":"Ask any question","input_schema":{"type":"object","properties":{"repo":{"type":"string","description":"Repo"},"q":{"type":"string"}},"required":["repo","q"]}}]}"#;
        let (s, _, better) = summarize_tool_special("search_tool", args, out, false).unwrap();
        assert_eq!(s, "ready · 1 tool (deepwiki)");
        let text = better.unwrap();
        assert!(text.contains("• deepwiki__ask_question(q: string, repo: string)"));
        assert!(text.contains("- repo (string, required): Repo"));
        assert!(text.contains("- q (string, required)"));
    }

    #[test]
    fn mcp_status_summarizes_correctly() {
        let out = r#"{"ready":2,"connecting":0,"unavailable":1,"total_tools":13,"servers":[{"server":"deepwiki","status":"ready","tool_count":3,"description":"GitHub docs"},{"server":"broken","status":"failed","tool_count":0,"description":""}]}"#;
        let (s, expand, better) = summarize_tool_special("mcp_status", "{}", out, false).unwrap();
        assert_eq!(s, "2 ready, 1 failed · 13 tools");
        assert!(!expand);
        let b = better.unwrap();
        assert!(b.contains("deepwiki: ready (3 tools)"));
        assert!(b.contains("broken: failed (0 tools)"));
    }

    #[test]
    fn use_tool_result_formats_structured_content_as_outline() {
        let args = r#"{"tool_name":"deepwiki__ask_question","tool_input":{"repoName":"xai/grok","question":"what is grok?"}}"#;
        let out = r#"Error processing question: Repository not found.

structuredContent:
{
  "result": "Error processing question: Repository not found.",
  "meta": { "retryable": false },
  "sources": ["a", "b"]
}"#;

        let (summary, expand, better) =
            summarize_tool_special("use_tool", args, out, true).unwrap();
        assert!(
            summary.contains("Repository not found"),
            "summary={summary}"
        );
        assert!(expand);
        let text = better.unwrap();
        assert!(
            text.contains("MCP result · deepwiki__ask_question"),
            "text={text}"
        );
        assert!(
            text.contains("• result: Error processing question"),
            "text={text}"
        );
        assert!(text.contains("• meta"), "text={text}");
        assert!(text.contains("• retryable: false"), "text={text}");
        assert!(text.contains("• sources: 2 item(s)"), "text={text}");
        assert!(!text.contains("structuredContent:"), "text={text}");
        assert!(!text.contains("\"result\""), "text={text}");
    }

    #[test]
    fn use_tool_result_formats_plain_json_as_outline() {
        let args = r#"{"tool_name":"linear__list_issues","tool_input":{"team":"eng"}}"#;
        let out = r#"{"items":[{"id":"A","title":"First"},{"id":"B","title":"Second"}],"nextCursor":null}"#;
        let (summary, expand, better) =
            summarize_tool_special("use_tool", args, out, false).unwrap();
        assert_eq!(summary, "2 items");
        assert!(!expand);
        let text = better.unwrap();
        assert!(
            text.contains("MCP result · linear__list_issues"),
            "text={text}"
        );
        assert!(text.contains("• items: 2 item(s)"), "text={text}");
        assert!(text.contains("• id: A"), "text={text}");
        assert!(text.contains("• title: First"), "text={text}");
        assert!(text.contains("• nextCursor: null"), "text={text}");
        assert!(!text.contains("\"items\""), "text={text}");
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

    #[test]
    fn test_maybe_pretty_json_and_extract_args() {
        let compact = r#"{"name":"test","count":10,"enabled":true}"#;
        let pretty = maybe_pretty_json(compact).unwrap();
        assert!(pretty.contains('\n'));
        assert!(pretty.contains("\"name\": \"test\""));

        let not_json = "plain text output";
        assert!(maybe_pretty_json(not_json).is_none());

        let use_tool_args = r#"{"tool_name":"deepwiki__ask_question","tool_input":{"repoName":"xai/grok","question":"what is grok?","nested":{"a":1}}}"#;
        let extracted = extract_use_tool_args(use_tool_args);
        assert!(extracted
            .iter()
            .any(|(k, v)| k == "repoName" && v == "xai/grok"));
        assert!(extracted
            .iter()
            .any(|(k, v)| k == "question" && v == "what is grok?"));
        assert!(extracted
            .iter()
            .any(|(k, v)| k == "nested" && v.contains("\"a\":1")));
    }

    #[test]
    fn test_highlight_json_line_and_is_json() {
        assert!(is_json_line("  {"));
        assert!(is_json_line("  },"));
        assert!(is_json_line("  \"repoName\": \"xai/grok\","));
        assert!(is_json_line("  \"count\": 42,"));
        assert!(is_json_line("  \"enabled\": true,"));
        assert!(is_json_line("  \"data\": null"));
        assert!(!is_json_line("plain bash output"));

        let spans = highlight_json_line("  \"key\": \"value\",");
        assert_eq!(spans[0].content, "  ");
        assert_eq!(spans[1].content, "\"key\"");
        assert_eq!(spans[2].content, ": ");
        assert_eq!(spans[3].content, "\"value\"");
        assert_eq!(spans[4].content, ",");

        let spans_num = highlight_json_line("    \"timeout\": 30");
        assert_eq!(spans_num[0].content, "    ");
        assert_eq!(spans_num[1].content, "\"timeout\"");
        assert_eq!(spans_num[2].content, ": ");
        assert_eq!(spans_num[3].content, "30");

        let field = highlight_tool_output_line("- repoName: facebook/react").unwrap();
        assert_eq!(field[1].content, "- ");
        assert_eq!(field[2].content, "repoName");
        assert_eq!(field[3].content, ": ");
        assert_eq!(field[4].content, "facebook/react");

        let outline = highlight_tool_output_line("  • result: not found").unwrap();
        assert_eq!(outline[1].content, "• ");
        assert_eq!(outline[2].content, "result");
        assert_eq!(outline[4].content, "not found");

        let spans_bool = highlight_json_line("    \"active\": true,");
        assert_eq!(spans_bool[1].content, "\"active\"");
        assert_eq!(spans_bool[3].content, "true");
        assert_eq!(spans_bool[4].content, ",");
    }
}
