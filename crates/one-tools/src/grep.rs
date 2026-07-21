//! Built-in content search (ripgrep-style), implemented in-process.
//!
//! Uses the same library stack family as ripgrep (`ignore` for gitignore-aware
//! walks, `regex` for matching). No host `rg` binary is required — works the
//! same on Linux, macOS, and Windows.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use regex::RegexBuilder;
use serde_json::json;

use crate::path_policy::{AccessKind, PathPolicy};
use crate::tool_args::{bool_arg, path_arg, u64_arg};

pub struct GrepTool {
    policy: PathPolicy,
}

impl GrepTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_policy(PathPolicy::workspace(cwd))
    }

    pub fn with_policy(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Content,
    FilesWithMatches,
    Count,
}

impl OutputMode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "content" | "" => Some(Self::Content),
            "files_with_matches" | "files" | "files-with-matches" => {
                Some(Self::FilesWithMatches)
            }
            "count" => Some(Self::Count),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct SearchOpts {
    pattern: String,
    ignore_case: bool,
    multiline: bool,
    glob: Option<String>,
    file_type: Option<String>,
    output_mode: OutputMode,
    head_limit: Option<usize>,
    context_before: usize,
    context_after: usize,
    /// Workspace root for display paths and policy.
    cwd: PathBuf,
}

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
            description: "Search file contents with an in-process ripgrep-style engine \
                 (no host `rg` required). Prefer this over bash `rg`/`grep`. Use `glob` or \
                 `type` to narrow files, `output_mode` for files_with_matches/count, and \
                 context lines for surrounding code."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search (default: workspace root)"
                    },
                    "file_path": {
                        "type": "string",
                        "description": "Alias for `path` (Claude Code compatibility)"
                    },
                    "glob": {
                        "type": "string",
                        "description": "Glob to filter files, e.g. \"*.rs\", \"**/*.{ts,tsx}\""
                    },
                    "type": {
                        "type": "string",
                        "description": "File type shorthand (rs, py, js, ts, go, …) — alternative to glob"
                    },
                    "output_mode": {
                        "type": "string",
                        "description": "content (default, matching lines) | files_with_matches | count",
                        "enum": ["content", "files_with_matches", "count"]
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "Case insensitive search (Claude name). Alias: ignore_case"
                    },
                    "ignore_case": {
                        "type": "boolean",
                        "description": "Alias for case_insensitive"
                    },
                    "multiline": {
                        "type": "boolean",
                        "description": "Enable multiline matching (dot matches newlines)"
                    },
                    "head_limit": {
                        "type": "integer",
                        "description": "Max matching lines/files/counts to return (0 = unlimited, still subject to output caps)"
                    },
                    "context": {
                        "type": "integer",
                        "description": "Lines of context before and after each match (-C). Alias for setting both -A and -B"
                    },
                    "-A": {
                        "type": "integer",
                        "description": "Lines of context after each match"
                    },
                    "-B": {
                        "type": "integer",
                        "description": "Lines of context before each match"
                    },
                    "-C": {
                        "type": "integer",
                        "description": "Lines of context before and after (same as context)"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let pattern = call
            .arguments
            .get("pattern")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid_args("grep", "missing `pattern`"))?
            .to_string();

        let path = path_arg(&call.arguments).unwrap_or(".").to_string();
        let ignore_case = bool_arg(&call.arguments, "case_insensitive", Some("ignore_case"))
            .unwrap_or(false);
        let multiline = bool_arg(&call.arguments, "multiline", None).unwrap_or(false);
        let glob = call
            .arguments
            .get("glob")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let file_type = call
            .arguments
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let output_mode = call
            .arguments
            .get("output_mode")
            .and_then(|v| v.as_str())
            .map(|s| {
                OutputMode::parse(s).ok_or_else(|| {
                    invalid_args(
                        "grep",
                        "output_mode must be content | files_with_matches | count",
                    )
                })
            })
            .transpose()?
            .unwrap_or(OutputMode::Content);

        let head_limit = u64_arg(&call.arguments, "head_limit").map(|n| n as usize);
        let context_c = u64_arg(&call.arguments, "context")
            .or_else(|| u64_arg(&call.arguments, "-C"))
            .map(|n| n as usize);
        let context_a = u64_arg(&call.arguments, "-A").map(|n| n as usize);
        let context_b = u64_arg(&call.arguments, "-B").map(|n| n as usize);

        let (context_before, context_after) = if let Some(c) = context_c {
            (c, c)
        } else {
            (context_b.unwrap_or(0), context_a.unwrap_or(0))
        };

        let resolved = self
            .policy
            .resolve(&path, AccessKind::Read)
            .map_err(|err| tool_error("grep", err))?;

        let opts = SearchOpts {
            pattern,
            ignore_case,
            multiline,
            glob,
            file_type,
            output_mode,
            head_limit,
            context_before,
            context_after,
            cwd: self.policy.cwd().to_path_buf(),
        };

        let policy = self.policy.clone();
        let search_result = tokio::task::spawn_blocking(move || run_search(resolved, opts, policy))
            .await
            .map_err(|err| tool_error("grep", format!("search task failed: {err}")))?;

        let result = search_result?;

        if result.lines.is_empty() {
            return Ok(ToolOutput::text_with_details(
                "no matches".to_string(),
                json!({
                    "matches": 0,
                    "output_mode": output_mode_label(output_mode),
                }),
            ));
        }

        let match_count = result.lines.len();
        let body = result.lines.join("\n");
        let text = crate::truncate::present_tool_output(
            &body,
            "grep",
            self.policy.cwd(),
            crate::truncate::PreviewStyle::Head,
        )
        .text;
        let mut details = json!({
            "matches": match_count,
            "total_before_head_limit": result.total_before_head_limit,
            "output_mode": output_mode_label(output_mode),
            "head_limit_applied": result.head_limit_applied,
            "engine": "in-process",
        });
        if let Some(obj) = details.as_object_mut() {
            if let Some(ref g) = result.glob {
                obj.insert("glob".into(), json!(g));
            }
            if let Some(ref t) = result.file_type {
                obj.insert("type".into(), json!(t));
            }
        }

        Ok(ToolOutput::text_with_details(text, details))
    }
}

struct SearchResult {
    lines: Vec<String>,
    total_before_head_limit: usize,
    head_limit_applied: bool,
    glob: Option<String>,
    file_type: Option<String>,
}

fn run_search(root: PathBuf, opts: SearchOpts, policy: PathPolicy) -> Result<SearchResult> {
    let mut builder = RegexBuilder::new(&opts.pattern);
    builder.case_insensitive(opts.ignore_case);
    if opts.multiline {
        builder.multi_line(true);
        builder.dot_matches_new_line(true);
    }
    let re = builder
        .build()
        .map_err(|err| tool_error("grep", format!("invalid regex: {err}")))?;

    let type_glob = opts
        .file_type
        .as_deref()
        .and_then(type_to_glob)
        .map(str::to_string);
    if opts.file_type.is_some() && type_glob.is_none() {
        return Err(tool_error(
            "grep",
            format!(
                "unknown file type `{}`; use `glob` instead (known: rs, py, js, ts, tsx, jsx, go, java, c, cpp, h, hpp, cs, rb, php, swift, kt, scala, sh, md, json, yaml, toml, xml, html, css, sql, txt)",
                opts.file_type.as_deref().unwrap_or("")
            ),
        ));
    }

    // Prefer explicit glob; else type mapping.
    let effective_glob = opts.glob.clone().or(type_glob);

    let files = collect_files(&root, effective_glob.as_deref(), &policy)?;

    let mut lines: Vec<String> = Vec::new();
    let mut total_hits: usize = 0; // mode-dependent count before head_limit
    let limit = opts.head_limit.filter(|&n| n > 0);
    let mut stopped_early = false;

    for file in files {
        if limit.is_some_and(|l| total_hits >= l) {
            stopped_early = true;
            break;
        }

        let content = match std::fs::read(&file) {
            Ok(bytes) => {
                // Skip binary (NUL in first 8 KiB), same idea as ripgrep default.
                let probe = &bytes[..bytes.len().min(8192)];
                if probe.contains(&0) {
                    continue;
                }
                match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
                }
            }
            Err(_) => continue,
        };

        let display = display_path(&file, &opts.cwd);

        match opts.output_mode {
            OutputMode::FilesWithMatches => {
                let hit = if opts.multiline {
                    re.is_match(&content)
                } else {
                    content.lines().any(|line| re.is_match(line))
                };
                if hit {
                    total_hits += 1;
                    if limit.is_none_or(|l| lines.len() < l) {
                        lines.push(display);
                    }
                }
            }
            OutputMode::Count => {
                let count = if opts.multiline {
                    re.find_iter(&content).count()
                } else {
                    content.lines().filter(|line| re.is_match(line)).count()
                };
                if count > 0 {
                    total_hits += 1;
                    if limit.is_none_or(|l| lines.len() < l) {
                        lines.push(format!("{display}:{count}"));
                    }
                }
            }
            OutputMode::Content => {
                let file_lines: Vec<&str> = content.lines().collect();
                let mut match_idxs: Vec<usize> = Vec::new();

                if opts.multiline {
                    for m in re.find_iter(&content) {
                        let line_idx = byte_to_line_idx(&content, m.start());
                        if match_idxs.last() != Some(&line_idx) {
                            match_idxs.push(line_idx);
                        }
                    }
                } else {
                    for (i, line) in file_lines.iter().enumerate() {
                        if re.is_match(line) {
                            match_idxs.push(i);
                        }
                    }
                }

                if match_idxs.is_empty() {
                    continue;
                }

                let use_context = opts.context_before > 0 || opts.context_after > 0;
                if !use_context {
                    for idx in match_idxs {
                        if limit.is_some_and(|l| total_hits >= l) {
                            stopped_early = true;
                            break;
                        }
                        total_hits += 1;
                        let text = file_lines.get(idx).copied().unwrap_or("");
                        let truncated =
                            crate::truncate::truncate_line(text, crate::truncate::GREP_MAX_LINE_LENGTH)
                                .0;
                        lines.push(format!("{display}:{}:{truncated}", idx + 1));
                    }
                } else {
                    // Emit rg-like context blocks with `--` between non-overlapping groups.
                    let mut ranges: Vec<(usize, usize, Vec<usize>)> = Vec::new();
                    for &idx in &match_idxs {
                        let start = idx.saturating_sub(opts.context_before);
                        let end = (idx + opts.context_after).min(file_lines.len().saturating_sub(1));
                        if let Some(last) = ranges.last_mut() {
                            if start <= last.1.saturating_add(1) {
                                last.1 = last.1.max(end);
                                last.2.push(idx);
                                continue;
                            }
                        }
                        ranges.push((start, end, vec![idx]));
                    }

                    let mut first_block = true;
                    for (start, end, mids) in ranges {
                        if limit.is_some_and(|l| total_hits >= l) {
                            stopped_early = true;
                            break;
                        }
                        if !first_block {
                            lines.push("--".to_string());
                        }
                        first_block = false;
                        let mid_set: std::collections::HashSet<usize> = mids.into_iter().collect();
                        for i in start..=end {
                            if mid_set.contains(&i) {
                                if limit.is_some_and(|l| total_hits >= l) {
                                    stopped_early = true;
                                    break;
                                }
                                total_hits += 1;
                                let text = file_lines.get(i).copied().unwrap_or("");
                                let truncated = crate::truncate::truncate_line(
                                    text,
                                    crate::truncate::GREP_MAX_LINE_LENGTH,
                                )
                                .0;
                                lines.push(format!("{display}:{}:{truncated}", i + 1));
                            } else {
                                let text = file_lines.get(i).copied().unwrap_or("");
                                let truncated = crate::truncate::truncate_line(
                                    text,
                                    crate::truncate::GREP_MAX_LINE_LENGTH,
                                )
                                .0;
                                // Context lines use `-` separator like rg.
                                lines.push(format!("{display}-{}-{truncated}", i + 1));
                            }
                        }
                    }
                }
            }
        }
    }

    let head_limit_applied = stopped_early
        || limit.is_some_and(|l| total_hits > l || lines.len() > l);
    // For content without early stop, total_hits == match lines; keep total_before as total_hits.
    let total_before_head_limit = total_hits.max(lines.len());

    Ok(SearchResult {
        lines,
        total_before_head_limit,
        head_limit_applied,
        glob: opts.glob,
        file_type: opts.file_type,
    })
}

fn collect_files(root: &Path, glob: Option<&str>, policy: &PathPolicy) -> Result<Vec<PathBuf>> {
    if root.is_file() {
        policy
            .check(root, AccessKind::Read)
            .map_err(|err| tool_error("grep", err))?;
        return Ok(vec![root.to_path_buf()]);
    }

    let mut walker = WalkBuilder::new(root);
    walker.hidden(true);
    walker.parents(true);
    walker.git_ignore(true);
    walker.git_global(true);
    walker.git_exclude(true);
    walker.ignore(true);
    // Follow rg-ish defaults: skip ignored, don't require git.
    walker.require_git(false);

    if let Some(g) = glob {
        let mut ob = OverrideBuilder::new(root);
        // Support brace expansion lightly: `*.{ts,tsx}` → two patterns.
        for part in expand_brace_glob(g) {
            ob.add(&part)
                .map_err(|err| tool_error("grep", format!("invalid glob `{part}`: {err}")))?;
        }
        let overrides = ob
            .build()
            .map_err(|err| tool_error("grep", format!("invalid glob: {err}")))?;
        walker.overrides(overrides);
    }

    let files = std::sync::Mutex::new(Vec::new());
    let err_slot = std::sync::Mutex::new(None::<String>);
    // Cap walk concurrency so agent tools stay light.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(1);
    walker.threads(threads);

    walker.build_parallel().run(|| {
        let files = &files;
        let err_slot = &err_slot;
        let policy = policy;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    // Non-fatal walk errors (broken symlink etc.).
                    let _ = err;
                    return WalkState::Continue;
                }
            };
            if !entry
                .file_type()
                .map(|ft| ft.is_file())
                .unwrap_or(false)
            {
                return WalkState::Continue;
            }
            let path = entry.path();
            if policy.check(path, AccessKind::Read).is_err() {
                return WalkState::Continue;
            }
            if let Ok(mut guard) = files.lock() {
                guard.push(path.to_path_buf());
            }
            // Soft cap: avoid walking unbounded trees for agent use.
            if let Ok(guard) = files.lock() {
                if guard.len() >= 50_000 {
                    if let Ok(mut e) = err_slot.lock() {
                        *e = Some("search capped at 50000 files; narrow path/glob".into());
                    }
                    return WalkState::Quit;
                }
            }
            WalkState::Continue
        })
    });

    if let Ok(slot) = err_slot.lock() {
        if let Some(msg) = slot.as_ref() {
            // Soft warning only if we got zero files; otherwise proceed with partial.
            let empty = files.lock().map(|g| g.is_empty()).unwrap_or(true);
            if empty {
                return Err(tool_error("grep", msg.clone()));
            }
        }
    }

    let mut files = files.into_inner().unwrap_or_default();
    files.sort();
    Ok(files)
}

/// Expand a single-level brace glob: `**/*.{ts,tsx}` → two globs.
fn expand_brace_glob(glob: &str) -> Vec<String> {
    let Some(open) = glob.find('{') else {
        return vec![glob.to_string()];
    };
    let Some(close) = glob[open..].find('}') else {
        return vec![glob.to_string()];
    };
    let close = open + close;
    let prefix = &glob[..open];
    let suffix = &glob[close + 1..];
    let inner = &glob[open + 1..close];
    if inner.is_empty() || inner.contains('{') {
        return vec![glob.to_string()];
    }
    inner
        .split(',')
        .map(|part| format!("{prefix}{}{suffix}", part.trim()))
        .collect()
}

fn type_to_glob(t: &str) -> Option<&'static str> {
    match t.trim().to_ascii_lowercase().as_str() {
        "rs" | "rust" => Some("*.rs"),
        "py" | "python" => Some("*.py"),
        "js" | "javascript" => Some("*.js"),
        "ts" | "typescript" => Some("*.ts"),
        "tsx" => Some("*.tsx"),
        "jsx" => Some("*.jsx"),
        "go" => Some("*.go"),
        "java" => Some("*.java"),
        "c" => Some("*.c"),
        "cpp" | "cc" | "cxx" => Some("*.{cpp,cc,cxx}"),
        "h" => Some("*.h"),
        "hpp" => Some("*.hpp"),
        "cs" | "csharp" => Some("*.cs"),
        "rb" | "ruby" => Some("*.rb"),
        "php" => Some("*.php"),
        "swift" => Some("*.swift"),
        "kt" | "kotlin" => Some("*.kt"),
        "scala" => Some("*.scala"),
        "sh" | "bash" | "zsh" => Some("*.{sh,bash,zsh}"),
        "md" | "markdown" => Some("*.md"),
        "json" => Some("*.json"),
        "yaml" | "yml" => Some("*.{yaml,yml}"),
        "toml" => Some("*.toml"),
        "xml" => Some("*.xml"),
        "html" | "htm" => Some("*.{html,htm}"),
        "css" => Some("*.css"),
        "scss" => Some("*.scss"),
        "sql" => Some("*.sql"),
        "txt" | "text" => Some("*.txt"),
        _ => None,
    }
}

fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

fn byte_to_line_idx(content: &str, byte_offset: usize) -> usize {
    content[..byte_offset.min(content.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
}

fn output_mode_label(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Content => "content",
        OutputMode::FilesWithMatches => "files_with_matches",
        OutputMode::Count => "count",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;
    use serde_json::json;

    fn temp_workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-grep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("readme.md"), "alpha docs\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn glob_filters_and_head_limit() {
        let dir = temp_workspace();
        let tool = GrepTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "grep".into(),
                arguments: json!({
                    "pattern": "alpha",
                    "glob": "*.rs",
                    "output_mode": "files_with_matches",
                    "head_limit": 1
                }),
            })
            .await
            .unwrap();
        let text = out.as_text();
        assert!(
            text.contains("a.rs") || text.contains("b.rs"),
            "got: {text}"
        );
        assert!(
            !text.contains("readme.md"),
            "glob should exclude md: {text}"
        );
        assert_eq!(out.details.as_ref().unwrap()["matches"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn case_insensitive_alias() {
        let dir = temp_workspace();
        let tool = GrepTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "grep".into(),
                arguments: json!({
                    "pattern": "ALPHA",
                    "path": "src/a.rs",
                    "case_insensitive": true
                }),
            })
            .await
            .unwrap();
        let text = out.as_text();
        assert!(
            text.to_lowercase().contains("alpha"),
            "expected match, got: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_matches() {
        let dir = temp_workspace();
        let tool = GrepTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "grep".into(),
                arguments: json!({ "pattern": "zzzz_not_found_zzzz" }),
            })
            .await
            .unwrap();
        assert_eq!(out.as_text(), "no matches");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn type_rs_filters() {
        let dir = temp_workspace();
        let tool = GrepTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "grep".into(),
                arguments: json!({
                    "pattern": "alpha",
                    "type": "rs",
                    "output_mode": "files_with_matches"
                }),
            })
            .await
            .unwrap();
        let text = out.as_text();
        assert!(text.contains("a.rs") || text.contains("b.rs"), "{text}");
        assert!(!text.contains("readme.md"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn brace_glob_expands() {
        let parts = expand_brace_glob("**/*.{ts,tsx}");
        assert_eq!(parts, vec!["**/*.ts".to_string(), "**/*.tsx".to_string()]);
    }
}
