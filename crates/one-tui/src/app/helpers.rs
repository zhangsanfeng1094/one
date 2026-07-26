//! Pure helpers for path completion, @file expansion, tool text parsing, etc.
//!
//! Kept free of [`App`] so unit tests and other crates can call them without
//! constructing full application state.

pub(crate) fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{}ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let m = (secs / 60.0).floor() as u64;
        let s = secs % 60.0;
        format!("{m}m{s:.0}s")
    }
}

/// Split input into (prefix, path_token) when the last token looks path-like or is `@…`.
pub(crate) fn path_token_at_end(input: &str) -> Option<(String, String)> {
    let trimmed_end = input.trim_end_matches(|c: char| c == ' ' || c == '\n');
    if trimmed_end.is_empty() {
        return None;
    }
    // Find last whitespace-separated token.
    let start = trimmed_end
        .rfind(|c: char| c.is_whitespace())
        .map(|i| i + 1)
        .unwrap_or(0);
    let token = &trimmed_end[start..];
    if token.is_empty() {
        return None;
    }
    let is_at = token.starts_with('@');
    let path_part = if is_at { &token[1..] } else { token };
    // Only complete when path-ish or @reference.
    if !is_at
        && !path_part.contains('/')
        && !path_part.starts_with('.')
        && !path_part.starts_with('~')
    {
        return None;
    }
    let prefix = input[..input.len() - token.len()].to_string();
    let partial = if is_at {
        format!("@{path_part}")
    } else {
        path_part.to_string()
    };
    Some((prefix, partial))
}

pub(crate) fn list_path_completions(partial: &str) -> Vec<String> {
    let at = partial.starts_with('@');
    let raw = if at { &partial[1..] } else { partial };
    let expanded = expand_tilde(raw);
    let (dir, file_prefix) = if expanded.ends_with('/') || expanded.is_empty() {
        (
            if expanded.is_empty() {
                ".".into()
            } else {
                expanded.clone()
            },
            String::new(),
        )
    } else {
        let path = std::path::Path::new(&expanded);
        match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => (
                if parent.as_os_str().is_empty() {
                    ".".into()
                } else {
                    parent.to_string_lossy().into_owned()
                },
                name.to_string_lossy().into_owned(),
            ),
            _ => (".".into(), expanded.clone()),
        }
    };

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !file_prefix.starts_with('.') {
            continue;
        }
        if !name.starts_with(&file_prefix) {
            continue;
        }
        let mut rendered = if dir == "." {
            name.clone()
        } else if dir.ends_with('/') {
            format!("{dir}{name}")
        } else {
            format!("{dir}/{name}")
        };
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            rendered.push('/');
        }
        if at {
            out.push(format!("@{rendered}"));
        } else {
            out.push(rendered);
        }
    }
    out.sort();
    out
}

pub(crate) fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return home;
        }
    }
    path.to_string()
}

pub(crate) fn longest_common_prefix(items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut prefix = items[0].as_str();
    for s in &items[1..] {
        while !s.starts_with(prefix) {
            if prefix.is_empty() {
                return String::new();
            }
            prefix = &prefix[..prefix.len() - 1];
        }
    }
    prefix.to_string()
}

/// Expand `@path` tokens into fenced file bodies for the model.
pub fn expand_at_files(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        // Token until whitespace.
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        let path_raw = &rest[..end];
        rest = &rest[end..];
        if path_raw.is_empty() {
            out.push('@');
            continue;
        }
        let path = expand_tilde(path_raw);
        match std::fs::read_to_string(&path) {
            Ok(body) => {
                let name = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path_raw);
                out.push_str(&format!(
                    "\n\n--- file: {path_raw} ---\n```\n{body}\n```\n--- end {name} ---\n"
                ));
            }
            Err(_) => {
                // Keep original token if unreadable.
                out.push('@');
                out.push_str(path_raw);
            }
        }
    }
    out.push_str(rest);
    out
}

/// Pull `job_…` id from task tool text (`id=job_…` or `job_id: job_…`).
pub(crate) fn extract_job_id_from_task_output(text: &str) -> Option<String> {
    for token in ["id=", "job_id:", "job_id="] {
        if let Some(pos) = text.find(token) {
            let rest = text[pos + token.len()..].trim_start();
            let id: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if id.starts_with("job_") || id.starts_with("task_") {
                return Some(id);
            }
        }
    }
    None
}

pub(crate) fn split_tool_text(content: &str) -> (String, String) {
    let content = content.trim();
    if let Some(open) = content.find('(') {
        if content.ends_with(')') && open > 0 {
            let name = content[..open].trim().to_string();
            let inner = content[open + 1..content.len() - 1].trim().to_string();
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return (name, inner);
            }
        }
    }
    let mut parts = content.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("tool").to_string();
    let detail = parts.next().unwrap_or("").to_string();
    (name, detail)
}

/// Slash commands handled by the CLI as UI ops (not agent user turns).
///
/// Keep in sync with `SLASH_COMMANDS` / `handle_slash` — anything here skips
/// chat transcript **and** ↑ prompt history. `/skill…` is intentionally excluded
/// (those are real user turns for the agent).
pub(crate) fn is_ui_slash(text: &str) -> bool {
    let cmd = text
        .split_whitespace()
        .next()
        .unwrap_or(text)
        .split(':')
        .next()
        .unwrap_or(text);
    matches!(
        cmd,
        "/session"
            | "/resume"
            | "/new"
            | "/name"
            | "/model"
            | "/login"
            | "/logout"
            | "/thinking"
            | "/compact"
            | "/settings"
            | "/skills"
            | "/agents"
            | "/mcp"
            | "/ps"
            | "/tasks"
            | "/jobs"
            | "/subagents"
            | "/tree"
            | "/rewind"
            | "/export"
            | "/reload"
            | "/clear"
            | "/help"
            | "/quit"
            | "/exit"
            | "/plan"
            | "/act"
            | "/build"
    )
}
