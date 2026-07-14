use std::path::{Path, PathBuf};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

pub struct WriteTool {
    cwd: PathBuf,
}

impl WriteTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_string(),
            description: "Create or overwrite a file.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let path = call
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid_args("write", "missing `path`"))?;
        let content = call
            .arguments
            .get("content")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid_args("write", "missing `content`"))?;

        let resolved = self.resolve(path);
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|err| tool_error("write", err.to_string()))?;
        }

        tokio::fs::write(&resolved, content)
            .await
            .map_err(|err| tool_error("write", err.to_string()))?;

        Ok(ToolOutput::text_with_details(
            format!("Wrote {} bytes to {path}", content.len()),
            json!({ "path": path, "bytes": content.len() }),
        ))
    }
}