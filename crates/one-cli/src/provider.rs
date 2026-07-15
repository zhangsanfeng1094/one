use one_ai::{MockProvider, ModelRegistry, ModelsConfig, OpenaiWireApi};
use one_core::agent::LlmProvider;

#[cfg(feature = "network")]
use one_ai::OllamaProvider;
#[cfg(feature = "http-providers")]
use one_ai::{AnthropicProvider, OpenAiProvider, OpenRouterProvider};

use crate::cli::{Cli, OpenaiApi, ProviderKind};
use crate::preferences;
use crate::settings;

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
        let (provider_id, kind) =
            resolve_initial_provider(cli, &user_settings, &models_config);
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
        "gemini" => std::env::var("GEMINI_BASE_URL").ok().or_else(|| {
            Some("https://generativelanguage.googleapis.com/v1beta/openai".into())
        }),
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

fn load_config() -> (ModelsConfig, Option<String>) {
    let path = one_session::agent_dir().join("models.json");
    if !path.exists() {
        return (
            ModelsConfig {
                registry: ModelRegistry::with_defaults(),
                providers: Vec::new(),
            },
            None,
        );
    }
    match one_ai::models_file::try_load_models_file(&path) {
        Ok(cfg) => (cfg, None),
        Err(err) => (
            ModelsConfig {
                registry: ModelRegistry::with_defaults(),
                providers: Vec::new(),
            },
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
                Err(
                    format!(
                        "provider `{provider_id}` needs OpenAI-compatible HTTP \
                         (rebuild with --features http-providers)"
                    )
                    .into(),
                )
            }
        }
    }
}
