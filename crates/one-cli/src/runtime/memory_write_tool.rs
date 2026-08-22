//! `memory_write` — atomic L3 body + L2 MEMORY.md index upsert (M6).
//!
//! Gated by feature package `memory` (see `runtime/features.rs`). Requires
//! settings `memory.write` and is not registered in read-only mode.

use std::path::PathBuf;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use one_resources::{upsert_memory_entry, MemoryUpsertInput};
use serde_json::json;

/// Persist a cross-session memory note (body + index line).
pub struct MemoryWriteTool {
    agent_dir: PathBuf,
    cwd: PathBuf,
}

impl MemoryWriteTool {
    pub fn new(agent_dir: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            agent_dir: agent_dir.into(),
            cwd: cwd.into(),
        }
    }
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_write".into(),
            description: "Write or update a **cross-session memory** entry atomically: body file \
+ matching MEMORY.md index line (L2 catalog). Prefer this over raw `write` to memory dirs. \
Default **NO-OP** — only call when a future agent would clearly benefit (stable prefs, project \
facts, hard-won lessons). Skip trivial / one-off corrections and facts already in AGENTS.md. \
(Note: Workflow rules and tool preferences are managed by Intent Graph via `/learn`). \
Search first with `memory_search` and **update** an existing id when possible. \
L2 in the current session is frozen — new index lines appear after `/reload` or a new session."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Stable id / filename stem (alphanumeric, `_`, `-`; max 64). Not `MEMORY`."
                    },
                    "scope": {
                        "type": "string",
                        "description": "global (user-wide under memory/_global) or project (default, under memory/projects/…)"
                    },
                    "type": {
                        "type": "string",
                        "description": "feedback | user | project | reference | tool_intent (default project)"
                    },
                    "tags": {
                        "type": "string",
                        "description": "Comma-separated tags for relevance (e.g. writing,oauth)"
                    },
                    "description": {
                        "type": "string",
                        "description": "One-line L2 catalog description (routing only; keep short)"
                    },
                    "body": {
                        "type": "string",
                        "description": "Markdown body. Frontmatter optional — tool rebuilds name/type/scope/tags/updated."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional human title in frontmatter (defaults to description)"
                    },
                    "triggers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional phrases that strongly activate a tool-intent rule"
                    },
                    "negative_triggers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional phrases that veto the rule"
                    },
                    "priority": {
                        "type": "integer",
                        "description": "Optional rule priority; higher values win ties"
                    }
                },
                "required": ["id", "description", "body"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let id = call
            .arguments
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("memory_write", "missing `id`"))?
            .to_string();

        let description = call
            .arguments
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("memory_write", "missing `description`"))?
            .to_string();

        let body = call
            .arguments
            .get("body")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_args("memory_write", "missing `body`"))?
            .to_string();

        let scope = call
            .arguments
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("project")
            .to_string();
        let type_name = call
            .arguments
            .get("type")
            .or_else(|| call.arguments.get("type_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("project")
            .to_string();
        let tags = call
            .arguments
            .get("tags")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let name = call
            .arguments
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let triggers = call
            .arguments
            .get("triggers")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let negative_triggers = call
            .arguments
            .get("negative_triggers")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let priority = call
            .arguments
            .get("priority")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;

        let input = MemoryUpsertInput {
            id,
            scope,
            type_name,
            tags,
            description,
            body,
            name,
            triggers,
            negative_triggers,
            priority,
        };

        // Disk I/O is small; keep off async runtime blocking concerns with spawn_blocking.
        let agent_dir = self.agent_dir.clone();
        let cwd = self.cwd.clone();
        let result =
            tokio::task::spawn_blocking(move || upsert_memory_entry(&agent_dir, &cwd, &input))
                .await
                .map_err(|e| tool_error("memory_write", format!("join: {e}")))?
                .map_err(|e| tool_error("memory_write", e))?;

        let action = if result.created {
            "created"
        } else if result.index_updated {
            "updated"
        } else {
            "wrote"
        };
        let text = format!(
            "memory_write · {action} `{id}` scope={scope} type={ty}\n\
             body: `{body}`\n\
             index: `{index}`\n\
             L2 catalog is session-frozen — run `/reload` or start a new session to see this id in the system map.\
             Use `memory_search` / `read` on the body path anytime.",
            id = result.id,
            scope = result.scope,
            ty = result.type_name,
            body = result.body_path.display(),
            index = result.index_path.display(),
        );

        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "id": result.id,
                "scope": result.scope,
                "type": result.type_name,
                "tags": result.tags,
                "description": result.description,
                "body_path": result.body_path.display().to_string(),
                "index_path": result.index_path.display().to_string(),
                "created": result.created,
                "index_updated": result.index_updated,
            }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;
    use one_resources::{project_memory_dir, search_memory_index};

    #[tokio::test]
    async fn writes_project_memory_and_is_searchable() {
        let tmp = std::env::temp_dir().join(format!(
            "one-mwrite-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = tmp.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        let tool = MemoryWriteTool::new(&tmp, &cwd);
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "memory_write".into(),
                arguments: json!({
                    "id": "oauth_device",
                    "scope": "project",
                    "type": "project",
                    "tags": "auth,oauth",
                    "description": "Staging uses device code",
                    "body": "Do not use client_secret flow on staging."
                }),
            })
            .await
            .unwrap();
        let text = out.as_text();
        assert!(text.contains("oauth_device"), "{text}");
        assert!(text.contains("created") || text.contains("wrote"), "{text}");

        let proj = project_memory_dir(&tmp, &cwd);
        assert!(proj.join("oauth_device.md").exists());
        assert!(proj.join("MEMORY.md").exists());
        let hits = search_memory_index(&tmp, &cwd, "oauth device", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.id, "oauth_device");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn rejects_missing_fields() {
        let tool = MemoryWriteTool::new("/tmp", "/tmp");
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "memory_write".into(),
                arguments: json!({ "id": "x" }),
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("description") || err.to_string().contains("body"),
            "{err}"
        );
    }
}
