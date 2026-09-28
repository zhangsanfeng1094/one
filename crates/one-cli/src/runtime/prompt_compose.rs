//! One host adapter for the standalone prompt compiler.
#[cfg(test)]
use super::features::FeatureState;
use super::AgentMode;

use one_prompt::{CompileContext, CompiledPrompt, ComponentRegistry, PromptError};
#[cfg(test)]
use one_resources::ResourceLoader;
use std::path::Path;

pub struct HostPromptInput<'a> {
    pub prompt: &'a crate::prompt_config::PromptConfig,
    pub cwd: &'a Path,
    pub provider: &'a str,
    pub model: &'a str,
    pub mode: AgentMode,
    pub plan_path: Option<&'a Path>,
    pub tool_names: Vec<String>,
    pub resources: &'a str,
    pub env_context: Option<&'a str>,
    pub memory_catalog: Option<&'a str>,
    pub quirks: Vec<one_ai::registry::ModelQuirk>,
}
pub fn compile_host(input: HostPromptInput<'_>) -> Result<CompiledPrompt, PromptError> {
    compile_host_with_agent_dir(input, &one_session::agent_dir())
}

/// Explicit host paths for Config Studio and isolated runtime tests.
pub fn compile_host_with_agent_dir(
    input: HostPromptInput<'_>,
    agent_dir: &Path,
) -> Result<CompiledPrompt, PromptError> {
    compile_host_resolved(input, agent_dir).map(|(compiled, _)| compiled)
}

/// Return the exact enhancer resolution used in this compilation for inspection.
pub fn compile_host_resolved(
    input: HostPromptInput<'_>,
    agent_dir: &Path,
) -> Result<
    (
        CompiledPrompt,
        Vec<super::prompt_enhancer::config::ResolvedEnhancer>,
    ),
    PromptError,
> {
    compile_host_scoped(input, agent_dir, false)
}

pub fn compile_host_scoped(
    input: HostPromptInput<'_>,
    agent_dir: &Path,
    global_only: bool,
) -> Result<
    (
        CompiledPrompt,
        Vec<super::prompt_enhancer::config::ResolvedEnhancer>,
    ),
    PromptError,
> {
    let mut spec = input.prompt.load(input.cwd)?;
    let registry = ComponentRegistry::with_builtins();

    // Dynamically inject behavioral patches based on current model quirks.
    let sources = if global_only {
        vec![agent_dir.join(super::prompt_enhancer::config::RELATIVE_PATH)]
    } else {
        super::prompt_enhancer::config::paths(input.cwd, agent_dir)
    };
    let enhancers = super::prompt_enhancer::config::resolve_paths(
        sources,
        input.provider,
        input.model,
        &spec.preset,
        &input.quirks,
    )?;
    super::prompt_enhancer::PromptEnhancer::apply_resolved(&mut spec, &registry, &enhancers);

    let mut context = CompileContext {
        provider: input.provider.into(),
        model: input.model.into(),
        mode: input.mode.as_str().into(),
        capabilities: input.tool_names.into_iter().collect(),
        ..CompileContext::default()
    };
    context
        .variables
        .insert("resources".into(), input.resources.into());
    let section = |s: Option<&str>| {
        s.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("\n\n{s}"))
            .unwrap_or_default()
    };
    context
        .variables
        .insert("environment".into(), section(input.env_context));
    context
        .variables
        .insert("memory_catalog".into(), section(input.memory_catalog));
    if input.memory_catalog.is_some_and(|m| !m.trim().is_empty()) {
        context.capabilities.insert("memory".into());
    }
    if let Some(path) = input.plan_path.filter(|_| input.mode == AgentMode::Plan) {
        context.capabilities.insert("plan".into());
        context
            .variables
            .insert("plan_path".into(), path.display().to_string());
    }
    one_prompt::compile(&spec, &registry, &context).map(|compiled| (compiled, enhancers))
}

// Legacy behavior assertions exercise this test-only adapter. Production callers
// always pass the materialized tools and actual provider through compile_host.
#[cfg(test)]
pub struct PromptComposeInput<'a> {
    pub features: &'a FeatureState,
    pub resources: &'a ResourceLoader,
    pub mode: AgentMode,
    pub plan_path: Option<&'a Path>,
    pub can_spawn: bool,
    pub env_context: Option<&'a str>,
    pub memory_catalog: Option<&'a str>,
}
#[cfg(test)]
pub struct ComposeBaseInput<'a> {
    pub features: &'a FeatureState,
    pub resources: &'a ResourceLoader,
    pub can_spawn: bool,
    pub env_context: Option<&'a str>,
    pub memory_catalog: Option<&'a str>,
}
#[cfg(test)]
pub fn compose_base_system_prompt(input: ComposeBaseInput<'_>) -> String {
    compose_system_prompt(PromptComposeInput {
        features: input.features,
        resources: input.resources,
        mode: AgentMode::Act,
        plan_path: None,
        can_spawn: input.can_spawn,
        env_context: input.env_context,
        memory_catalog: input.memory_catalog,
    })
}
#[cfg(test)]
pub fn compose_system_prompt(input: PromptComposeInput<'_>) -> String {
    let mut tools = vec!["monitor".into()];
    if input.features.subagent_enabled() && input.can_spawn {
        tools.push("task".into());
    }
    if input.features.memory_enabled() {
        tools.push("memory_write".into());
    }
    compile_host(HostPromptInput {
        prompt: &Default::default(),
        cwd: &input.resources.cwd,
        provider: "test",
        model: "test",
        mode: input.mode,
        plan_path: input.plan_path,
        tool_names: tools,
        resources: &input.resources.build_system_prompt(""),
        env_context: input.env_context,
        memory_catalog: input.memory_catalog,
        quirks: vec![],
    })
    .unwrap()
    .text
}

#[cfg(test)]
pub fn compose_system_prompt_with_quirks(
    input: PromptComposeInput<'_>,
    quirks: Vec<one_ai::registry::ModelQuirk>,
) -> String {
    let mut tools = vec!["monitor".into()];
    if input.features.subagent_enabled() && input.can_spawn {
        tools.push("task".into());
    }
    if input.features.memory_enabled() {
        tools.push("memory_write".into());
    }
    compile_host(HostPromptInput {
        prompt: &Default::default(),
        cwd: &input.resources.cwd,
        provider: "test",
        model: "test",
        mode: input.mode,
        plan_path: input.plan_path,
        tool_names: tools,
        resources: &input.resources.build_system_prompt(""),
        env_context: input.env_context,
        memory_catalog: input.memory_catalog,
        quirks,
    })
    .unwrap()
    .text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::features::{FeatureState, FEATURE_SUBAGENT};
    use crate::settings::Settings;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn empty_resources() -> ResourceLoader {
        ResourceLoader {
            cwd: PathBuf::from("/tmp"),
            agent_dir: PathBuf::from("/tmp"),
            agents_files: vec![],
            skills: vec![],
            prompts: vec![],
            system_append: None,
        }
    }

    fn base(
        features: &FeatureState,
        resources: &ResourceLoader,
        can_spawn: bool,
        env: Option<&str>,
        mem: Option<&str>,
    ) -> String {
        compose_base_system_prompt(ComposeBaseInput {
            features,
            resources,
            can_spawn,
            env_context: env,
            memory_catalog: mem,
        })
    }

    #[test]
    fn subagent_section_only_when_enabled() {
        let resources = empty_resources();
        let on = FeatureState::default();
        let prompt_on = base(&on, &resources, true, None, None);
        assert!(
            prompt_on.contains("`task` tool"),
            "enabled feature should include task policy"
        );
        assert!(
            prompt_on.contains("wait_tasks"),
            "full TASK_TOOL_PROMPT_HINT should be attached"
        );

        let mut settings = Settings::default();
        let mut m = HashMap::new();
        m.insert(FEATURE_SUBAGENT.into(), false);
        settings.features = Some(m);
        let off = FeatureState::from_settings(&settings);
        let prompt_off = base(&off, &resources, true, None, None);
        assert!(
            !prompt_off.contains("`task` tool"),
            "disabled feature must omit task section"
        );
    }

    #[test]
    fn can_spawn_false_omits_subagent_even_if_feature_on() {
        let resources = empty_resources();
        let on = FeatureState::default();
        let prompt = base(&on, &resources, false, None, None);
        assert!(!prompt.contains("`task` tool"));
    }

    #[test]
    fn injects_env_and_memory_sections() {
        let resources = empty_resources();
        let on = FeatureState::default();
        let prompt = base(
            &on,
            &resources,
            false,
            Some("## Environment\n<env>\ncwd: /x\n</env>"),
            Some("## Memory (L2 index)\n<memory-catalog></memory-catalog>"),
        );
        assert!(prompt.contains("<env>"));
        assert!(prompt.contains("cwd: /x"));
        assert!(prompt.contains("<memory-catalog>"));
        assert!(
            prompt.contains("memory_write"),
            "memory feature + L2 catalog should add write hint"
        );
    }

    #[test]
    fn memory_write_hint_omitted_without_catalog() {
        let resources = empty_resources();
        let on = FeatureState::default();
        let prompt = base(&on, &resources, false, None, None);
        assert!(
            !prompt.contains("## Memory write"),
            "no L2 catalog → no memory_write section"
        );
    }

    #[test]
    fn memory_write_hint_off_when_feature_disabled() {
        let resources = empty_resources();
        let mut settings = Settings::default();
        let mut m = HashMap::new();
        m.insert(crate::runtime::features::FEATURE_MEMORY.into(), false);
        settings.features = Some(m);
        let off = FeatureState::from_settings(&settings);
        let prompt = base(
            &off,
            &resources,
            false,
            None,
            Some("## Memory (L2 index)\n<memory-catalog></memory-catalog>"),
        );
        assert!(!prompt.contains("## Memory write"));
    }
}
