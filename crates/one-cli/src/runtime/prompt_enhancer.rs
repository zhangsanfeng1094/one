//! ModelBehaviorProfile and PromptEnhancer for dynamic model quirk compensation.
//!
//! Bridges model post-training / inference tendencies (e.g. OverPlanning, StopsEarly,
//! ReluctantToUseTools, WeakVerification) into clean prompt patches that attach
//! to stable behavior hooks or standard slots across diverse PromptProfiles.

use one_ai::registry::ModelQuirk;
use one_prompt::{PromptSpec, SlotOperation};

/// A synthesized model behavior patch emitted by an enhancer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BehaviorPatch {
    pub quirk: ModelQuirk,
    pub title: &'static str,
    pub instruction: &'static str,
}

/// PromptEnhancer generates composable, delta-only behavioral patches
/// for known model quirks without requiring duplicated full prompts.
pub struct PromptEnhancer;

impl PromptEnhancer {
    /// Return the canonical behavior patch for a specific model quirk.
    pub fn patch_for(quirk: ModelQuirk) -> BehaviorPatch {
        match quirk {
            ModelQuirk::OverPlanning => BehaviorPatch {
                quirk,
                title: "Over-Planning Compensation",
                instruction: "\n<model_behavior_guidance>\n- Execution over speculation: Do not output lengthy, multi-paragraph plans or speculative analysis when direct action is required. Execute the necessary tool calls immediately.\n- Keep intermediate explanations concise; verify outcomes directly using tools.\n</model_behavior_guidance>\n",
            },
            ModelQuirk::StopsEarly => BehaviorPatch {
                quirk,
                title: "Early Termination Compensation",
                instruction: "\n<model_behavior_guidance>\n- Persistence: Do not terminate after a single preliminary step or partial result. Continue executing through all required tool steps and verifications until the entire task is genuinely completed.\n- Only report completion when all concrete actions and checks have succeeded.\n</model_behavior_guidance>\n",
            },
            ModelQuirk::ReluctantToUseTools => BehaviorPatch {
                quirk,
                title: "Tool Usage Compensation",
                instruction: "\n<model_behavior_guidance>\n- Mandatory tool usage: Never guess, hallucinate, or assume system states, file paths, or file contents in text. Always invoke the corresponding tool (e.g. read, ls, grep) to inspect ground truth before proceeding.\n</model_behavior_guidance>\n",
            },
            ModelQuirk::WeakVerification => BehaviorPatch {
                quirk,
                title: "Verification Rigor Compensation",
                instruction: "\n<model_behavior_guidance>\n- Mandatory verification: Always verify your changes before claiming completion. Run relevant tests, linters, or inspect the file diff to confirm correctness.\n- Never claim an issue is resolved without empirical evidence from tool execution.\n</model_behavior_guidance>\n",
            },
        }
    }

    /// Stable semantic anchor, independent of a profile's business slots.
    pub fn select_target_slot(
        _spec: &PromptSpec,
        _registry: &one_prompt::ComponentRegistry,
    ) -> String {
        "behavior_hooks".into()
    }

    fn ensure_hook(spec: &mut PromptSpec, registry: &one_prompt::ComponentRegistry) {
        let has_hook = spec
            .components
            .iter()
            .chain(
                registry
                    .preset_components(&spec.preset)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|id| registry.component(id)),
            )
            .any(|c| c.slots.iter().any(|s| s.id == "behavior_hooks"));
        if !has_hook {
            spec.components.push(one_prompt::Component {
                id: "model_enhancer.behavior".into(),
                slots: vec![one_prompt::Slot {
                    id: "behavior_hooks".into(),
                    body: one_prompt::Body::text(""),
                    when: Default::default(),
                }],
            });
        }
    }

    /// Enhance a PromptSpec by appending non-destructive delta patches for active quirks.
    /// Multiple quirks are composed without overriding each other.
    pub fn enhance(
        spec: &mut PromptSpec,
        registry: &one_prompt::ComponentRegistry,
        quirks: &[ModelQuirk],
    ) -> Vec<BehaviorPatch> {
        if quirks.is_empty() {
            return Vec::new();
        }

        Self::ensure_hook(spec, registry);
        let target_slot = Self::select_target_slot(spec, registry);
        let mut applied = Vec::new();

        for quirk in quirks {
            let patch = Self::patch_for(*quirk);
            let mut op = SlotOperation::append(&target_slot, patch.instruction);
            op.source = format!("PromptEnhancer::quirk::{}", quirk.as_str());
            spec.operations.push(op);
            applied.push(patch);
        }

        applied
    }
}

pub mod config;

impl PromptEnhancer {
    /// Apply already resolved, independent enhancer deltas through one-prompt.
    pub fn apply_resolved(
        spec: &mut PromptSpec,
        registry: &one_prompt::ComponentRegistry,
        enhancers: &[config::ResolvedEnhancer],
    ) {
        if !enhancers.iter().any(|e| e.enabled) {
            return;
        }
        Self::ensure_hook(spec, registry);
        for enhancer in enhancers.iter().filter(|e| e.enabled) {
            let mut op = SlotOperation::append(&enhancer.hook, &enhancer.prompt);
            op.source = format!(
                "ModelEnhancer::{} [{}; binding: {}]",
                enhancer.id, enhancer.prompt_source, enhancer.binding_source
            );
            spec.operations.push(op);
        }
    }
}
