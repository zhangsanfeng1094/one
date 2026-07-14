use std::path::{Path, PathBuf};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

pub struct ReadTool {
    cwd: PathBuf,
}

impl ReadTool {
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
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".to_string(),
            description: "Read a file from the filesystem.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" },
                    "offset": { "type": "integer", "description": "1-based line offset" },
                    "limit": { "type": "integer", "description": "Maximum lines to read" }
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

        let content = tokio::fs::read_to_string(self.resolve(path))
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
        let end = limit.map(|limit| start + limit as usize).unwrap_or(lines.len());
        let slice = &lines[start..end.min(lines.len())];

        let numbered = slice
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{}|{}", start + index + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolOutput::text_with_details(
            numbered,
            json!({ "path": path, "lines": slice.len() }),
        ))
    }
}