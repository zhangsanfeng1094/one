//! Load `~/.one/agent/models.json` (Pi-compatible shape).
//!
//! Supports:
//! 1. **Legacy flat list**
//!    ```json
//!    { "models": [{ "provider": "openai", "id": "gpt-4o", "api": "openai-responses" }] }
//!    ```
//! 2. **Pi-style providers block**
//!    ```json
//!    {
//!      "providers": {
//!        "openai": {
//!          "baseUrl": "https://api.openai.com/v1",
//!          "api": "openai-responses",
//!          "apiKey": "$OPENAI_API_KEY",
//!          "models": [{ "id": "gpt-4o", "name": "GPT-4o" }]
//!        }
//!      }
//!    }
//!    ```

use std::path::Path;

use serde::Deserialize;

use crate::openai::OpenaiWireApi;
use crate::registry::{ModelEntry, ModelRegistry, ProviderConfig};

#[derive(Debug, Deserialize)]
struct ModelsFile {
    /// Legacy flat model list.
    #[serde(default)]
    models: Vec<FlatModelEntry>,
    /// Pi-style provider map: `"openai" → { baseUrl, api, apiKey, models }`.
    #[serde(default)]
    providers: std::collections::BTreeMap<String, ProviderFileEntry>,
}

#[derive(Debug, Deserialize)]
struct FlatModelEntry {
    provider: String,
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default, alias = "baseUrl")]
    base_url: Option<String>,
    #[serde(default, alias = "apiKey")]
    api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProviderFileEntry {
    #[serde(default, alias = "baseUrl")]
    base_url: Option<String>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default, alias = "apiKey")]
    api_key: Option<String>,
    #[serde(default)]
    models: Vec<ProviderModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ProviderModelEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_window: Option<u32>,
    /// Per-model override of provider `api`.
    #[serde(default)]
    api: Option<String>,
    #[serde(default, alias = "baseUrl")]
    base_url: Option<String>,
}

/// Result of loading models.json: registry + provider-level settings.
#[derive(Debug, Clone, Default)]
pub struct ModelsConfig {
    pub registry: ModelRegistry,
    pub providers: Vec<ProviderConfig>,
}

impl ModelsConfig {
    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn find_model(&self, provider: &str, model_id: &str) -> Option<&ModelEntry> {
        self.registry.find(provider, model_id)
    }
}

pub fn load_models_file(path: &Path) -> ModelsConfig {
    try_load_models_file(path).unwrap_or_else(|_| ModelsConfig {
        registry: ModelRegistry::with_defaults(),
        providers: Vec::new(),
    })
}

/// Load models.json, returning a parse error instead of silently falling back.
pub fn try_load_models_file(path: &Path) -> Result<ModelsConfig, String> {
    let mut registry = ModelRegistry::with_defaults();
    let mut providers = Vec::new();

    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("read failed: {e}"))?;
    // Tolerate trailing commas (common hand-edit mistake) before strict JSON parse.
    let cleaned = strip_json_trailing_commas(&content);
    let file: ModelsFile = serde_json::from_str(&cleaned).map_err(|e| {
        format!(
            "invalid JSON: {e}. Tip: remove trailing commas after the last array/object item."
        )
    })?;

    // 1) Pi-style providers
    for (id, entry) in file.providers {
        let provider_api = entry.api.clone();
        let provider_base = entry.base_url.clone();
        let api_key = entry.api_key.as_deref().map(resolve_secret);

        for m in &entry.models {
            let api = m.api.clone().or_else(|| provider_api.clone());
            let base_url = m.base_url.clone().or_else(|| provider_base.clone());
            registry.add(ModelEntry {
                provider: id.clone(),
                name: m.name.clone().unwrap_or_else(|| m.id.clone()),
                id: m.id.clone(),
                context_window: m.context_window,
                api,
                base_url,
                api_key: None, // key lives on provider
            });
        }

        providers.push(ProviderConfig {
            id: id.clone(),
            base_url: provider_base,
            api: provider_api
                .as_deref()
                .and_then(OpenaiWireApi::parse),
            api_key,
            default_model: entry.models.first().map(|m| m.id.clone()),
        });
    }

    // 2) Legacy flat models
    for entry in file.models {
        let id = entry.id;
        registry.add(ModelEntry {
            provider: entry.provider.clone(),
            name: entry.name.unwrap_or_else(|| id.clone()),
            id: id.clone(),
            context_window: entry.context_window,
            api: entry.api.clone(),
            base_url: entry.base_url.clone(),
            api_key: entry.api_key.as_deref().map(resolve_secret),
        });

        // Ensure a provider config exists for flat entries that carry baseUrl.
        if entry.base_url.is_some() || entry.api.is_some() || entry.api_key.is_some() {
            if !providers.iter().any(|p| p.id == entry.provider) {
                providers.push(ProviderConfig {
                    id: entry.provider.clone(),
                    base_url: entry.base_url,
                    api: entry.api.as_deref().and_then(OpenaiWireApi::parse),
                    api_key: entry.api_key.as_deref().map(resolve_secret),
                    default_model: Some(id),
                });
            }
        }
    }

    Ok(ModelsConfig {
        registry,
        providers,
    })
}

/// Remove trailing commas before `]` or `}` (not inside strings).
fn strip_json_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escape = false;
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == ',' {
            // Look ahead for whitespace then ] or }
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == ']' || chars[j] == '}') {
                // skip the comma
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Resolve `$ENV`, `${ENV}`, or literal secrets (Pi-style).
pub fn resolve_secret(raw: &str) -> String {
    let s = raw.trim();
    if let Some(rest) = s.strip_prefix("${") {
        if let Some(name) = rest.strip_suffix('}') {
            return std::env::var(name).unwrap_or_default();
        }
    }
    if let Some(name) = s.strip_prefix('$') {
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return std::env::var(name).unwrap_or_default();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn resolve_literal_and_env() {
        assert_eq!(resolve_secret("sk-literal"), "sk-literal");
        std::env::set_var("ONE_TEST_KEY", "from-env");
        assert_eq!(resolve_secret("$ONE_TEST_KEY"), "from-env");
        assert_eq!(resolve_secret("${ONE_TEST_KEY}"), "from-env");
        std::env::remove_var("ONE_TEST_KEY");
    }

    #[test]
    fn load_pi_style_providers() {
        let dir = tempfile_dir();
        let path = dir.join("models.json");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"{{
              "providers": {{
                "openai": {{
                  "baseUrl": "https://example.com/v1",
                  "api": "openai-completions",
                  "apiKey": "sk-test",
                  "models": [{{ "id": "gpt-test", "name": "Test" }}]
                }}
              }}
            }}"#
        )
        .unwrap();

        let cfg = load_models_file(&path);
        let p = cfg.provider("openai").expect("openai provider");
        assert_eq!(p.base_url.as_deref(), Some("https://example.com/v1"));
        assert_eq!(p.api, Some(OpenaiWireApi::Completions));
        assert_eq!(p.api_key.as_deref(), Some("sk-test"));
        let m = cfg.find_model("openai", "gpt-test").unwrap();
        assert_eq!(m.base_url.as_deref(), Some("https://example.com/v1"));
        assert_eq!(m.api.as_deref(), Some("openai-completions"));
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("one-models-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}
