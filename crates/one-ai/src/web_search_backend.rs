//! Provider-native web search as a **separate** Responses request (Grok Build style).
//!
//! The main agent keeps a normal function tool `web_search`; the host calls this
//! helper from inside that tool when the active model supports backend search.

use one_core::agent::{Citation, CompletionRequest, LlmProvider, ServerTool, ThinkingLevel};
use one_core::error::{OneError, Result};
use one_core::message::AgentMessage;

use crate::openai::{server_tools_for, OpenAiProvider, ProviderApi};

/// Whether this wire + model can host server-side web search.
pub fn supports_backend_search(wire_api: ProviderApi, model: &str) -> bool {
    !server_tools_for(wire_api, model).is_empty()
}

/// Credentials + model for a one-shot Responses search hop.
#[derive(Debug, Clone)]
pub struct ResponsesWebSearchConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub provider_id: String,
    pub wire_api: ProviderApi,
    pub extra_headers: std::collections::BTreeMap<String, String>,
}

impl ResponsesWebSearchConfig {
    pub fn server_tools(&self) -> Vec<ServerTool> {
        server_tools_for(self.wire_api, &self.model)
    }

    pub fn is_usable(&self) -> bool {
        !self.api_key.is_empty()
            && !self.base_url.is_empty()
            && !self.model.is_empty()
            && supports_backend_search(self.wire_api, &self.model)
    }
}

/// Formatted result for the `web_search` tool output.
#[derive(Debug, Clone)]
pub struct ResponsesWebSearchResult {
    pub text: String,
    pub citations: Vec<Citation>,
}

/// Run a non-streaming Responses completion with only server search tools.
///
/// No local function tools are offered — the model is asked to search and summarize.
#[cfg(feature = "http-providers")]
pub async fn responses_web_search(
    cfg: &ResponsesWebSearchConfig,
    query: &str,
    count: usize,
) -> Result<ResponsesWebSearchResult> {
    if !cfg.is_usable() {
        return Err(OneError::Provider(
            "backend web_search: model/provider does not support server search".into(),
        ));
    }
    let tools = cfg.server_tools();
    if tools.is_empty() {
        return Err(OneError::Provider(
            "backend web_search: no server tools for model".into(),
        ));
    }

    let count = count.clamp(1, 10);
    let provider = OpenAiProvider::with_base(&cfg.api_key, &cfg.model, &cfg.base_url)
        .with_wire_api(cfg.wire_api)
        .with_provider_id(&cfg.provider_id)
        .with_extra_headers(cfg.extra_headers.clone());

    let request = CompletionRequest {
        system_prompt: format!(
            "You are a web search assistant. Use the provided search tools to find \
             up-to-date information. Return a concise answer with up to {count} relevant \
             sources (title + URL + short snippet). Prefer primary docs and official pages. \
             Do not invent URLs."
        ),
        messages: vec![AgentMessage::user_text(format!(
            "Search the web for:\n{query}"
        ))],
        tools: Vec::new(),
        server_tools: tools,
        thinking_level: ThinkingLevel::Off,
    };

    let response = provider.complete(request).await?;
    let answer = one_core::agent::extract_text(&response.content);
    let text = format_search_output(
        &cfg.provider_id,
        &cfg.model,
        query,
        answer.trim(),
        &response.citations,
    );

    Ok(ResponsesWebSearchResult {
        text,
        citations: response.citations,
    })
}

#[cfg(not(feature = "http-providers"))]
pub async fn responses_web_search(
    _cfg: &ResponsesWebSearchConfig,
    _query: &str,
    _count: usize,
) -> Result<ResponsesWebSearchResult> {
    Err(OneError::Provider(
        "backend web_search requires http-providers feature".into(),
    ))
}

fn format_search_output(
    provider_id: &str,
    model: &str,
    query: &str,
    answer: &str,
    citations: &[Citation],
) -> String {
    let mut out = format!("Web search (provider-native · {provider_id}/{model}) for: {query}\n\n");
    if !answer.is_empty() {
        out.push_str(answer);
        if !answer.ends_with('\n') {
            out.push('\n');
        }
    } else {
        out.push_str("(no answer text from provider)\n");
    }

    if !citations.is_empty() {
        out.push_str("\nSources:\n");
        let mut seen = std::collections::HashSet::new();
        for c in citations {
            if !seen.insert(c.url.as_str()) {
                continue;
            }
            if c.title.trim().is_empty() || c.title == c.url {
                out.push_str(&format!("- {}\n", c.url));
            } else {
                out.push_str(&format!("- {} — {}\n", c.title, c.url));
            }
        }
    }
    out.push_str("\nTip: use web_fetch on a Link for full page text.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::ProviderApi;

    #[test]
    fn capability_matches_server_tools_matrix() {
        assert!(supports_backend_search(ProviderApi::Responses, "gpt-5.2"));
        assert!(supports_backend_search(ProviderApi::Responses, "grok-4.5"));
        assert!(supports_backend_search(ProviderApi::Responses, "composer-2.5"));
        assert!(!supports_backend_search(ProviderApi::Completions, "gpt-5.2"));
        assert!(!supports_backend_search(ProviderApi::Completions, "composer-2.5"));
        assert!(!supports_backend_search(ProviderApi::Responses, "claude-sonnet"));
    }

    #[test]
    fn formats_sources_deduped() {
        let text = format_search_output(
            "xai",
            "grok-4.5",
            "rust release",
            "Rust 1.90 is out.",
            &[
                Citation {
                    url: "https://example.com/a".into(),
                    title: "A".into(),
                    start_index: 0,
                    end_index: 1,
                },
                Citation {
                    url: "https://example.com/a".into(),
                    title: "A again".into(),
                    start_index: 0,
                    end_index: 1,
                },
            ],
        );
        assert!(text.contains("provider-native · xai/grok-4.5"));
        assert!(text.contains("Rust 1.90 is out."));
        assert_eq!(text.matches("https://example.com/a").count(), 1);
    }

    #[test]
    fn config_usable_requires_responses_search_model() {
        let mut cfg = ResponsesWebSearchConfig {
            api_key: "k".into(),
            base_url: "https://api.x.ai/v1".into(),
            model: "grok-4.5".into(),
            provider_id: "xai".into(),
            wire_api: ProviderApi::Responses,
            extra_headers: Default::default(),
        };
        assert!(cfg.is_usable());
        cfg.wire_api = ProviderApi::Completions;
        assert!(!cfg.is_usable());
    }
}
