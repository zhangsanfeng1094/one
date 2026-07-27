//! `memory_search` — L2 index (+ L4 session archive) lookup without full-body dump (M5).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, Tool, ToolCall, ToolDefinition, ToolOutput};
use one_resources::{search_memory_index, MemorySearchSource};
use serde_json::json;

const DEFAULT_MAX: usize = 12;

/// Search cross-session memory indexes; returns map entries only (progressive disclosure).
pub struct MemorySearchTool {
    agent_dir: PathBuf,
    cwd: PathBuf,
}

impl MemorySearchTool {
    pub fn new(agent_dir: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            agent_dir: agent_dir.into(),
            cwd: cwd.into(),
        }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_search".into(),
            description: "Search the cross-session memory **index** (L2) and optional session \
archives (L4) by keywords. Returns matching ids, tags, one-line descriptions, and body paths — \
**not** full bodies. Use `read` on a body path when relevant. Prefer this over grepping the whole \
memory tree. External memory MCP servers (e.g. Mem0) remain optional L4 backends when configured."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords to match against id, tags, type, description"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": format!("Max hits (default {DEFAULT_MAX})")
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let query = call
            .arguments
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("memory_search", "missing `query`"))?;

        let max = call
            .arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX);

        let hits = search_memory_index(&self.agent_dir, &self.cwd, query, max);
        if hits.is_empty() {
            let text = format!(
                "No memory index hits for `{query}`.\n\
                 (L2 catalogs under memory/_global and memory/projects; L4 under memory/sessions.)\n\
                 Empty result is fine — continue without memory or write a new note if warranted."
            );
            return Ok(ToolOutput::text_with_details(
                text,
                json!({ "query": query, "count": 0, "hits": [] }),
            ));
        }

        let mut text = format!("memory_search · {} hit(s) for `{query}`\n\n", hits.len());
        let mut hit_json = Vec::new();
        for h in &hits {
            let loc = h
                .entry
                .body_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| format!("{}.md (next to MEMORY.md)", h.entry.id));
            text.push_str(&format!(
                "- [{id}] type={ty} scope={scope} tags={tags} source={src} score={score}\n  {desc}\n  body: `{loc}`\n",
                id = h.entry.id,
                ty = h.entry.type_name,
                scope = h.entry.scope,
                tags = h.entry.tags,
                src = h.source.as_str(),
                score = h.score,
                desc = h.entry.description,
                loc = loc,
            ));
            hit_json.push(json!({
                "id": h.entry.id,
                "type": h.entry.type_name,
                "scope": h.entry.scope,
                "tags": h.entry.tags,
                "description": h.entry.description,
                "location": loc,
                "source": h.source.as_str(),
                "score": h.score,
            }));
        }
        text.push_str(
            "\nUse `read` on a body path when needed. Memory is point-in-time — verify against code.",
        );

        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "query": query,
                "count": hits.len(),
                "hits": hit_json,
            }),
        ))
    }
}

/// Shared handle when registering from AppRuntime.
pub type SharedMemorySearch = Arc<MemorySearchTool>;

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;
    use one_resources::{format_index_entry_line, project_memory_dir};

    #[tokio::test]
    async fn finds_project_index_entry() {
        let tmp = std::env::temp_dir().join(format!(
            "one-msearch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = tmp.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let proj = project_memory_dir(&tmp, &cwd);
        std::fs::create_dir_all(&proj).unwrap();
        let line = format_index_entry_line(
            "oauth_device",
            "project",
            "project",
            "auth,oauth",
            "Staging uses device code",
        );
        std::fs::write(proj.join("MEMORY.md"), format!("{line}\n")).unwrap();
        std::fs::write(proj.join("oauth_device.md"), "details\n").unwrap();

        let tool = MemorySearchTool::new(&tmp, &cwd);
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "memory_search".into(),
                arguments: json!({ "query": "oauth device" }),
            })
            .await
            .unwrap();
        assert!(out.as_text().contains("oauth_device"), "{}", out.as_text());
        assert!(out.as_text().contains("device code"), "{}", out.as_text());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn empty_query_rejected() {
        let tool = MemorySearchTool::new("/tmp", "/tmp");
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "memory_search".into(),
                arguments: json!({ "query": "  " }),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("query"), "{err}");
    }

    #[allow(dead_code)]
    fn _source_str() {
        let _ = MemorySearchSource::Index.as_str();
    }
}
