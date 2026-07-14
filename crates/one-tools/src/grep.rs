use std::path::{Path, PathBuf};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;
use tokio::process::Command;

pub struct GrepTool {
    cwd: PathBuf,
}

impl GrepTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
            description: "Search file contents with ripgrep-style output.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "description": "File or directory" },
                    "ignore_case": { "type": "boolean" }
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
        let path = call
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or(".");
        let ignore_case = call
            .arguments
            .get("ignore_case")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);

        let resolved = resolve_path(&self.cwd, path);
        let mut cmd = Command::new("rg");
        cmd.arg("--line-number").arg("--color=never").arg(pattern);
        if ignore_case {
            cmd.arg("-i");
        }
        cmd.arg(&resolved).current_dir(&self.cwd);

        let output = cmd
            .output()
            .await
            .map_err(|err| tool_error("grep", err.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let text = if stdout.is_empty() {
            if stderr.is_empty() {
                "no matches".to_string()
            } else {
                stderr.to_string()
            }
        } else {
            stdout.to_string()
        };

        Ok(ToolOutput::text(text))
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