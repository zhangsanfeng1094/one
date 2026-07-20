//! Application runtime: assembles core, tools, MCP, extensions, session.
//!
//! Split by concern (not by type):
//! - [`build`] — cold start assembly
//! - [`plan`] — Plan / Act mode
//! - [`tools`] — tool list rebuild + MCP sync
//! - [`prompt`] — user prompt + compaction
//! - [`session`] — session open/new/metadata
//! - [`reload`] — `/reload` resources + extensions
//! - [`subscribe`] — agent event fans-out

mod build;
pub mod explore_tools;
pub mod harness;
mod helpers;
mod mode;
mod plan;
mod policy;
pub mod presets;
mod prompt;
pub mod provider_limit;
mod reload;
mod session;
mod subscribe;
pub mod task_tool;
mod tools;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use one_core::agent::{Agent, LlmProvider};
use one_ext::ExtensionRuntime;
use one_mcp::McpManager;
use one_resources::ResourceLoader;
use one_session::SessionManager;
use one_tools::{AskUserHandler, BackgroundTaskRegistry, PathPolicy, PlanExitState};

use crate::approval::PermissionGate;
use crate::hitl::HitlChannel;
use crate::langfuse::LangfuseTraceSink;

pub use mode::AgentMode;
pub use task_tool::TaskToolHost;

pub struct AppRuntime {
    pub agent: Arc<tokio::sync::Mutex<Agent>>,
    abort_flag: Arc<AtomicBool>,
    steering_queue: Arc<std::sync::Mutex<Vec<String>>>,
    followup_queue: Arc<std::sync::Mutex<Vec<String>>>,
    pub session: Option<SessionManager>,
    /// Shared extension runtime (tools, hooks, lifecycle).
    pub extensions: Arc<ExtensionRuntime>,
    pub resources: ResourceLoader,
    pub auto_approve: bool,
    pub cwd: PathBuf,
    read_only: bool,
    /// Workspace path boundary + add-dir roots (rebuilt into tools on mode switch).
    path_policy: PathPolicy,
    /// Interactive `-r`: open session picker on TUI start.
    pub open_session_picker: bool,
    /// Current agent mode (Plan vs Act/Build).
    mode: AgentMode,
    /// Path of the active plan markdown file (set while/after plan mode).
    plan_path: Option<PathBuf>,
    /// Shared exit_plan_mode signal.
    plan_exit: Arc<Mutex<PlanExitState>>,
    /// Shared background bash registry (reused when leaving plan mode).
    bg_registry: Arc<BackgroundTaskRegistry>,
    /// Base system prompt without plan-mode overlay.
    base_system_prompt: String,
    /// Shared permission gate (interactive ask / fail-closed / auto).
    pub permission_gate: Arc<PermissionGate>,
    /// Human-in-the-loop channel for `ask_user` select prompts.
    pub hitl: HitlChannel,
    ask_user_handler: Arc<dyn AskUserHandler>,
    /// Active model context window (tokens). 0 = unknown → fallback compact threshold.
    context_window: usize,
    /// MCP platform runtime (stdio / HTTP servers → tools).
    /// Connections are process-scoped and **survive `/new`**.
    pub mcp: McpManager,
    /// Last applied MCP tool generation (re-sync when background load advances).
    mcp_tools_generation: u64,
    /// Langfuse sink (if `--trace`); held so we can flush before process exit.
    langfuse: Option<Arc<LangfuseTraceSink>>,
    /// Host for the `task` meta-tool (None when spawn disabled).
    pub task_host: Option<Arc<TaskToolHost>>,
}

impl AppRuntime {
    /// Bind the active LLM so `task` can call `harness::run` with the same provider.
    pub async fn bind_task_provider(&self, provider: Arc<dyn LlmProvider>) {
        if let Some(host) = &self.task_host {
            host.bind_provider(provider).await;
        }
    }

    /// Refresh session id on the task host (after session open / resume).
    pub async fn sync_task_session(&self) {
        if let Some(host) = &self.task_host {
            let id = self.session.as_ref().map(|s| s.header().id.clone());
            host.set_session_id(id).await;
        }
    }

    pub fn mode(&self) -> AgentMode {
        self.mode
    }

    /// Whether the `task` tool is registered for this runtime.
    pub fn task_enabled(&self) -> bool {
        self.task_host
            .as_ref()
            .map(|h| h.can_spawn())
            .unwrap_or(false)
    }

    pub fn plan_path(&self) -> Option<&std::path::Path> {
        self.plan_path.as_deref()
    }

    /// True if the model called `exit_plan_mode` since the last clear.
    pub fn take_plan_exit_request(&self) -> bool {
        let mut state = self.plan_exit.lock().expect("plan exit lock");
        let requested = state.requested;
        state.clear();
        requested
    }

    /// Update the model context window used for auto-compact thresholds.
    pub fn set_context_window(&mut self, window: usize) {
        self.context_window = window;
    }

    /// Flush Langfuse batches and stop the upload worker (idempotent).
    pub fn flush_trace(&self) {
        if let Some(sink) = &self.langfuse {
            sink.shutdown();
        }
    }

    /// Optional notice for TUI when MCP is still loading / just became ready.
    pub fn mcp_status_line(&self) -> Option<String> {
        self.mcp.status_line()
    }

    pub fn session_path(&self) -> Option<PathBuf> {
        self.session
            .as_ref()
            .and_then(|session| session.session_file().map(|path| path.to_path_buf()))
    }

    pub fn session_summary_line(&self) -> String {
        match &self.session {
            None => "session: (ephemeral)".into(),
            Some(s) => {
                let path = s
                    .session_file()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(memory)".into());
                let name = s
                    .session_name()
                    .or_else(|| s.first_user_preview())
                    .unwrap_or_else(|| "—".into());
                let leaf = s.get_leaf_id().unwrap_or("root");
                format!(
                    "session {name} · {} msgs · leaf={leaf} · {path}",
                    s.message_count()
                )
            }
        }
    }

    pub fn steer(&self, text: impl Into<String>) {
        Agent::push_queue(&self.steering_queue, text);
    }

    pub fn follow_up(&self, text: impl Into<String>) {
        Agent::push_queue(&self.followup_queue, text);
    }

    pub fn steering_queue(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
        self.steering_queue.clone()
    }

    pub fn followup_queue(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
        self.followup_queue.clone()
    }

    pub fn clear_abort(&self) {
        self.abort_flag.store(false, Ordering::Relaxed);
    }

    pub fn abort(&self) {
        self.abort_flag.store(true, Ordering::Relaxed);
    }
}
