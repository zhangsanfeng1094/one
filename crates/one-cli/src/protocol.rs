//! Harness JSON protocol: **Agent ≡ Subagent** (same `AgentSpec`).
//!
//! Spec: `docs/protocol.md` (`one.protocol.v1`).
//!
//! - **Full JSON**: any `AgentSpec` via `--spec` / inline `agent_spec`.
//! - **Preset**: short name (`explore`) expands to a full `AgentSpec`.
//! - **Order**: CLI `harness::run` first; `TaskTool` is a thin wrapper (P1b done).
//!
//! Center: system prompt, tools, model, permissions, spawn_policy — all serializable.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PROTOCOL_ID: &str = "one.protocol.v1";
pub const PROTOCOL_VERSION: u32 = 1;

pub const CAP_RESULT_V1: &str = "result_v1";
pub const CAP_AGENT_SPEC_V1: &str = "agent_spec_v1";
pub const CAP_PARENT_TOOL_USE_ID_V1: &str = "parent_tool_use_id_v1";

// ── Error ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ProtocolError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: Some(false),
            details: None,
        }
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProtocolError {}

pub mod error_code {
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const INVALID_AGENT_SPEC: &str = "invalid_agent_spec";
    pub const UNKNOWN_AGENT: &str = "unknown_agent";
    pub const SPAWN_DEPTH_EXCEEDED: &str = "spawn_depth_exceeded";
    pub const SPAWN_NOT_ALLOWED: &str = "spawn_not_allowed";
    pub const AUTH_REQUIRED: &str = "auth_required";
    pub const PERMISSION_DENIED: &str = "permission_denied";
    pub const TOOL_ERROR: &str = "tool_error";
    pub const PROVIDER_ERROR: &str = "provider_error";
    pub const MAX_TURNS: &str = "max_turns";
    pub const ABORTED: &str = "aborted";
    pub const TIMEOUT: &str = "timeout";
    pub const INTERNAL: &str = "internal";
}

/// Machine-readable end state of a harness run / `task` tool.
/// Parent agents must distinguish "finished research" vs "hit turn cap".
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskExitStatus {
    Success,
    MaxTurnsExceeded,
    Aborted,
    RuntimeError,
    /// Sub-agent ended with ERROR: … or lacks info (not a question to the user).
    IncompleteInfo,
}

impl TaskExitStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::MaxTurnsExceeded => "max_turns_exceeded",
            Self::Aborted => "aborted",
            Self::RuntimeError => "runtime_error",
            Self::IncompleteInfo => "incomplete_info",
        }
    }

    pub fn is_ok(self) -> bool {
        matches!(self, Self::Success)
    }
}

// ── ToolSpec ───────────────────────────────────────────────────────────

/// One tool as exposed to the model / host (JSON Schema parameters).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema object for tool arguments.
    pub parameters: Value,
}

// ── Tools constraint on an AgentSpec ───────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolProfile {
    #[default]
    Coding,
    ReadOnly,
    Plan,
    /// Empty base; only `allow` / `extra` matter.
    None,
}

impl ToolProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Coding => "coding",
            Self::ReadOnly => "read_only",
            Self::Plan => "plan",
            Self::None => "none",
        }
    }
}

/// Which tools this agent may use. Same shape for root and subagent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolsSpec {
    #[serde(default)]
    pub profile: ToolProfile,
    /// If non-empty, **only** these tool names (profile is ignored as membership).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Mount MCP tools.
    #[serde(default)]
    pub mcp: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_allow: Vec<String>,
    /// Extension / plugin tools by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
}

impl ToolsSpec {
    pub fn read_only() -> Self {
        Self {
            profile: ToolProfile::ReadOnly,
            mcp: false,
            ..Default::default()
        }
    }

    pub fn coding() -> Self {
        Self {
            profile: ToolProfile::Coding,
            mcp: true,
            ..Default::default()
        }
    }

    /// Resolve final tool **names** from a profile catalog.
    ///
    /// `profile_tools(profile)` returns the default name list for that profile.
    pub fn resolve_names(&self, profile_tools: &dyn Fn(&ToolProfile) -> Vec<String>) -> Vec<String> {
        let mut base = if !self.allow.is_empty() {
            self.allow.clone()
        } else {
            profile_tools(&self.profile)
        };
        base.retain(|n| !self.deny.iter().any(|d| d == n));
        for e in &self.extra {
            if !base.iter().any(|b| b == e) {
                base.push(e.clone());
            }
        }
        // MCP stripping is done by harness when building real tools (names unknown here).
        base
    }
}

// ── Model / skills / resources / spawn ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// off | low | medium | high
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Inherit provider/id/thinking from parent run (subagent default).
    #[serde(default)]
    pub inherit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SkillsSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Inject skills catalog XML into system prompt.
    #[serde(default = "default_true")]
    pub catalog: bool,
    /// Skill names whose full body is preloaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preload: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ResourcesSpec {
    #[serde(default = "default_true")]
    pub agents_md: bool,
    #[serde(default = "default_true")]
    pub claude_md: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SpawnPolicy {
    /// Agent names this run may spawn via `task` / `agent` tool. Empty = no spawn tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Max nesting depth for **children** from this agent (0 = cannot spawn).
    #[serde(default)]
    pub max_depth: u32,
    #[serde(default)]
    pub max_concurrent: u32,
}

impl SpawnPolicy {
    pub fn none() -> Self {
        Self {
            allow: vec![],
            max_depth: 0,
            max_concurrent: 0,
        }
    }

    pub fn explore_only() -> Self {
        Self {
            allow: vec!["explore".into()],
            max_depth: 1,
            max_concurrent: 4,
        }
    }
}

// ── AgentSpec ──────────────────────────────────────────────────────────

/// Harness configuration for **one** agent run (root or sub — same type).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// When the parent model should delegate (Claude-style description).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Full system prompt; `None` = harness default template for this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    #[serde(default)]
    pub tools: ToolsSpec,
    #[serde(default)]
    pub model: ModelSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
    /// default | accept_edits | dont_ask | plan | bypass
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// workspace-write | read_only | full-access
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_dirs: Vec<String>,
    #[serde(default)]
    pub skills: SkillsSpec,
    #[serde(default)]
    pub resources: ResourcesSpec,
    #[serde(default)]
    pub spawn_policy: SpawnPolicy,
    /// Child role table; values are nested AgentSpecs (isomorphic).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub agents: std::collections::BTreeMap<String, AgentSpec>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            name: Some("main".into()),
            description: None,
            system_prompt: None,
            append_system_prompt: None,
            tools: ToolsSpec::coding(),
            model: ModelSpec {
                inherit: false,
                ..Default::default()
            },
            max_turns: Some(32),
            permission_mode: Some("default".into()),
            sandbox: Some("workspace-write".into()),
            cwd: None,
            add_dirs: vec![],
            skills: SkillsSpec::default(),
            resources: ResourcesSpec::default(),
            spawn_policy: SpawnPolicy::explore_only(),
            agents: std::collections::BTreeMap::new(),
            meta: Value::Null,
        }
    }
}

/// How an agent is specified at a CLI/RPC/tool boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AgentRef {
    /// Preset or disk name: `"explore"`.
    Preset(String),
    /// Full harness JSON (isomorphic AgentSpec).
    Spec(AgentSpec),
}

impl AgentRef {
    pub fn preset(name: impl Into<String>) -> Self {
        Self::Preset(name.into())
    }

    pub fn from_spec(spec: AgentSpec) -> Self {
        Self::Spec(spec)
    }
}

impl AgentSpec {
    /// Built-in read-only explore worker (same type as root).
    /// This **is** the base subagent harness JSON (serialize for dump/export).
    pub fn builtin_explore() -> Self {
        Self {
            name: Some("explore".into()),
            description: Some(
                "Read-only codebase research. Use for multi-file exploration so the parent context stays small."
                    .into(),
            ),
            system_prompt: Some(
                "You are a read-only sub-agent of One.\n\
                 Complete the research task, then stop.\n\
                 - Use only the tools provided.\n\
                 - Do not ask the user questions.\n\
                 - Final answer: findings first, key paths/symbols, residual risks. Be concise.\n\
                 - Do not restate the entire task prompt."
                    .into(),
            ),
            append_system_prompt: None,
            tools: {
                let mut t = ToolsSpec::read_only();
                t.deny = vec!["ask_user".into()];
                t
            },
            model: ModelSpec {
                inherit: true,
                thinking: Some("off".into()),
                ..Default::default()
            },
            max_turns: Some(16),
            permission_mode: Some("dont_ask".into()),
            sandbox: Some("workspace-write".into()),
            cwd: None,
            add_dirs: vec![],
            skills: SkillsSpec {
                enabled: false,
                catalog: false,
                preload: vec![],
            },
            resources: ResourcesSpec {
                agents_md: false,
                claude_md: false,
            },
            spawn_policy: SpawnPolicy::none(),
            agents: std::collections::BTreeMap::new(),
            meta: Value::Null,
        }
    }

    /// Default root coding agent with explore as a child role.
    pub fn builtin_main() -> Self {
        let mut main = Self::default();
        main.agents
            .insert("explore".into(), Self::builtin_explore());
        main
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("anonymous")
    }

    /// Whether this agent may register a spawn tool.
    pub fn can_spawn(&self) -> bool {
        !self.spawn_policy.allow.is_empty() && self.spawn_policy.max_depth > 0
    }

    /// Resolve a child agent by name (table or built-in aliases).
    pub fn resolve_child(&self, name: &str) -> Option<AgentSpec> {
        if let Some(spec) = self.agents.get(name) {
            return Some(spec.clone());
        }
        // Built-in alias even if not copied into agents map.
        if name == "explore" && self.spawn_policy.allow.iter().any(|a| a == "explore") {
            return Some(Self::builtin_explore());
        }
        None
    }

    pub fn spawn_allowed(&self, name: &str) -> bool {
        self.spawn_policy.allow.iter().any(|a| a == name || a == "*")
    }
}

// ── RunRequest / parent ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    #[default]
    Ephemeral,
    Persist,
    Resume,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionSpec {
    #[serde(default)]
    pub mode: SessionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PromptInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Value>,
}

/// Present only when this run is a subagent (still the same RunRequest type).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunParent {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Parent tool call id that invoked `task` / `agent`.
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub depth: u32,
}

/// Single harness entry: root or subagent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunRequest {
    pub protocol: String,
    pub protocol_version: u32,
    #[serde(rename = "type")]
    pub type_field: String,
    pub agent: AgentSpec,
    pub prompt: PromptInput,
    #[serde(default)]
    pub session: SessionSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<RunParent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

impl RunRequest {
    pub fn new(agent: AgentSpec, text: impl Into<String>) -> Self {
        let is_child = false;
        Self {
            protocol: PROTOCOL_ID.into(),
            protocol_version: PROTOCOL_VERSION,
            type_field: "run_request".into(),
            agent,
            prompt: PromptInput {
                role: Some("user".into()),
                text: text.into(),
                images: vec![],
            },
            session: SessionSpec {
                mode: if is_child {
                    SessionMode::Ephemeral
                } else {
                    SessionMode::Persist
                },
                ..Default::default()
            },
            parent: None,
            run_id: None,
            meta: Value::Null,
        }
    }

    /// Build a child run from parent agent table + task args.
    pub fn child(
        parent_agent: &AgentSpec,
        child_name: &str,
        prompt: impl Into<String>,
        parent: RunParent,
    ) -> Result<Self, ProtocolError> {
        if !parent_agent.spawn_allowed(child_name) {
            return Err(ProtocolError::new(
                error_code::SPAWN_NOT_ALLOWED,
                format!("agent `{child_name}` not in spawn_policy.allow"),
            ));
        }
        if parent.depth > parent_agent.spawn_policy.max_depth {
            return Err(ProtocolError::new(
                error_code::SPAWN_DEPTH_EXCEEDED,
                format!(
                    "depth {} exceeds max_depth {}",
                    parent.depth, parent_agent.spawn_policy.max_depth
                ),
            ));
        }
        let child_spec = parent_agent.resolve_child(child_name).ok_or_else(|| {
            ProtocolError::new(
                error_code::UNKNOWN_AGENT,
                format!("unknown agent `{child_name}`"),
            )
        })?;
        Ok(Self {
            protocol: PROTOCOL_ID.into(),
            protocol_version: PROTOCOL_VERSION,
            type_field: "run_request".into(),
            agent: child_spec,
            prompt: PromptInput {
                role: Some("user".into()),
                text: prompt.into(),
                images: vec![],
            },
            session: SessionSpec {
                mode: SessionMode::Ephemeral,
                ..Default::default()
            },
            parent: Some(parent),
            run_id: None,
            meta: Value::Null,
        })
    }

    pub fn is_subagent(&self) -> bool {
        self.parent.is_some()
    }
}

// ── RunResult ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageSnapshot {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_read_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// Echo of what harness actually ran (for hosts / eval).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AgentRunEcho {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Materialized tool **names** for this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunResult {
    pub protocol: String,
    pub protocol_version: u32,
    #[serde(rename = "type")]
    pub type_field: String,
    pub ok: bool,
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Prefer this over parsing free text: success | max_turns_exceeded | …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskExitStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageSnapshot>,
    /// What harness actually used (prompt/tools face).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentRunEcho>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<RunParent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Value>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

impl Default for RunResult {
    fn default() -> Self {
        Self {
            protocol: PROTOCOL_ID.into(),
            protocol_version: PROTOCOL_VERSION,
            type_field: "result".into(),
            ok: true,
            result: String::new(),
            error: None,
            run_id: None,
            session_id: None,
            session_path: None,
            duration_ms: 0,
            turns: None,
            stop_reason: None,
            status: None,
            usage: None,
            agent: None,
            parent: None,
            children: vec![],
            meta: Value::Null,
        }
    }
}

impl RunResult {
    pub fn success(text: impl Into<String>, duration_ms: u64) -> Self {
        Self {
            ok: true,
            result: text.into(),
            duration_ms,
            stop_reason: Some("end_turn".into()),
            status: Some(TaskExitStatus::Success),
            ..Self::default()
        }
    }

    pub fn failure(err: ProtocolError, duration_ms: u64) -> Self {
        Self {
            ok: false,
            result: String::new(),
            duration_ms,
            stop_reason: Some("error".into()),
            status: Some(TaskExitStatus::RuntimeError),
            error: Some(err),
            ..Self::default()
        }
    }

    pub fn failure_msg(code: &str, message: impl Into<String>, duration_ms: u64) -> Self {
        Self::failure(ProtocolError::new(code, message), duration_ms)
    }

    pub fn with_status(mut self, status: TaskExitStatus) -> Self {
        self.status = Some(status);
        self.ok = status.is_ok();
        self
    }

    pub fn with_session(mut self, id: Option<String>, path: Option<String>) -> Self {
        self.session_id = id;
        self.session_path = path;
        self
    }

    pub fn with_usage(mut self, usage: UsageSnapshot) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn with_agent_echo(mut self, echo: AgentRunEcho) -> Self {
        self.agent = Some(echo);
        self
    }

    pub fn exit_code(&self) -> i32 {
        if self.ok {
            0
        } else {
            1
        }
    }

    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).expect("RunResult serializes")
    }

    /// RPC body with dual `text` + `result` keys.
    pub fn to_rpc_result_value(&self) -> Value {
        let mut v = serde_json::to_value(self).expect("RunResult value");
        if let Some(obj) = v.as_object_mut() {
            obj.insert("text".into(), json!(self.result));
        }
        v
    }
}

// ── Profile tool catalogs (names only; harness maps to real Tool impls) ─

/// Default tool names per profile (must stay aligned with `one-tools` assembly).
pub fn profile_tool_names(profile: &ToolProfile) -> Vec<String> {
    match profile {
        ToolProfile::None => vec![],
        ToolProfile::ReadOnly => vec![
            "read".into(),
            "grep".into(),
            "find".into(),
            "ls".into(),
            "ask_user".into(),
            "web_search".into(),
            "web_fetch".into(),
        ],
        ToolProfile::Plan => vec![
            "read".into(),
            "grep".into(),
            "find".into(),
            "ls".into(),
            "ask_user".into(),
            "web_search".into(),
            "web_fetch".into(),
            // plan tools registered by runtime; names documented here
            "plan".into(),
            "exit_plan_mode".into(),
        ],
        ToolProfile::Coding => vec![
            "read".into(),
            "write".into(),
            "edit".into(),
            "bash".into(),
            "bash_output".into(),
            "bash_kill".into(),
            "grep".into(),
            "find".into(),
            "ls".into(),
            "ask_user".into(),
            "web_search".into(),
            "web_fetch".into(),
        ],
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_spec_main_and_explore_are_same_type() {
        let main = AgentSpec::builtin_main();
        let explore = main.resolve_child("explore").expect("explore child");
        assert_eq!(explore.display_name(), "explore");
        assert!(main.can_spawn());
        assert!(!explore.can_spawn());
        // Same struct: re-serialize explore as standalone agent
        let j = serde_json::to_string(&explore).unwrap();
        let back: AgentSpec = serde_json::from_str(&j).unwrap();
        assert_eq!(back.name.as_deref(), Some("explore"));
        assert!(back.system_prompt.is_some());
    }

    #[test]
    fn tools_allow_overrides_profile() {
        let t = ToolsSpec {
            profile: ToolProfile::Coding,
            allow: vec!["read".into(), "grep".into()],
            deny: vec![],
            ..Default::default()
        };
        let names = t.resolve_names(&profile_tool_names);
        assert_eq!(names, vec!["read", "grep"]);
    }

    #[test]
    fn tools_deny_strips_from_profile() {
        let t = ToolsSpec {
            profile: ToolProfile::ReadOnly,
            deny: vec!["ask_user".into()],
            ..Default::default()
        };
        let names = t.resolve_names(&profile_tool_names);
        assert!(!names.iter().any(|n| n == "ask_user"));
        assert!(names.iter().any(|n| n == "read"));
    }

    #[test]
    fn run_request_child_isomorphic() {
        let main = AgentSpec::builtin_main();
        let parent = RunParent {
            run_id: "run_p".into(),
            session_id: Some("s1".into()),
            tool_use_id: "call_1".into(),
            agent_name: Some("main".into()),
            depth: 1,
        };
        let req = RunRequest::child(&main, "explore", "find auth", parent).unwrap();
        assert!(req.is_subagent());
        assert_eq!(req.agent.display_name(), "explore");
        assert!(matches!(req.session.mode, SessionMode::Ephemeral));
        assert!(req.agent.system_prompt.is_some());
        assert!(!req.agent.tools.mcp);
    }

    #[test]
    fn spawn_not_allowed() {
        let mut main = AgentSpec::builtin_main();
        main.spawn_policy.allow.clear();
        let parent = RunParent {
            run_id: "r".into(),
            session_id: None,
            tool_use_id: "c".into(),
            agent_name: None,
            depth: 1,
        };
        let err = RunRequest::child(&main, "explore", "x", parent).unwrap_err();
        assert_eq!(err.code, error_code::SPAWN_NOT_ALLOWED);
    }

    #[test]
    fn run_result_echoes_agent_tools() {
        let echo = AgentRunEcho {
            name: Some("explore".into()),
            tools: vec!["read".into(), "grep".into()],
            depth: Some(1),
            ..Default::default()
        };
        let rr = RunResult::success("done", 10).with_agent_echo(echo);
        let v: Value = serde_json::from_str(&rr.to_json_line()).unwrap();
        assert_eq!(v["type"], "result");
        assert_eq!(v["agent"]["name"], "explore");
        assert_eq!(v["agent"]["tools"][0], "read");
        assert_eq!(v["agent"]["depth"], 1);
    }

    #[test]
    fn explore_json_roundtrip_preserves_prompt() {
        let spec = AgentSpec::builtin_explore();
        let raw = serde_json::to_value(&spec).unwrap();
        assert!(raw["system_prompt"].as_str().unwrap().contains("read-only"));
        assert_eq!(raw["tools"]["profile"], "read_only");
        assert_eq!(raw["skills"]["catalog"], false);
    }

    #[test]
    fn task_exit_status_in_run_result() {
        let rr = RunResult::success("partial", 1)
            .with_status(TaskExitStatus::MaxTurnsExceeded);
        assert!(!rr.ok);
        let v: Value = serde_json::from_str(&rr.to_json_line()).unwrap();
        assert_eq!(v["status"], "max_turns_exceeded");
        assert_eq!(v["ok"], false);
    }

    #[test]
    fn agent_ref_preset_and_full_spec() {
        let p: AgentRef = serde_json::from_str(r#""explore""#).unwrap();
        assert!(matches!(p, AgentRef::Preset(n) if n == "explore"));

        let full = AgentSpec::builtin_explore();
        let j = serde_json::to_string(&full).unwrap();
        let s: AgentRef = serde_json::from_str(&j).unwrap();
        assert!(matches!(s, AgentRef::Spec(spec) if spec.display_name() == "explore"));
    }

    #[test]
    fn builtin_explore_is_exportable_harness_json() {
        let j = serde_json::to_value(AgentSpec::builtin_explore()).unwrap();
        assert_eq!(j["name"], "explore");
        assert!(j.get("system_prompt").is_some());
        assert!(j.get("tools").is_some());
        assert_eq!(j["spawn_policy"]["max_depth"], 0);
    }
}
