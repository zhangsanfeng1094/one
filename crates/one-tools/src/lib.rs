pub mod ask_user;
pub mod bash;
pub mod bash_kill;
pub mod bash_output;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod os_sandbox;
pub mod path_policy;
pub mod permissions;
pub mod plan;
pub mod read;
pub mod sandbox;
pub mod tasks;
pub mod write;
#[cfg(feature = "network")]
pub mod web_fetch;
#[cfg(feature = "network")]
pub mod web_search;

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
    call_fingerprint, call_summary, evaluate as evaluate_permissions, PermissionRules,
    PermissionRule, PermissionVerdict, RuleAction,
};
pub use plan::{
    plan_mode_system_overlay, plan_mode_tools, plan_mode_tools_with_policy, ExitPlanModeTool,
    PlanEditTool, PlanExitState, PlanWriteTool,
};
pub use read::ReadTool;
pub use tasks::BackgroundTaskRegistry;
pub use write::WriteTool;
#[cfg(feature = "network")]
pub use web_fetch::WebFetchTool;
#[cfg(feature = "network")]
pub use web_search::WebSearchTool;

/// Options for building the default coding tool set.
#[derive(Clone)]
pub struct ToolBuildOptions {
    pub policy: PathPolicy,
    pub auto_approve: bool,
    pub registry: Arc<BackgroundTaskRegistry>,
    /// Human-in-the-loop bridge for `ask_user` (fail-closed when None).
    pub ask_user: Option<Arc<dyn AskUserHandler>>,
}

impl ToolBuildOptions {
    pub fn new(cwd: std::path::PathBuf) -> Self {
        Self {
            policy: PathPolicy::workspace(cwd),
            auto_approve: true,
            registry: Arc::new(BackgroundTaskRegistry::new()),
            ask_user: None,
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
}

pub fn default_tools(cwd: std::path::PathBuf) -> Vec<Arc<dyn Tool>> {
    coding_tools(cwd)
}

pub fn coding_tools(cwd: std::path::PathBuf) -> Vec<Arc<dyn Tool>> {
    coding_tools_with_approve(cwd, true)
}

pub fn coding_tools_with_approve(cwd: std::path::PathBuf, auto_approve: bool) -> Vec<Arc<dyn Tool>> {
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
    })
}

/// Build coding tools with an explicit path policy (workspace / full-access).
pub fn coding_tools_with_options(opts: ToolBuildOptions) -> Vec<Arc<dyn Tool>> {
    let policy = opts.policy;
    let ask_handler = opts
        .ask_user
        .clone()
        .unwrap_or_else(|| Arc::new(FailClosedAskUser) as Arc<dyn AskUserHandler>);
    #[allow(unused_mut)] // mut when `network` feature pushes extra tools
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::with_policy(policy.clone())),
        Arc::new(WriteTool::with_policy(policy.clone())),
        Arc::new(EditTool::with_policy(policy.clone())),
        Arc::new(BashTool::with_policy(
            policy.clone(),
            opts.auto_approve,
            opts.registry.clone(),
        )),
        Arc::new(BashOutputTool::new(opts.registry.clone())),
        Arc::new(BashKillTool::new(opts.registry)),
        Arc::new(GrepTool::with_policy(policy.clone())),
        Arc::new(FindTool::with_policy(policy.clone())),
        Arc::new(LsTool::with_policy(policy)),
        Arc::new(AskUserTool::new(ask_handler)),
    ];
    #[cfg(feature = "network")]
    {
        tools.push(Arc::new(WebSearchTool::new()));
        tools.push(Arc::new(WebFetchTool::new()));
    }
    tools
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
    let ask_handler =
        ask_user.unwrap_or_else(|| Arc::new(FailClosedAskUser) as Arc<dyn AskUserHandler>);
    #[allow(unused_mut)] // mut when `network` feature pushes extra tools
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::with_policy(policy.clone())),
        Arc::new(GrepTool::with_policy(policy.clone())),
        Arc::new(FindTool::with_policy(policy.clone())),
        Arc::new(LsTool::with_policy(policy)),
        Arc::new(AskUserTool::new(ask_handler)),
    ];
    #[cfg(feature = "network")]
    {
        tools.push(Arc::new(WebSearchTool::new()));
        tools.push(Arc::new(WebFetchTool::new()));
    }
    tools
}
