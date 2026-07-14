use std::path::{Path, PathBuf};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

pub struct FindTool {
    cwd: PathBuf,
}

impl FindTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl Tool for FindTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "find".to_string(),
            description: "Find files by glob pattern.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob like **/*.rs" },
                    "path": { "type": "string" }
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
            .ok_or_else(|| invalid_args("find", "missing `pattern`"))?;
        let path = call
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or(".");

        let root = resolve_path(&self.cwd, path);
        let glob_pattern = root.join(pattern);
        let glob_str = glob_pattern.to_string_lossy().to_string();

        let mut matches = Vec::new();
        for entry in glob::glob(&glob_str).into_iter().flatten().flatten() {
            matches.push(entry.display().to_string());
        }
        matches.sort();

        Ok(ToolOutput::text(matches.join("\n")))
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