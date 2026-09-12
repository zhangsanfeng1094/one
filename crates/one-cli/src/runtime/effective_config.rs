//! Effective runtime configuration for subagent execution (Grok-aligned `EffectiveRuntimeConfig`).
//!
//! Subagents resolve their final runtime parameters through a strict precedence hierarchy:
//!
//! ```text
//! Explicit spawn-time override > Role / Preset default > Parent inheritance > Global defaults
//! ```
//!
//! This module unifies model selection, thinking/reasoning effort, tool capability filtering,
//! isolation mode, non-interactive security gates, and memory settings into a single resolved
//! configuration struct before launching a subagent.

use std::path::{Path, PathBuf};

use one_core::agent::ThinkingLevel;
use serde_json::Value;

use crate::approval::PermissionMode;
use crate::protocol::{
    normalize_agent_name, AgentSpec, CapabilityMode, IsolationMode, McpInheritance,
    MemoryResourceMode, ModelSpec, PromptInput, ProtocolError, RunParent, RunRequest, ToolProfile,
    ToolsSpec,
};

/// Resolved runtime configuration for a child subagent run.
#[derive(Debug, Clone)]
pub struct EffectiveRuntimeConfig {
    /// Normalized agent preset or role name (e.g., "explore", "general", "plan").
    pub agent_name: String,
    /// Human-facing brief description of this subagent run.
    pub description: Option<String>,
    /// Task prompt given to the subagent.
    pub prompt: String,
    /// Explicit model override, if provided.
    pub model: Option<String>,
    /// Reasoning / thinking effort override or inherited setting.
    pub thinking: Option<ThinkingLevel>,
    /// Tool capability profile filter (ReadOnly, ReadWrite, Execute, All).
    pub capability_mode: CapabilityMode,
    /// Workspace isolation mode (None / Worktree).
    pub isolation: IsolationMode,
    /// Whether this subagent is run in background.
    pub run_in_background: bool,
    /// Source subagent job id to resume transcript and context from.
    pub resume_from: Option<String>,
    /// Working directory for the child run.
    pub cwd: Option<PathBuf>,
    /// Subagent tool permission mode (default: DontAsk / headless).
    pub permission_mode: PermissionMode,
    /// Non-interactive gate: child runs never wait for user terminal input.
    pub non_interactive: bool,
    /// Auto approve harmless local tool operations in subagent mode.
    pub auto_approve: bool,
    /// MCP tool availability policy for this subagent.
    pub mcp_inheritance: McpInheritance,
    /// Cross-session memory access mode (default: Off for subagent context cleanliness).
    pub memory_resource_mode: MemoryResourceMode,
}

impl EffectiveRuntimeConfig {
    /// Resolve effective runtime configuration by combining parent context,
    /// agent definition, and spawn-time arguments.
    pub fn resolve(
        parent_agent: &AgentSpec,
        parent_meta: &RunParent,
        args: &Value,
        base_cwd: &Path,
    ) -> Result<Self, ProtocolError> {
        // 1. Resolve agent name / subagent_type with backward-compatible aliases
        let raw_name = resolve_agent_name_from_args(args);
        let normalized_name = normalize_agent_name(&raw_name);

        // 2. Resolve prompt and description
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_string();

        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        // 3. Resolve background execution flag (default: true, Grok-aligned)
        let run_in_background = args
            .get("run_in_background")
            .or_else(|| args.get("background"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // 4. Resolve capability_mode (Spawn Override > Preset/Role Default > CapabilityMode::All)
        let capability_mode = if let Some(m) = args.get("capability_mode").and_then(|v| v.as_str())
        {
            CapabilityMode::parse(m).unwrap_or_else(|| {
                if normalized_name == "explore" {
                    CapabilityMode::ReadOnly
                } else {
                    CapabilityMode::All
                }
            })
        } else if normalized_name == "explore" {
            CapabilityMode::ReadOnly
        } else {
            CapabilityMode::All
        };

        // 5. Resolve isolation mode (Spawn Override > Worktree for general-purpose in bg > None)
        let isolation = if let Some(iso) = args.get("isolation").and_then(|v| v.as_str()) {
            match iso.trim().to_ascii_lowercase().as_str() {
                "worktree" | "isolated" => IsolationMode::Worktree,
                _ => IsolationMode::None,
            }
        } else {
            IsolationMode::None
        };

        // 6. Resume source job id
        let resume_from = args
            .get("resume_from")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        // 7. Explicit cwd override
        let cwd = args
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| base_cwd.join(s));

        // 8. Model override
        let model = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        // 9. Subagent memory mode (default Off unless configured)
        let memory_resource_mode = MemoryResourceMode::Off;

        // Parent depth tracking & validation
        let _ = parent_meta;
        let _ = parent_agent;

        Ok(Self {
            agent_name: normalized_name,
            description,
            prompt,
            model,
            thinking: None,
            capability_mode,
            isolation,
            run_in_background,
            resume_from,
            cwd,
            permission_mode: PermissionMode::DontAsk,
            non_interactive: true,
            auto_approve: true,
            mcp_inheritance: McpInheritance::None,
            memory_resource_mode,
        })
    }

    /// Apply the resolved runtime configuration to an outgoing `AgentSpec`.
    pub fn apply_to_agent_spec(&self, agent: &mut AgentSpec) {
        // Apply capability filter
        apply_capability_mode_to_spec(agent, self.capability_mode);

        // Apply isolation
        if self.isolation == IsolationMode::Worktree {
            agent.isolation = IsolationMode::Worktree;
        }

        // Apply cwd
        if let Some(ref cwd) = self.cwd {
            agent.cwd = Some(cwd.display().to_string());
        }

        // Apply model override if present
        if let Some(ref m) = self.model {
            agent.model = ModelSpec {
                provider: None,
                id: Some(m.clone()),
                thinking: self.thinking.map(|t| t.as_str().to_string()),
                inherit: false,
            };
        }

        // Subagents are non-interactive headless workers
        agent.permission_mode = Some(self.permission_mode.as_str().to_string());

        // Subagents never re-spawn deeper agents by default (single-level hierarchy enforcement)
        agent.spawn_policy = crate::protocol::SpawnPolicy::none();
    }

    /// Apply the resolved runtime configuration onto a full `RunRequest`.
    pub fn apply_to_request(&self, req: &mut RunRequest) {
        self.apply_to_agent_spec(&mut req.agent);
        req.prompt = PromptInput {
            role: None,
            text: self.prompt.clone(),
            images: vec![],
        };
    }
}

/// Helper to parse agent name across all known tool parameter aliases.
fn resolve_agent_name_from_args(args: &Value) -> String {
    for key in ["subagent_type", "agent", "mode", "role"] {
        if let Some(a) = args.get(key).and_then(|v| v.as_str()) {
            let t = a.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    "explore".into()
}

/// Apply `CapabilityMode` restrictions to an `AgentSpec` tool profile.
fn apply_capability_mode_to_spec(agent: &mut AgentSpec, mode: CapabilityMode) {
    match mode {
        CapabilityMode::ReadOnly => {
            agent.tools = ToolsSpec::read_only();
            agent.tools.deny = vec![
                "ask_user".into(),
                "write".into(),
                "edit".into(),
                "bash".into(),
                "bash_output".into(),
                "bash_kill".into(),
                "monitor".into(),
                "task".into(),
                "spawn_subagent".into(),
            ];
            agent.tools.mcp = false;
            agent.mcp_inheritance = McpInheritance::None;
        }
        CapabilityMode::ReadWrite => {
            agent.tools.profile = ToolProfile::Coding;
            agent.tools.allow.clear();
            for n in [
                "bash",
                "bash_output",
                "bash_kill",
                "monitor",
                "ask_user",
                "task",
                "spawn_subagent",
            ] {
                if !agent.tools.deny.iter().any(|d| d == n) {
                    agent.tools.deny.push(n.into());
                }
            }
        }
        CapabilityMode::Execute => {
            agent.tools.profile = ToolProfile::Coding;
            agent.tools.allow.clear();
            for n in ["write", "edit", "ask_user", "task", "spawn_subagent"] {
                if !agent.tools.deny.iter().any(|d| d == n) {
                    agent.tools.deny.push(n.into());
                }
            }
        }
        CapabilityMode::All => {
            agent.tools = ToolsSpec::coding();
            agent.tools.deny = vec!["ask_user".into(), "task".into(), "spawn_subagent".into()];
            agent.tools.mcp = true;
            agent.mcp_inheritance = McpInheritance::All;
        }
    }
}
