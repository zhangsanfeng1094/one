use std::path::{Path, PathBuf};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::path_policy::{AccessKind, PathPolicy};
use crate::tool_args::{path_arg_or, u64_arg};

/// Soft cap so `ls` on `node_modules` cannot dump tens of thousands of rows.
const DEFAULT_LIMIT: usize = 500;
/// Skip line-counting files larger than this (show size instead).
const MAX_LINE_COUNT_BYTES: u64 = 1024 * 1024;

pub struct LsTool {
    policy: PathPolicy,
}

impl LsTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_policy(PathPolicy::workspace(cwd))
    }

    pub fn with_policy(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for LsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ls".to_string(),
            description: "List files in a directory. Text files include a line count (same \
                 semantics as `read`); binaries and files over 1MB show size. Default is \
                 the workspace root; paths outside need interactive approval or --add-dir."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path"
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!(
                            "Maximum entries to return (default: {DEFAULT_LIMIT})"
                        )
                    }
                }
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let path = path_arg_or(&call.arguments, ".").map_err(|msg| invalid_args("ls", msg))?;
        let limit = u64_arg(&call.arguments, "limit")
            .map(|n| n.max(1) as usize)
            .unwrap_or(DEFAULT_LIMIT);
        let resolved = self
            .policy
            .resolve(path, AccessKind::Read)
            .map_err(|err| tool_error("ls", err))?;

        let meta = tokio::fs::metadata(&resolved)
            .await
            .map_err(|err| tool_error("ls", err.to_string()))?;
        if !meta.is_dir() {
            return Err(tool_error(
                "ls",
                format!("Not a directory: {}", resolved.display()),
            ));
        }

        let mut entries = tokio::fs::read_dir(&resolved)
            .await
            .map_err(|err| tool_error("ls", err.to_string()))?;
        let mut rows = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|err| tool_error("ls", err.to_string()))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = entry
                .file_type()
                .await
                .map_err(|err| tool_error("ls", err.to_string()))?;
            let line = if file_type.is_dir() {
                format!("{name} [dir]")
            } else if let Some(extra) = describe_file(&entry.path()).await {
                format!("{name} [file] {extra}")
            } else {
                format!("{name} [file]")
            };
            rows.push((name, line));
        }
        rows.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

        let total = rows.len();
        let truncated = total > limit;
        let shown = rows.into_iter().take(limit).map(|(_, line)| line);
        let mut lines: Vec<String> = shown.collect();

        if lines.is_empty() {
            return Ok(ToolOutput::text_with_details(
                "(empty directory)",
                json!({
                    "path": resolved.display().to_string(),
                    "entries": 0,
                    "truncated": false,
                }),
            ));
        }
        if truncated {
            lines.push(format!("... (truncated at {limit} entries, {total} total)"));
        }

        Ok(ToolOutput::text_with_details(
            lines.join("\n"),
            json!({
                "path": resolved.display().to_string(),
                "entries": total,
                "truncated": truncated,
            }),
        ))
    }
}

/// Line count for text files (matches `read`'s `str::lines()`), size otherwise.
async fn describe_file(path: &Path) -> Option<String> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    if !meta.is_file() {
        return None;
    }
    let size = meta.len();
    if size == 0 {
        return Some("0 lines".into());
    }
    if size > MAX_LINE_COUNT_BYTES {
        return Some(format_size(size));
    }
    let bytes = tokio::fs::read(path).await.ok()?;
    if bytes.contains(&0) {
        return Some(format_size(size));
    }
    let text = String::from_utf8_lossy(&bytes);
    Some(format_line_count(text.lines().count()))
}

fn format_line_count(n: usize) -> String {
    if n == 1 {
        "1 line".into()
    } else {
        format!("{n} lines")
    }
}

fn format_size(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if n < KIB {
        format!("{n} bytes")
    } else if n < MIB {
        format!("{:.1}K", n as f64 / KIB as f64)
    } else {
        format!("{:.1}M", n as f64 / MIB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;

    fn temp_workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-ls-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("readme.md"), "# docs\n\nmore\n").unwrap();
        std::fs::write(dir.join("empty.txt"), "").unwrap();
        std::fs::write(dir.join("one.txt"), "only").unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        dir
    }

    async fn run_ls(dir: &Path, args: serde_json::Value) -> ToolOutput {
        let tool = LsTool::new(dir.to_path_buf());
        tool.execute(&ToolCall {
            id: "1".into(),
            name: "ls".into(),
            arguments: args,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn lists_line_counts_for_text_files() {
        let dir = temp_workspace();
        let out = run_ls(&dir, json!({})).await;
        let text = out.as_text();
        assert!(
            text.contains("readme.md [file] 3 lines"),
            "readme should report str::lines() count, got:\n{text}"
        );
        assert!(
            text.contains("empty.txt [file] 0 lines"),
            "empty file should be 0 lines, got:\n{text}"
        );
        assert!(
            text.contains("one.txt [file] 1 line"),
            "single line without trailing newline, got:\n{text}"
        );
        assert!(text.contains("src [dir]"), "dirs stay typed, got:\n{text}");
        assert!(
            text.contains("blob.bin [file] 4 bytes"),
            "binary should show size, got:\n{text}"
        );
        assert_eq!(out.details.as_ref().unwrap()["entries"], 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_directory_message() {
        let dir = temp_workspace();
        let empty = dir.join("vacant");
        std::fs::create_dir_all(&empty).unwrap();
        let out = run_ls(&dir, json!({ "path": "vacant" })).await;
        assert_eq!(out.as_text(), "(empty directory)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_files() {
        let dir = temp_workspace();
        let tool = LsTool::new(dir.clone());
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "ls".into(),
                arguments: json!({ "path": "readme.md" }),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Not a directory"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn honors_limit() {
        let dir = temp_workspace();
        let out = run_ls(&dir, json!({ "limit": 2 })).await;
        let text = out.as_text();
        let listed = text.lines().filter(|l| !l.starts_with("...")).count();
        assert_eq!(listed, 2, "got:\n{text}");
        assert!(text.contains("truncated at 2 entries, 5 total"), "{text}");
        assert_eq!(out.details.as_ref().unwrap()["truncated"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_and_line_helpers() {
        assert_eq!(format_line_count(0), "0 lines");
        assert_eq!(format_line_count(1), "1 line");
        assert_eq!(format_line_count(2), "2 lines");
        assert_eq!(format_size(4), "4 bytes");
        assert_eq!(format_size(1536), "1.5K");
    }
}
