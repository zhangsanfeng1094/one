//! Single entry point for system-prompt assembly (features + resources + mode).

use std::path::Path;

use one_resources::ResourceLoader;
use one_tools::plan_mode_system_overlay;

use super::features::FeatureState;
use super::task_tool::TASK_TOOL_PROMPT_HINT;
use super::AgentMode;

/// Short write policy when feature `memory` is on and L2 catalog is present.
pub const MEMORY_WRITE_PROMPT_HINT: &str = "\
## Memory write & Self-Learning Tool Intent

Use `memory_write` to persist cross-session notes (atomic body + MEMORY.md index). \
Default NO-OP — only when a future agent would clearly benefit. Prefer updating an \
existing id after `memory_search`. Do not use raw `write` under memory dirs unless \
`memory_write` is unavailable. New L2 index lines apply after `/reload` or a new session.

### Tool Intent Self-Learning (自学习工具意图):
When the user teaches a tool preference, corrects tool choice (e.g., 'for open source library docs use deepwiki', 'search web for real-time news'), or when you discover an effective tool mapping:
- Automatically call `memory_write` with `type=\"tool_intent\"` (e.g. `id=\"tool-intent-<name>\"`, `scope=\"global\"`, `tags=\"<intent keywords>\"`, `description=\"<intent> -> <tool>\"`, `body=\"trigger condition and concrete action\"`).
- When a `<system-reminder>` with `[Learned Tool Intent Rule]` is injected into your turn, you must proactively apply it by searching or invoking the indicated tool.
";

/// One-style output guidance (always injected for high quality)
pub const ONE_OUTPUT_GUIDE: &str = r#"

## One Output Style (始终生效)

- 写完整、专业、结构化的技术文章风格
- 使用 **bold**、`inline code`、`### 标题`、`表格` 自然且丰富
- 解释为什么这样做（而非只给出结果）
- 保持简洁但信息丰富
- 最终输出用清晰的 Markdown 格式
- 优先使用 bullet lists 和 numbered lists
"#;

/// Inputs for composing the live system prompt.
pub struct PromptComposeInput<'a> {
    pub features: &'a FeatureState,
    pub resources: &'a ResourceLoader,
    pub mode: AgentMode,
    pub plan_path: Option<&'a Path>,
    /// Whether spawn_policy allows children (independent of feature flag).
    pub can_spawn: bool,
    pub env_context: Option<&'a str>,
    pub memory_catalog: Option<&'a str>,
}

/// Shared pieces for base system prompt (no plan overlay).
pub struct ComposeBaseInput<'a> {
    pub features: &'a FeatureState,
    pub resources: &'a ResourceLoader,
    pub can_spawn: bool,
    pub env_context: Option<&'a str>,
    pub memory_catalog: Option<&'a str>,
}

/// Base system prompt **without** plan-mode overlay.
///
/// Order:
/// 1. `DEFAULT_SYSTEM_PROMPT` (core role + tool policy; no feature packages)
/// 2. AGENTS.md / skills catalog / plugin+ext append (`ResourceLoader`)
/// 3. Environment snapshot (cwd / git / date) — session-frozen
/// 4. Memory L2 catalog — session-frozen progressive disclosure
/// 5. Feature sections (subagent when enabled + can_spawn)
pub fn compose_base_system_prompt(input: ComposeBaseInput<'_>) -> String {
    let mut base = input
        .resources
        .build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);
    if let Some(env) = input.env_context.map(str::trim).filter(|s| !s.is_empty()) {
        base.push_str("\n\n");
        base.push_str(env);
    }
    if let Some(mem) = input
        .memory_catalog
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        base.push_str("\n\n");
        base.push_str(mem);
    }
    if input.features.subagent_enabled() && input.can_spawn {
        base.push('\n');
        base.push_str(TASK_TOOL_PROMPT_HINT);
    }
    // Feature `memory` on + L2 catalog present → write discipline for memory_write tool.
    if input.features.memory_enabled()
        && input
            .memory_catalog
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
    {
        base.push('\n');
        base.push_str(MEMORY_WRITE_PROMPT_HINT);
    }

    // One output style — always injected for high-quality responses
    base.push_str(ONE_OUTPUT_GUIDE);
    base
}

/// Full system prompt for the current mode (base + optional plan overlay).
pub fn compose_system_prompt(input: PromptComposeInput<'_>) -> String {
    let base = compose_base_system_prompt(ComposeBaseInput {
        features: input.features,
        resources: input.resources,
        can_spawn: input.can_spawn,
        env_context: input.env_context,
        memory_catalog: input.memory_catalog,
    });
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
