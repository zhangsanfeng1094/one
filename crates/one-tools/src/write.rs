use std::path::PathBuf;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use crate::path_policy::{AccessKind, PathPolicy};

pub struct WriteTool {
    policy: PathPolicy,
}

impl WriteTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_policy(PathPolicy::workspace(cwd))
    }

    pub fn with_policy(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        let scope = if self.policy.is_full_access() {
            "any path".to_string()
        } else {
            format!(
                "paths under workspace `{}` (and --add-dir roots)",
                self.policy.cwd().display()
            )
        };
        ToolDefinition {
            name: "write".to_string(),
            description: format!(
                "Create a new file or intentionally overwrite an entire file with `content`. \
                 Prefer `edit` for small/localized changes — do not rewrite a whole file when \
                 a unique string replace would suffice. Allowed: {scope}."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to create or overwrite" },
                    "content": { "type": "string", "description": "Full new file contents" }
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

        let resolved = self
            .policy
            .resolve(path, AccessKind::Write)
            .map_err(|err| tool_error("write", err))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let dir = std::env::temp_dir().join(format!(
            "one-write-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = WriteTool::new(dir.clone());
        let outside = format!("/etc/one-write-deny-{}", std::process::id());
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "write".into(),
                arguments: json!({ "path": outside, "content": "nope" }),
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("outside workspace"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn writes_inside_workspace() {
        let dir = std::env::temp_dir().join(format!(
            "one-write-ok-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = WriteTool::new(dir.clone());
        tool.execute(&ToolCall {
            id: "1".into(),
            name: "write".into(),
            arguments: json!({ "path": "hello.txt", "content": "hi" }),
        })
        .await
        .unwrap();
        let content = std::fs::read_to_string(dir.join("hello.txt")).unwrap();
        assert_eq!(content, "hi");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
