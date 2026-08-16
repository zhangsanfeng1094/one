//! MCP tool catalog: lightweight server summaries + BM25 search (Grok-style deferred discovery).
//!
//! MCP tool schemas stay out of the model tools list. The model uses `search_tool` to
//! retrieve schemas on demand, then `use_tool` to dispatch.

use serde_json::Value;

/// Max chars for server/tool descriptions shown to the model.
pub const MAX_DESCRIPTION_LENGTH: usize = 2048;

const TRUNCATION_SUFFIX: &str = "… [truncated]";

/// How MCP tools are exposed to the LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolExposure {
    /// Full schemas registered as model-visible tools (legacy).
    Direct,
    /// Only `search_tool` + `use_tool`; schemas via search (default, Grok-style).
    #[default]
    Deferred,
}

impl ToolExposure {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "direct" | "full" | "all" => Some(Self::Direct),
            "deferred" | "lazy" | "search" => Some(Self::Deferred),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Deferred => "deferred",
        }
    }
}

/// One tool entry in the search index.
#[derive(Debug, Clone)]
pub struct ToolMeta {
    /// Canonical name, e.g. `linear__save_issue`.
    pub qualified_name: String,
    pub server_name: String,
    /// Unqualified remote tool name.
    pub tool_name: String,
    pub description: String,
    pub parameters: Vec<String>,
    pub input_schema: Value,
}

/// Connected server summary for system-prompt announcements.
#[derive(Debug, Clone)]
pub struct ServerSummary {
    pub name: String,
    pub description: Option<String>,
    pub tool_count: usize,
    pub tool_names: Vec<String>,
}

/// One search hit (includes full input schema for `use_tool`).
#[derive(Debug, Clone)]
pub struct ToolSearchResult {
    pub tool_name: String,
    pub server_name: String,
    pub description: String,
    pub score: f32,
    pub input_schema: Value,
}

/// Snapshot of a catalog search.
#[derive(Debug, Clone)]
pub struct SearchSnapshot {
    pub results: Vec<ToolSearchResult>,
    /// Total indexed tools (including those not returned).
    pub total_tools: usize,
    /// `false` while background MCP handshakes are still running.
    pub is_ready: bool,
}

/// Collapse whitespace / newlines for announcement lines.
pub fn sanitize_description(s: &str) -> String {
    s.split(['\n', '\r'])
        .flat_map(|line| line.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truncate to [`MAX_DESCRIPTION_LENGTH`] chars (char-boundary safe).
pub fn truncate_description(s: &str) -> String {
    if s.chars().count() <= MAX_DESCRIPTION_LENGTH {
        return s.to_owned();
    }
    let budget = MAX_DESCRIPTION_LENGTH.saturating_sub(TRUNCATION_SUFFIX.chars().count());
    let truncated: String = s.chars().take(budget).collect();
    format!("{truncated}{TRUNCATION_SUFFIX}")
}

/// Parameter names from a JSON Schema object (`properties` keys).
pub fn param_names_from_schema(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Build BM25 document text for a tool (Grok-style field set + identifier splits).
fn tool_document(meta: &ToolMeta) -> String {
    let params = meta.parameters.join(" ");
    let doc = format!(
        "{} {} {} {}",
        meta.server_name, meta.tool_name, meta.description, params
    );
    let extra: String = [meta.server_name.as_str(), meta.tool_name.as_str()]
        .iter()
        .flat_map(|s| split_identifier(s))
        .chain(meta.parameters.iter().flat_map(|p| split_identifier(p)))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{doc} {extra}")
}

/// Split compound identifiers (`__`, `_`, `-`, camelCase).
fn split_identifier(s: &str) -> Vec<&str> {
    let mut words: Vec<&str> = Vec::new();
    for part in s
        .split("__")
        .flat_map(|p| p.split('_'))
        .flat_map(|p| p.split('-'))
    {
        if part.is_empty() {
            continue;
        }
        let bytes = part.as_bytes();
        let mut start = 0;
        for i in 1..bytes.len() {
            if bytes[i - 1].is_ascii_lowercase() && bytes[i].is_ascii_uppercase() {
                words.push(&part[start..i]);
                start = i;
            }
        }
        words.push(&part[start..]);
    }
    words
}

fn normalize_query(query: &str) -> String {
    let needs_split = query.contains("__")
        || query.contains('_')
        || query.contains('-')
        || query
            .as_bytes()
            .windows(2)
            .any(|w| w[0].is_ascii_lowercase() && w[1].is_ascii_uppercase());
    if !needs_split {
        return query.to_owned();
    }
    let extra: Vec<&str> = query
        .split_whitespace()
        .flat_map(split_identifier)
        .collect();
    if extra.is_empty() {
        return query.to_owned();
    }
    format!("{query} {}", extra.join(" "))
}

/// Search tools with exact-name fast path, then BM25.
pub fn search_tools(
    tools: &[ToolMeta],
    query: &str,
    limit: usize,
    is_ready: bool,
) -> SearchSnapshot {
    let total_tools = tools.len();
    if tools.is_empty() {
        return SearchSnapshot {
            results: Vec::new(),
            total_tools,
            is_ready,
        };
    }

    let limit = limit.max(1);
    let query_lower = query.trim().to_lowercase();
    if !query_lower.is_empty() {
        if let Some(exact) = tools.iter().find(|t| {
            t.qualified_name.to_lowercase() == query_lower
                || t.tool_name.to_lowercase() == query_lower
        }) {
            return SearchSnapshot {
                results: vec![ToolSearchResult {
                    tool_name: exact.qualified_name.clone(),
                    server_name: exact.server_name.clone(),
                    description: exact.description.clone(),
                    score: 1.0,
                    input_schema: exact.input_schema.clone(),
                }],
                total_tools,
                is_ready,
            };
        }
    }

    let documents: Vec<String> = tools.iter().map(tool_document).collect();
    let search_engine =
        bm25::SearchEngineBuilder::<u32>::with_corpus(bm25::Language::English, documents).build();
    let normalized = normalize_query(query);
    let bm25_results = search_engine.search(&normalized, limit);

    let results = bm25_results
        .into_iter()
        .filter_map(|sr| {
            let meta = tools.get(sr.document.id as usize)?;
            Some(ToolSearchResult {
                tool_name: meta.qualified_name.clone(),
                server_name: meta.server_name.clone(),
                description: meta.description.clone(),
                score: sr.score,
                input_schema: meta.input_schema.clone(),
            })
        })
        .collect();

    SearchSnapshot {
        results,
        total_tools,
        is_ready,
    }
}

/// Format one server line for the system-prompt announcement.
pub fn format_server_line(server: &ServerSummary) -> String {
    let tool_word = if server.tool_count == 1 {
        "tool"
    } else {
        "tools"
    };
    match server
        .description
        .as_deref()
        .map(sanitize_description)
        .map(|s| truncate_description(&s))
        .filter(|s| !s.is_empty())
    {
        Some(d) => format!(
            "- {} ({} {}): {}\n",
            server.name, server.tool_count, tool_word, d
        ),
        None => format!("- {} ({} {})\n", server.name, server.tool_count, tool_word),
    }
}

/// Protocol hint appended to MCP announcements (fixed small text).
pub fn mcp_usage_hint() -> &'static str {
    "\nTo use MCP tools, you MUST call `search_tool` first to retrieve the tool's input schema \
     before calling `use_tool`. NEVER guess parameter names — always use the exact schema \
     returned by `search_tool`.\n\
     MCP tools are not listed as native functions; call them only via `use_tool` with the \
     qualified `server__tool` name."
}

/// Build the deferred-mode system-prompt section (server list + usage hint).
///
/// Returns `None` when there is nothing useful to announce.
pub fn build_prompt_announcement(
    servers: &[ServerSummary],
    is_loading: bool,
    has_configured: bool,
) -> Option<String> {
    if !has_configured {
        return None;
    }

    let mut text = String::new();
    if !servers.is_empty() {
        text.push_str("## MCP integrations\n\n");
        text.push_str("Connected MCP servers:\n");
        for s in servers {
            text.push_str(&format_server_line(s));
        }
        text.push_str(mcp_usage_hint());
    } else if is_loading {
        text.push_str("## MCP integrations\n\n");
        text.push_str(
            "MCP servers are still connecting. Tools will become available shortly.\n\
             Use `search_tool` once servers are ready; if a call reports no tools, retry after other work.\n",
        );
    } else {
        return None;
    }
    Some(text)
}

/// Whether a tool name is the deferred MCP meta-tool surface.
pub fn is_mcp_meta_tool(name: &str) -> bool {
    name == "search_tool" || name == "use_tool" || name == "mcp_status"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(server: &str, tool: &str, desc: &str) -> ToolMeta {
        ToolMeta {
            qualified_name: format!("{server}__{tool}"),
            server_name: server.into(),
            tool_name: tool.into(),
            description: desc.into(),
            parameters: vec!["title".into()],
            input_schema: json!({
                "type": "object",
                "properties": { "title": { "type": "string" } },
                "required": ["title"]
            }),
        }
    }

    #[test]
    fn exact_qualified_match() {
        let tools = vec![
            meta("linear", "save_issue", "Create or update a Linear issue"),
            meta("slack", "post_message", "Post to Slack"),
        ];
        let snap = search_tools(&tools, "linear__save_issue", 5, true);
        assert_eq!(snap.results.len(), 1);
        assert_eq!(snap.results[0].tool_name, "linear__save_issue");
        assert!((snap.results[0].score - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn bm25_finds_by_description() {
        let tools = vec![
            meta("linear", "save_issue", "Create or update a Linear issue"),
            meta("slack", "post_message", "Post a message to a Slack channel"),
        ];
        let snap = search_tools(&tools, "create linear issue", 3, true);
        assert!(!snap.results.is_empty());
        assert_eq!(snap.results[0].tool_name, "linear__save_issue");
        assert!(snap.results[0].input_schema.get("properties").is_some());
    }

    #[test]
    fn announcement_includes_hint() {
        let servers = vec![ServerSummary {
            name: "linear".into(),
            description: Some("Issue tracker".into()),
            tool_count: 2,
            tool_names: vec!["save_issue".into(), "list".into()],
        }];
        let text = build_prompt_announcement(&servers, false, true).unwrap();
        assert!(text.contains("linear"));
        assert!(text.contains("search_tool"));
        assert!(text.contains("use_tool"));
    }

    #[test]
    fn loading_announcement_without_servers() {
        let text = build_prompt_announcement(&[], true, true).unwrap();
        assert!(text.contains("still connecting"));
    }
}
