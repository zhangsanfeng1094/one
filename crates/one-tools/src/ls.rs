use std::path::PathBuf;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::path_policy::{AccessKind, PathPolicy};
use crate::tool_args::path_arg_or;

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
            description: "List files in a directory. Default is the workspace root; paths \
                 outside need interactive approval or --add-dir."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path"
                    }
                }
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let path = path_arg_or(&call.arguments, ".").map_err(|msg| invalid_args("ls", msg))?;
        let resolved = self
            .policy
            .resolve(path, AccessKind::Read)
            .map_err(|err| tool_error("ls", err))?;

        let mut entries = tokio::fs::read_dir(&resolved)
            .await
            .map_err(|err| tool_error("ls", err.to_string()))?;
        let mut lines = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|err| tool_error("ls", err.to_string()))?
        {
            let file_type = if entry
                .file_type()
                .await
                .map_err(|err| tool_error("ls", err.to_string()))?
                .is_dir()
            {
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
