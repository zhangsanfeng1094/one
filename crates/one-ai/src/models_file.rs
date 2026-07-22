//! Load / save `~/.one/agent/models.json` (Pi-compatible shape).
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
//!
//! ## `includeDefaults`
//!
//! - Missing / `true` (default): merge file over built-in defaults (legacy-friendly).
//! - `false`: file is authoritative (written by CRUD after first mutation).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::compat::CompatConfig;
use crate::openai::OpenaiWireApi;
use crate::registry::{ModelEntry, ModelRegistry, ProviderConfig};

#[derive(Debug, Deserialize)]
struct ModelsFile {
    /// When `false`, do not seed built-in defaults (authoritative snapshot).
    #[serde(default, rename = "includeDefaults", alias = "include_defaults")]
    include_defaults: Option<bool>,
    /// Legacy flat model list.
    #[serde(default)]
    models: Vec<FlatModelEntry>,
    /// Pi-style provider map: `"openai" → { baseUrl, api, apiKey, models }`.
    #[serde(default)]
    providers: BTreeMap<String, ProviderFileEntry>,
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

#[derive(Debug, Deserialize, Serialize)]
struct ProviderFileEntry {
    #[serde(
        default,
        rename = "providerType",
        alias = "provider_type",
        alias = "providertype",
        skip_serializing_if = "Option::is_none"
    )]
    provider_type: Option<String>,
    #[serde(
        default,
        rename = "baseUrl",
        alias = "base_url",
        skip_serializing_if = "Option::is_none"
    )]
    base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api: Option<String>,
    #[serde(
        default,
        rename = "apiKey",
        alias = "api_key",
        skip_serializing_if = "Option::is_none"
    )]
    api_key: Option<String>,
    /// Provider-level Pi `compat` defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compat: Option<CompatConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    models: Vec<ProviderModelEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ProviderModelEntry {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_window: Option<u32>,
    /// Per-model override of provider `api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api: Option<String>,
    #[serde(
        default,
        rename = "baseUrl",
        alias = "base_url",
        skip_serializing_if = "Option::is_none"
    )]
    base_url: Option<String>,
    /// Whether the model supports extended thinking (Pi `reasoning`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<bool>,
    /// Pi `thinkingLevelMap`.
    #[serde(
        default,
        rename = "thinkingLevelMap",
        alias = "thinking_level_map",
        skip_serializing_if = "Option::is_none"
    )]
    thinking_level_map: Option<crate::compat::ThinkingLevelMap>,
    /// Per-model `compat` overrides (merged over provider-level).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compat: Option<CompatConfig>,
}

/// On-disk snapshot written by CRUD.
#[derive(Debug, Serialize)]
struct ModelsFileOut {
    #[serde(rename = "includeDefaults")]
    include_defaults: bool,
    providers: BTreeMap<String, ProviderFileEntry>,
}

/// Result of loading models.json: registry + provider-level settings.
#[derive(Debug, Clone, Default)]
pub struct ModelsConfig {
    pub registry: ModelRegistry,
    pub providers: Vec<ProviderConfig>,
}

impl ModelsConfig {
    pub fn with_defaults() -> Self {
        Self {
            registry: ModelRegistry::with_defaults(),
            providers: Vec::new(),
        }
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn provider_mut(&mut self, id: &str) -> Option<&mut ProviderConfig> {
        self.providers.iter_mut().find(|p| p.id == id)
    }

    pub fn find_model(&self, provider: &str, model_id: &str) -> Option<&ModelEntry> {
        self.registry.find(provider, model_id)
    }

    /// Insert or replace a provider config (does not touch models).
    pub fn upsert_provider(&mut self, cfg: ProviderConfig) {
        if let Some(existing) = self.providers.iter_mut().find(|p| p.id == cfg.id) {
            *existing = cfg;
        } else {
            self.providers.push(cfg);
        }
    }

    /// Ensure a provider slot exists (empty defaults if missing).
    pub fn ensure_provider(&mut self, id: &str) {
        if self.provider(id).is_none() {
            self.providers.push(ProviderConfig {
                id: id.to_string(),
                ..Default::default()
            });
        }
    }

    /// Remove provider config and all of its models. Returns `true` if the provider existed.
    pub fn remove_provider(&mut self, id: &str) -> bool {
        let before = self.providers.len();
        self.providers.retain(|p| p.id != id);
        let removed_cfg = self.providers.len() != before;
        let removed_models = self.registry.remove_by_provider(id);
        removed_cfg || removed_models > 0
    }

    /// Insert or replace a model; ensures a provider slot exists.
    pub fn upsert_model(&mut self, entry: ModelEntry) {
        self.ensure_provider(&entry.provider);
        // Keep default_model if unset.
        if let Some(p) = self.provider_mut(&entry.provider) {
            if p.default_model.is_none() {
                p.default_model = Some(entry.id.clone());
            }
        }
        self.registry.add(entry);
    }

    /// Remove one model. Returns `true` if it existed.
    pub fn remove_model(&mut self, provider: &str, id: &str) -> bool {
        let removed = self.registry.remove(provider, id);
        if removed {
            let next_default = self
                .registry
                .list_by_provider(provider)
                .first()
                .map(|m| m.id.clone());
            if let Some(p) = self.provider_mut(provider) {
                if p.default_model.as_deref() == Some(id) {
                    p.default_model = next_default;
                }
            }
        }
        removed
    }
}

pub fn load_models_file(path: &Path) -> ModelsConfig {
    try_load_models_file(path).unwrap_or_else(|_| ModelsConfig::with_defaults())
}

/// Load models.json, returning a parse error instead of silently falling back.
pub fn try_load_models_file(path: &Path) -> Result<ModelsConfig, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("read failed: {e}"))?;
    // Tolerate trailing commas (common hand-edit mistake) before strict JSON parse.
    let cleaned = strip_json_trailing_commas(&content);
    let file: ModelsFile = serde_json::from_str(&cleaned).map_err(|e| {
        format!("invalid JSON: {e}. Tip: remove trailing commas after the last array/object item.")
    })?;

    let include_defaults = file.include_defaults.unwrap_or(true);
    let mut registry = if include_defaults {
        ModelRegistry::with_defaults()
    } else {
        ModelRegistry::empty()
    };
    let mut providers = Vec::new();

    // 1) Pi-style providers
    for (id, entry) in file.providers {
        let provider_api = entry.api.clone();
        let provider_base = entry.base_url.clone();
        let api_key_raw = entry.api_key.clone();
        let api_key = api_key_raw.as_deref().map(resolve_secret);
        let provider_compat = entry.compat.clone();

        for m in &entry.models {
            let api = m.api.clone().or_else(|| provider_api.clone());
            let base_url = m.base_url.clone().or_else(|| provider_base.clone());
            // Model compat is stored raw; merge with provider happens at resolve time.
            let compat = match (&provider_compat, &m.compat) {
                (Some(p), Some(m)) => Some(p.merge_override(m)),
                (Some(p), None) => Some(p.clone()),
                (None, Some(m)) => Some(m.clone()),
                (None, None) => None,
            };
            registry.add(ModelEntry {
                provider: id.clone(),
                name: m.name.clone().unwrap_or_else(|| m.id.clone()),
                id: m.id.clone(),
                context_window: m.context_window,
                api,
                base_url,
                api_key: None, // key lives on provider
                reasoning: m.reasoning,
                thinking_level_map: m.thinking_level_map.clone(),
                compat,
            });
        }

        let default_model = entry.models.first().map(|m| m.id.clone());
        // Prefer explicit `api`; fall back to `providerType` (same fixed protocol set).
        let api = provider_api
            .as_deref()
            .and_then(OpenaiWireApi::parse)
            .or_else(|| {
                entry
                    .provider_type
                    .as_deref()
                    .and_then(OpenaiWireApi::parse)
            });
        let provider_type = api.map(|a| a.as_str().to_string()).or(entry.provider_type);
        providers.push(ProviderConfig {
            id: id.clone(),
            provider_type,
            base_url: provider_base,
            api,
            api_key,
            api_key_raw,
            default_model,
            compat: provider_compat,
        });
    }

    // 2) Legacy flat models
    for entry in file.models {
        let id = entry.id;
        let api_key_raw = entry.api_key.clone();
        registry.add(ModelEntry {
            provider: entry.provider.clone(),
            name: entry.name.unwrap_or_else(|| id.clone()),
            id: id.clone(),
            context_window: entry.context_window,
            api: entry.api.clone(),
            base_url: entry.base_url.clone(),
            api_key: api_key_raw.as_deref().map(resolve_secret),
            reasoning: None,
            thinking_level_map: None,
            compat: None,
        });

        // Ensure a provider config exists for flat entries that carry baseUrl.
        if entry.base_url.is_some() || entry.api.is_some() || api_key_raw.is_some() {
            if !providers.iter().any(|p| p.id == entry.provider) {
                providers.push(ProviderConfig {
                    id: entry.provider.clone(),
                    provider_type: None,
                    base_url: entry.base_url,
                    api: entry.api.as_deref().and_then(OpenaiWireApi::parse),
                    api_key: api_key_raw.as_deref().map(resolve_secret),
                    api_key_raw,
                    default_model: Some(id),
                    compat: None,
                });
            }
        }
    }

    Ok(ModelsConfig {
        registry,
        providers,
    })
}

/// Persist full config as Pi-style snapshot with `includeDefaults: false`.
///
/// Atomic write: `path.tmp` then rename to `path`.
pub fn save_models_file(path: &Path, cfg: &ModelsConfig) -> Result<(), String> {
    let out = build_file_out(cfg);
    let json = serde_json::to_string_pretty(&out).map_err(|e| format!("serialize: {e}"))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{json}\n")).map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

fn build_file_out(cfg: &ModelsConfig) -> ModelsFileOut {
    let mut providers: BTreeMap<String, ProviderFileEntry> = BTreeMap::new();

    // Seed from provider configs.
    for p in &cfg.providers {
        providers.insert(
            p.id.clone(),
            ProviderFileEntry {
                provider_type: p.provider_type.clone(),
                base_url: p.base_url.clone(),
                api: p.api.map(|a| a.as_str().to_string()),
                api_key: serialize_api_key(p),
                compat: p.compat.clone().filter(|c| !c.is_empty()),
                models: Vec::new(),
            },
        );
    }

    // Attach models (create provider entry if only models exist).
    // First pass: lift common base_url / api onto provider when missing.
    for m in cfg.registry.list() {
        let entry = providers
            .entry(m.provider.clone())
            .or_insert_with(|| ProviderFileEntry {
                provider_type: None,
                base_url: None,
                api: None,
                api_key: None,
                compat: None,
                models: Vec::new(),
            });
        if entry.base_url.is_none() {
            if let Some(b) = &m.base_url {
                entry.base_url = Some(b.clone());
            }
        }
        if entry.api.is_none() {
            if let Some(a) = &m.api {
                entry.api = Some(a.clone());
            }
        }
    }

    // Second pass: emit model rows; omit fields that match provider-level.
    for m in cfg.registry.list() {
        let entry = providers
            .get_mut(&m.provider)
            .expect("provider seeded in first pass");

        let model_api = match (&m.api, &entry.api) {
            (Some(a), Some(pa)) if a == pa => None,
            (Some(a), _) => Some(a.clone()),
            _ => None,
        };
        let model_base = match (&m.base_url, &entry.base_url) {
            (Some(b), Some(pb)) if b == pb => None,
            (Some(b), _) => Some(b.clone()),
            _ => None,
        };

        let name = if m.name == m.id {
            None
        } else {
            Some(m.name.clone())
        };

        // Prefer model-only delta when provider already has the same base compat.
        // For simplicity, write the stored merged model compat when present and
        // distinct from provider-level (full blob is fine for round-trip).
        let model_compat = m.compat.clone().filter(|c| {
            if c.is_empty() {
                return false;
            }
            match &entry.compat {
                Some(pc) if pc == c => false,
                _ => true,
            }
        });

        entry.models.push(ProviderModelEntry {
            id: m.id.clone(),
            name,
            context_window: m.context_window,
            api: model_api,
            base_url: model_base,
            reasoning: m.reasoning,
            thinking_level_map: m.thinking_level_map.clone().filter(|map| !map.is_empty()),
            compat: model_compat,
        });
    }

    // Reorder models so default_model is first when known.
    for p in &cfg.providers {
        if let Some(def) = &p.default_model {
            if let Some(entry) = providers.get_mut(&p.id) {
                if let Some(pos) = entry.models.iter().position(|m| &m.id == def) {
                    if pos != 0 {
                        let m = entry.models.remove(pos);
                        entry.models.insert(0, m);
                    }
                }
            }
        }
    }

    ModelsFileOut {
        include_defaults: false,
        providers,
    }
}

fn serialize_api_key(p: &ProviderConfig) -> Option<String> {
    if let Some(raw) = &p.api_key_raw {
        return Some(raw.clone());
    }
    // Prefer known env refs for built-in provider ids.
    if let Some(env) = known_api_key_env(&p.id) {
        if p.api_key.is_some() {
            // If resolved key matches env, keep env ref; else fall through to literal.
            if let Ok(from_env) = std::env::var(env) {
                if p.api_key.as_deref() == Some(from_env.as_str()) {
                    return Some(format!("${env}"));
                }
            } else {
                // No env set — still prefer env ref for known providers.
                return Some(format!("${env}"));
            }
        } else {
            // No resolved key but known provider: omit (runtime uses env).
            return None;
        }
    }
    p.api_key.clone()
}

fn known_api_key_env(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "gemini" => Some("GEMINI_API_KEY"),
        _ => None,
    }
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
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
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
    fn load_compat_merge_provider_and_model() {
        let dir = tempfile_dir("compat");
        let path = dir.join("models.json");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"{{
              "includeDefaults": false,
              "providers": {{
                "ollama": {{
                  "baseUrl": "http://127.0.0.1:11434/v1",
                  "api": "openai-completions",
                  "apiKey": "ollama",
                  "compat": {{
                    "supportsDeveloperRole": false,
                    "supportsReasoningEffort": false
                  }},
                  "models": [
                    {{
                      "id": "gpt-oss:20b",
                      "reasoning": true,
                      "compat": {{
                        "thinkingFormat": "openai",
                        "supportsReasoningEffort": true
                      }}
                    }}
                  ]
                }}
              }}
            }}"#
        )
        .unwrap();

        let cfg = try_load_models_file(&path).unwrap();
        let m = cfg.find_model("ollama", "gpt-oss:20b").unwrap();
        assert_eq!(m.reasoning, Some(true));
        let c = m.compat.as_ref().expect("merged compat");
        assert_eq!(c.openai.supports_developer_role, Some(false));
        // Model override wins for reasoning effort.
        assert_eq!(c.openai.supports_reasoning_effort, Some(true));
        assert_eq!(
            c.openai.thinking_format,
            Some(crate::compat::ThinkingFormat::Openai)
        );

        let resolved = c
            .openai
            .resolve("ollama", "http://127.0.0.1:11434/v1", "gpt-oss:20b");
        assert!(!resolved.supports_developer_role);
        assert!(resolved.supports_reasoning_effort);
        assert_eq!(
            resolved.thinking_format,
            crate::compat::ThinkingFormat::Openai
        );
    }

    #[test]
    fn load_pi_style_providers() {
        let dir = tempfile_dir("pi");
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
        assert_eq!(p.api_key_raw.as_deref(), Some("sk-test"));
        let m = cfg.find_model("openai", "gpt-test").unwrap();
        assert_eq!(m.base_url.as_deref(), Some("https://example.com/v1"));
        assert_eq!(m.api.as_deref(), Some("openai-completions"));
        // Legacy merge keeps defaults.
        assert!(cfg.find_model("mock", "mock-v1").is_some());
    }

    #[test]
    fn provider_type_roundtrips() {
        let dir = tempfile_dir("provider-type");
        let path = dir.join("models.json");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"{{
              "providers": {{
                "proxy": {{
                  "providerType": "openai-compatible",
                  "baseUrl": "https://proxy.example/v1",
                  "models": [{{ "id": "m1" }}]
                }}
              }}
            }}"#
        )
        .unwrap();

        let cfg = try_load_models_file(&path).unwrap();
        // Alias normalizes to canonical protocol id.
        assert_eq!(
            cfg.provider("proxy")
                .and_then(|p| p.provider_type.as_deref()),
            Some("openai-completions")
        );
        assert_eq!(
            cfg.provider("proxy").and_then(|p| p.api),
            Some(OpenaiWireApi::OpenaiCompletions)
        );

        save_models_file(&path, &cfg).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"providerType\": \"openai-completions\""));
        assert!(saved.contains("\"api\": \"openai-completions\""));
    }

    #[test]
    fn save_roundtrip_and_drop_defaults() {
        let dir = tempfile_dir("roundtrip");
        let path = dir.join("models.json");

        let mut cfg = ModelsConfig::with_defaults();
        assert!(cfg.find_model("mock", "mock-v1").is_some());
        cfg.remove_model("mock", "mock-v1");
        cfg.upsert_model(ModelEntry {
            provider: "myproxy".into(),
            id: "foo".into(),
            name: "Foo".into(),
            context_window: Some(64_000),
            api: Some("openai-completions".into()),
            base_url: Some("https://x/v1".into()),
            api_key: None,
            reasoning: None,
            thinking_level_map: None,
            compat: None,
        });
        if let Some(p) = cfg.provider_mut("myproxy") {
            p.api_key = Some("sk-test".into());
            p.api_key_raw = Some("sk-test".into());
            p.base_url = Some("https://x/v1".into());
            p.api = Some(OpenaiWireApi::Completions);
        }

        save_models_file(&path, &cfg).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"includeDefaults\": false"));
        assert!(content.contains("myproxy"));
        assert!(!content.contains("mock-v1"));

        let reloaded = try_load_models_file(&path).unwrap();
        assert!(reloaded.find_model("mock", "mock-v1").is_none());
        assert!(reloaded.find_model("myproxy", "foo").is_some());
        let p = reloaded.provider("myproxy").unwrap();
        assert_eq!(p.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn legacy_without_include_defaults_still_merges() {
        let dir = tempfile_dir("legacy");
        let path = dir.join("models.json");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"{{
              "providers": {{
                "custom": {{
                  "baseUrl": "https://c/v1",
                  "api": "openai-completions",
                  "apiKey": "k",
                  "models": [{{ "id": "m1" }}]
                }}
              }}
            }}"#
        )
        .unwrap();

        let cfg = try_load_models_file(&path).unwrap();
        assert!(cfg.find_model("mock", "mock-v1").is_some());
        assert!(cfg.find_model("custom", "m1").is_some());
    }

    #[test]
    fn remove_provider_clears_models() {
        let mut cfg = ModelsConfig::with_defaults();
        assert!(cfg.find_model("openai", "gpt-4o").is_some());
        assert!(cfg.remove_provider("openai"));
        assert!(cfg.find_model("openai", "gpt-4o").is_none());
        assert!(cfg.provider("openai").is_none());
    }

    fn tempfile_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-models-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}
