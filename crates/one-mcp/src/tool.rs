//! Wrap an MCP remote tool as `one_core::Tool`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use one_core::tool::{tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use one_core::Result as CoreResult;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::ServerSink;
use serde_json::{json, Value};

use crate::naming::public_tool_name;

pub struct McpTool {
    pub server: String,
    pub remote_name: String,
    pub public_name: String,
    pub description: String,
    pub parameters: Value,
    pub peer: ServerSink,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl McpTool {
    pub fn new(
        server: impl Into<String>,
        remote: &rmcp::model::Tool,
        peer: ServerSink,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Self {
        let server = server.into();
        let remote_name = remote.name.to_string();
        let public_name = public_tool_name(&server, &remote_name);
        let description = remote
            .description
            .as_ref()
            .map(|d| d.to_string())
            .unwrap_or_else(|| format!("MCP tool `{remote_name}` from server `{server}`"));
        // Prefix description so the model knows provenance.
        let description = format!("[MCP:{server}] {description}");
        let parameters = schema_to_value(remote.input_schema.as_ref());
        Self {
            server,
            remote_name,
            public_name,
            description,
            parameters,
            peer,
            timeout,
            max_output_bytes,
        }
    }
}

fn schema_to_value(schema: &serde_json::Map<String, Value>) -> Value {
    if schema.is_empty() {
        json!({
            "type": "object",
            "properties": {}
        })
    } else {
        Value::Object(schema.clone())
    }
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.public_name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(&self, call: &ToolCall) -> CoreResult<ToolOutput> {
        let args_map = match &call.arguments {
            Value::Object(m) => Some(m.clone()),
            Value::Null => None,
            other => {
                // Coerce non-object into a single-field object if possible
                return Err(tool_error(
                    &self.public_name,
                    format!("MCP tool arguments must be a JSON object, got {other}"),
                ));
            }
        };

        let mut params = CallToolRequestParams::new(self.remote_name.clone());
        if let Some(args) = args_map {
            params = params.with_arguments(args);
        }

        let peer = self.peer.clone();
        let timeout = self.timeout;
        let result = match tokio::time::timeout(timeout, peer.call_tool(params)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return Err(tool_error(
                    &self.public_name,
                    format!("MCP call failed: {e}"),
                ));
            }
            Err(_) => {
                return Err(tool_error(
                    &self.public_name,
                    format!(
                        "MCP call timed out after {}s",
                        timeout.as_secs()
                    ),
                ));
            }
        };

        let is_error = result.is_error.unwrap_or(false);
        let mut text = content_blocks_to_text(&result.content);

        if let Some(structured) = &result.structured_content {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str("structuredContent:\n");
            text.push_str(&serde_json::to_string_pretty(structured).unwrap_or_default());
        }

        if text.is_empty() {
            text = if is_error {
                "(MCP tool error with empty content)".into()
            } else {
                "(empty MCP tool result)".into()
            };
        }

        let truncated = truncate_bytes(&text, self.max_output_bytes);
        let details = json!({
            "mcp_server": self.server,
            "mcp_tool": self.remote_name,
            "is_error": is_error,
            "truncated": truncated.was_truncated,
            "original_bytes": truncated.original_bytes,
        });

        // Tool-level MCP errors still return content to the model (not a protocol error).
        Ok(ToolOutput::text_with_details(truncated.text, details))
    }
}

struct Truncated {
    text: String,
    was_truncated: bool,
    original_bytes: usize,
}

fn truncate_bytes(s: &str, max: usize) -> Truncated {
    let original_bytes = s.len();
    if max == 0 || s.len() <= max {
        return Truncated {
            text: s.to_string(),
            was_truncated: false,
            original_bytes,
        };
    }
    // Cut on char boundary
    let mut end = max.saturating_sub(80);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = s[..end].to_string();
    text.push_str(&format!(
        "\n\n…[truncated: {original_bytes} bytes → {max}; set ONE_MAX_MCP_OUTPUT_BYTES or mcp.json maxOutputBytes]"
    ));
    Truncated {
        text,
        was_truncated: true,
        original_bytes,
    }
}

fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::Image(img) => {
                parts.push(format!(
                    "[image mime={} bytes~{}]",
                    img.mime_type,
                    img.data.len()
                ));
            }
            ContentBlock::Audio(a) => {
                parts.push(format!(
                    "[audio mime={} bytes~{}]",
                    a.mime_type,
                    a.data.len()
                ));
            }
            ContentBlock::Resource(r) => {
                parts.push(format!(
                    "[embedded resource: {}]",
                    serde_json::to_string(&r.resource).unwrap_or_else(|_| "?".into())
                ));
            }
            ContentBlock::ResourceLink(link) => {
                parts.push(format!(
                    "[resource_link name={} uri={}]",
                    link.name, link.uri
                ));
            }
            // Forward-compat: serialize unknown variants if added
            other => {
                parts.push(format!(
                    "[content: {}]",
                    serde_json::to_string(other).unwrap_or_else(|_| "?".into())
                ));
            }
        }
    }
    parts.join("\n")
}

/// Build tool list from a peer's listed tools.
pub fn tools_from_list(
    server: &str,
    listed: Vec<rmcp::model::Tool>,
    allowlist: Option<&[String]>,
    peer: ServerSink,
    timeout: Duration,
    max_output_bytes: usize,
) -> Vec<Arc<dyn Tool>> {
    listed
        .into_iter()
        .filter(|t| {
            allowlist
                .map(|allow| allow.iter().any(|a| a == t.name.as_ref()))
                .unwrap_or(true)
        })
        .map(|t| {
            Arc::new(McpTool::new(
                server,
                &t,
                peer.clone(),
                timeout,
                max_output_bytes,
            )) as Arc<dyn Tool>
        })
        .collect()
}
