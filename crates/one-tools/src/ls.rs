use std::path::{Path, PathBuf};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

pub struct LsTool {
    cwd: PathBuf,
}

impl LsTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl Tool for LsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ls".to_string(),
            description: "List files in a directory.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                }
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let path = call
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or(".");
        let resolved = resolve_path(&self.cwd, path);

        let mut entries = tokio::fs::read_dir(&resolved).await?;
        let mut lines = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let file_type = if entry.file_type().await?.is_dir() {
                "dir"
            } else {
                "file"
            };
            lines.push(format!(
                "{} [{}]",
                entry.file_name().to_string_lossy(),
                file_type
            ));
        }
        lines.sort();
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}