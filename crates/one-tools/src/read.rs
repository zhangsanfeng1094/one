use std::path::{Path, PathBuf};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::image::{is_image_path, mime_from_bytes, mime_from_path, MAX_IMAGE_BYTES};
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::path_policy::{AccessKind, PathPolicy};

pub struct ReadTool {
    policy: PathPolicy,
}

impl ReadTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_policy(PathPolicy::workspace(cwd))
    }

    pub fn with_policy(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        let scope = if self.policy.is_full_access() {
            "any path".to_string()
        } else {
            format!(
                "workspace `{}`, --add-dir roots, and agent skills dir",
                self.policy.cwd().display()
            )
        };
        ToolDefinition {
            name: "read".to_string(),
            description: format!(
                "Read a file from the filesystem. Text files return numbered lines; \
                 image files (png/jpeg/gif/webp/bmp) return image content for vision models. \
                 Text output is capped (~2000 lines / 50KB from the requested window; use offset/limit for slices). \
                 Allowed: {scope}."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" },
                    "offset": { "type": "integer", "description": "1-based line offset (text only)" },
                    "limit": { "type": "integer", "description": "Maximum lines to read (text only; still subject to 50KB cap)" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let path = call
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid_args("read", "missing `path`"))?;

        let resolved = self
            .policy
            .resolve(path, AccessKind::Read)
            .map_err(|err| tool_error("read", err))?;

        // Prefer extension, then magic-byte sniff for extension-less images.
        if is_image_path(&resolved) || looks_like_image_file(&resolved).await {
            return read_image(path, &resolved).await;
        }

        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|err| tool_error("read", err.to_string()))?;

        let offset = call
            .arguments
            .get("offset")
            .and_then(|value| value.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = call.arguments.get("limit").and_then(|value| value.as_u64());

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.saturating_sub(1);
        // Cap explicit limit at DEFAULT_MAX_LINES so a huge limit cannot flood context.
        let max_window = limit
            .map(|n| (n as usize).min(crate::truncate::DEFAULT_MAX_LINES))
            .unwrap_or(crate::truncate::DEFAULT_MAX_LINES);
        let end = (start + max_window).min(lines.len());
        let slice = &lines[start..end];

        let numbered = slice
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{}|{}", start + index + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        // Cap by lines/bytes; Claude-style PARTIAL view tells model how to continue.
        let presented = crate::truncate::present_file_read(&numbered, lines.len(), offset);
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

        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "path": path,
                "lines": end.saturating_sub(start),
                "offset": offset,
                "fileLines": lines.len(),
                "truncated": presented.truncated || more_in_file,
            }),
        ))
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
    async fn reads_text_with_line_numbers() {
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
        assert!(text.contains("1|hello"), "{text}");
        assert!(text.contains("2|world"), "{text}");
        assert!(!out.has_images());
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
        let mut f = std::fs::File::create(dir.join(".keep")).unwrap();
        let _ = writeln!(f, "test");
        dir
    }
}
