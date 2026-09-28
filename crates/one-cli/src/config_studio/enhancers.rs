use super::api::{ConfigStudio, DraftView, SaveRequest};
use crate::runtime::prompt_enhancer::config::{
    self, Binding, BoundModel, EnhancerDefinition, EnhancerOverride,
};
use one_web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    scope: String,
    provider: String,
    model: String,
    #[serde(default)]
    preset: Option<String>,
    id: String,
    version: String,
    #[serde(default)]
    reset: bool,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    prompt: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Apply {
    version: String,
    id: String,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    models: Vec<BoundModel>,
    #[serde(default)]
    presets: Option<Vec<String>>,
}

fn bad(message: impl Into<String>) -> HttpResponse {
    HttpResponse::error(422, message)
}
fn doc_id(scope: &str) -> Result<&'static str, HttpResponse> {
    match scope {
        "global" => Ok("enhancers.global"),
        "project" => Ok("enhancers.project"),
        _ => Err(bad("scope must be global or project")),
    }
}

impl ConfigStudio {
    pub(super) fn enhancer_models(&self) -> Result<one_ai::ModelsConfig, HttpResponse> {
        let path = self.paths().agent_dir.join("models.json");
        if !path.exists() {
            return Ok(one_ai::ModelsConfig::with_defaults());
        }
        one_ai::models_file::try_load_models_file(&path).map_err(bad)
    }

    pub(super) fn enhancer_view(&self, req: &HttpRequest) -> Result<HttpResponse, HttpResponse> {
        let scope = req.query("scope").unwrap_or_else(|| "project".into());
        let doc = self.view(doc_id(&scope)?, false)?;
        if let Some(error) = doc.read_error {
            return Err(bad(error));
        }
        let config = config::parse(&doc.content).map_err(bad)?;
        let models = self.enhancer_models()?;
        // Explicit projection: registry entries also contain credentials.
        let catalog: Vec<_> = models
            .registry
            .list()
            .iter()
            .map(
                |m| json!({"provider": m.provider, "id": m.id, "name": m.name, "quirks": m.quirks}),
            )
            .collect();
        let provider = req.query("provider").unwrap_or_default();
        let model = req.query("model").unwrap_or_default();
        let preset = req.query("preset").unwrap_or_else(|| "code".into());
        let quirks = models
            .find_model(&provider, &model)
            .map(|m| m.quirks.as_slice())
            .unwrap_or_default();
        let sources = if scope == "global" {
            vec![self.paths().agent_dir.join(config::RELATIVE_PATH)]
        } else {
            config::paths(&self.paths().cwd, &self.paths().agent_dir)
        };
        let resolved = config::resolve_paths(sources, &provider, &model, &preset, quirks)
            .map_err(|e| bad(e.to_string()))?;
        let agents: Vec<_> = self
            .documents()
            .into_iter()
            .filter(|d| d.doc.id.starts_with("prompts.ref."))
            .filter_map(|d| {
                let agent = crate::runtime::presets::load_spec_file(&d.path()).ok()?;
                let prompt = agent.prompt.load(&self.paths().cwd).ok()?;
                Some(json!({"name": agent.name, "preset": prompt.preset}))
            })
            .collect();
        let presets: Vec<String> = one_prompt::ComponentRegistry::with_builtins()
            .preset_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let model_rows: Vec<_> = models
            .registry
            .list()
            .iter()
            .map(|m| (m.provider.clone(), m.id.clone(), m.quirks.clone()))
            .collect();
        let catalog_items = config::catalog(&config, &model_rows, &presets);
        let matches = if provider.is_empty() || model.is_empty() {
            Vec::new()
        } else {
            config::explain(
                &config,
                &model_rows,
                &presets,
                &provider,
                &model,
                &preset,
                quirks,
            )
        };
        Ok(HttpResponse::json(200, &json!({
            "models": catalog, "presets": presets,
            "enhancers": resolved, "catalog": catalog_items, "matches": matches,
            "agents": agents, "config": config, "version": doc.version, "path": doc.document.path,
            "effect_note": doc.document.effect_note,
        })).with_header("Cache-Control", "no-store"))
    }

    pub(super) fn enhancer_save(&self, req: &HttpRequest) -> Result<HttpResponse, HttpResponse> {
        let edit: Edit = serde_json::from_value(req.json_body().map_err(bad)?)
            .map_err(|e| bad(e.to_string()))?;
        let id = doc_id(&edit.scope)?;
        if self
            .enhancer_models()?
            .find_model(&edit.provider, &edit.model)
            .is_none()
        {
            return Err(bad("select a model from the registry"));
        }
        if edit
            .preset
            .as_ref()
            .is_some_and(|p| !one_prompt::ComponentRegistry::with_builtins().has_preset(p))
        {
            return Err(bad("unknown preset"));
        }
        let doc = self.view(id, false)?;
        if let Some(error) = doc.read_error {
            return Err(bad(error));
        }
        let mut config = config::parse(&doc.content).map_err(bad)?;
        if !config::is_builtin_id(&edit.id) && !config.definitions.iter().any(|d| d.id == edit.id) {
            return Err(bad("unknown enhancer"));
        }
        let index = config.bindings.iter().position(|b| {
            b.provider == edit.provider && b.model == edit.model && b.preset == edit.preset
        });
        if edit.reset {
            if let Some(index) = index {
                config.bindings[index].enhancers.remove(&edit.id);
            }
            config.bindings.retain(|b| !b.enhancers.is_empty());
        } else {
            let index = index.unwrap_or_else(|| {
                config.bindings.push(Binding {
                    provider: edit.provider,
                    model: edit.model,
                    preset: edit.preset,
                    enhancers: Default::default(),
                });
                config.bindings.len() - 1
            });
            config.bindings[index].enhancers.insert(
                edit.id,
                EnhancerOverride {
                    enabled: edit.enabled,
                    prompt: edit.prompt,
                },
            );
        }
        let result = self.save(
            id,
            &SaveRequest {
                draft: serde_json::to_string_pretty(&config).map_err(|e| bad(e.to_string()))?
                    + "\n",
                version: edit.version,
                view: DraftView::Source,
                confirm: true,
                unlocked: false,
            },
        )?;
        Ok(HttpResponse::json(200, &json!(result)).with_header("Cache-Control", "no-store"))
    }

    pub(super) fn enhancer_apply(&self, req: &HttpRequest) -> Result<HttpResponse, HttpResponse> {
        let edit: Apply = serde_json::from_value(req.json_body().map_err(bad)?)
            .map_err(|e| bad(e.to_string()))?;
        let registry = self.enhancer_models()?;
        for BoundModel { provider, model } in &edit.models {
            if registry.find_model(provider, model).is_none() {
                return Err(bad(format!("unknown model: {provider}/{model}")));
            }
        }
        let all_presets: Vec<String> = one_prompt::ComponentRegistry::with_builtins()
            .preset_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(presets) = &edit.presets {
            for preset in presets {
                if !all_presets.iter().any(|p| p == preset) {
                    return Err(bad(format!("unknown agent: {preset}")));
                }
            }
        }
        let doc = self.view("enhancers.global", false)?;
        if let Some(error) = doc.read_error {
            return Err(bad(error));
        }
        let mut config = config::parse(&doc.content).map_err(bad)?;
        if edit.delete {
            config::delete_custom(&mut config, &edit.id).map_err(bad)?;
        } else {
            let definition = if config::is_builtin_id(&edit.id) {
                None
            } else {
                Some(EnhancerDefinition {
                    id: edit.id.clone(),
                    name: edit.name.unwrap_or_default(),
                    description: edit.description.unwrap_or_default(),
                    prompt: edit.prompt.clone().unwrap_or_default(),
                })
            };
            config::apply_enhancer(
                &mut config,
                &edit.id,
                definition,
                edit.enabled,
                &edit.models,
                edit.presets.as_deref(),
                &all_presets,
                edit.prompt,
            )
            .map_err(bad)?;
        }
        let result = self.save(
            "enhancers.global",
            &SaveRequest {
                draft: serde_json::to_string_pretty(&config).map_err(|e| bad(e.to_string()))?
                    + "\n",
                version: edit.version,
                view: DraftView::Source,
                confirm: true,
                unlocked: false,
            },
        )?;
        Ok(HttpResponse::json(200, &json!(result)).with_header("Cache-Control", "no-store"))
    }
}
