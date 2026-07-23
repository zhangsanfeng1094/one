pub mod ask_user;
pub mod bash;
pub mod bash_kill;
pub mod bash_output;
pub mod edit;
pub mod edit_diff;
pub mod find;
pub mod grep;
pub mod ls;
pub mod os_sandbox;
pub mod path_policy;
pub mod permissions;
pub mod plan;
pub(crate) mod process_io;
pub mod read;
pub mod registry;
pub mod sandbox;
pub mod sandbox_permissions;
pub mod tasks;
pub mod tool_args;
pub mod truncate;
#[cfg(feature = "network")]
pub mod web_fetch;
#[cfg(feature = "network")]
pub mod web_search;
pub mod write;

use std::sync::Arc;

use one_core::tool::Tool;

pub use ask_user::{AskUserHandler, AskUserTool, FailClosedAskUser};
pub use bash::BashTool;
pub use bash_kill::BashKillTool;
pub use bash_output::BashOutputTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use os_sandbox::OsSandbox;
pub use path_policy::{AccessKind, PathPolicy, SandboxMode};
pub use permissions::{
    bash_command, call_fingerprint, call_summary, command_matches_prefix,
    evaluate as evaluate_permissions, suggested_command_prefix, suggested_command_prefix_from_cmd,
    PermissionRule, PermissionRules, PermissionVerdict, RuleAction,
};
pub use plan::{
    plan_mode_system_overlay, plan_mode_tools, plan_mode_tools_with_policy, ExitPlanModeTool,
    PlanEditTool, PlanExitState, PlanWriteTool,
};
pub use read::ReadTool;
pub use registry::{
    materialize_coding, materialize_explore, materialize_read_only, resolve_tool_names,
    BuiltinToolProfile, ToolBuildContext, ToolRegistry, UnknownToolError,
};
pub use sandbox_permissions::{
    justification_of, looks_like_sandbox_denial, requires_escalation, sandbox_permissions_of,
    SandboxPermissions,
};
pub use tasks::{BackgroundTaskRegistry, TaskMeta, TaskSnapshot, TaskState};
pub use truncate::{
    apply_head_default, apply_tail_default, cleanup_tool_outputs, cleanup_tool_outputs_before,
    format_size, present_file_read, present_tool_output, present_tool_output_with,
    set_tool_output_limits, spill_full_output, tool_output_limits, tool_outputs_root,
    truncate_head, truncate_line, truncate_tail, CleanupReport, PresentedOutput, PreviewStyle,
    ToolOutputLimits, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH,
    TOOL_OUTPUT_RETENTION_DAYS,
};
#[cfg(feature = "network")]
pub use web_fetch::WebFetchTool;
#[cfg(feature = "network")]
pub use web_search::WebSearchTool;
pub use write::WriteTool;

/// Options for building the default coding tool set.
#[derive(Clone)]
pub struct ToolBuildOptions {
    pub policy: PathPolicy,
    pub auto_approve: bool,
    pub registry: Arc<BackgroundTaskRegistry>,
    /// Human-in-the-loop bridge for `ask_user` (fail-closed when None).
    pub ask_user: Option<Arc<dyn AskUserHandler>>,
    /// Permission gate for bash escalate-on-failure (Codex-aligned).
    pub tool_gate: Option<Arc<dyn one_core::tool_gate::ToolGate>>,
}

impl ToolBuildOptions {
    pub fn new(cwd: std::path::PathBuf) -> Self {
        Self {
            policy: PathPolicy::workspace(cwd),
            auto_approve: true,
            registry: Arc::new(BackgroundTaskRegistry::new()),
            ask_user: None,
            tool_gate: None,
        }
    }

    pub fn with_policy(mut self, policy: PathPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_auto_approve(mut self, auto_approve: bool) -> Self {
        self.auto_approve = auto_approve;
        self
    }

    pub fn with_registry(mut self, registry: Arc<BackgroundTaskRegistry>) -> Self {
        self.registry = registry;
        self
    }

    pub fn with_ask_user(mut self, handler: Arc<dyn AskUserHandler>) -> Self {
        self.ask_user = Some(handler);
        self
    }

    pub fn with_tool_gate(mut self, gate: Arc<dyn one_core::tool_gate::ToolGate>) -> Self {
        self.tool_gate = Some(gate);
        self
    }
}

pub fn default_tools(cwd: std::path::PathBuf) -> Vec<Arc<dyn Tool>> {
    coding_tools(cwd)
}

pub fn coding_tools(cwd: std::path::PathBuf) -> Vec<Arc<dyn Tool>> {
    coding_tools_with_approve(cwd, true)
}

pub fn coding_tools_with_approve(
    cwd: std::path::PathBuf,
    auto_approve: bool,
) -> Vec<Arc<dyn Tool>> {
    let registry = Arc::new(BackgroundTaskRegistry::new());
    coding_tools_with_registry(cwd, auto_approve, registry)
}

/// Build coding tools sharing a background-task registry.
///
/// Wire `registry.notification_queue()` into [`one_core::Agent`] so completed
/// background tasks inject Claude-style notices into the conversation.
pub fn coding_tools_with_registry(
    cwd: std::path::PathBuf,
    auto_approve: bool,
    registry: Arc<BackgroundTaskRegistry>,
) -> Vec<Arc<dyn Tool>> {
    coding_tools_with_options(ToolBuildOptions {
        policy: PathPolicy::workspace(cwd),
        auto_approve,
        registry,
        ask_user: None,
        tool_gate: None,
    })
}

/// Build coding tools with an explicit path policy (workspace / full-access).
pub fn coding_tools_with_options(opts: ToolBuildOptions) -> Vec<Arc<dyn Tool>> {
    materialize_coding(&ToolBuildContext::from_options(opts))
}

pub fn read_only_tools(cwd: std::path::PathBuf) -> Vec<Arc<dyn Tool>> {
    read_only_tools_with_policy(PathPolicy::workspace(cwd))
}

pub fn read_only_tools_with_policy(policy: PathPolicy) -> Vec<Arc<dyn Tool>> {
    read_only_tools_with_ask(policy, None)
}

pub fn read_only_tools_with_ask(
    policy: PathPolicy,
    ask_user: Option<Arc<dyn AskUserHandler>>,
) -> Vec<Arc<dyn Tool>> {
    let mut ctx = ToolBuildContext::workspace(policy.cwd().to_path_buf()).with_policy(policy);
    if let Some(h) = ask_user {
        ctx = ctx.with_ask_user(h);
    }
    materialize_read_only(&ctx)
}
