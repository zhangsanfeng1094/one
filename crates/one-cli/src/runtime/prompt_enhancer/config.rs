//! User-editable model behavior deltas. No wire compatibility or agent settings.
use super::PromptEnhancer;
use one_ai::registry::ModelQuirk;
use one_prompt::{ErrorKind, PromptError};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const RELATIVE_PATH: &str = "prompts/enhancers.json";
pub const QUIRKS: [ModelQuirk; 4] = [
    ModelQuirk::OverPlanning,
    ModelQuirk::StopsEarly,
    ModelQuirk::ReluctantToUseTools,
    ModelQuirk::WeakVerification,
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancerConfig {
    pub version: u32,
    /// Custom enhancer metadata. Built-in quirks are never stored here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub definitions: Vec<EnhancerDefinition>,
    #[serde(default)]
    pub bindings: Vec<Binding>,
}
impl Default for EnhancerConfig {
    fn default() -> Self {
        Self {
            version: 1,
            definitions: vec![],
            bindings: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancerDefinition {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub provider: String,
    pub model: String,
    /// Absent means all profiles. Specific profiles override this within a layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub enhancers: BTreeMap<String, EnhancerOverride>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancerOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedEnhancer {
    pub id: String,
    pub name: String,
    pub description: String,
    pub hook: String,
    pub default_prompt: String,
    pub prompt: String,
    pub enabled: bool,
    pub prompt_source: String,
    pub binding_source: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnhancerCatalogItem {
    pub id: String,
    pub name: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub enabled: bool,
    pub prompt: String,
    pub default_prompt: String,
    pub models: Vec<BoundModel>,
    pub agents: Vec<String>,
    pub all_agents: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoundModel {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnhancerMatch {
    pub id: String,
    pub name: String,
    pub title: String,
    pub kind: String,
    pub active: bool,
    pub reasons: Vec<String>,
}

pub fn is_builtin_id(id: &str) -> bool {
    ModelQuirk::parse(id).is_some()
}

pub fn validate_custom_id(id: &str) -> Result<(), String> {
    if id.len() < 2 || id.len() > 64 {
        return Err("enhancer id must be 2–64 characters".into());
    }
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return Err("enhancer id is required".into());
    };
    if !first.is_ascii_alphabetic() || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(
            "enhancer id must start with a letter and contain only letters, digits, or _".into(),
        );
    }
    if is_builtin_id(id) {
        return Err(format!("{id} is a built-in enhancer id"));
    }
    Ok(())
}

fn known_ids(config: &EnhancerConfig) -> impl Iterator<Item = &str> {
    QUIRKS
        .iter()
        .map(|q| q.as_str())
        .chain(config.definitions.iter().map(|d| d.id.as_str()))
}

pub fn parse(text: &str) -> Result<EnhancerConfig, String> {
    let config: EnhancerConfig = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if config.version != 1 {
        return Err("enhancers.version must be 1".into());
    }
    let mut defined = std::collections::BTreeSet::new();
    for definition in &config.definitions {
        validate_custom_id(&definition.id)?;
        if definition.name.trim().is_empty() {
            return Err(format!("{}: name is required", definition.id));
        }
        if definition.prompt.trim().is_empty() {
            return Err(format!("{}: prompt must not be empty", definition.id));
        }
        if !defined.insert(definition.id.as_str()) {
            return Err(format!("duplicate enhancer definition: {}", definition.id));
        }
    }
    let mut keys = std::collections::BTreeSet::new();
    for binding in &config.bindings {
        if binding.provider.trim().is_empty()
            || binding.model.trim().is_empty()
            || binding.preset.as_ref().is_some_and(|s| s.trim().is_empty())
        {
            return Err("provider, model and optional preset must not be empty".into());
        }
        if !keys.insert((&binding.provider, &binding.model, &binding.preset)) {
            return Err("duplicate model/profile binding".into());
        }
        for (id, value) in &binding.enhancers {
            if !known_ids(&config).any(|known| known == id) {
                return Err(format!("unknown enhancer: {id}"));
            }
            if value.prompt.as_ref().is_some_and(|s| s.trim().is_empty()) {
                return Err(format!(
                    "{id}: prompt must not be empty; delete prompt to inherit default"
                ));
            }
        }
    }
    Ok(config)
}

/// Same project chain for runtime, preview and document catalog; nearest last.
pub fn project_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![];
    let mut cur = cwd.to_path_buf();
    loop {
        roots.push(cur.clone());
        if cur.join(".git").exists() || !cur.pop() {
            break;
        }
    }
    roots.reverse();
    roots
}

pub fn paths(cwd: &Path, agent_dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![agent_dir.join(RELATIVE_PATH)];
    for root in project_roots(cwd) {
        let path = root.join(".one").join(RELATIVE_PATH);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

pub fn load(path: &Path) -> Result<EnhancerConfig, PromptError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => {
            return Err(PromptError {
                kind: ErrorKind::Io,
                source_location: path.display().to_string(),
                message: e.to_string(),
            })
        }
    };
    parse(&text).map_err(|message| PromptError {
        kind: ErrorKind::Parse,
        source_location: path.display().to_string(),
        message,
    })
}

pub fn resolve(
    cwd: &Path,
    agent_dir: &Path,
    provider: &str,
    model: &str,
    preset: &str,
    quirks: &[ModelQuirk],
) -> Result<Vec<ResolvedEnhancer>, PromptError> {
    resolve_paths(paths(cwd, agent_dir), provider, model, preset, quirks)
}

/// Resolve an explicit source chain for the global-only Studio preview.
pub fn resolve_paths(
    sources: Vec<PathBuf>,
    provider: &str,
    model: &str,
    preset: &str,
    quirks: &[ModelQuirk],
) -> Result<Vec<ResolvedEnhancer>, PromptError> {
    let mut configs = Vec::new();
    for path in &sources {
        configs.push((path.clone(), load(path)?));
    }
    let mut result: Vec<_> = QUIRKS
        .iter()
        .map(|q| {
            let patch = PromptEnhancer::patch_for(*q);
            ResolvedEnhancer {
                id: q.as_str().into(),
                name: format!("{q:?}"),
                description: patch.title.into(),
                hook: "behavior_hooks".into(),
                default_prompt: patch.instruction.into(),
                prompt: patch.instruction.into(),
                enabled: quirks.contains(q),
                prompt_source: "builtin".into(),
                binding_source: "model registry".into(),
                kind: "builtin".into(),
            }
        })
        .collect();
    let mut seen_custom = std::collections::BTreeSet::new();
    for (_, config) in &configs {
        for definition in &config.definitions {
            if !seen_custom.insert(definition.id.clone()) {
                continue;
            }
            result.push(ResolvedEnhancer {
                id: definition.id.clone(),
                name: definition.name.clone(),
                description: definition.description.clone(),
                hook: "behavior_hooks".into(),
                default_prompt: definition.prompt.clone(),
                prompt: definition.prompt.clone(),
                enabled: false,
                prompt_source: "custom definition".into(),
                binding_source: "unbound".into(),
                kind: "custom".into(),
            });
        }
    }
    for (path, config) in &configs {
        for profile in [None, Some(preset)] {
            let mut matched_bindings: Vec<&Binding> = config
                .bindings
                .iter()
                .filter(|b| {
                    pattern_matches(&b.provider, provider)
                        && pattern_matches(&b.model, model)
                        && b.preset.as_deref() == profile
                })
                .collect();
            matched_bindings.sort_by_key(|b| {
                let exact_p = b.provider.eq_ignore_ascii_case(provider);
                let exact_m = b.model.eq_ignore_ascii_case(model);
                match (exact_p, exact_m) {
                    (true, true) => 2,
                    _ => 1,
                }
            });

            for binding in matched_bindings {
                for enhancer in &mut result {
                    if let Some(value) = binding.enhancers.get(&enhancer.id) {
                        let source = format!(
                            "user override: {} ({})",
                            path.display(),
                            profile.unwrap_or("all profiles")
                        );
                        if let Some(enabled) = value.enabled {
                            enhancer.enabled = enabled;
                            enhancer.binding_source = source.clone();
                        }
                        if let Some(prompt) = &value.prompt {
                            enhancer.prompt = prompt.clone();
                            enhancer.prompt_source = source;
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}

pub fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    let p = pattern.trim();
    let c = candidate.trim();
    if p.eq_ignore_ascii_case(c) || p == "*" {
        return true;
    }
    let p_lower = p.to_ascii_lowercase();
    let c_lower = c.to_ascii_lowercase();

    if p_lower.contains('*') {
        let parts: Vec<&str> = p_lower.split('*').collect();
        let mut remainder = &c_lower[..];
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() {
                continue;
            }
            if i == 0 {
                if !remainder.starts_with(part) {
                    return false;
                }
                remainder = &remainder[part.len()..];
            } else if i == parts.len() - 1 && !p_lower.ends_with('*') {
                return remainder.ends_with(part);
            } else {
                match remainder.find(part) {
                    Some(pos) => remainder = &remainder[pos + part.len()..],
                    None => return false,
                }
            }
        }
        true
    } else {
        c_lower.contains(&p_lower)
    }
}

pub fn builtin_copy(id: &str) -> Option<(&'static str, &'static str, String, String)> {
    let quirk = ModelQuirk::parse(id)?;
    let patch = PromptEnhancer::patch_for(quirk);
    let (title, name, description) = match quirk {
        ModelQuirk::OverPlanning => (
            "规划过多",
            "OverPlanning",
            "模型容易花大量时间规划，而不是直接执行。",
        ),
        ModelQuirk::StopsEarly => (
            "过早结束",
            "StopsEarly",
            "任务还没完成就停止。让它继续执行并检查，直到要求得到满足。",
        ),
        ModelQuirk::ReluctantToUseTools => (
            "不愿调用工具",
            "ReluctantToUseTools",
            "该用工具时只解释不执行。让它依据实际工具结果回答。",
        ),
        ModelQuirk::WeakVerification => (
            "验证不足",
            "WeakVerification",
            "修改后缺少检查就声称完成。让它先验证结果，再汇报完成情况。",
        ),
    };
    Some((
        title,
        name,
        description.into(),
        patch.instruction.to_string(),
    ))
}

fn binding_value<'a>(
    config: &'a EnhancerConfig,
    provider: &str,
    model: &str,
    preset: Option<&str>,
    id: &str,
) -> Option<&'a EnhancerOverride> {
    config
        .bindings
        .iter()
        .find(|b| {
            b.provider.eq_ignore_ascii_case(provider)
                && b.model.eq_ignore_ascii_case(model)
                && b.preset.as_deref() == preset
                && b.enhancers.contains_key(id)
        })
        .or_else(|| {
            config.bindings.iter().find(|b| {
                pattern_matches(&b.provider, provider)
                    && pattern_matches(&b.model, model)
                    && b.preset.as_deref() == preset
                    && b.enhancers.contains_key(id)
            })
        })
        .and_then(|b| b.enhancers.get(id))
}

fn resolved_enabled(
    config: &EnhancerConfig,
    provider: &str,
    model: &str,
    preset: &str,
    id: &str,
    quirk_on: bool,
) -> bool {
    let mut enabled = quirk_on;
    for profile in [None, Some(preset)] {
        if let Some(value) = binding_value(config, provider, model, profile, id) {
            if let Some(flag) = value.enabled {
                enabled = flag;
            }
        }
    }
    enabled
}

pub fn catalog(
    config: &EnhancerConfig,
    models: &[(String, String, Vec<ModelQuirk>)],
    presets: &[String],
) -> Vec<EnhancerCatalogItem> {
    let mut items = Vec::new();
    for quirk in QUIRKS {
        let (title, name, description, default_prompt) =
            builtin_copy(quirk.as_str()).expect("builtin quirk");
        items.push(summarize(
            config,
            models,
            presets,
            quirk.as_str(),
            name,
            title,
            description,
            default_prompt,
            "builtin",
        ));
    }
    for definition in &config.definitions {
        items.push(summarize(
            config,
            models,
            presets,
            &definition.id,
            &definition.name,
            &definition.name,
            definition.description.clone(),
            definition.prompt.clone(),
            "custom",
        ));
    }
    items
}

#[allow(clippy::too_many_arguments)]
fn summarize(
    config: &EnhancerConfig,
    models: &[(String, String, Vec<ModelQuirk>)],
    presets: &[String],
    id: &str,
    name: &str,
    title: &str,
    description: String,
    default_prompt: String,
    kind: &str,
) -> EnhancerCatalogItem {
    let quirk = ModelQuirk::parse(id);
    let mut bound = Vec::new();
    let mut prompt = default_prompt.clone();
    for (provider, model, quirks) in models {
        let quirk_on = quirk.is_some_and(|q| quirks.contains(&q));
        let model_on = presets
            .iter()
            .any(|preset| resolved_enabled(config, provider, model, preset, id, quirk_on));
        let bound_here = config
            .bindings
            .iter()
            .any(|b| b.provider == *provider && b.model == *model && b.enhancers.contains_key(id));
        if model_on || bound_here {
            bound.push(BoundModel {
                provider: provider.clone(),
                model: model.clone(),
            });
        }
        for binding in config.bindings.iter().filter(|b| {
            b.provider == *provider && b.model == *model && b.enhancers.contains_key(id)
        }) {
            if let Some(text) = &binding.enhancers[id].prompt {
                prompt = text.clone();
            }
        }
    }
    let has_specific = config
        .bindings
        .iter()
        .any(|b| b.preset.is_some() && b.enhancers.contains_key(id));
    let has_general = config
        .bindings
        .iter()
        .any(|b| b.preset.is_none() && b.enhancers.contains_key(id));
    let all_agents = !has_specific || has_general;
    let agents = if all_agents {
        Vec::new()
    } else {
        config
            .bindings
            .iter()
            .filter_map(|b| {
                b.preset.clone().filter(|_| match b.enhancers.get(id) {
                    Some(override_) => {
                        // Built-ins write an explicit `enabled: false` entry for
                        // every unselected preset, so only non-disabled entries
                        // mark the scope. Custom enhancers only record their
                        // selected scope, and the scope must survive a global
                        // disable so re-enabling cannot silently widen it.
                        kind != "builtin" || override_.enabled != Some(false)
                    }
                    None => false,
                })
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    EnhancerCatalogItem {
        id: id.into(),
        name: name.into(),
        title: title.into(),
        description,
        kind: kind.into(),
        enabled: models.iter().any(|(provider, model, quirks)| {
            let quirk_on = quirk.is_some_and(|q| quirks.contains(&q));
            presets
                .iter()
                .any(|preset| resolved_enabled(config, provider, model, preset, id, quirk_on))
        }),
        prompt,
        default_prompt,
        models: bound,
        agents,
        all_agents,
    }
}

pub fn explain(
    config: &EnhancerConfig,
    models: &[(String, String, Vec<ModelQuirk>)],
    presets: &[String],
    provider: &str,
    model: &str,
    preset: &str,
    quirks: &[ModelQuirk],
) -> Vec<EnhancerMatch> {
    catalog(config, models, presets)
        .into_iter()
        .map(|item| {
            let quirk_on = ModelQuirk::parse(&item.id).is_some_and(|q| quirks.contains(&q));
            let active = resolved_enabled(config, provider, model, preset, &item.id, quirk_on);
            let model_listed = item
                .models
                .iter()
                .any(|m| m.provider == provider && m.model == model);
            let agent_ok = item.all_agents || item.agents.iter().any(|a| a == preset);
            let mut reasons = Vec::new();
            if active {
                reasons.push(format!("当前模型 {model} 在适用模型中"));
                reasons.push(if item.all_agents {
                    format!("当前 Agent {preset} 在适用 Agent 范围中（所有 Agent）")
                } else {
                    format!("当前 Agent {preset} 在适用 Agent 范围中")
                });
            } else if item.models.is_empty() {
                reasons.push("当前模型未绑定".into());
            } else if !item.enabled {
                reasons.push("该增强已关闭".into());
            } else if !model_listed {
                reasons.push("当前模型未绑定".into());
            } else if !agent_ok {
                reasons.push(format!("当前 Agent {preset} 不在适用范围内"));
            } else {
                reasons.push("当前组合未命中该增强".into());
            }
            EnhancerMatch {
                id: item.id,
                name: item.name,
                title: item.title,
                kind: item.kind,
                active,
                reasons,
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn apply_enhancer(
    config: &mut EnhancerConfig,
    id: &str,
    definition: Option<EnhancerDefinition>,
    enabled: bool,
    models: &[BoundModel],
    presets: Option<&[String]>,
    all_presets: &[String],
    prompt: Option<String>,
) -> Result<(), String> {
    if let Some(definition) = definition {
        validate_custom_id(&definition.id)?;
        if definition.id != id {
            return Err("definition id must match enhancer id".into());
        }
        if is_builtin_id(id) {
            return Err("built-in enhancers cannot change id or be stored as definitions".into());
        }
        if definition.name.trim().is_empty() || definition.prompt.trim().is_empty() {
            return Err("name and behavior guidance are required".into());
        }
        if let Some(existing) = config.definitions.iter_mut().find(|d| d.id == id) {
            *existing = definition;
        } else {
            config.definitions.push(definition);
        }
    } else if !is_builtin_id(id) && !config.definitions.iter().any(|d| d.id == id) {
        return Err(format!("unknown enhancer: {id}"));
    }
    let default_prompt = if is_builtin_id(id) {
        builtin_copy(id).map(|c| c.3).unwrap_or_default()
    } else {
        config
            .definitions
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.prompt.clone())
            .unwrap_or_default()
    };
    let prompt =
        prompt.filter(|text| text.trim() != default_prompt.trim() && !text.trim().is_empty());
    for binding in &mut config.bindings {
        binding.enhancers.remove(id);
    }
    config.bindings.retain(|b| !b.enhancers.is_empty());
    // Each profile is (preset scope, scope selected, effectively enabled).
    let profiles: Vec<(Option<String>, bool, bool)> = match presets {
        None | Some([]) => vec![(None, true, enabled)],
        Some(list) => all_presets
            .iter()
            .map(|preset| {
                let selected = list.iter().any(|p| p == preset);
                (Some(preset.clone()), selected, enabled && selected)
            })
            .collect(),
    };
    for BoundModel { provider, model } in models {
        for (preset, selected, flag) in &profiles {
            // Built-ins write an explicit entry for every preset so a scoped
            // override can turn a default off. Custom enhancers only record the
            // selected scope, and a disabled enhancer still keeps its bindings so
            // toggling it off does not erase the model/agent configuration.
            if !is_builtin_id(id) && !*selected {
                continue;
            }
            let index = config
                .bindings
                .iter()
                .position(|b| b.provider == *provider && b.model == *model && b.preset == *preset);
            let index = index.unwrap_or_else(|| {
                config.bindings.push(Binding {
                    provider: provider.clone(),
                    model: model.clone(),
                    preset: preset.clone(),
                    enhancers: Default::default(),
                });
                config.bindings.len() - 1
            });
            config.bindings[index].enhancers.insert(
                id.to_string(),
                EnhancerOverride {
                    enabled: Some(*flag),
                    prompt: prompt.clone(),
                },
            );
        }
    }
    Ok(())
}

pub fn delete_custom(config: &mut EnhancerConfig, id: &str) -> Result<(), String> {
    if is_builtin_id(id) {
        return Err("built-in enhancers cannot be deleted".into());
    }
    let before = config.definitions.len();
    config.definitions.retain(|d| d.id != id);
    if config.definitions.len() == before {
        return Err(format!("unknown custom enhancer: {id}"));
    }
    for binding in &mut config.bindings {
        binding.enhancers.remove(id);
    }
    config.bindings.retain(|b| !b.enhancers.is_empty());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(
        provider: &str,
        id: &str,
        quirks: Vec<ModelQuirk>,
    ) -> (String, String, Vec<ModelQuirk>) {
        (provider.into(), id.into(), quirks)
    }

    #[test]
    fn old_files_without_definitions_still_parse() {
        let config = parse(r#"{"version":1,"bindings":[{"provider":"gemini","model":"gemini-2.5-pro","enhancers":{"stops_early":{"enabled":true}}}]}"#).unwrap();
        assert!(config.definitions.is_empty());
        assert_eq!(config.bindings.len(), 1);
    }

    #[test]
    fn unknown_enhancer_without_definition_is_rejected() {
        let err = parse(r#"{"version":1,"bindings":[{"provider":"gemini","model":"g","enhancers":{"ParallelToolCalls":{"enabled":true}}}]}"#).unwrap_err();
        assert!(err.contains("unknown enhancer"));
    }

    #[test]
    fn custom_definition_roundtrip_and_resolve() {
        let mut config = EnhancerConfig::default();
        apply_enhancer(
            &mut config,
            "ParallelToolCalls",
            Some(EnhancerDefinition {
                id: "ParallelToolCalls".into(),
                name: "并行工具调用".into(),
                description: "独立工具优先并行。".into(),
                prompt: "Prefer parallel tool calls when operations are independent.".into(),
            }),
            true,
            &[BoundModel {
                provider: "anthropic".into(),
                model: "claude-sonnet-4".into(),
            }],
            None,
            &["code".into(), "general".into()],
            None,
        )
        .unwrap();
        let text = serde_json::to_string(&config).unwrap();
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.definitions[0].id, "ParallelToolCalls");
        let dir = std::env::temp_dir().join(format!("one-enhancer-def-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::write(dir.join("prompts/enhancers.json"), text).unwrap();
        let resolved = resolve_paths(
            vec![dir.join("prompts/enhancers.json")],
            "anthropic",
            "claude-sonnet-4",
            "code",
            &[],
        )
        .unwrap();
        let custom = resolved
            .iter()
            .find(|e| e.id == "ParallelToolCalls")
            .unwrap();
        assert!(custom.enabled);
        assert!(custom.prompt.contains("parallel tool calls"));
        let miss = resolve_paths(
            vec![dir.join("prompts/enhancers.json")],
            "gemini",
            "gemini-2.5-pro",
            "code",
            &[],
        )
        .unwrap();
        assert!(
            !miss
                .iter()
                .find(|e| e.id == "ParallelToolCalls")
                .unwrap()
                .enabled
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn disabling_custom_enhancer_keeps_bindings() {
        let mut config = EnhancerConfig::default();
        let models = [BoundModel {
            provider: "gemini".into(),
            model: "gemini-2.5-pro".into(),
        }];
        let presets = ["code".to_string(), "general".to_string()];
        apply_enhancer(
            &mut config,
            "ParallelToolCalls",
            Some(EnhancerDefinition {
                id: "ParallelToolCalls".into(),
                name: "并行工具调用".into(),
                description: "d".into(),
                prompt: "Prefer parallel tool calls.".into(),
            }),
            true,
            &models,
            Some(&["code".into()]),
            &presets,
            None,
        )
        .unwrap();
        assert_eq!(config.bindings.len(), 1);
        assert_eq!(config.bindings[0].preset.as_deref(), Some("code"));
        apply_enhancer(
            &mut config,
            "ParallelToolCalls",
            None,
            false,
            &models,
            Some(&["code".into()]),
            &presets,
            None,
        )
        .unwrap();
        assert_eq!(config.bindings.len(), 1, "disable must keep model bindings");
        assert_eq!(
            config.bindings[0].enhancers["ParallelToolCalls"].enabled,
            Some(false)
        );
        assert_eq!(config.bindings[0].preset.as_deref(), Some("code"));
        let items = catalog(
            &config,
            &[model("gemini", "gemini-2.5-pro", vec![])],
            &presets,
        );
        let custom = items.iter().find(|i| i.id == "ParallelToolCalls").unwrap();
        assert!(!custom.enabled);
        assert_eq!(custom.models.len(), 1, "bound model must stay visible");
        assert!(!custom.all_agents);
        assert_eq!(
            custom.agents,
            vec!["code".to_string()],
            "agent scope must survive disable so re-enable cannot widen it"
        );
        let matches = explain(
            &config,
            &[model("gemini", "gemini-2.5-pro", vec![])],
            &presets,
            "gemini",
            "gemini-2.5-pro",
            "code",
            &[],
        );
        let hit = matches
            .iter()
            .find(|m| m.id == "ParallelToolCalls")
            .unwrap();
        assert!(!hit.active);
        assert!(hit.reasons.iter().any(|r| r.contains("已关闭")));
    }

    #[test]
    fn builtin_cannot_be_deleted_and_custom_can() {
        let mut config = EnhancerConfig::default();
        assert!(delete_custom(&mut config, "stops_early").is_err());
        apply_enhancer(
            &mut config,
            "ParallelToolCalls",
            Some(EnhancerDefinition {
                id: "ParallelToolCalls".into(),
                name: "并行工具调用".into(),
                description: "d".into(),
                prompt: "Prefer parallel tool calls.".into(),
            }),
            true,
            &[BoundModel {
                provider: "gemini".into(),
                model: "gemini-2.5-pro".into(),
            }],
            Some(&["code".into()]),
            &["code".into(), "general".into()],
            None,
        )
        .unwrap();
        let items = catalog(
            &config,
            &[model("gemini", "gemini-2.5-pro", vec![])],
            &["code".into(), "general".into()],
        );
        let custom = items.iter().find(|i| i.id == "ParallelToolCalls").unwrap();
        assert!(custom.enabled);
        assert!(!custom.all_agents);
        assert_eq!(custom.agents, vec!["code".to_string()]);
        let matches = explain(
            &config,
            &[model("gemini", "gemini-2.5-pro", vec![])],
            &["code".into(), "general".into()],
            "gemini",
            "gemini-2.5-pro",
            "code",
            &[],
        );
        let hit = matches
            .iter()
            .find(|m| m.id == "ParallelToolCalls")
            .unwrap();
        assert!(hit.active);
        let miss = explain(
            &config,
            &[model("gemini", "gemini-2.5-pro", vec![])],
            &["code".into(), "general".into()],
            "gemini",
            "gemini-2.5-pro",
            "general",
            &[],
        );
        assert!(
            !miss
                .iter()
                .find(|m| m.id == "ParallelToolCalls")
                .unwrap()
                .active
        );
        delete_custom(&mut config, "ParallelToolCalls").unwrap();
        assert!(config.definitions.is_empty());
        assert!(config
            .bindings
            .iter()
            .all(|b| !b.enhancers.contains_key("ParallelToolCalls")));
    }

    #[test]
    fn pattern_matches_wildcards_and_substrings() {
        assert!(pattern_matches("*", "gemini-3.8-flash-high"));
        assert!(pattern_matches("gemini*", "gemini-3.8-flash-high"));
        assert!(pattern_matches("gemini-3.*", "gemini-3.8-flash-high"));
        assert!(!pattern_matches("gemini-3.7*", "gemini-3.8-flash-high"));
        assert!(pattern_matches("*flash*", "gemini-3.8-flash-high"));
        assert!(pattern_matches("gemini", "gemini-3.8-flash-high"));
        assert!(pattern_matches("cpa", "CPA"));
    }
}
