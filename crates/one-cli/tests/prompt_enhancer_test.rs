//! Tests for ModelBehaviorProfile, PromptEnhancer, multi-profile and multi-quirk composability.

use one_ai::registry::ModelQuirk;
use one_cli::prompt_config::PromptConfig;
use one_cli::runtime::prompt_compose::{compile_host, HostPromptInput};
use one_cli::runtime::prompt_enhancer::PromptEnhancer;
use one_prompt::{ComponentRegistry, ModelRule, PromptSpec, SlotOperation};
use std::path::Path;

#[test]
fn test_acceptance_1_different_prompt_profiles_accept_same_enhancers() {
    let registry = ComponentRegistry::with_builtins();

    // 1. Code profile (has style, tools, role, behavior_hooks)
    let mut code_spec = PromptSpec {
        preset: "code".into(),
        ..Default::default()
    };
    let code_patches =
        PromptEnhancer::enhance(&mut code_spec, &registry, &[ModelQuirk::OverPlanning]);
    assert_eq!(code_patches.len(), 1);
    let code_compiled = one_prompt::compile(
        &code_spec,
        &registry,
        &one_prompt::CompileContext {
            mode: "act".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(code_compiled.text.contains("Execution over speculation"));

    // 2. General profile (has general_role, general_subagent, behavior_hooks, but NO tools/style slots)
    let mut general_spec = PromptSpec {
        preset: "general".into(),
        ..Default::default()
    };
    let general_patches =
        PromptEnhancer::enhance(&mut general_spec, &registry, &[ModelQuirk::OverPlanning]);
    assert_eq!(general_patches.len(), 1);
    let general_compiled = one_prompt::compile(
        &general_spec,
        &registry,
        &one_prompt::CompileContext {
            mode: "act".into(),
            ..Default::default()
        },
    )
    .unwrap();
    // Verify both different profiles accepted the same behavior enhancer
    assert!(general_compiled.text.contains("Execution over speculation"));
}

#[test]
fn test_acceptance_2_multi_quirks_composition_without_overwrite() {
    let registry = ComponentRegistry::with_builtins();
    let mut spec = PromptSpec {
        preset: "code".into(),
        ..Default::default()
    };

    let quirks = vec![ModelQuirk::OverPlanning, ModelQuirk::WeakVerification];
    let patches = PromptEnhancer::enhance(&mut spec, &registry, &quirks);
    assert_eq!(patches.len(), 2);

    let compiled = one_prompt::compile(
        &spec,
        &registry,
        &one_prompt::CompileContext {
            mode: "act".into(),
            ..Default::default()
        },
    )
    .unwrap();

    // Both guidance must exist simultaneously in the compiled prompt
    assert!(compiled.text.contains("Execution over speculation"));
    assert!(compiled.text.contains("Mandatory verification"));
}

#[test]
fn test_acceptance_3_model_switch_quirks_recalculation() {
    let cwd = Path::new(".");
    let prompt = PromptConfig::default();

    // Compile with a model that has quirks
    let compiled_quirky = compile_host(HostPromptInput {
        prompt: &prompt,
        cwd,
        provider: "test-provider",
        model: "quirky-model",
        mode: one_cli::runtime::AgentMode::Act,
        plan_path: None,
        tool_names: vec!["read".into()],
        resources: "",
        env_context: None,
        memory_catalog: None,
        quirks: vec![ModelQuirk::StopsEarly, ModelQuirk::ReluctantToUseTools],
    })
    .unwrap();
    assert!(compiled_quirky
        .text
        .contains("Persistence: Do not terminate"));
    assert!(compiled_quirky
        .text
        .contains("Mandatory tool usage: Never guess"));

    // Switch to a clean model without quirks
    let compiled_clean = compile_host(HostPromptInput {
        prompt: &prompt,
        cwd,
        provider: "test-provider",
        model: "clean-model",
        mode: one_cli::runtime::AgentMode::Act,
        plan_path: None,
        tool_names: vec!["read".into()],
        resources: "",
        env_context: None,
        memory_catalog: None,
        quirks: vec![],
    })
    .unwrap();
    // Old quirks must NOT leak into the clean model's compiled prompt
    assert!(!compiled_clean
        .text
        .contains("Persistence: Do not terminate"));
    assert!(!compiled_clean
        .text
        .contains("Mandatory tool usage: Never guess"));
}

#[test]
fn test_acceptance_4_subagent_independent_model_quirks() {
    let cwd = Path::new(".");
    let parent_prompt_cfg = PromptConfig::default();
    let child_prompt_cfg = PromptConfig::default();

    // 1. Parent uses a model with OverPlanning
    let parent_compiled = compile_host(HostPromptInput {
        prompt: &parent_prompt_cfg,
        cwd,
        provider: "openai",
        model: "gpt-4o",
        mode: one_cli::runtime::AgentMode::Act,
        plan_path: None,
        tool_names: vec!["task".into()],
        resources: "",
        env_context: None,
        memory_catalog: None,
        quirks: vec![ModelQuirk::OverPlanning],
    })
    .unwrap();

    // 2. Child agent (e.g. explore/research subagent) uses a different model with WeakVerification
    let child_compiled = compile_host(HostPromptInput {
        prompt: &child_prompt_cfg,
        cwd,
        provider: "anthropic",
        model: "claude-3-5-haiku",
        mode: one_cli::runtime::AgentMode::Act,
        plan_path: None,
        tool_names: vec!["read".into(), "grep".into()],
        resources: "",
        env_context: None,
        memory_catalog: None,
        quirks: vec![ModelQuirk::WeakVerification],
    })
    .unwrap();

    // Parent has OverPlanning, but NOT WeakVerification
    assert!(parent_compiled.text.contains("Execution over speculation"));
    assert!(!parent_compiled.text.contains("Mandatory verification"));

    // Child has WeakVerification, but NOT OverPlanning
    assert!(child_compiled.text.contains("Mandatory verification"));
    assert!(!child_compiled.text.contains("Execution over speculation"));
}

#[test]
fn test_acceptance_5_legacy_model_rule_compatibility_preserved() {
    let registry = ComponentRegistry::with_builtins();
    let mut spec = PromptSpec {
        preset: "code".into(),
        rules: vec![ModelRule {
            provider: "custom-provider".into(),
            model: "custom-model".into(),
            operations: vec![SlotOperation::append(
                "extra",
                "\nCustom Legacy Rule Text\n",
            )],
        }],
        ..Default::default()
    };

    // Enhance with a quirk as well
    PromptEnhancer::enhance(&mut spec, &registry, &[ModelQuirk::OverPlanning]);

    let compiled = one_prompt::compile(
        &spec,
        &registry,
        &one_prompt::CompileContext {
            provider: "custom-provider".into(),
            model: "custom-model".into(),
            mode: "act".into(),
            ..Default::default()
        },
    )
    .unwrap();

    // Both legacy rule and new enhancer work seamlessly together
    assert!(compiled.text.contains("Custom Legacy Rule Text"));
    assert!(compiled.text.contains("Execution over speculation"));
}
