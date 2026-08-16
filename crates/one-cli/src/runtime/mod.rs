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
pub mod env_context;
pub mod explore_tools;
pub mod features;
pub mod harness;
mod helpers;
pub mod job_tools;
pub mod jobs;
pub mod memory_search_tool;
pub mod memory_write_tool;
mod mode;
mod plan;
mod policy;
pub mod presets;
mod prompt;
mod prompt_compose;
pub mod provider_limit;
mod reload;
mod session;
mod session_meta;
mod subscribe;
pub mod task_tool;
pub mod tool_materialize;
mod tools;
pub mod worktree;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use one_core::agent::{Agent, LlmProvider};
use one_ext::ExtensionRuntime;
use one_mcp::{McpManager, McpReminderState};
use one_resources::ResourceLoader;
use one_session::{SessionManager, ToolAuditItem};
use one_tools::{AskUserHandler, BackgroundTaskRegistry, PathPolicy, PlanExitState, TodoListState};

use crate::approval::PermissionGate;
use crate::hitl::HitlChannel;
use crate::langfuse::LangfuseTraceSink;

pub use features::{FeatureState, FEATURE_SUBAGENT};
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
    /// Session todo list for `todo_write` (survives plan/act rebuilds).
    todo_state: TodoListState,
    /// Per-turn memory read/grep budget (M3; reset each user prompt).
    memory_lookups: std::sync::Arc<one_tools::MemoryLookupBudget>,
    /// Frozen env snapshot (cwd/git/date) — boot / `/new` / `/reload` only.
    env_context: String,
    /// Frozen memory L2 catalog section (None when disabled).
    memory_catalog: Option<String>,
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
    /// Per-conversation state for MCP full/delta reminders. Reminders are
    /// injected as user-visible `<system-reminder>` notices so system prompt
    /// cache remains stable while MCP runtime status changes.
    mcp_reminder_state: McpReminderState,
    /// Langfuse sink (if `--trace`); held so we can flush before process exit.
    langfuse: Option<Arc<LangfuseTraceSink>>,
    /// Host for the `task` meta-tool (None when spawn disabled).
    pub task_host: Option<Arc<TaskToolHost>>,
    /// Parent / main AgentSpec (tools face for Act mode materialize).
    pub main_agent: crate::protocol::AgentSpec,
    /// Feature flags currently driving tools + system prompt.
    applied_features: FeatureState,
    /// Settings features that differ from `applied_features` (awaiting `/new`).
    pending_features: Option<FeatureState>,
    /// Process kill-switch: never enable subagent this process (`--no-subagent`).
    no_subagent_process: bool,
    /// Process kill-switch: never enable memory this process (`--no-memory` / env).
    no_memory_process: bool,
    /// Whether the active LLM advertises Responses hosted search tools
    /// (`provider.server_tools()` non-empty). Combined with feature
    /// `server_search` → request inject only (not response handling).
    hosted_search_capable: bool,
    /// Monotonic user-prompt index for `one.usage` / `one.tool_audit` rows.
    prompt_index: u64,
    /// In-run tool lifecycle buffer (flushed after each prompt; not LLM context).
    tool_audit: Vec<ToolAuditItem>,
    /// tool_call_id → (name, started_at_ms) for duration calculation.
    tool_starts: HashMap<String, (String, u64)>,
}

impl AppRuntime {
    /// Bind the active LLM so `task` can call `harness::run` with the same provider.
    pub async fn bind_task_provider(&self, provider: Arc<dyn LlmProvider>) {
        if let Some(host) = &self.task_host {
            host.bind_provider(provider).await;
        }
    }

    /// Refresh hosted-search capability from the active provider and rematerialize
    /// tools. Feature `server_search` only controls request inject (hosted tools
    /// vs local function `web_search`); response events/citations stay ungated.
    pub async fn refresh_web_search_backend(
        &mut self,
        providers: &crate::provider::ProviderSet,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Truth source = what the live LlmProvider would attach via server_tools().
        self.hosted_search_capable = !providers.as_llm().server_tools().is_empty();
        self.rebuild_mode_tools_and_prompt().await
    }

    /// True when we should **inject** hosted search on the main request
    /// (feature on + model capable). Response handling ignores this flag.
    pub(super) fn hosted_search_active(&self) -> bool {
        self.applied_features.server_search_enabled() && self.hosted_search_capable
    }

    /// Push current extension + MCP tools into the task host so children with
    /// `tools.mcp: true` (or allow-listed MCP names) can materialize them.
    ///
    /// Uses [`one_mcp::McpManager::model_visible_tools`] so deferred mode
    /// children get `search_tool` / `use_tool` rather than every MCP schema.
    pub async fn refresh_task_dynamic_tools(&self) {
        let Some(host) = &self.task_host else {
            return;
        };
        let mut dyn_tools = self.extensions.tools();
        if self.mode != AgentMode::Plan {
            dyn_tools.extend(self.mcp.model_visible_tools());
        }
        host.set_dynamic_tools(dyn_tools).await;
    }

    /// Live system prompt: base (+ plan overlay) + optional deferred MCP announcement.
    pub(super) fn effective_system_prompt(&self) -> String {
        let base = if self.mode == AgentMode::Plan {
            if let Some(path) = &self.plan_path {
                format!(
                    "{}{}",
                    self.base_system_prompt,
                    one_tools::plan_mode_system_overlay(path)
                )
            } else {
                self.base_system_prompt.clone()
            }
        } else {
            self.base_system_prompt.clone()
        };
        // Plan mode never exposes MCP tools. MCP runtime state is injected as
        // per-turn `<system-reminder>` notices instead of being appended here,
        // so the cached base system prompt stays stable while servers connect.
        base
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

    /// Background bash task registry (shared with `bash` / `bash_output` / `bash_kill`).
    pub fn bg_registry(&self) -> Arc<BackgroundTaskRegistry> {
        self.bg_registry.clone()
    }

    /// Background agent jobs registry (`task(background=true)`), if spawn is enabled.
    pub fn agent_jobs(&self) -> Option<Arc<jobs::AgentJobRegistry>> {
        self.task_host.as_ref().map(|h| h.jobs())
    }

    /// Task tool host (live job bindings for TUI), if subagent is enabled.
    pub fn task_host(&self) -> Option<Arc<task_tool::TaskToolHost>> {
        self.task_host.clone()
    }

    /// Whether the `task` tool is registered for this runtime.
    pub fn task_enabled(&self) -> bool {
        self.applied_features.subagent_enabled()
            && self
                .task_host
                .as_ref()
                .map(|h| h.can_spawn())
                .unwrap_or(false)
    }

    /// Features currently applied to tools + prompt.
    pub fn applied_features(&self) -> &FeatureState {
        &self.applied_features
    }

    /// True when settings features diverge from the live agent context.
    pub fn features_pending(&self) -> bool {
        self.pending_features.is_some()
    }

    /// Short notice for UI when feature changes need `/new`.
    pub fn features_pending_notice(&self) -> Option<String> {
        self.pending_features
            .as_ref()
            .map(|p| format!("features pending ({}) · /new to apply", p.fingerprint()))
    }

    fn can_spawn_policy(&self) -> bool {
        self.task_host
            .as_ref()
            .map(|h| h.can_spawn())
            .unwrap_or(false)
    }

    /// Recompose base + mode system prompt from applied features + resources.
    ///
    /// Keeps frozen `env_context` / `memory_catalog` (session-stable for prompt cache).
    pub(super) fn recompose_base_prompt(&mut self) {
        self.base_system_prompt =
            prompt_compose::compose_base_system_prompt(prompt_compose::ComposeBaseInput {
                features: &self.applied_features,
                resources: &self.resources,
                can_spawn: self.can_spawn_policy(),
                env_context: Some(self.env_context.as_str()),
                memory_catalog: self.memory_catalog.as_deref(),
            });
    }

    /// Refresh env + memory L2 snapshots and recompose (cold start, `/new`, `/reload`).
    pub(super) async fn refresh_context_snapshots(
        &mut self,
        memory_opts: &one_resources::MemoryLoadOptions,
    ) {
        self.memory_lookups
            .set_max(memory_opts.max_lookups_per_turn);
        self.env_context = env_context::build_env_context(&self.cwd);
        self.memory_catalog = if memory_opts.enabled {
            one_resources::load_memory_catalog(&self.resources.agent_dir, &self.cwd, memory_opts)
                .await
                .map(|c| c.prompt_section)
        } else {
            None
        };
        self.recompose_base_prompt();
    }

    /// Whether the agent currently has conversation messages (context-bound).
    pub async fn has_messages(&self) -> bool {
        !self.agent.lock().await.messages.is_empty()
    }

    /// Persist feature flag; context-affecting changes apply on `/new` if messages exist.
    ///
    /// Returns `(enabled, applied_now)` — `applied_now` is false when pending `/new`.
    pub async fn set_feature_enabled(
        &mut self,
        id: &str,
        enabled: bool,
    ) -> Result<(bool, bool), Box<dyn std::error::Error>> {
        use features::{
            env_no_memory, env_no_subagent, feature_affects_context, feature_def, FEATURE_MEMORY,
        };

        if feature_def(id).is_none() {
            return Err(format!(
                "unknown feature `{id}` · known: {}",
                features::FEATURE_REGISTRY
                    .iter()
                    .map(|d| d.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into());
        }
        if id == FEATURE_SUBAGENT && (self.no_subagent_process || env_no_subagent()) {
            return Err(
                "subagent disabled for this process (--no-subagent / ONE_DISABLE_SUBAGENT)".into(),
            );
        }
        if (id == FEATURE_MEMORY || id == features::FEATURE_MEMORY_LEGACY)
            && (self.no_memory_process || env_no_memory())
        {
            return Err("memory disabled for this process (--no-memory / ONE_NO_MEMORY)".into());
        }

        let mut s = crate::settings::load();
        // Normalize legacy id so settings store `memory`, not `memory_write`.
        let store_id = if id == features::FEATURE_MEMORY_LEGACY {
            FEATURE_MEMORY
        } else {
            id
        };
        s.set_feature(store_id, enabled);
        crate::settings::save(&s)?;

        let desired = FeatureState::from_settings(&s).with_process_overrides(
            self.no_subagent_process || env_no_subagent(),
            self.no_memory_process || env_no_memory(),
        );

        if desired.fingerprint() == self.applied_features.fingerprint() {
            self.pending_features = None;
            return Ok((enabled, true));
        }

        let affects = feature_affects_context(store_id);
        let has_msgs = self.has_messages().await;
        if affects && has_msgs {
            self.pending_features = Some(desired);
            return Ok((enabled, false));
        }

        // Apply immediately (no messages, or non-context feature).
        self.applied_features = desired;
        self.pending_features = None;
        // Feature memory toggles L2 catalog + tools (path policy is cold-start; `/new` refreshes).
        if store_id == FEATURE_MEMORY {
            let mem_opts = features::effective_memory_options(&self.applied_features, &s);
            self.refresh_context_snapshots(&mem_opts).await;
        }
        self.rebuild_mode_tools_and_prompt().await?;
        Ok((enabled, true))
    }

    /// Load features from settings and apply to tools + prompt (cold start / `/new`).
    pub async fn apply_features_from_settings(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        use features::{env_no_memory, env_no_subagent};
        let s = crate::settings::load();
        self.applied_features = FeatureState::from_settings(&s).with_process_overrides(
            self.no_subagent_process || env_no_subagent(),
            self.no_memory_process || env_no_memory(),
        );
        self.pending_features = None;
        let mem_opts = features::effective_memory_options(&self.applied_features, &s);
        self.refresh_context_snapshots(&mem_opts).await;
        self.recompose_base_prompt();
        self.rebuild_mode_tools_and_prompt().await
    }

    /// Rebuild tools + prompt for the current Plan/Act mode.
    pub(super) async fn rebuild_mode_tools_and_prompt(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match self.mode {
            AgentMode::Plan => {
                // Re-enter plan tooling without rewriting plan file.
                if let Some(path) = self.plan_path.clone() {
                    self.apply_plan_tools_and_prompt(&path).await?;
                } else {
                    self.apply_act_tools_and_prompt().await?;
                }
            }
            AgentMode::Act => {
                self.apply_act_tools_and_prompt().await?;
            }
        }
        // Feature `server_search` → request inject only:
        // - active → declare hosted tools; local function web_search stripped (server wins)
        // - inactive → no hosted declare; local function web_search kept if registered
        // Response path always accepts web_search_call / citations (proxy may inject).
        let inject = self.hosted_search_active();
        self.agent.lock().await.config.server_search = inject;
        if inject {
            tracing::info!(
                "server_search: inject hosted tools on main request (no local web_search function)"
            );
        } else if self.applied_features.server_search_enabled() {
            tracing::info!(
                "server_search: no inject (model not capable); local function web_search if present"
            );
        } else {
            tracing::info!(
                "server_search: feature off — no hosted inject; response still parses upstream search if any"
            );
        }
        Ok(())
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

    /// Context window currently used for auto-compaction thresholds.
    pub fn context_window(&self) -> usize {
        self.context_window
    }

    /// Flush Langfuse batches and stop the upload worker (idempotent).
    pub fn flush_trace(&self) {
        if let Some(sink) = &self.langfuse {
            sink.shutdown();
        }
    }

    /// Kill session-owned background work (bash tasks + agent jobs).
    ///
    /// Call on process exit and when switching sessions (`/new`, `/resume`).
    /// Does **not** run on Esc turn-abort (long-lived bash servers like
    /// `npm run dev` should survive soft cancel).
    pub fn shutdown_owned_tasks(&self) {
        self.bg_registry.kill_all_running();
        if let Some(host) = &self.task_host {
            host.jobs()
                .kill_all_with_reason(jobs::KillReason::SessionTeardown);
        }
        // Drop any completion notices produced by job kill so the next session
        // turn does not see teardown noise.
        if let Ok(mut q) = self.bg_registry.notification_queue().lock() {
            q.clear();
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

    /// Shared abort flag (ACP / external hosts can signal without locking runtime).
    pub fn abort_handle(&self) -> Arc<AtomicBool> {
        self.abort_flag.clone()
    }

    pub fn is_aborted(&self) -> bool {
        self.abort_flag.load(Ordering::Relaxed)
    }

    pub fn abort(&self) {
        self.abort_flag.store(true, Ordering::Relaxed);
        // Cancel background subagent jobs (signals child abort_flag + notifies).
        // Background bash is intentionally left running (dev servers, watches).
        if let Some(host) = &self.task_host {
            host.jobs()
                .kill_all_with_reason(jobs::KillReason::ParentAbort);
        }
    }
}

impl Drop for AppRuntime {
    fn drop(&mut self) {
        // Best-effort: process exit / early return paths that skip explicit cleanup.
        self.shutdown_owned_tasks();
    }
}
