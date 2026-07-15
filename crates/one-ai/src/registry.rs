use serde::{Deserialize, Serialize};

use crate::openai::OpenaiWireApi;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub provider: String,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Wire API: `openai-completions` | `openai-responses`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Per-model or inherited base URL (e.g. `https://api.openai.com/v1`).
    #[serde(default, alias = "baseUrl", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Optional resolved API key override (usually lives on provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// Provider-level settings from `models.json` `providers` block.
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    pub id: String,
    pub provider_type: Option<String>,
    pub base_url: Option<String>,
    pub api: Option<OpenaiWireApi>,
    /// Resolved API key (env refs expanded).
    pub api_key: Option<String>,
    /// Original `apiKey` string from file (e.g. `$OPENAI_API_KEY`) for round-trip save.
    pub api_key_raw: Option<String>,
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelRegistry {
    models: Vec<ModelEntry>,
}

impl ModelRegistry {
    pub fn with_defaults() -> Self {
        Self {
            models: vec![
                ModelEntry {
                    provider: "mock".into(),
                    id: "mock-v1".into(),
                    name: "Mock".into(),
                    context_window: Some(128_000),
                    api: None,
                    base_url: None,
                    api_key: None,
                },
                ModelEntry {
                    provider: "anthropic".into(),
                    id: "claude-sonnet-4-20250514".into(),
                    name: "Claude Sonnet 4".into(),
                    context_window: Some(200_000),
                    api: None,
                    base_url: None,
                    api_key: None,
                },
                ModelEntry {
                    provider: "openai".into(),
                    id: "gpt-4o".into(),
                    name: "GPT-4o".into(),
                    context_window: Some(128_000),
                    api: Some("openai-responses".into()),
                    base_url: Some("https://api.openai.com/v1".into()),
                    api_key: None,
                },
                ModelEntry {
                    provider: "openai".into(),
                    id: "gpt-4o-mini".into(),
                    name: "GPT-4o mini".into(),
                    context_window: Some(128_000),
                    api: Some("openai-responses".into()),
                    base_url: Some("https://api.openai.com/v1".into()),
                    api_key: None,
                },
                ModelEntry {
                    provider: "ollama".into(),
                    id: "llama3.2".into(),
                    name: "Llama 3.2 (Ollama)".into(),
                    context_window: Some(128_000),
                    api: Some("openai-completions".into()),
                    base_url: Some("http://127.0.0.1:11434/v1".into()),
                    api_key: None,
                },
                ModelEntry {
                    provider: "openrouter".into(),
                    id: "anthropic/claude-sonnet-4".into(),
                    name: "Claude Sonnet 4 (OpenRouter)".into(),
                    context_window: Some(200_000),
                    api: Some("openai-completions".into()),
                    base_url: Some("https://openrouter.ai/api/v1".into()),
                    api_key: None,
                },
                ModelEntry {
                    provider: "deepseek".into(),
                    id: "deepseek-chat".into(),
                    name: "DeepSeek Chat (V3)".into(),
                    context_window: Some(128_000),
                    api: Some("openai-completions".into()),
                    base_url: Some("https://api.deepseek.com".into()),
                    api_key: None,
                },
                ModelEntry {
                    provider: "deepseek".into(),
                    id: "deepseek-reasoner".into(),
                    name: "DeepSeek Reasoner (R1)".into(),
                    context_window: Some(128_000),
                    api: Some("openai-completions".into()),
                    base_url: Some("https://api.deepseek.com".into()),
                    api_key: None,
                },
                ModelEntry {
                    provider: "gemini".into(),
                    id: "gemini-2.5-flash".into(),
                    name: "Gemini 2.5 Flash".into(),
                    context_window: Some(1_000_000),
                    api: Some("openai-completions".into()),
                    base_url: Some(
                        "https://generativelanguage.googleapis.com/v1beta/openai".into(),
                    ),
                    api_key: None,
                },
                ModelEntry {
                    provider: "gemini".into(),
                    id: "gemini-2.5-pro".into(),
                    name: "Gemini 2.5 Pro".into(),
                    context_window: Some(1_000_000),
                    api: Some("openai-completions".into()),
                    base_url: Some(
                        "https://generativelanguage.googleapis.com/v1beta/openai".into(),
                    ),
                    api_key: None,
                },
            ],
        }
    }

    /// Built-in provider ids with a one-line description for `--list-providers`.
    pub fn builtin_provider_catalog() -> &'static [(&'static str, &'static str, &'static str)] {
        &[
            ("mock", "Mock (offline)", "—"),
            ("anthropic", "Anthropic Messages", "ANTHROPIC_API_KEY"),
            ("openai", "OpenAI Responses/Completions", "OPENAI_API_KEY"),
            ("ollama", "Ollama local", "—"),
            ("openrouter", "OpenRouter", "OPENROUTER_API_KEY"),
            ("deepseek", "DeepSeek (OpenAI-compat)", "DEEPSEEK_API_KEY"),
            (
                "gemini",
                "Google Gemini (OpenAI-compat)",
                "GEMINI_API_KEY / GOOGLE_API_KEY",
            ),
        ]
    }

    pub fn list(&self) -> &[ModelEntry] {
        &self.models
    }

    pub fn find(&self, provider: &str, id: &str) -> Option<&ModelEntry> {
        self.models
            .iter()
            .find(|model| model.provider == provider && model.id == id)
    }

    pub fn add(&mut self, entry: ModelEntry) {
        if let Some(existing) = self
            .models
            .iter()
            .position(|m| m.provider == entry.provider && m.id == entry.id)
        {
            self.models[existing] = entry;
        } else {
            self.models.push(entry);
        }
    }

    /// Remove a single model. Returns `true` if something was removed.
    pub fn remove(&mut self, provider: &str, id: &str) -> bool {
        let before = self.models.len();
        self.models
            .retain(|m| !(m.provider == provider && m.id == id));
        self.models.len() != before
    }

    /// Remove all models for a provider. Returns how many were removed.
    pub fn remove_by_provider(&mut self, provider: &str) -> usize {
        let before = self.models.len();
        self.models.retain(|m| m.provider != provider);
        before - self.models.len()
    }

    pub fn list_by_provider(&self, provider: &str) -> Vec<&ModelEntry> {
        self.models
            .iter()
            .filter(|m| m.provider == provider)
            .collect()
    }

    /// Empty registry (no built-in defaults).
    pub fn empty() -> Self {
        Self { models: Vec::new() }
    }
}
