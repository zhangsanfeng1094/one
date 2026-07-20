use std::path::PathBuf;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;
use tokio::process::Command;

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

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
            description: "Search file contents with ripgrep (Claude Code Grep-compatible). \
                 Prefer this over bash `rg`/`grep`. Use `glob` or `type` to narrow files, \
                 `output_mode` for files_with_matches/count, and context lines for surrounding code."
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
                        "description": "ripgrep file type (rs, py, js, …) — alternative to glob"
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
            .ok_or_else(|| invalid_args("grep", "missing `pattern`"))?;

        let path = path_arg(&call.arguments).unwrap_or(".");
        let ignore_case = bool_arg(&call.arguments, "case_insensitive", Some("ignore_case"))
            .unwrap_or(false);
        let multiline = bool_arg(&call.arguments, "multiline", None).unwrap_or(false);
        let glob = call
            .arguments
            .get("glob")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let file_type = call
            .arguments
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

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

        let resolved = self
            .policy
            .resolve(path, AccessKind::Read)
            .map_err(|err| tool_error("grep", err))?;

        let mut cmd = Command::new("rg");
        cmd.arg("--color=never");

        match output_mode {
            OutputMode::Content => {
                cmd.arg("--line-number");
                if let Some(c) = context_c {
                    cmd.arg("-C").arg(c.to_string());
                } else {
                    if let Some(a) = context_a {
                        cmd.arg("-A").arg(a.to_string());
                    }
                    if let Some(b) = context_b {
                        cmd.arg("-B").arg(b.to_string());
                    }
                }
            }
            OutputMode::FilesWithMatches => {
                cmd.arg("--files-with-matches");
            }
            OutputMode::Count => {
                cmd.arg("--count");
            }
        }

        if ignore_case {
            cmd.arg("-i");
        }
        if multiline {
            cmd.arg("-U").arg("--multiline-dotall");
        }
        if let Some(g) = glob {
            cmd.arg("--glob").arg(g);
        }
        if let Some(t) = file_type {
            cmd.arg("--type").arg(t);
        }

        cmd.arg("--").arg(pattern).arg(&resolved);
        cmd.current_dir(self.policy.cwd());

        let output = cmd
            .output()
            .await
            .map_err(|err| tool_error("grep", format!("{err} (is ripgrep `rg` installed?)")))?;

        // rg exit 1 = no matches (not an error); 2 = real error
        let code = output.status.code().unwrap_or(2);
        if code >= 2 {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(tool_error(
                "grep",
                if stderr.trim().is_empty() {
                    format!("rg failed with exit {code}")
                } else {
                    stderr.trim().to_string()
                },
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if stdout.is_empty() {
            if !stderr.trim().is_empty() && code != 0 {
                return Ok(ToolOutput::text(stderr.to_string()));
            }
            return Ok(ToolOutput::text_with_details(
                "no matches".to_string(),
                json!({
                    "matches": 0,
                    "output_mode": output_mode_label(output_mode),
                }),
            ));
        }

        let mut lines: Vec<String> = stdout
            .lines()
            .map(|line| {
                crate::truncate::truncate_line(line, crate::truncate::GREP_MAX_LINE_LENGTH).0
            })
            .collect();

        let total_before_limit = lines.len();
        if let Some(limit) = head_limit {
            if limit > 0 && lines.len() > limit {
                lines.truncate(limit);
            }
        }

        let match_count = lines.len();
        let truncated_by_head = head_limit.is_some_and(|l| l > 0 && total_before_limit > l);

        let body = lines.join("\n");
        let text = crate::truncate::apply_head_default(&body);
        let mut details = json!({
            "matches": match_count,
            "total_before_head_limit": total_before_limit,
            "output_mode": output_mode_label(output_mode),
            "head_limit_applied": truncated_by_head,
        });
        if let Some(obj) = details.as_object_mut() {
            if let Some(g) = glob {
                obj.insert("glob".into(), json!(g));
            }
            if let Some(t) = file_type {
                obj.insert("type".into(), json!(t));
            }
        }

        Ok(ToolOutput::text_with_details(text, details))
    }
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
        if Command::new("rg").arg("--version").output().await.is_err() {
            return;
        }
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
        if Command::new("rg").arg("--version").output().await.is_err() {
            return;
        }
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
        let text = format!("{out:?}");
        assert!(text.contains("alpha") || text.to_lowercase().contains("alpha"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
