//! Single entry point for system-prompt assembly (features + resources + mode).

use std::path::Path;

use one_resources::ResourceLoader;
use one_tools::plan_mode_system_overlay;

use super::features::FeatureState;
use super::task_tool::TASK_TOOL_PROMPT_HINT;
use super::AgentMode;

/// Inputs for composing the live system prompt.
pub struct PromptComposeInput<'a> {
    pub features: &'a FeatureState,
    pub resources: &'a ResourceLoader,
    pub mode: AgentMode,
    pub plan_path: Option<&'a Path>,
    /// Whether spawn_policy allows children (independent of feature flag).
    pub can_spawn: bool,
}

/// Base system prompt **without** plan-mode overlay.
///
/// Order:
/// 1. `DEFAULT_SYSTEM_PROMPT` (core role + tool policy; no feature packages)
/// 2. AGENTS.md / skills catalog / plugin+ext append (`ResourceLoader`)
/// 3. Feature sections (subagent when enabled + can_spawn)
pub fn compose_base_system_prompt(
    features: &FeatureState,
    resources: &ResourceLoader,
    can_spawn: bool,
) -> String {
    let mut base =
        resources.build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);
    if features.subagent_enabled() && can_spawn {
        base.push('\n');
        base.push_str(TASK_TOOL_PROMPT_HINT);
    }
    base
}

/// Full system prompt for the current mode (base + optional plan overlay).
pub fn compose_system_prompt(input: PromptComposeInput<'_>) -> String {
    let base = compose_base_system_prompt(
        input.features,
        input.resources,
        input.can_spawn,
    );
    if input.mode == AgentMode::Plan {
        if let Some(path) = input.plan_path {
            return format!("{}{}", base, plan_mode_system_overlay(path));
        }
    }
    base
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

    #[test]
    fn subagent_section_only_when_enabled() {
        let resources = empty_resources();
        let on = FeatureState::default();
        let prompt_on = compose_base_system_prompt(&on, &resources, true);
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
        let prompt_off = compose_base_system_prompt(&off, &resources, true);
        assert!(
            !prompt_off.contains("`task` tool"),
            "disabled feature must omit task section"
        );
    }

    #[test]
    fn can_spawn_false_omits_subagent_even_if_feature_on() {
        let resources = empty_resources();
        let on = FeatureState::default();
        let prompt = compose_base_system_prompt(&on, &resources, false);
        assert!(!prompt.contains("`task` tool"));
    }
}
