//! Built-in **local function** web search (optional `network` feature).
//!
//! Feature `server_search` only controls **request inject** of hosted
//! `{ type: web_search }`. When inject is active, this function is **not**
//! registered (server wins — pi-xai `mergeXaiTools`). When inject is off,
//! this tool stays available.
//!
//! Response handling is independent: `web_search_call` / citations from the
//! upstream (or a proxy that injects tools) are always parsed.
//!
//! Local backend priority (when this function is registered):
//! 1. Optional [`BackendWebSearch`] hop (tests / advanced hosts)
//! 2. Brave Search API when `BRAVE_API_KEY` is set
//! 3. DuckDuckGo HTML fallback (no key; best-effort)

use std::sync::Arc;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

const MAX_RESULTS: usize = 10;
const USER_AGENT: &str = "one-agent/0.1 (+https://github.com/local/one; web_search)";

/// Result from a provider-native (server-side) search hop.
#[derive(Debug, Clone)]
pub struct BackendSearchResult {
    /// Full text returned to the model as the tool output body.
    pub text: String,
}

/// Separate search hop implemented by the host (CLI wires Responses/native tools).
///
/// The main agent always sees a normal function tool `web_search`; this trait is
/// only used inside the tool implementation.
#[async_trait]
pub trait BackendWebSearch: Send + Sync {
    async fn search(&self, query: &str, count: usize) -> Result<BackendSearchResult>;
}

pub struct WebSearchTool {
    client: reqwest::Client,
    backend: Option<Arc<dyn BackendWebSearch>>,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self::with_backend(None)
    }

    pub fn with_backend(backend: Option<Arc<dyn BackendWebSearch>>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client, backend }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        let backend_hint = if self.backend.is_some() {
            " Uses a host search backend when available, otherwise Brave/DuckDuckGo."
        } else {
            " Uses Brave Search when BRAVE_API_KEY is set, otherwise DuckDuckGo HTML. \
             (When the provider supports hosted web_search, that path is preferred and \
             this local tool is not registered.)"
        };
        ToolDefinition {
            name: "web_search".to_string(),
            description: format!(
                "Search the public web for current information, docs, or facts. \
                 Prefer this over guessing.{backend_hint}"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of results (default 5, max 10)"
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
            .ok_or_else(|| invalid_args("web_search", "missing `query`"))?
            .trim();
        if query.is_empty() {
            return Err(invalid_args("web_search", "empty `query`"));
        }

        let count = call
            .arguments
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, MAX_RESULTS as u64) as usize;

        // 1) Provider-native hop (optional). Any failure falls through to local search.
        if let Some(backend) = &self.backend {
            if let Ok(result) = backend.search(query, count).await {
                if !result.text.trim().is_empty() {
                    return Ok(ToolOutput::text(result.text));
                }
            }
        }

        // 2) Brave / 3) DDG
        let text = if let Ok(key) = std::env::var("BRAVE_API_KEY") {
            if !key.trim().is_empty() {
                brave_search(&self.client, key.trim(), query, count).await?
            } else {
                ddg_search(&self.client, query, count).await?
            }
        } else {
            ddg_search(&self.client, query, count).await?
        };

        Ok(ToolOutput::text(text))
    }
}

async fn brave_search(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<String> {
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencoding(query),
        count
    );
    let response = client
        .get(&url)
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| tool_error("web_search", format!("brave request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(tool_error(
            "web_search",
            format!("brave HTTP {status}: {}", truncate(&body, 400)),
        ));
    }

    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|e| tool_error("web_search", format!("brave json: {e}")))?;

    let results = value
        .pointer("/web/results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if results.is_empty() {
        return Ok(format!("No Brave results for: {query}"));
    }

    let mut out = format!("Web search (Brave) for: {query}\n");
    for (i, item) in results.iter().take(count).enumerate() {
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(no title)");
        let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let desc = item
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        out.push_str(&format!(
            "\n--- Result {} ---\nTitle: {title}\nLink: {url}\nSnippet: {desc}\n",
            i + 1
        ));
    }
    out.push_str(
        "\nTip: use web_fetch on a Link for full page text, or read skill docs if installed.\n",
    );
    Ok(out)
}

async fn ddg_search(client: &reqwest::Client, query: &str, count: usize) -> Result<String> {
    // DuckDuckGo HTML endpoint — no API key. Best-effort parse of result links.
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(query));
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| tool_error("web_search", format!("ddg request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        return Err(tool_error(
            "web_search",
            format!("ddg HTTP {status}; set BRAVE_API_KEY for better search"),
        ));
    }

    let html = response
        .text()
        .await
        .map_err(|e| tool_error("web_search", e.to_string()))?;

    let results = parse_ddg_html(&html, count);
    if results.is_empty() {
        return Ok(format!(
            "No DuckDuckGo results parsed for: {query}\n\
             Set BRAVE_API_KEY for Brave Search API (recommended), or install the brave-search skill."
        ));
    }

    let mut out = format!("Web search (DuckDuckGo) for: {query}\n");
    for (i, (title, link, snippet)) in results.iter().enumerate() {
        out.push_str(&format!(
            "\n--- Result {} ---\nTitle: {title}\nLink: {link}\nSnippet: {snippet}\n",
            i + 1
        ));
    }
    out.push_str("\nTip: use web_fetch on a Link for full page text.\n");
    Ok(out)
}

/// Very small HTML scrape for DDG result blocks.
fn parse_ddg_html(html: &str, count: usize) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    // Links look like: <a rel="nofollow" class="result__a" href="https://...">Title</a>
    let mut rest = html;
    while results.len() < count {
        let Some(idx) = rest.find("result__a") else {
            break;
        };
        rest = &rest[idx..];
        let Some(href_pos) = rest.find("href=\"") else {
            break;
        };
        let after_href = &rest[href_pos + 6..];
        let Some(end_href) = after_href.find('"') else {
            break;
        };
        let mut link = after_href[..end_href].to_string();
        // DDG sometimes wraps redirects
        if let Some(uddg) = extract_uddg(&link) {
            link = uddg;
        }
        let after_tag = after_href[end_href..]
            .find('>')
            .map(|i| &after_href[end_href + i + 1..]);
        let Some(title_src) = after_tag else {
            if let Some(ch) = rest.chars().next() {
                rest = &rest[ch.len_utf8()..];
            } else {
                break;
            }
            continue;
        };
        let Some(end_a) = title_src.find("</a>") else {
            if let Some(ch) = rest.chars().next() {
                rest = &rest[ch.len_utf8()..];
            } else {
                break;
            }
            continue;
        };
        let title = strip_tags(&title_src[..end_a]).trim().to_string();

        // Snippet: result__snippet nearby
        let snippet = rest
            .find("result__snippet")
            .and_then(|s| {
                let after = &rest[s..];
                let start = after.find('>')? + 1;
                let end = after[start..].find("</")?;
                if end > 0 {
                    Some(strip_tags(&after[start..start + end]).trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        if !title.is_empty() && link.starts_with("http") {
            results.push((title, link, snippet));
        }
        rest = &title_src[end_a + 4..];
    }
    results
}

fn extract_uddg(link: &str) -> Option<String> {
    // https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com
    let idx = link.find("uddg=")?;
    let enc = &link[idx + 5..];
    let enc = enc.split('&').next().unwrap_or(enc);
    urlencoding_decode(enc)
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    html_unescape(&out)
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urlencoding_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailingBackend;
    #[async_trait]
    impl BackendWebSearch for FailingBackend {
        async fn search(&self, _query: &str, _count: usize) -> Result<BackendSearchResult> {
            Err(tool_error("web_search", "backend down"))
        }
    }

    struct OkBackend {
        hits: AtomicUsize,
    }
    #[async_trait]
    impl BackendWebSearch for OkBackend {
        async fn search(&self, query: &str, _count: usize) -> Result<BackendSearchResult> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(BackendSearchResult {
                text: format!("native:{query}"),
            })
        }
    }

    #[tokio::test]
    async fn prefers_backend_when_present() {
        let backend = Arc::new(OkBackend {
            hits: AtomicUsize::new(0),
        });
        let tool = WebSearchTool::with_backend(Some(backend.clone()));
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "web_search".into(),
                arguments: json!({"query": "rust async"}),
            })
            .await
            .unwrap();
        assert_eq!(out.as_text(), "native:rust async");
        assert_eq!(backend.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn backend_failure_does_not_block_local_path_args() {
        // Ensure failing backend still validates args the same way.
        let tool = WebSearchTool::with_backend(Some(Arc::new(FailingBackend)));
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "web_search".into(),
                arguments: json!({}),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("query"));
    }
}
