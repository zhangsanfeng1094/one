//! Meta tools for deferred MCP exposure: `search_tool` + `use_tool`.

use std::sync::Arc;

use async_trait::async_trait;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use one_core::Result as CoreResult;
use serde_json::{json, Value};

use crate::catalog::{self, truncate_description, SearchSnapshot};
use crate::manager::McpCatalog;

/// Discover MCP tools and retrieve full input schemas (BM25 + exact match).
pub struct SearchTool {
    catalog: McpCatalog,
}

impl SearchTool {
    pub fn new(catalog: McpCatalog) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for SearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "search_tool".into(),
            description:
                "Search for MCP integration tools by keyword and retrieve their input schemas.\n\n\
                If status is \"partial\", some servers may still be connecting.\n\
                Include the server name and action for best results (e.g. \"linear create issue\")."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords to match against tool names, server names, and descriptions."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default 5, max 20).",
                        "minimum": 1,
                        "maximum": 20
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> CoreResult<ToolOutput> {
        let query = call
            .arguments
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("search_tool", "missing required string field `query`"))?;

        let limit = call
            .arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(5)
            .clamp(1, 20);

        let snap = self.catalog.search(query, limit);
        let body = format_search_output(&snap);
        Ok(ToolOutput::text(body))
    }
}

fn format_search_output(snap: &SearchSnapshot) -> String {
    // Group by server, preserving score order within groups; groups by best score.
    let mut groups: Vec<(String, f32, Vec<Value>)> = Vec::new();
    for r in &snap.results {
        let tool_json = json!({
            "tool_name": r.tool_name,
            "description": truncate_description(&r.description),
            "score": r.score,
            "input_schema": r.input_schema,
        });
        if let Some(group) = groups
            .iter_mut()
            .find(|(name, _, _)| name == &r.server_name)
        {
            group.2.push(tool_json);
        } else {
            groups.push((r.server_name.clone(), r.score, vec![tool_json]));
        }
    }
    groups.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let result_groups: Vec<Value> = groups
        .into_iter()
        .map(|(server, _, tools)| {
            json!({
                "server": server,
                "tools": tools,
            })
        })
        .collect();

    let status = if snap.is_ready { "ready" } else { "partial" };
    let total_hidden = snap.total_tools.saturating_sub(snap.results.len());
    let note = if !snap.is_ready {
        Some("Some MCP servers are still connecting. Results may be incomplete.".to_string())
    } else if snap.total_tools == 0 && result_groups.is_empty() {
        Some(
            "No MCP tools are available in this session. Connect MCP servers, or if this is a \
             subagent, ensure tools.mcp is enabled and the parent has connected servers."
                .to_string(),
        )
    } else {
        None
    };

    let response = json!({
        "results": result_groups,
        "total_hidden_tools": total_hidden,
        "total_tools": snap.total_tools,
        "status": status,
        "note": note,
    });
    serde_json::to_string_pretty(&response).unwrap_or_else(|_| "{}".into())
}

/// Report live MCP server state and discovered tool names.
///
/// This is intentionally separate from `search_tool`: search only reports
/// indexed tools, while this reports configured servers that are also
/// connecting, disabled, unavailable, or waiting for authentication.
pub struct McpStatusTool {
    catalog: McpCatalog,
}

impl McpStatusTool {
    pub fn new(catalog: McpCatalog) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for McpStatusTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "mcp_status".into(),
            description: "Report the current runtime status of all configured MCP servers, including ready, connecting, unavailable, disabled, and auth_required servers. Includes discovered tool names but not tool schemas.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, _call: &ToolCall) -> CoreResult<ToolOutput> {
        let snapshot = self.catalog.status_snapshot();
        let servers: Vec<Value> = snapshot
            .servers
            .iter()
            .map(|server| {
                json!({
                    "server": server.name,
                    "status": server.status,
                    "enabled": server.enabled,
                    "transport": server.transport,
                    "source": server.source,
                    "tool_count": server.tool_count,
                    "tools": server.tools,
                    "description": server.description,
                    "detail": server.detail,
                })
            })
            .collect();
        let response = json!({
            "status": snapshot.status,
            "catalog_ready": snapshot.status == "ready",
            "configured": snapshot.configured,
            "enabled": snapshot.enabled,
            "ready": snapshot.ready,
            "connecting": snapshot.connecting,
            "unavailable": snapshot.unavailable,
            "auth_required": snapshot.auth_required,
            "disabled": snapshot.disabled,
            "total_tools": snapshot.tool_count,
            "servers": servers,
        });
        Ok(ToolOutput::text(
            serde_json::to_string_pretty(&response).unwrap_or_else(|_| "{}".into()),
        ))
    }
}

/// Dispatch to a discovered MCP tool by qualified name.
pub struct UseTool {
    catalog: McpCatalog,
}

impl UseTool {
    pub fn new(catalog: McpCatalog) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for UseTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "use_tool".into(),
            description: "Call an MCP integration tool discovered via `search_tool`.\n\n\
                The `tool_name` must be the qualified `server__tool` name (e.g. `linear__save_issue`).\n\
                The `tool_input` must conform exactly to the input schema returned by `search_tool`.\n\
                Do not use this for built-in tools (read, write, bash, …) — call those directly."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "Qualified MCP tool name, e.g. linear__save_issue"
                    },
                    "tool_input": {
                        "type": "object",
                        "description": "Arguments object matching the tool's input_schema from search_tool",
                        "additionalProperties": true
                    }
                },
                "required": ["tool_name", "tool_input"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> CoreResult<ToolOutput> {
        let tool_name = call
            .arguments
            .get("tool_name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("use_tool", "missing required string field `tool_name`"))?;

        if catalog::is_mcp_meta_tool(tool_name) {
            return Err(invalid_args(
                "use_tool",
                format!("`{tool_name}` is a meta-tool; call it directly, not via use_tool"),
            ));
        }

        // Native / builtin names never contain `__` (MCP public names always do).
        if !tool_name.contains("__") {
            return Err(invalid_args(
                "use_tool",
                format!(
                    "`{tool_name}` is not a qualified MCP tool name (expected server__tool). \
                     Call built-in tools directly; use search_tool to discover MCP tools."
                ),
            ));
        }

        let tool_input = match call.arguments.get("tool_input") {
            Some(Value::Object(_)) => call.arguments.get("tool_input").cloned().unwrap(),
            Some(Value::String(s)) => {
                // Models sometimes stringify the object.
                match serde_json::from_str::<Value>(s) {
                    Ok(v @ Value::Object(_)) => v,
                    _ => {
                        return Err(invalid_args(
                            "use_tool",
                            "tool_input must be a JSON object (or a JSON object string)",
                        ));
                    }
                }
            }
            Some(Value::Null) | None => json!({}),
            Some(other) => {
                return Err(invalid_args(
                    "use_tool",
                    format!("tool_input must be a JSON object, got {other}"),
                ));
            }
        };

        let Some(tool) = self.catalog.find_tool(tool_name) else {
            return Err(tool_error(
                "use_tool",
                format!(
                    "MCP tool `{tool_name}` not found. Call search_tool first with a keyword query \
                     and use the exact tool_name from results. Catalog has {} tool(s).",
                    self.catalog.tool_count()
                ),
            ));
        };

        let inner = ToolCall {
            id: call.id.clone(),
            name: tool.definition().name,
            arguments: tool_input,
        };
        tool.execute(&inner).await
    }
}

/// Build the three meta tools for a catalog handle.
pub fn meta_tools(catalog: McpCatalog) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SearchTool::new(catalog.clone())) as Arc<dyn Tool>,
        Arc::new(UseTool::new(catalog.clone())) as Arc<dyn Tool>,
        Arc::new(McpStatusTool::new(catalog)) as Arc<dyn Tool>,
    ]
}
