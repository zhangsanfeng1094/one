use std::path::PathBuf;

use one_ai::{
    save_models_file, AuthStorage, MockProvider, ModelEntry, ModelRegistry, ModelsConfig,
    OpenaiWireApi, ProviderApi, ProviderConfig, PROVIDER_OPENAI_CODEX, PROVIDER_OPENCODE,
    PROVIDER_OPENCODE_GO, PROVIDER_XAI,
};
use one_core::agent::LlmProvider;

#[cfg(feature = "network")]
use one_ai::OllamaProvider;
#[cfg(feature = "http-providers")]
use one_ai::{
    AnthropicProvider, GeminiProvider, OpenAiCodexProvider, OpenAiProvider, OpenRouterProvider,
};

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

    /// Reload `models.json` into this set (e.g. after OAuth login seeded Codex models).
    pub fn reload_models_config(&mut self) {
        let (models_config, config_warning) = load_config();
        self.registry = models_config.registry.clone();
        self.models_config = models_config;
        self.config_warning = config_warning;
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
            "openai-codex" | "codex" | "chatgpt" => ProviderKind::OpenaiCodex,
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
            tui: false,
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
            no_mcp: false,
            no_skills: false,
            no_subagent: false,
            trace: false,
            trace_full: false,
            max_turns: 32,
            output_format: None,
            command: None,
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
                // Protocol is a fixed enum; providerType mirrors api when set.
                let api = provider_cfg
                    .and_then(|p| p.api.map(|a| a.as_str().to_string()))
                    .or_else(|| {
                        provider_cfg
                            .and_then(|p| p.provider_type.clone())
                            .and_then(|t| ProviderApi::parse(&t).map(|a| a.as_str().to_string()))
                    })
                    .or_else(|| {
                        first_model
                            .and_then(|m| m.api.as_deref())
                            .and_then(ProviderApi::parse)
                            .map(|a| a.as_str().to_string())
                    })
                    .unwrap_or_else(|| "default/unset".into());
                let provider_type = api.clone();
                let base_url = provider_cfg
                    .and_then(|p| p.base_url.clone())
                    .or_else(|| first_model.and_then(|m| m.base_url.clone()))
                    .unwrap_or_else(|| "unset".into());
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
                let compat = provider_cfg
                    .and_then(|p| p.compat.as_ref())
                    .filter(|c| !c.is_empty())
                    .map(|c| c.summary())
                    .unwrap_or_else(|| "auto (detect)".into());
                let thinking_format = provider_cfg
                    .and_then(|p| p.compat.as_ref())
                    .map(|c| c.thinking_format_display())
                    .unwrap_or_else(|| "auto".into());
                let max_tokens_field = provider_cfg
                    .and_then(|p| p.compat.as_ref())
                    .map(|c| c.max_tokens_field_display())
                    .unwrap_or_else(|| "auto".into());
                let mut rows = vec![
                    (format!("{id}:provider_type"), provider_type),
                    (format!("{id}:base_url"), base_url),
                    (format!("{id}:api"), api),
                    (format!("{id}:api_key"), api_key),
                    (format!("{id}:default_model"), default_model),
                    (format!("{id}:compat"), compat),
                    (format!("{id}:thinking_format"), thinking_format),
                    (format!("{id}:max_tokens_field"), max_tokens_field),
                ];
                // Expose individual tri-state bools for Settings rows.
                for (label, key) in one_ai::COMPAT_BOOL_FIELDS {
                    let display = provider_cfg
                        .and_then(|p| p.compat.as_ref())
                        .map(|c| c.get_tri(key).to_string())
                        .unwrap_or_else(|| "auto".into());
                    rows.push((format!("{id}:compat.{label}"), display));
                }
                rows
            })
            .collect()
    }

    /// Effective resolved compat summary for active or named provider (debug / UI).
    pub fn provider_compat_summary(&self, id: &str) -> String {
        let p = self.models_config.provider(id);
        match p.and_then(|p| p.compat.as_ref()) {
            Some(c) if !c.is_empty() => c.summary(),
            _ => "auto (detect from baseUrl / provider id)".into(),
        }
    }

    /// Cycle a provider-level compat tri-state bool; returns new display value.
    pub fn provider_cycle_compat(&mut self, id: &str, key: &str) -> Result<String, String> {
        let mut cfg = self.models_config.clone();
        cfg.ensure_provider(id);
        let p = cfg.provider_mut(id).expect("ensured");
        let mut c = p.compat.clone().unwrap_or_default();
        let next = c.cycle_tri(key)?;
        p.compat = if c.is_empty() { None } else { Some(c) };
        self.commit_config(cfg)?;
        Ok(next.to_string())
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
                let reasoning = match m.reasoning {
                    Some(true) => " reasoning=true",
                    Some(false) => " reasoning=false",
                    None => "",
                };
                let format = m
                    .compat
                    .as_ref()
                    .map(|c| format!(" format={}", c.thinking_format_display()))
                    .unwrap_or_default();
                let map = m
                    .thinking_level_map
                    .as_ref()
                    .filter(|map| !map.is_empty())
                    .map(|map| format!(" map={}", one_ai::format_thinking_level_map(map)))
                    .unwrap_or_default();
                let compat_bits = m
                    .compat
                    .as_ref()
                    .map(|c| {
                        format!(
                            " devRole={} effort={}",
                            c.get_tri("supports_developer_role"),
                            c.get_tri("supports_reasoning_effort")
                        )
                    })
                    .unwrap_or_default();
                (
                    format!("{}:{}", m.provider, m.id),
                    format!("{}{ctx}{reasoning}{format}{map}{compat_bits}{mark}", m.name),
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
            reasoning: None,
            thinking_level_map: None,
            compat: p.compat.clone(),
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

    /// Batch upsert remote model ids under one provider (single write to models.json).
    ///
    /// `models` rows are `(id, name_or_detail)` as returned by [`Self::remote_model_rows`].
    /// Existing models keep `context_window` / compat; name is refreshed when remote
    /// provides a real display name (not the placeholder `"remote"`).
    pub fn model_add_batch(
        &mut self,
        provider: &str,
        models: &[(String, String)],
    ) -> Result<String, String> {
        let provider = validate_id(provider, "provider")?;
        if models.is_empty() {
            return Ok(format!("no remote models to import for `{provider}`"));
        }

        let mut cfg = self.models_config.clone();
        cfg.ensure_provider(&provider);
        let p = cfg.provider(&provider).cloned().unwrap_or_default();

        let mut added = 0usize;
        let mut updated = 0usize;
        let mut skipped = 0usize;

        for (raw_id, detail) in models {
            let id = raw_id.trim();
            if id.is_empty() || id.chars().any(|c| c.is_whitespace()) {
                skipped += 1;
                continue;
            }
            let remote_name = detail.trim();
            let name = if remote_name.is_empty() || remote_name == "remote" {
                id.to_string()
            } else {
                remote_name.to_string()
            };

            let existing = cfg.find_model(&provider, id).cloned();
            if let Some(mut entry) = existing {
                if remote_name != "" && remote_name != "remote" {
                    entry.name = name;
                }
                cfg.upsert_model(entry);
                updated += 1;
            } else {
                cfg.upsert_model(ModelEntry {
                    provider: provider.clone(),
                    id: id.to_string(),
                    name,
                    context_window: None,
                    api: p.api.map(|a| a.as_str().to_string()),
                    base_url: p.base_url.clone(),
                    api_key: None,
                    reasoning: None,
                    thinking_level_map: None,
                    compat: p.compat.clone(),
                });
                added += 1;
            }
        }

        self.commit_config(cfg)?;
        let skip = if skipped > 0 {
            format!(" · skipped {skipped} invalid")
        } else {
            String::new()
        };
        Ok(format!(
            "imported {added} new · {updated} existing for `{provider}`{skip} · {}",
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
        let key_raw = key.trim();
        let key_l = key_raw.to_ascii_lowercase();
        match key_l.as_str() {
            "provider_type" | "provider-type" | "providertype" | "type" => {
                // Same fixed protocol set as `api` — select, never free-form.
                apply_provider_api(p, value)?;
            }
            "base_url" | "base-url" | "baseurl" | "url" => {
                p.base_url = if value.is_empty() {
                    None
                } else {
                    Some(value.clone())
                };
            }
            "api" => {
                apply_provider_api(p, value)?;
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
            "compat" | "compat_clear" | "compat-clear" | "clear_compat" => {
                if value.is_empty()
                    || value.eq_ignore_ascii_case("clear")
                    || value.eq_ignore_ascii_case("null")
                    || value.eq_ignore_ascii_case("none")
                {
                    p.compat = None;
                } else {
                    // Full JSON object replacement.
                    let c: one_ai::CompatConfig = serde_json::from_str(value)
                        .map_err(|e| format!("compat JSON: {e}"))?;
                    p.compat = if c.is_empty() { None } else { Some(c) };
                }
            }
            "thinking_format" | "thinkingformat" | "thinking-format" => {
                let mut c = p.compat.clone().unwrap_or_default();
                c.set_thinking_format(value)?;
                p.compat = if c.is_empty() { None } else { Some(c) };
            }
            "max_tokens_field" | "maxtokensfield" | "max-tokens-field" => {
                let mut c = p.compat.clone().unwrap_or_default();
                c.set_max_tokens_field(value)?;
                p.compat = if c.is_empty() { None } else { Some(c) };
            }
            other if other.starts_with("compat.")
                || other.starts_with("compat_")
                || is_compat_field(other) =>
            {
                let mut c = p.compat.clone().unwrap_or_default();
                apply_compat_field(&mut c, key_raw, value)?;
                p.compat = if c.is_empty() { None } else { Some(c) };
            }
            other => {
                return Err(format!(
                    "unknown provider field `{other}` · known: provider_type base_url api api_key default_model \
                     thinking_format max_tokens_field compat.* (supportsDeveloperRole, …)"
                ));
            }
        }
    }
    Ok(())
}

fn is_compat_field(key: &str) -> bool {
    let n = one_ai::normalize_compat_key(key);
    matches!(
        n.as_str(),
        "supports_developer_role"
            | "supports_reasoning_effort"
            | "supports_usage_in_streaming"
            | "supports_store"
            | "requires_tool_result_name"
            | "requires_assistant_after_tool_result"
            | "requires_thinking_as_text"
            | "requires_reasoning_content_on_assistant_messages"
            | "supports_strict_mode"
            | "thinking_format"
            | "max_tokens_field"
            | "force_adaptive_thinking"
            | "allow_empty_signature"
            | "supports_eager_tool_input_streaming"
            | "zai_tool_stream"
            | "open_router_routing"
    )
}

fn apply_compat_field(
    c: &mut one_ai::CompatConfig,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let n = one_ai::normalize_compat_key(key);
    match n.as_str() {
        "thinking_format" => c.set_thinking_format(value),
        "max_tokens_field" => c.set_max_tokens_field(value),
        "open_router_routing" => {
            if value.trim().is_empty() || value.eq_ignore_ascii_case("clear") {
                c.openai.open_router_routing = None;
            } else {
                let v: serde_json::Value = serde_json::from_str(value)
                    .map_err(|e| format!("openRouterRouting JSON: {e}"))?;
                c.openai.open_router_routing = Some(v);
            }
            Ok(())
        }
        _ => c.set_tri(key, value),
    }
}

/// Set wire protocol from a fixed enum string; mirrors onto `api` + `provider_type`.
fn apply_provider_api(p: &mut ProviderConfig, value: &str) -> Result<(), String> {
    if value.is_empty() {
        p.api = None;
        p.provider_type = None;
        return Ok(());
    }
    let api = ProviderApi::parse(value).ok_or_else(|| {
        format!(
            "invalid api `{value}` · choose: openai-completions | openai-responses | anthropic-messages | gemini-generate-content"
        )
    })?;
    p.api = Some(api);
    p.provider_type = Some(api.as_str().to_string());
    Ok(())
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!("expected bool, got `{other}`")),
    }
}

fn apply_model_kv(m: &mut ModelEntry, kv: &[(String, String)]) -> Result<(), String> {
    for (key, value) in kv {
        let key_raw = key.trim();
        match key_raw.to_ascii_lowercase().as_str() {
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
                    let api = ProviderApi::parse(value).ok_or_else(|| {
                        format!(
                            "invalid api `{value}` · choose: openai-completions | openai-responses | anthropic-messages | gemini-generate-content"
                        )
                    })?;
                    m.api = Some(api.as_str().to_string());
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
            "reasoning" | "reason" => {
                if value.is_empty() {
                    m.reasoning = None;
                } else {
                    m.reasoning = Some(parse_bool(value)?);
                }
            }
            "thinking_level_map"
            | "thinkinglevelmap"
            | "thinking-level-map"
            | "thinking_map" => {
                if value.is_empty() || value.eq_ignore_ascii_case("clear") {
                    m.thinking_level_map = None;
                } else {
                    let map = one_ai::parse_thinking_level_map(value)?;
                    m.thinking_level_map = if map.is_empty() { None } else { Some(map) };
                }
            }
            "compat" | "compat_clear" | "clear_compat" => {
                if value.is_empty()
                    || value.eq_ignore_ascii_case("clear")
                    || value.eq_ignore_ascii_case("null")
                {
                    m.compat = None;
                } else {
                    let c: one_ai::CompatConfig = serde_json::from_str(value)
                        .map_err(|e| format!("compat JSON: {e}"))?;
                    m.compat = if c.is_empty() { None } else { Some(c) };
                }
            }
            "thinking_format" | "thinkingformat" | "thinking-format" => {
                let mut c = m.compat.clone().unwrap_or_default();
                c.set_thinking_format(value)?;
                m.compat = if c.is_empty() { None } else { Some(c) };
            }
            "max_tokens_field" | "maxtokensfield" | "max-tokens-field" => {
                let mut c = m.compat.clone().unwrap_or_default();
                c.set_max_tokens_field(value)?;
                m.compat = if c.is_empty() { None } else { Some(c) };
            }
            other if other.starts_with("compat.")
                || other.starts_with("compat_")
                || is_compat_field(other) =>
            {
                let mut c = m.compat.clone().unwrap_or_default();
                apply_compat_field(&mut c, key_raw, value)?;
                m.compat = if c.is_empty() { None } else { Some(c) };
            }
            other => {
                return Err(format!(
                    "unknown model field `{other}` · known: name ctx api base_url api_key reasoning \
                     thinking_level_map thinking_format compat.*"
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
    /// ChatGPT account id for openai-codex OAuth.
    account_id: Option<String>,
    /// Auth provenance label (oauth / auth.json / env / --api-key).
    auth_source: Option<String>,
    /// Fully resolved Pi `compat` for OpenAI-compatible chat/completions.
    openai_compat: one_ai::ResolvedOpenAiCompat,
    /// Anthropic Messages compat (forceAdaptiveThinking, etc.).
    anthropic_compat: one_ai::ResolvedAnthropicCompat,
    /// Pi `reasoning` flag for the selected model.
    reasoning_model: bool,
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
        "openai-codex" | "codex" | "chatgpt" => ProviderKind::OpenaiCodex,
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
        .and_then(ProviderApi::parse)
    {
        wire_to_cli(api)
    } else if let Some(api) = provider_cfg.and_then(|p| p.api) {
        wire_to_cli(api)
    } else if let Some(api) = provider_cfg
        .and_then(|p| p.provider_type.as_deref())
        .and_then(ProviderApi::parse)
    {
        wire_to_cli(api)
    } else if let Ok(env) = std::env::var("ONE_OPENAI_API") {
        ProviderApi::parse(&env)
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

    let mut api_key = cli
        .api_key
        .clone()
        .or_else(|| model_entry.and_then(|m| m.api_key.clone()))
        .or_else(|| provider_cfg.and_then(|p| p.api_key.clone()))
        .or_else(|| env_api_key_for(provider_id));
    let mut account_id = None;
    let mut auth_source = None;
    let mut base_url = base_url;

    // AuthStorage (`~/.one/agent/auth.json`) — OAuth / stored keys, with refresh.
    // CLI --api-key already filled above wins over stored credentials.
    if cli.api_key.is_none() {
        if let Ok(storage) = AuthStorage::create() {
            // Normalize aliases so `codex` resolves the same credential as `openai-codex`.
            let auth_provider = match provider_id {
                "codex" | "chatgpt" => PROVIDER_OPENAI_CODEX,
                "zen" | "opencode-zen" => PROVIDER_OPENCODE,
                "go" => PROVIDER_OPENCODE_GO,
                "grok" | "supergrok" | "xai-oauth" => PROVIDER_XAI,
                other => other,
            };
            if let Ok(Some(auth)) = storage.resolve_api_key_blocking(auth_provider) {
                if api_key.is_none() {
                    api_key = auth.api_key;
                    auth_source = auth.source;
                }
                if base_url.is_none() {
                    base_url = auth.base_url;
                }
                account_id = auth.headers.get("chatgpt-account-id").cloned().or_else(|| {
                    storage
                        .get(auth_provider)
                        .and_then(|c| c.as_oauth().and_then(|o| o.account_id.clone()))
                });
            }
        }
    } else {
        auth_source = Some("--api-key".into());
    }

    let base_for_detect = base_url
        .clone()
        .unwrap_or_else(|| default_base_for_detect(provider_id).to_string());

    // Merge provider-level + model-level compat, then resolve against auto-detect.
    let partial = match (
        provider_cfg.and_then(|p| p.compat.as_ref()),
        model_entry.and_then(|m| m.compat.as_ref()),
    ) {
        (Some(p), Some(m)) => p.merge_override(m),
        (Some(p), None) => p.clone(),
        (None, Some(m)) => m.clone(),
        (None, None) => one_ai::CompatConfig::default(),
    };
    let mut openai_compat = partial
        .openai
        .resolve(provider_id, &base_for_detect, &model_id);
    if let Some(map) = model_entry.and_then(|m| m.thinking_level_map.clone()) {
        openai_compat = openai_compat.with_thinking_level_map(map);
    }
    let anthropic_compat = partial.anthropic().resolve();
    let reasoning_model = model_entry
        .and_then(|m| m.reasoning)
        .unwrap_or(false);

    Resolved {
        model: model_id,
        openai_api,
        base_url,
        api_key,
        account_id,
        auth_source,
        openai_compat,
        anthropic_compat,
        reasoning_model,
    }
}

fn default_base_for_detect(provider_id: &str) -> &'static str {
    match provider_id {
        "openai" => "https://api.openai.com/v1",
        "openai-codex" | "codex" | "chatgpt" => "https://chatgpt.com/backend-api",
        "openrouter" => "https://openrouter.ai/api/v1",
        "deepseek" => "https://api.deepseek.com",
        "ollama" => "http://127.0.0.1:11434/v1",
        "anthropic" => "https://api.anthropic.com",
        "gemini" => "https://generativelanguage.googleapis.com/v1beta",
        PROVIDER_OPENCODE | "zen" | "opencode-zen" => "https://opencode.ai/zen/v1",
        PROVIDER_OPENCODE_GO | "go" => "https://opencode.ai/zen/go/v1",
        PROVIDER_XAI | "grok" | "supergrok" => "https://cli-chat-proxy.grok.com/v1",
        _ => "",
    }
}

fn wire_to_cli(api: ProviderApi) -> OpenaiApi {
    match api {
        ProviderApi::OpenaiCompletions => OpenaiApi::Completions,
        ProviderApi::OpenaiResponses => OpenaiApi::Responses,
        ProviderApi::AnthropicMessages => OpenaiApi::AnthropicMessages,
        ProviderApi::GeminiGenerateContent => OpenaiApi::GeminiGenerateContent,
    }
}

fn provider_id_of(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Mock => "mock",
        ProviderKind::Ollama => "ollama",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Openai => "openai",
        ProviderKind::OpenaiCodex => PROVIDER_OPENAI_CODEX,
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
        "openai-codex" | "codex" | "chatgpt" => one_ai::OPENAI_CODEX_DEFAULT_MODEL,
        PROVIDER_OPENCODE | "zen" | "opencode-zen" => one_ai::OPENCODE_ZEN_DEFAULT_MODEL,
        PROVIDER_OPENCODE_GO | "go" => one_ai::OPENCODE_GO_DEFAULT_MODEL,
        PROVIDER_XAI | "grok" | "supergrok" => one_ai::XAI_DEFAULT_MODEL,
        "openrouter" => "anthropic/claude-sonnet-4",
        "deepseek" => "deepseek-chat",
        "gemini" => "gemini-2.5-flash",
        _ => "default",
    }
}

fn default_wire_for(provider_id: &str) -> OpenaiApi {
    match provider_id {
        "openai" => OpenaiApi::Responses,
        "openai-codex" | "codex" | "chatgpt" => OpenaiApi::Responses,
        "anthropic" => OpenaiApi::AnthropicMessages,
        "gemini" => OpenaiApi::GeminiGenerateContent,
        PROVIDER_XAI | "grok" | "supergrok" => OpenaiApi::Responses,
        PROVIDER_OPENCODE | PROVIDER_OPENCODE_GO | "zen" | "go" => OpenaiApi::Completions,
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
        "gemini" => std::env::var("GEMINI_BASE_URL").ok().or_else(|| {
            Some("https://generativelanguage.googleapis.com/v1beta".into())
        }),
        PROVIDER_OPENCODE | "zen" | "opencode-zen" => Some("https://opencode.ai/zen/v1".into()),
        PROVIDER_OPENCODE_GO | "go" => Some("https://opencode.ai/zen/go/v1".into()),
        PROVIDER_XAI | "grok" | "supergrok" => {
            Some("https://cli-chat-proxy.grok.com/v1".into())
        }
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
        PROVIDER_OPENCODE | PROVIDER_OPENCODE_GO | "zen" | "go" => std::env::var("OPENCODE_API_KEY")
            .ok()
            .or_else(|| std::env::var("OPENCODE_ZEN_API_KEY").ok()),
        PROVIDER_XAI | "grok" | "supergrok" => std::env::var("XAI_API_KEY")
            .ok()
            .or_else(|| std::env::var("XAI_OAUTH_TOKEN").ok()),
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
            reasoning: None,
            thinking_level_map: None,
            compat: None,
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
        // Alias `openai-compatible` normalizes to canonical `openai-completions`.
        providers
            .provider_set("proxy", "provider_type", "openai-compatible")
            .unwrap();

        let p = providers.models_config.provider("proxy").unwrap();
        assert_eq!(p.provider_type.as_deref(), Some("openai-completions"));
        assert_eq!(p.api, Some(ProviderApi::OpenaiCompletions));
        let saved = std::fs::read_to_string(models_json_path()).unwrap();
        assert!(saved.contains("\"providerType\": \"openai-completions\""));
        assert!(saved.contains("\"api\": \"openai-completions\""));

        providers
            .provider_set("proxy", "api", "anthropic-messages")
            .unwrap();
        let p = providers.models_config.provider("proxy").unwrap();
        assert_eq!(p.api, Some(ProviderApi::AnthropicMessages));
        assert_eq!(p.provider_type.as_deref(), Some("anthropic-messages"));

        restore_home(old_home);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn model_add_batch_imports_new_and_keeps_existing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_home = std::env::var_os("HOME");
        let home = temp_home("batch-import");
        std::env::set_var("HOME", &home);

        let mut providers = provider_set_for_test();
        // proxy already has m1 from fixture.
        let msg = providers
            .model_add_batch(
                "proxy",
                &[
                    ("m1".into(), "M1 Renamed".into()),
                    ("m2".into(), "Model Two".into()),
                    ("m3".into(), "remote".into()),
                    ("".into(), "skip-me".into()),
                ],
            )
            .unwrap();

        // m1 existing · m2/m3 new · empty id skipped
        assert!(msg.contains("2 new"), "{msg}");
        assert!(msg.contains("1 existing"), "{msg}");
        assert!(msg.contains("skipped 1"), "{msg}");

        let m1 = providers.models_config.find_model("proxy", "m1").unwrap();
        assert_eq!(m1.name, "M1 Renamed");
        let m2 = providers.models_config.find_model("proxy", "m2").unwrap();
        assert_eq!(m2.name, "Model Two");
        let m3 = providers.models_config.find_model("proxy", "m3").unwrap();
        assert_eq!(m3.name, "m3");

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
    // Protocol drives request/response codecs. Built-in ids still get special hosts.
    let api: ProviderApi = resolved.openai_api.into();

    if provider_id == "mock" {
        return Ok(std::sync::Arc::new(MockProvider::new()));
    }

    if provider_id == "ollama" {
        #[cfg(feature = "network")]
        {
            let host = resolved
                .base_url
                .as_deref()
                .map(|u| u.trim_end_matches('/').trim_end_matches("/v1").to_string())
                .unwrap_or_else(|| {
                    std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".into())
                });
            return Ok(std::sync::Arc::new(OllamaProvider::new(host, &resolved.model)));
        }
        #[cfg(not(feature = "network"))]
        {
            return Err("Ollama requires network feature".into());
        }
    }

    if provider_id == "openrouter" {
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
            return Ok(std::sync::Arc::new(
                OpenRouterProvider::with_base(key, &resolved.model, base)
                    .with_compat(resolved.openai_compat.clone())
                    .with_reasoning_model(resolved.reasoning_model),
            ));
        }
        #[cfg(not(feature = "http-providers"))]
        {
            return Err("OpenRouter requires --features http-providers".into());
        }
    }

    // OpenAI Codex (ChatGPT OAuth subscription).
    if provider_id == PROVIDER_OPENAI_CODEX
        || provider_id == "codex"
        || provider_id == "chatgpt"
    {
        #[cfg(feature = "http-providers")]
        {
            let key = resolved.api_key.clone().ok_or_else(|| {
                "openai-codex: not logged in · run `one login` (or /login in TUI)".to_string()
            })?;
            let account = resolved.account_id.clone().unwrap_or_default();
            let base = resolved
                .base_url
                .clone()
                .unwrap_or_else(|| "https://chatgpt.com/backend-api".into());
            return Ok(std::sync::Arc::new(OpenAiCodexProvider::with_base(
                key,
                &resolved.model,
                account,
                base,
            )));
        }
        #[cfg(not(feature = "http-providers"))]
        {
            return Err("openai-codex requires --features http-providers".into());
        }
    }

    // Anthropic Messages protocol — built-in `anthropic` or any custom id with api=anthropic-messages.
    if api == ProviderApi::AnthropicMessages || provider_id == "anthropic" {
        #[cfg(feature = "http-providers")]
        {
            let key = resolved.api_key.clone().ok_or_else(|| {
                if provider_id == "anthropic" {
                    "ANTHROPIC_API_KEY is not set (or pass --api-key)".to_string()
                } else if matches!(
                    provider_id,
                    PROVIDER_OPENCODE | PROVIDER_OPENCODE_GO | "zen" | "go"
                ) {
                    format!(
                        "{provider_id}: not logged in · run `one login {provider_id}` \
                         (or set OPENCODE_API_KEY)"
                    )
                } else {
                    format!(
                        "API key missing for provider `{provider_id}` \
                         (set models.json apiKey / --api-key / env)"
                    )
                }
            })?;
            let base = resolved
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.anthropic.com".into());
            return Ok(std::sync::Arc::new(
                AnthropicProvider::with_base(key, &resolved.model, base)
                    .with_compat(resolved.anthropic_compat.clone()),
            ));
        }
        #[cfg(not(feature = "http-providers"))]
        {
            return Err("Anthropic requires --features http-providers".into());
        }
    }

    // Gemini native generateContent — built-in `gemini` or api=gemini-generate-content.
    // If user explicitly set openai-completions for gemini (compat endpoint), fall through.
    if api == ProviderApi::GeminiGenerateContent
        || (provider_id == "gemini" && !api.is_openai_wire())
    {
        #[cfg(feature = "http-providers")]
        {
            let key = resolved.api_key.clone().ok_or_else(|| {
                if provider_id == "gemini" {
                    "GEMINI_API_KEY / GOOGLE_API_KEY is not set (or pass --api-key)".to_string()
                } else {
                    format!(
                        "API key missing for provider `{provider_id}` \
                         (set models.json apiKey / --api-key / env)"
                    )
                }
            })?;
            let base = resolved.base_url.clone().unwrap_or_else(|| {
                "https://generativelanguage.googleapis.com/v1beta".into()
            });
            return Ok(std::sync::Arc::new(GeminiProvider::with_base(
                key,
                &resolved.model,
                base,
            )));
        }
        #[cfg(not(feature = "http-providers"))]
        {
            return Err("Gemini requires --features http-providers".into());
        }
    }

    // OpenAI Chat Completions / Responses (and OpenAI-compatible endpoints).
    #[cfg(feature = "http-providers")]
    {
        let key = resolved.api_key.clone().ok_or_else(|| {
            if matches!(
                provider_id,
                PROVIDER_OPENCODE | PROVIDER_OPENCODE_GO | "zen" | "go"
            ) {
                format!(
                    "{provider_id}: not logged in · run `one login {provider_id}` \
                     (or set OPENCODE_API_KEY)"
                )
            } else if matches!(provider_id, PROVIDER_XAI | "grok" | "supergrok") {
                format!(
                    "{provider_id}: not logged in · run `one login xai` \
                     (or set XAI_API_KEY)"
                )
            } else {
                format!(
                    "API key missing for provider `{provider_id}` \
                     (set models.json apiKey / --api-key / env)"
                )
            }
        })?;
        let base = resolved.base_url.clone().ok_or_else(|| {
            format!(
                "baseUrl missing for provider `{provider_id}` \
                 (set models.json baseUrl / --base-url)"
            )
        })?;
        let wire = match api {
            ProviderApi::OpenaiCompletions => OpenaiWireApi::OpenaiCompletions,
            ProviderApi::OpenaiResponses => OpenaiWireApi::OpenaiResponses,
            ProviderApi::AnthropicMessages | ProviderApi::GeminiGenerateContent => {
                unreachable!("handled above")
            }
        };
        let mut extra = std::collections::BTreeMap::new();
        if matches!(provider_id, PROVIDER_XAI | "grok" | "supergrok")
            || base.contains("cli-chat-proxy.grok.com")
            || base.contains("api.x.ai")
        {
            extra = one_ai::auth::xai_cli_headers();
            extra.insert("x-grok-model-override".into(), resolved.model.clone());
        }
        Ok(std::sync::Arc::new(
            OpenAiProvider::with_base(key, &resolved.model, base)
                .with_wire_api(wire)
                .with_provider_id(provider_id)
                .with_compat(resolved.openai_compat.clone())
                .with_reasoning_model(resolved.reasoning_model)
                .with_extra_headers(extra),
        ))
    }
    #[cfg(not(feature = "http-providers"))]
    {
        Err(format!(
            "provider `{provider_id}` needs HTTP providers \
                 (rebuild with --features http-providers)"
        )
        .into())
    }
}
