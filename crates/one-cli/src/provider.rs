use std::path::PathBuf;

use one_ai::{
    save_models_file, MockProvider, ModelEntry, ModelRegistry, ModelsConfig, OpenaiWireApi,
    ProviderConfig,
};
use one_core::agent::LlmProvider;

#[cfg(feature = "network")]
use one_ai::OllamaProvider;
#[cfg(feature = "http-providers")]
use one_ai::{AnthropicProvider, OpenAiProvider, OpenRouterProvider};

use crate::cli::{Cli, OpenaiApi, ProviderKind};
use crate::preferences;
use crate::settings;

/// Path to `~/.one/agent/models.json`.
pub fn models_json_path() -> PathBuf {
    one_session::agent_dir().join("models.json")
}

pub struct ProviderSet {
    inner: std::sync::Arc<dyn LlmProvider>,
    /// Built-in kind when known; custom providers use `Openai` wire under the hood.
    pub kind: ProviderKind,
    /// Active provider id string (`openai`, `opencode`, …).
    pub provider_id: String,
    pub registry: ModelRegistry,
    pub models_config: ModelsConfig,
    pub openai_api: OpenaiApi,
    pub base_url: Option<String>,
    /// Load warning (e.g. models.json parse error) for UI.
    pub config_warning: Option<String>,
}

impl ProviderSet {
    pub fn build(cli: &Cli) -> Result<Self, Box<dyn std::error::Error>> {
        let (models_config, config_warning) = load_config();
        let user_settings = settings::load();
        let (provider_id, kind) = resolve_initial_provider(cli, &user_settings, &models_config);
        let effective_cli = effective_cli_with_settings(cli, &user_settings);
        let resolved = resolve_settings(&effective_cli, &models_config, &provider_id);
        let inner = build_provider_llm(&provider_id, &resolved)?;
        Ok(Self {
            inner,
            kind,
            provider_id,
            registry: models_config.registry.clone(),
            models_config,
            openai_api: resolved.openai_api,
            base_url: resolved.base_url.clone(),
            config_warning,
        })
    }

    pub fn as_llm(&self) -> &dyn LlmProvider {
        self.inner.as_ref()
    }

    pub fn as_arc(&self) -> std::sync::Arc<dyn LlmProvider> {
        self.inner.clone()
    }

    /// Switch by free-form provider name + optional model.
    /// Accepts built-ins (`openai`) and custom entries from models.json (`opencode`).
    pub fn switch_named(
        &mut self,
        provider_name: &str,
        model: Option<String>,
    ) -> Result<(), String> {
        let provider_id = provider_name.trim().to_string();
        if provider_id.is_empty() {
            return Err("provider name is empty".into());
        }

        // Prefer built-in mapping when the name matches.
        let kind = match provider_id.as_str() {
            "mock" => ProviderKind::Mock,
            "ollama" => ProviderKind::Ollama,
            "anthropic" => ProviderKind::Anthropic,
            "openai" => ProviderKind::Openai,
            "openrouter" => ProviderKind::Openrouter,
            "deepseek" => ProviderKind::Deepseek,
            "gemini" => ProviderKind::Gemini,
            _ => {
                // Custom: must exist in models.json providers or model list.
                let known = self.models_config.provider(&provider_id).is_some()
                    || self
                        .registry
                        .list()
                        .iter()
                        .any(|m| m.provider == provider_id);
                if !known {
                    return Err(format!(
                        "unknown provider `{provider_id}`. available: {}",
                        self.available_providers().join(", ")
                    ));
                }
                // Treat custom OpenAI-compatible endpoints as Openai kind for wire defaults.
                ProviderKind::Openai
            }
        };

        let cli = Cli {
            print: None,
            mode: crate::cli::RunMode::Interactive,
            r#continue: false,
            resume: false,
            session: None,
            no_session: true,
            provider: Some(kind.clone()),
            model,
            openai_api: None, // re-resolve from config for this provider/model
            base_url: None,
            api_key: None,
            cwd: std::path::PathBuf::from("."),
            add_dir: Vec::new(),
            full_access: false,
            name: None,
            read_only: false,
            plan: false,
            export: None,
            list_models: false,
            list_providers: false,
            auto_approve: false,
            share: false,
        };

        let resolved = resolve_settings(&cli, &self.models_config, &provider_id);
        self.inner = build_provider_llm(&provider_id, &resolved).map_err(|e| e.to_string())?;
        self.kind = kind;
        self.provider_id = provider_id;
        self.openai_api = resolved.openai_api;
        self.base_url = resolved.base_url;
        // Persist into unified settings + legacy preferences.
        let mut s = settings::load();
        s.provider = Some(self.provider_id.clone());
        s.model = Some(self.as_llm().model().to_string());
        if let Err(err) = settings::save(&s) {
            tracing::warn!("failed to save settings: {err}");
        }
        if let Err(err) = preferences::save(&self.provider_id, self.as_llm().model()) {
            tracing::warn!("failed to save model preferences: {err}");
        }
        Ok(())
    }

    pub fn switch(&mut self, kind: ProviderKind, model: Option<String>) -> Result<(), String> {
        self.switch_named(provider_id_of(&kind), model)
    }

    pub fn available_providers(&self) -> Vec<String> {
        let mut ids: Vec<String> = ModelRegistry::builtin_provider_catalog()
            .iter()
            .map(|(id, _, _)| (*id).to_string())
            .collect();
        for m in self.registry.list() {
            ids.push(m.provider.clone());
        }
        for p in &self.models_config.providers {
            ids.push(p.id.clone());
        }
        ids.sort();
        ids.dedup();
        ids
    }

    /// Context window for the active model (settings override > registry).
    pub fn context_window(&self) -> usize {
        let s = settings::load();
        if let Some(n) = s.context_window {
            return n;
        }
        self.registry
            .find(&self.provider_id, self.as_llm().model())
            .and_then(|m| m.context_window)
            .unwrap_or(0) as usize
    }

    pub fn available_models_line(&self) -> String {
        self.registry
            .list()
            .iter()
            .map(|m| format!("{}:{}", m.provider, m.id))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Sync registry view after models_config mutation, then save to disk.
    fn commit_config(&mut self, cfg: ModelsConfig) -> Result<(), String> {
        let path = models_json_path();
        save_models_file(&path, &cfg)?;
        self.registry = cfg.registry.clone();
        self.models_config = cfg;
        self.config_warning = None;
        Ok(())
    }

    /// Add or update a provider (`base_url` / `api` / `api_key` / `default_model` via kv).
    pub fn provider_add(&mut self, id: &str, kv: &[(String, String)]) -> Result<String, String> {
        let id = validate_id(id, "provider")?;
        let mut cfg = self.models_config.clone();
        let mut p = cfg.provider(&id).cloned().unwrap_or(ProviderConfig {
            id: id.clone(),
            ..Default::default()
        });
        apply_provider_kv(&mut p, kv)?;
        cfg.upsert_provider(p);
        self.commit_config(cfg)?;
        Ok(format!(
            "provider `{id}` saved · {}",
            models_json_path().display()
        ))
    }

    /// Set a single provider field.
    pub fn provider_set(&mut self, id: &str, key: &str, value: &str) -> Result<String, String> {
        let id = validate_id(id, "provider")?;
        if self.models_config.provider(&id).is_none()
            && !self.registry.list().iter().any(|m| m.provider == id)
        {
            return Err(format!(
                "unknown provider `{id}` · use /provider add {id} …"
            ));
        }
        let mut cfg = self.models_config.clone();
        cfg.ensure_provider(&id);
        {
            let p = cfg.provider_mut(&id).expect("ensured");
            apply_provider_kv(p, &[(key.to_string(), value.to_string())])?;
        }
        // Sync base/api onto models of this provider when set at provider level.
        let key_l = key.trim().to_ascii_lowercase();
        if matches!(key_l.as_str(), "base_url" | "base-url" | "baseurl" | "api") {
            let p = cfg.provider(&id).cloned().unwrap_or_default();
            let models: Vec<ModelEntry> = cfg
                .registry
                .list_by_provider(&id)
                .into_iter()
                .cloned()
                .collect();
            for mut m in models {
                if matches!(key_l.as_str(), "base_url" | "base-url" | "baseurl") {
                    m.base_url = p.base_url.clone();
                }
                if key_l == "api" {
                    m.api = p.api.map(|a| a.as_str().to_string());
                }
                cfg.registry.add(m);
            }
        }
        self.commit_config(cfg)?;
        Ok(format!(
            "provider.{id}.{key} = {value} · {}",
            models_json_path().display()
        ))
    }

    /// Remove a provider and all its models.
    pub fn provider_rm(&mut self, id: &str) -> Result<String, String> {
        let id = validate_id(id, "provider")?;
        if self.provider_id == id {
            return Err(format!(
                "cannot remove active provider `{id}` · switch with /model first"
            ));
        }
        let mut cfg = self.models_config.clone();
        if !cfg.remove_provider(&id) {
            // Still might only exist as models under defaults without provider slot.
            let n = cfg.registry.remove_by_provider(&id);
            if n == 0 {
                return Err(format!("provider `{id}` not found"));
            }
        }
        self.commit_config(cfg)?;
        Ok(format!(
            "removed provider `{id}` · {}",
            models_json_path().display()
        ))
    }

    /// Rows for `/providers` info float: (id, summary).
    pub fn providers_rows(&self) -> Vec<(String, String)> {
        let mut ids = self.available_providers();
        ids.sort();
        ids.into_iter()
            .map(|id| {
                let n = self.registry.list_by_provider(&id).len();
                let base = self
                    .models_config
                    .provider(&id)
                    .and_then(|p| p.base_url.clone())
                    .or_else(|| {
                        self.registry
                            .list_by_provider(&id)
                            .first()
                            .and_then(|m| m.base_url.clone())
                    })
                    .unwrap_or_else(|| "—".into());
                let mark = if id == self.provider_id {
                    " · active"
                } else {
                    ""
                };
                (id, format!("{n} model(s) · {base}{mark}"))
            })
            .collect()
    }

    /// Provider field rows for Settings detail readback: (`provider:key`, display value).
    pub fn provider_field_rows(&self) -> Vec<(String, String)> {
        let mut ids = self.available_providers();
        ids.sort();
        ids.into_iter()
            .flat_map(|id| {
                let provider_cfg = self.models_config.provider(&id);
                let first_model = self.registry.list_by_provider(&id).first().copied();
                let provider_type = provider_cfg
                    .and_then(|p| p.provider_type.clone())
                    .unwrap_or_else(|| "default/unset".into());
                let base_url = provider_cfg
                    .and_then(|p| p.base_url.clone())
                    .or_else(|| first_model.and_then(|m| m.base_url.clone()))
                    .unwrap_or_else(|| "unset".into());
                let api = provider_cfg
                    .and_then(|p| p.api.map(|a| a.as_str().to_string()))
                    .or_else(|| first_model.and_then(|m| m.api.clone()))
                    .unwrap_or_else(|| "default/unset".into());
                let api_key = provider_cfg
                    .and_then(|p| {
                        p.api_key_raw
                            .clone()
                            .filter(|raw| raw.starts_with('$'))
                            .or_else(|| p.api_key.as_ref().map(|_| "set".to_string()))
                    })
                    .unwrap_or_else(|| "unset".into());
                let default_model = provider_cfg
                    .and_then(|p| p.default_model.clone())
                    .or_else(|| first_model.map(|m| m.id.clone()))
                    .unwrap_or_else(|| "unset".into());
                [
                    (format!("{id}:provider_type"), provider_type),
                    (format!("{id}:base_url"), base_url),
                    (format!("{id}:api"), api),
                    (format!("{id}:api_key"), api_key),
                    (format!("{id}:default_model"), default_model),
                ]
            })
            .collect()
    }

    /// Rows for `/models [provider]`.
    pub fn models_rows(&self, filter: Option<&str>) -> Vec<(String, String)> {
        let current = self.as_llm().model();
        self.registry
            .list()
            .iter()
            .filter(|m| filter.map(|f| m.provider == f).unwrap_or(true))
            .map(|m| {
                let mark = if m.provider == self.provider_id && m.id == current {
                    " · active"
                } else {
                    ""
                };
                let ctx = m
                    .context_window
                    .map(|n| format!(" ctx={n}"))
                    .unwrap_or_default();
                (
                    format!("{}:{}", m.provider, m.id),
                    format!("{}{ctx}{mark}", m.name),
                )
            })
            .collect()
    }

    /// Add or upsert a model (`provider:id` + optional kv).
    pub fn model_add(&mut self, spec: &str, kv: &[(String, String)]) -> Result<String, String> {
        let (provider, id) = parse_model_spec(spec)?;
        let mut cfg = self.models_config.clone();
        cfg.ensure_provider(&provider);

        let existing = cfg.find_model(&provider, &id).cloned();
        let p = cfg.provider(&provider).cloned().unwrap_or_default();

        let mut entry = existing.unwrap_or(ModelEntry {
            provider: provider.clone(),
            id: id.clone(),
            name: id.clone(),
            context_window: None,
            api: p.api.map(|a| a.as_str().to_string()),
            base_url: p.base_url.clone(),
            api_key: None,
        });
        apply_model_kv(&mut entry, kv)?;
        // If model sets base/api and provider lacks them, lift.
        if let Some(prov) = cfg.provider_mut(&provider) {
            if prov.base_url.is_none() {
                prov.base_url = entry.base_url.clone();
            }
            if prov.api.is_none() {
                if let Some(a) = &entry.api {
                    prov.api = OpenaiWireApi::parse(a);
                }
            }
            if let Some((_, v)) = kv
                .iter()
                .find(|(k, _)| matches!(k.as_str(), "api_key" | "api-key" | "apikey" | "key"))
            {
                prov.api_key = Some(one_ai::resolve_secret(v));
                prov.api_key_raw = Some(v.clone());
            }
        }
        cfg.upsert_model(entry);
        self.commit_config(cfg)?;
        Ok(format!(
            "model `{provider}:{id}` saved · {}",
            models_json_path().display()
        ))
    }

    pub fn model_set(&mut self, spec: &str, key: &str, value: &str) -> Result<String, String> {
        let (provider, id) = parse_model_spec(spec)?;
        let mut cfg = self.models_config.clone();
        let Some(mut entry) = cfg.find_model(&provider, &id).cloned() else {
            return Err(format!(
                "unknown model `{provider}:{id}` · use /model-add {provider}:{id} …"
            ));
        };
        apply_model_kv(&mut entry, &[(key.to_string(), value.to_string())])?;
        cfg.upsert_model(entry);
        self.commit_config(cfg)?;
        Ok(format!(
            "model.{provider}:{id}.{key} = {value} · {}",
            models_json_path().display()
        ))
    }

    pub fn model_rm(&mut self, spec: &str) -> Result<String, String> {
        let (provider, id) = parse_model_spec(spec)?;
        if self.provider_id == provider && self.as_llm().model() == id {
            return Err(format!(
                "cannot remove active model `{provider}:{id}` · switch with /model first"
            ));
        }
        let mut cfg = self.models_config.clone();
        if !cfg.remove_model(&provider, &id) {
            return Err(format!("model `{provider}:{id}` not found"));
        }
        self.commit_config(cfg)?;
        Ok(format!(
            "removed model `{provider}:{id}` · {}",
            models_json_path().display()
        ))
    }

    /// Fetch OpenAI-compatible remote model ids using provider-level connection settings.
    pub async fn remote_model_rows(&self, id: &str) -> Result<Vec<(String, String)>, String> {
        let id = validate_id(id, "provider")?;
        let provider_cfg = self.models_config.provider(&id);
        let known = provider_cfg.is_some() || self.registry.list().iter().any(|m| m.provider == id);
        if !known {
            return Err(format!("unknown provider `{id}`"));
        }
        let base_url = provider_cfg
            .and_then(|p| p.base_url.clone())
            .or_else(|| {
                self.registry
                    .list_by_provider(&id)
                    .first()
                    .and_then(|m| m.base_url.clone())
            })
            .or_else(|| env_base_url_for(&id))
            .ok_or_else(|| format!("provider `{id}` has no base_url"))?;
        let api_key = provider_cfg
            .and_then(|p| p.api_key.clone())
            .or_else(|| env_api_key_for(&id));

        remote_model_rows_impl(&base_url, api_key.as_deref()).await
    }
}

#[cfg(feature = "network")]
async fn remote_model_rows_impl(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let models = one_ai::list_openai_compatible_models(base_url, api_key).await?;
    Ok(models
        .into_iter()
        .map(|m| {
            let detail = m.name.unwrap_or_else(|| "remote".into());
            (m.id, detail)
        })
        .collect())
}

#[cfg(not(feature = "network"))]
async fn remote_model_rows_impl(
    _base_url: &str,
    _api_key: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    Err("remote model fetch requires the network feature".into())
}

fn validate_id(id: &str, kind: &str) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err(format!("{kind} id is empty"));
    }
    if id.chars().any(|c| c.is_whitespace()) {
        return Err(format!("{kind} id must not contain whitespace"));
    }
    Ok(id.to_string())
}

/// Parse `provider:id` (id may contain `:` e.g. openrouter paths).
pub fn parse_model_spec(spec: &str) -> Result<(String, String), String> {
    let spec = spec.trim();
    let Some((p, m)) = spec.split_once(':') else {
        return Err("expected <provider>:<id>".into());
    };
    let provider = validate_id(p, "provider")?;
    let id = m.trim();
    if id.is_empty() {
        return Err("model id is empty".into());
    }
    Ok((provider, id.to_string()))
}

/// Parse trailing `key=value` tokens.
pub fn parse_kv_args(args: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for a in args {
        let Some((k, v)) = a.split_once('=') else {
            return Err(format!("expected key=value, got `{a}`"));
        };
        let k = k.trim();
        let v = v.trim();
        if k.is_empty() {
            return Err(format!("empty key in `{a}`"));
        }
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

fn apply_provider_kv(p: &mut ProviderConfig, kv: &[(String, String)]) -> Result<(), String> {
    for (key, value) in kv {
        match key.trim().to_ascii_lowercase().as_str() {
            "provider_type" | "provider-type" | "providertype" | "type" => {
                p.provider_type = if value.is_empty() {
                    None
                } else {
                    Some(value.clone())
                };
            }
            "base_url" | "base-url" | "baseurl" | "url" => {
                p.base_url = if value.is_empty() {
                    None
                } else {
                    Some(value.clone())
                };
            }
            "api" => {
                if value.is_empty() {
                    p.api = None;
                } else {
                    p.api = Some(OpenaiWireApi::parse(value).ok_or_else(|| {
                        format!("invalid api `{value}` (openai-completions|openai-responses)")
                    })?);
                }
            }
            "api_key" | "api-key" | "apikey" | "key" => {
                if value.is_empty() {
                    p.api_key = None;
                    p.api_key_raw = None;
                } else {
                    p.api_key_raw = Some(value.clone());
                    p.api_key = Some(one_ai::resolve_secret(value));
                }
            }
            "default_model" | "default-model" | "default" | "model" => {
                p.default_model = if value.is_empty() {
                    None
                } else {
                    Some(value.clone())
                };
            }
            other => {
                return Err(format!(
                    "unknown provider field `{other}` · known: provider_type base_url api api_key default_model"
                ));
            }
        }
    }
    Ok(())
}

fn apply_model_kv(m: &mut ModelEntry, kv: &[(String, String)]) -> Result<(), String> {
    for (key, value) in kv {
        match key.trim().to_ascii_lowercase().as_str() {
            "name" => {
                m.name = if value.is_empty() {
                    m.id.clone()
                } else {
                    value.clone()
                };
            }
            "ctx" | "context" | "context_window" | "context-window" => {
                if value.is_empty() || value == "0" {
                    m.context_window = None;
                } else {
                    let n: u32 = value
                        .parse()
                        .map_err(|_| format!("context_window must be a number, got `{value}`"))?;
                    m.context_window = Some(n);
                }
            }
            "api" => {
                if value.is_empty() {
                    m.api = None;
                } else {
                    let _ = OpenaiWireApi::parse(value).ok_or_else(|| {
                        format!("invalid api `{value}` (openai-completions|openai-responses)")
                    })?;
                    m.api = Some(value.clone());
                }
            }
            "base_url" | "base-url" | "baseurl" | "url" => {
                m.base_url = if value.is_empty() {
                    None
                } else {
                    Some(value.clone())
                };
            }
            "api_key" | "api-key" | "apikey" | "key" => {
                // Stored on provider via model_add; allow on model for legacy.
                m.api_key = if value.is_empty() {
                    None
                } else {
                    Some(one_ai::resolve_secret(value))
                };
            }
            other => {
                return Err(format!(
                    "unknown model field `{other}` · known: name ctx api base_url api_key"
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Resolved {
    model: String,
    openai_api: OpenaiApi,
    base_url: Option<String>,
    api_key: Option<String>,
}

fn resolve_initial_provider(
    cli: &Cli,
    user_settings: &settings::Settings,
    cfg: &ModelsConfig,
) -> (String, ProviderKind) {
    let provider_id = cli
        .provider
        .as_ref()
        .map(provider_id_of)
        .map(str::to_string)
        .or_else(|| user_settings.provider.clone())
        .unwrap_or_else(|| "mock".to_string());
    let kind = cli
        .provider
        .clone()
        .unwrap_or_else(|| kind_from_provider_id(&provider_id, cfg));
    (provider_id, kind)
}

fn effective_cli_with_settings(cli: &Cli, user_settings: &settings::Settings) -> Cli {
    let mut effective = cli.clone();
    if effective.model.is_some() {
        return effective;
    }
    let Some(saved_model) = user_settings.model.as_ref() else {
        return effective;
    };
    let provider_id = cli
        .provider
        .as_ref()
        .map(provider_id_of)
        .map(str::to_string)
        .or_else(|| user_settings.provider.clone())
        .unwrap_or_else(|| "mock".into());
    if cli.provider.is_none()
        || user_settings
            .provider
            .as_deref()
            .is_some_and(|p| p == provider_id)
    {
        effective.model = Some(saved_model.clone());
    }
    effective
}

fn kind_from_provider_id(provider_id: &str, cfg: &ModelsConfig) -> ProviderKind {
    match provider_id {
        "mock" => ProviderKind::Mock,
        "ollama" => ProviderKind::Ollama,
        "anthropic" => ProviderKind::Anthropic,
        "openai" => ProviderKind::Openai,
        "openrouter" => ProviderKind::Openrouter,
        "deepseek" => ProviderKind::Deepseek,
        "gemini" => ProviderKind::Gemini,
        _ => {
            let known = cfg.provider(provider_id).is_some()
                || cfg
                    .registry
                    .list()
                    .iter()
                    .any(|m| m.provider == provider_id);
            if known {
                ProviderKind::Openai
            } else {
                ProviderKind::Mock
            }
        }
    }
}

fn resolve_settings(cli: &Cli, cfg: &ModelsConfig, provider_id: &str) -> Resolved {
    let provider_cfg = cfg.provider(provider_id);
    let model_id = cli
        .model
        .clone()
        .or_else(|| provider_cfg.and_then(|p| p.default_model.clone()))
        .or_else(|| {
            cfg.registry
                .list_by_provider(provider_id)
                .first()
                .map(|m| m.id.clone())
        })
        .unwrap_or_else(|| default_model_for(provider_id).to_string());

    let model_entry = cfg.find_model(provider_id, &model_id);

    let openai_api = if let Some(api) = cli.openai_api {
        api
    } else if let Some(api) = model_entry
        .and_then(|m| m.api.as_deref())
        .and_then(OpenaiWireApi::parse)
    {
        wire_to_cli(api)
    } else if let Some(api) = provider_cfg.and_then(|p| p.api) {
        wire_to_cli(api)
    } else if let Ok(env) = std::env::var("ONE_OPENAI_API") {
        OpenaiWireApi::parse(&env)
            .map(wire_to_cli)
            .unwrap_or_else(|| default_wire_for(provider_id))
    } else {
        default_wire_for(provider_id)
    };

    let base_url = cli
        .base_url
        .clone()
        .or_else(|| model_entry.and_then(|m| m.base_url.clone()))
        .or_else(|| provider_cfg.and_then(|p| p.base_url.clone()))
        .or_else(|| env_base_url_for(provider_id));

    let api_key = cli
        .api_key
        .clone()
        .or_else(|| model_entry.and_then(|m| m.api_key.clone()))
        .or_else(|| provider_cfg.and_then(|p| p.api_key.clone()))
        .or_else(|| env_api_key_for(provider_id));

    Resolved {
        model: model_id,
        openai_api,
        base_url,
        api_key,
    }
}

fn wire_to_cli(api: OpenaiWireApi) -> OpenaiApi {
    match api {
        OpenaiWireApi::Completions => OpenaiApi::Completions,
        OpenaiWireApi::Responses => OpenaiApi::Responses,
    }
}

fn provider_id_of(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Mock => "mock",
        ProviderKind::Ollama => "ollama",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Openai => "openai",
        ProviderKind::Openrouter => "openrouter",
        ProviderKind::Deepseek => "deepseek",
        ProviderKind::Gemini => "gemini",
    }
}

fn default_model_for(provider_id: &str) -> &'static str {
    match provider_id {
        "mock" => "mock-v1",
        "ollama" => "llama3.2",
        "anthropic" => "claude-sonnet-4-20250514",
        "openai" => "gpt-4o",
        "openrouter" => "anthropic/claude-sonnet-4",
        "deepseek" => "deepseek-chat",
        "gemini" => "gemini-2.5-flash",
        _ => "default",
    }
}

fn default_wire_for(provider_id: &str) -> OpenaiApi {
    match provider_id {
        "openai" => OpenaiApi::Responses,
        _ => OpenaiApi::Completions,
    }
}

fn env_base_url_for(provider_id: &str) -> Option<String> {
    match provider_id {
        "openai" => std::env::var("OPENAI_BASE_URL")
            .ok()
            .or_else(|| std::env::var("OPENAI_API_BASE").ok()),
        "ollama" => std::env::var("OLLAMA_HOST").ok().map(|h| {
            if h.contains("/v1") {
                h
            } else {
                format!("{}/v1", h.trim_end_matches('/'))
            }
        }),
        "openrouter" => std::env::var("OPENROUTER_BASE_URL").ok(),
        "anthropic" => std::env::var("ANTHROPIC_BASE_URL").ok(),
        "deepseek" => std::env::var("DEEPSEEK_BASE_URL")
            .ok()
            .or_else(|| Some("https://api.deepseek.com".into())),
        "gemini" => std::env::var("GEMINI_BASE_URL")
            .ok()
            .or_else(|| Some("https://generativelanguage.googleapis.com/v1beta/openai".into())),
        _ => None,
    }
}

fn env_api_key_for(provider_id: &str) -> Option<String> {
    match provider_id {
        "openai" => std::env::var("OPENAI_API_KEY").ok(),
        "anthropic" => std::env::var("ANTHROPIC_API_KEY").ok(),
        "openrouter" => std::env::var("OPENROUTER_API_KEY").ok(),
        "deepseek" => std::env::var("DEEPSEEK_API_KEY").ok(),
        "gemini" => std::env::var("GEMINI_API_KEY")
            .ok()
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok()),
        "ollama" => Some("ollama".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-provider-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn restore_home(old: Option<std::ffi::OsString>) {
        match old {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }

    fn provider_set_for_test() -> ProviderSet {
        let mut registry = ModelRegistry::empty();
        registry.add(ModelEntry {
            provider: "proxy".into(),
            id: "m1".into(),
            name: "m1".into(),
            context_window: None,
            api: Some("openai-completions".into()),
            base_url: Some("https://proxy.example/v1".into()),
            api_key: None,
        });
        let mut cfg = ModelsConfig {
            registry: registry.clone(),
            providers: Vec::new(),
        };
        cfg.ensure_provider("proxy");
        if let Some(p) = cfg.provider_mut("proxy") {
            p.base_url = Some("https://proxy.example/v1".into());
            p.api = Some(OpenaiWireApi::Completions);
            p.default_model = Some("m1".into());
        }

        ProviderSet {
            inner: Arc::new(MockProvider::new()),
            kind: ProviderKind::Openai,
            provider_id: "proxy".into(),
            registry,
            models_config: cfg,
            openai_api: OpenaiApi::Completions,
            base_url: Some("https://proxy.example/v1".into()),
            config_warning: None,
        }
    }

    #[test]
    fn provider_set_api_enum_value_syncs_models() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_home = std::env::var_os("HOME");
        let home = temp_home("api-sync");
        std::env::set_var("HOME", &home);

        let mut providers = provider_set_for_test();
        providers
            .provider_set("proxy", "api", "openai-responses")
            .unwrap();

        let p = providers.models_config.provider("proxy").unwrap();
        assert_eq!(p.api, Some(OpenaiWireApi::Responses));
        let m = providers.models_config.find_model("proxy", "m1").unwrap();
        assert_eq!(m.api.as_deref(), Some("openai-responses"));

        restore_home(old_home);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn provider_set_empty_api_clears_provider_and_models() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_home = std::env::var_os("HOME");
        let home = temp_home("api-clear");
        std::env::set_var("HOME", &home);

        let mut providers = provider_set_for_test();
        providers.provider_set("proxy", "api", "").unwrap();

        let p = providers.models_config.provider("proxy").unwrap();
        assert_eq!(p.api, None);
        let m = providers.models_config.find_model("proxy", "m1").unwrap();
        assert_eq!(m.api, None);

        restore_home(old_home);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn provider_set_provider_type_persists() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_home = std::env::var_os("HOME");
        let home = temp_home("provider-type");
        std::env::set_var("HOME", &home);

        let mut providers = provider_set_for_test();
        providers
            .provider_set("proxy", "provider_type", "openai-compatible")
            .unwrap();

        let p = providers.models_config.provider("proxy").unwrap();
        assert_eq!(p.provider_type.as_deref(), Some("openai-compatible"));
        let saved = std::fs::read_to_string(models_json_path()).unwrap();
        assert!(saved.contains("\"providerType\": \"openai-compatible\""));

        restore_home(old_home);
        let _ = std::fs::remove_dir_all(home);
    }
}

fn load_config() -> (ModelsConfig, Option<String>) {
    let path = models_json_path();
    if !path.exists() {
        return (ModelsConfig::with_defaults(), None);
    }
    match one_ai::models_file::try_load_models_file(&path) {
        Ok(cfg) => (cfg, None),
        Err(err) => (
            ModelsConfig::with_defaults(),
            Some(format!(
                "models.json load failed ({}): {err}",
                path.display()
            )),
        ),
    }
}

fn build_provider_llm(
    provider_id: &str,
    resolved: &Resolved,
) -> Result<std::sync::Arc<dyn LlmProvider>, Box<dyn std::error::Error>> {
    match provider_id {
        "mock" => Ok(std::sync::Arc::new(MockProvider::new())),
        "ollama" => {
            #[cfg(feature = "network")]
            {
                let host = resolved
                    .base_url
                    .as_deref()
                    .map(|u| u.trim_end_matches('/').trim_end_matches("/v1").to_string())
                    .unwrap_or_else(|| {
                        std::env::var("OLLAMA_HOST")
                            .unwrap_or_else(|_| "http://127.0.0.1:11434".into())
                    });
                Ok(std::sync::Arc::new(OllamaProvider::new(
                    host,
                    &resolved.model,
                )))
            }
            #[cfg(not(feature = "network"))]
            {
                Err("Ollama requires network feature".into())
            }
        }
        "anthropic" => {
            #[cfg(feature = "http-providers")]
            {
                let key = resolved
                    .api_key
                    .clone()
                    .ok_or("ANTHROPIC_API_KEY is not set (or pass --api-key)")?;
                Ok(std::sync::Arc::new(AnthropicProvider::new(
                    key,
                    &resolved.model,
                )))
            }
            #[cfg(not(feature = "http-providers"))]
            {
                Err("Anthropic requires --features http-providers".into())
            }
        }
        "openrouter" => {
            #[cfg(feature = "http-providers")]
            {
                let key = resolved
                    .api_key
                    .clone()
                    .ok_or("OPENROUTER_API_KEY is not set (or pass --api-key)")?;
                let base = resolved
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
                Ok(std::sync::Arc::new(OpenRouterProvider::with_base(
                    key,
                    &resolved.model,
                    base,
                )))
            }
            #[cfg(not(feature = "http-providers"))]
            {
                Err("OpenRouter requires --features http-providers".into())
            }
        }
        // openai + any custom OpenAI-compatible provider (opencode, proxy, …)
        _ => {
            #[cfg(feature = "http-providers")]
            {
                let key = resolved.api_key.clone().ok_or_else(|| {
                    format!(
                        "API key missing for provider `{provider_id}` \
                         (set models.json apiKey / --api-key / env)"
                    )
                })?;
                let base = resolved.base_url.clone().ok_or_else(|| {
                    format!(
                        "baseUrl missing for provider `{provider_id}` \
                         (set models.json baseUrl / --base-url)"
                    )
                })?;
                let wire: OpenaiWireApi = resolved.openai_api.into();
                Ok(std::sync::Arc::new(
                    OpenAiProvider::with_base(key, &resolved.model, base).with_wire_api(wire),
                ))
            }
            #[cfg(not(feature = "http-providers"))]
            {
                Err(format!(
                    "provider `{provider_id}` needs OpenAI-compatible HTTP \
                         (rebuild with --features http-providers)"
                )
                .into())
            }
        }
    }
}
