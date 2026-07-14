//! Fetch a URL and return readable text (optional `network` feature).

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

const MAX_BYTES: usize = 200_000;
const USER_AGENT: &str = "one-agent/0.1 (+https://github.com/local/one; web_fetch)";

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Fetch a public HTTP(S) URL and return text content \
                 (HTML is roughly stripped to readable text). Use after web_search \
                 to read a specific page."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "https:// or http:// URL"
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Max characters to return (default 20000)"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let url = call
            .arguments
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_args("web_fetch", "missing `url`"))?
            .trim();

        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(invalid_args(
                "web_fetch",
                "url must start with http:// or https://",
            ));
        }

        let max_chars = call
            .arguments
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(20_000)
            .clamp(500, 100_000) as usize;

        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| tool_error("web_fetch", format!("request failed: {e}")))?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let bytes = response
            .bytes()
            .await
            .map_err(|e| tool_error("web_fetch", e.to_string()))?;

        let slice = if bytes.len() > MAX_BYTES {
            &bytes[..MAX_BYTES]
        } else {
            &bytes[..]
        };
        let raw = String::from_utf8_lossy(slice);

        let body = if content_type.contains("html") || raw.trim_start().starts_with('<') {
            html_to_text(&raw)
        } else {
            raw.to_string()
        };

        let body = if body.chars().count() > max_chars {
            format!(
                "{}…\n\n[truncated to {max_chars} chars]",
                body.chars().take(max_chars).collect::<String>()
            )
        } else {
            body
        };

        Ok(ToolOutput::text(format!(
            "URL: {url}\nStatus: {status}\nContent-Type: {content_type}\n\n{body}"
        )))
    }
}

fn html_to_text(html: &str) -> String {
    // Drop script/style blocks first.
    let mut s = strip_blocks(html, "script");
    s = strip_blocks(&s, "style");
    s = strip_blocks(&s, "noscript");

    let mut out = String::new();
    let mut in_tag = false;
    let mut prev_space = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            }
            _ if in_tag => {}
            '\n' | '\r' | '\t' | ' ' => {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            }
            c => {
                out.push(c);
                prev_space = false;
            }
        }
    }
    html_unescape(out.trim())
}

fn strip_blocks(html: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let lower = html.to_ascii_lowercase();
    let mut out = String::new();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if let Some(rel) = lower[i..].find(&open) {
            let start = i + rel;
            out.push_str(&html[i..start]);
            if let Some(end_rel) = lower[start..].find(&close) {
                i = start + end_rel + close.len();
            } else {
                break;
            }
        } else {
            out.push_str(&html[i..]);
            break;
        }
    }
    out
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}
