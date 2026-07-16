use std::path::PathBuf;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::path_policy::{AccessKind, PathPolicy};

pub struct FindTool {
    policy: PathPolicy,
}

impl FindTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_policy(PathPolicy::workspace(cwd))
    }

    pub fn with_policy(policy: PathPolicy) -> Self {
        Self { policy }
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

        let root = self
            .policy
            .resolve(path, AccessKind::Read)
            .map_err(|err| tool_error("find", err))?;
        let glob_pattern = root.join(pattern);
        let glob_str = glob_pattern.to_string_lossy().to_string();

        let mut matches = Vec::new();
        for entry in glob::glob(&glob_str).into_iter().flatten().flatten() {
            // Drop matches that escape the allowed root (symlink / .. in glob).
            if self.policy.check(&entry, AccessKind::Read).is_ok() {
                matches.push(entry.display().to_string());
            }
        }
        matches.sort();

        let joined = matches.join("\n");
        Ok(ToolOutput::text(crate::truncate::apply_head_default(&joined)))
    }
}
