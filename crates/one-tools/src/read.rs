use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::image::{is_image_path, mime_from_bytes, mime_from_path, MAX_IMAGE_BYTES};
use one_core::tool::{tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::memory_io::{is_memory_path, wrap_memory_read, MemoryLookupBudget};
use crate::path_policy::{AccessKind, PathPolicy};
use crate::tool_args::{path_arg_for_tool, path_properties};

pub struct ReadTool {
    policy: PathPolicy,
    memory_lookups: Arc<MemoryLookupBudget>,
}

impl ReadTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_policy(PathPolicy::workspace(cwd))
    }

    pub fn with_policy(policy: PathPolicy) -> Self {
        Self {
            policy,
            memory_lookups: MemoryLookupBudget::unlimited(),
        }
    }

    pub fn with_memory_lookups(mut self, budget: Arc<MemoryLookupBudget>) -> Self {
        self.memory_lookups = budget;
        self
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        let scope = if self.policy.is_full_access() {
            "any path".to_string()
        } else {
            format!(
                "workspace `{}`, --add-dir roots, and agent skills dir \
                 (interactive sessions may grant one-path or session-root read after approval; \
                 prefer staying in the workspace — do not blind-scan home config dirs)",
                self.policy.cwd().display()
            )
        };
        let mut properties = path_properties("Required file path.");
        if let Some(obj) = properties.as_object_mut() {
            obj.insert(
                "offset".into(),
                json!({
                    "type": "integer",
                    "description": "1-based line offset (text only)"
                }),
            );
            obj.insert(
                "limit".into(),
                json!({
                    "type": "integer",
                    "description": "Maximum lines to read (text only; still subject to 50KB cap)"
                }),
            );
        }
        ToolDefinition {
            name: "read".to_string(),
            description: format!(
                "Read a file from the filesystem (Claude Code Read-compatible). Always pass \
                 `path`. Text files return clean content (no line-number prefixes); \
                 image files (png/jpeg/gif/webp/bmp) return image content for vision models. \
                 Text output is capped (~2000 lines / 50KB from the requested window; use offset/limit for slices). \
                 Allowed: {scope}."
            ),
            parameters: json!({
                "type": "object",
                "properties": properties,
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let path = path_arg_for_tool(
            &call.arguments,
            "read",
            "missing `path`. Every read call must include the file path.",
        )?;

        let resolved = self
            .policy
            .resolve(path, AccessKind::Read)
            .map_err(|err| tool_error("read", err))?;

        let metadata = tokio::fs::metadata(&resolved).await.map_err(|err| {
            tool_error(
                "read",
                format_read_io_error(path, &resolved, self.policy.cwd(), &err),
            )
        })?;
        if metadata.is_dir() {
            return Err(tool_error(
                "read",
                format!(
                    "is a directory, not a file: `{}` (resolved to `{}`). Use `ls` or `find` to inspect directories.",
                    path,
                    resolved.display()
                ),
            ));
        }

        let is_mem = is_memory_path(&resolved);
        if is_mem {
            if let Err(msg) = self.memory_lookups.try_consume() {
                let text = one_core::system_reminder(msg);
                return Ok(ToolOutput::text_with_details(
                    text,
                    json!({
                        "path": path,
                        "memoryLookupBudgetExceeded": true,
                    }),
                ));
            }
        }

        // Prefer extension, then magic-byte sniff for extension-less images.
        if is_image_path(&resolved) || looks_like_image_file(&resolved).await {
            return read_image(path, &resolved).await;
        }

        let content = tokio::fs::read_to_string(&resolved).await.map_err(|err| {
            tool_error(
                "read",
                format_read_io_error(path, &resolved, self.policy.cwd(), &err),
            )
        })?;

        // Empty files still "succeed" — remind the model not to invent contents.
        if content.is_empty() {
            let text = one_core::system_reminder(format!(
                "File exists but is empty: `{path}`.\n\
                 Do not invent file contents. If you expected content, check the path or create it with `write`."
            ));
            return Ok(ToolOutput::text_with_details(
                text,
                json!({
                    "path": path,
                    "lines": 0,
                    "offset": 1,
                    "fileLines": 0,
                    "truncated": false,
                    "empty": true,
                }),
            ));
        }

        let offset = call
            .arguments
            .get("offset")
            .and_then(|value| value.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = call.arguments.get("limit").and_then(|value| value.as_u64());

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.saturating_sub(1);
        // Cap explicit limit at tool_output max_lines so a huge limit cannot flood context.
        let line_cap = crate::truncate::tool_output_limits().max_lines;
        let max_window = limit
            .map(|n| (n as usize).min(line_cap))
            .unwrap_or(line_cap);

        // Offset past EOF used to panic: `&lines[start..end]` with start > end
        // (e.g. offset=1680 on a 1674-line file → start=1679, end=1674).
        if start >= lines.len() {
            let file_lines = lines.len();
            let text = one_core::system_reminder(format!(
                "Offset {offset} is past the end of `{path}` ({file_lines} lines).\n\
                 Use offset=1 to read from the start, or a value ≤ {file_lines}."
            ));
            return Ok(ToolOutput::text_with_details(
                text,
                json!({
                    "path": path,
                    "lines": 0,
                    "offset": offset,
                    "fileLines": file_lines,
                    "truncated": false,
                    "offsetPastEnd": true,
                }),
            ));
        }

        let end = (start + max_window).min(lines.len());
        let slice = &lines[start..end];

        // Clean output: no line-number prefixes (much cleaner for model + user)
        // Numbering is still available in ToolOutput.details if needed.
        let text = if slice.is_empty() {
            String::new()
        } else {
            slice.join("\n")
        };

        // Cap by lines/bytes; Claude-style PARTIAL view tells model how to continue.
        let presented = crate::truncate::present_file_read(&text, lines.len(), offset);
        // Also note when the file continues past this window even if bytes fit.
        let mut text = presented.text;
        let more_in_file = end < lines.len();
        if more_in_file && !text.contains("PARTIAL view") {
            text.push_str(&format!(
                "\n\n--- PARTIAL view ---\n\
                 window ends at line {end} of {} total. \
                 Continue with offset={} (or use grep).",
                lines.len(),
                end + 1
            ));
        }

        if is_mem {
            text = wrap_memory_read(&resolved, &content, &text);
        }

        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "path": path,
                "lines": end.saturating_sub(start),
                "offset": offset,
                "fileLines": lines.len(),
                "truncated": presented.truncated || more_in_file,
                "memory": is_mem,
                "resolvedPath": resolved.display().to_string(),
            }),
        ))
    }
}

fn format_read_io_error(path: &str, resolved: &Path, cwd: &Path, err: &std::io::Error) -> String {
    match err.kind() {
        ErrorKind::NotFound => {
            let mut msg = format!(
                "file not found: `{path}` (resolved to `{}`). Current working directory: `{}`.",
                resolved.display(),
                cwd.display()
            );
            msg.push_str(
                "\nIf this path came from a guess, use `find`, `grep`, or `ls` to locate the file before retrying.",
            );
            msg
        }
        ErrorKind::PermissionDenied => format!(
            "permission denied reading `{path}` (resolved to `{}`). Current working directory: `{}`.",
            resolved.display(),
            cwd.display()
        ),
        _ => format!(
            "failed to read `{path}` (resolved to `{}`): {err}. Current working directory: `{}`.",
            resolved.display(),
            cwd.display()
        ),
    }
}

async fn looks_like_image_file(path: &Path) -> bool {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return false;
    };
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 16];
    let Ok(n) = file.read(&mut buf).await else {
        return false;
    };
    mime_from_bytes(&buf[..n]).is_some()
}

async fn read_image(path: &str, resolved: &Path) -> Result<ToolOutput> {
    let bytes = tokio::fs::read(resolved)
        .await
        .map_err(|err| tool_error("read", err.to_string()))?;

    if bytes.is_empty() {
        return Err(tool_error("read", "image file is empty"));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(tool_error(
            "read",
            format!(
                "image too large ({} bytes > {} max); resize or use a smaller file",
                bytes.len(),
                MAX_IMAGE_BYTES
            ),
        ));
    }

    let mime = mime_from_bytes(&bytes)
        .or_else(|| mime_from_path(resolved))
        .ok_or_else(|| {
            tool_error(
                "read",
                "file is not a supported image (png/jpeg/gif/webp/bmp)",
            )
        })?;

    // Keep the workspace path (no media copy) — file is durable in the project.
    Ok(ToolOutput::image_path_with_details(
        mime,
        resolved.display().to_string(),
        json!({
            "path": path,
            "mimeType": mime,
            "bytes": bytes.len(),
            "kind": "image",
            "resolvedPath": resolved.display().to_string(),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::image::decode_base64;
    use one_core::message::TextOrImage;
    use one_core::tool::ToolCall;
    use serde_json::json;
    use std::io::Write;

    // 1×1 PNG
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[tokio::test]
    async fn reads_png_as_image_block() {
        let dir = tempfile_dir();
        let path = dir.join("dot.png");
        let bytes = decode_base64(TINY_PNG_B64).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        let tool = ReadTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": "dot.png" }),
            })
            .await
            .unwrap();

        assert!(out.has_images());
        assert!(matches!(
            &out.content[0],
            TextOrImage::Image { mime_type, .. } if mime_type == "image/png"
        ));
        let ui = out.as_ui_text();
        assert!(ui.contains("image"), "{ui}");
    }

    #[tokio::test]
    async fn reads_text_cleanly() {
        let dir = tempfile_dir();
        let path = dir.join("a.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();

        let tool = ReadTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": "a.txt" }),
            })
            .await
            .unwrap();

        let text = out.as_text();
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("world"), "{text}");
        assert!(!out.has_images());
        assert_eq!(
            out.details.as_ref().and_then(|d| d.get("resolvedPath")),
            Some(&json!(path.display().to_string()))
        );
    }

    #[tokio::test]
    async fn missing_file_error_includes_resolved_path_and_cwd() {
        let dir = tempfile_dir();
        let tool = ReadTool::new(dir.clone());
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": "missing.rs" }),
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("file not found"), "{msg}");
        assert!(msg.contains("missing.rs"), "{msg}");
        assert!(msg.contains(&dir.display().to_string()), "{msg}");
        assert!(msg.contains("find") && msg.contains("grep"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn directory_error_suggests_directory_tools() {
        let dir = tempfile_dir();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let tool = ReadTool::new(dir.clone());
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": "src" }),
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("is a directory"), "{msg}");
        assert!(msg.contains("ls") && msg.contains("find"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reads_text_without_line_numbers() {
        let dir = tempfile_dir();
        let path = dir.join("wide.txt");
        // 12 lines → should output clean content (no prefixes).
        let body: String = (1..=12).map(|i| format!("L{i}\n")).collect();
        std::fs::write(&path, &body).unwrap();

        let tool = ReadTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": "wide.txt" }),
            })
            .await
            .unwrap();

        let text = out.as_text();
        assert!(
            text.contains("L1") && text.contains("L9") && text.contains("L10"),
            "should contain clean content without prefixes, got:\n{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offset_past_eof_does_not_panic() {
        // Regression: offset past file end used to panic with
        // "slice index starts at N but ends at M" (start > end).
        let dir = tempfile_dir();
        let path = dir.join("short.txt");
        let body: String = (1..=10).map(|i| format!("L{i}\n")).collect();
        std::fs::write(&path, &body).unwrap();

        let tool = ReadTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": "short.txt", "offset": 1680, "limit": 50 }),
            })
            .await
            .unwrap();

        let text = out.as_text();
        assert!(
            text.contains("past the end") || text.contains("offset"),
            "expected past-EOF guidance, got:\n{text}"
        );
        assert_eq!(
            out.details.as_ref().and_then(|d| d.get("offsetPastEnd")),
            Some(&json!(true))
        );
        assert_eq!(
            out.details.as_ref().and_then(|d| d.get("fileLines")),
            Some(&json!(10))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offset_exactly_last_line_ok() {
        let dir = tempfile_dir();
        let path = dir.join("tail.txt");
        let body: String = (1..=5).map(|i| format!("L{i}\n")).collect();
        std::fs::write(&path, &body).unwrap();

        let tool = ReadTool::new(dir.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": "tail.txt", "offset": 5, "limit": 10 }),
            })
            .await
            .unwrap();

        let text = out.as_text();
        assert!(text.contains("L5"), "expected last line, got:\n{text}");
        assert!(!text.contains("past the end"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn denies_read_outside_workspace() {
        let dir = tempfile_dir();
        let tool = ReadTool::new(dir.clone());
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": "/etc/passwd" }),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outside workspace"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn memory_body_gets_age_reminder() {
        let dir = tempfile_dir();
        let mem = dir.join("memory").join("_global");
        std::fs::create_dir_all(&mem).unwrap();
        let body = mem.join("tip.md");
        std::fs::write(
            &body,
            "---\nupdated: 2020-01-01\n---\n\nPrefer cargo test.\n",
        )
        .unwrap();

        let policy = PathPolicy::workspace(dir.clone()).with_readable_root(mem.clone());
        let tool = ReadTool::with_policy(policy);
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": body.to_string_lossy() }),
            })
            .await
            .unwrap();
        let text = out.as_text();
        assert!(text.contains("system-reminder"), "{text}");
        assert!(
            text.contains("Point-in-time") || text.contains("verify"),
            "{text}"
        );
        assert!(
            text.contains("Prefer cargo test") || text.contains("cargo"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn memory_lookup_budget_blocks() {
        let dir = tempfile_dir();
        let mem = dir.join("memory").join("_global");
        std::fs::create_dir_all(&mem).unwrap();
        let body = mem.join("a.md");
        std::fs::write(&body, "hello memory\n").unwrap();

        let budget = MemoryLookupBudget::new(1);
        let policy = PathPolicy::workspace(dir.clone()).with_readable_root(mem);
        let tool = ReadTool::with_policy(policy).with_memory_lookups(budget.clone());
        let path = body.to_string_lossy().to_string();
        tool.execute(&ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: json!({ "path": path.clone() }),
        })
        .await
        .unwrap();
        let out = tool
            .execute(&ToolCall {
                id: "2".into(),
                name: "read".into(),
                arguments: json!({ "path": path }),
            })
            .await
            .unwrap();
        assert!(
            out.as_text().contains("budget exceeded") || out.as_text().contains("Lookup budget"),
            "{}",
            out.as_text()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "one-read-test-{}-{}-{}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // keep dir non-empty for walkers; also used by empty-dir tests
        let mut f = std::fs::File::create(dir.join(".keep")).unwrap();
        let _ = writeln!(f, "test");
        dir
    }
}
