//! `task` meta-tool: thin wrapper around [`super::harness::run`].
//!
//! Lives in **one-cli only** (not one-tools). Parent agents register this tool
//! when `spawn_policy` allows children; explore children never get it.
//!
//! Default is **async** (Grok-aligned): omit `background` or set it true →
//! `status=started` + `job_id`; completion arrives as `[job completed]`.
//! `background=false` **blocks until the child ends**. Admission lives on the
//! independent [`super::coordinator::SubagentCoordinator`] queue: slot-full
//! background spawns return immediately (`queued=true`); a foreground caller
//! still queued after `ONE_TASK_FOREGROUND_BUDGET_MS` is handed off
//! (`backgrounded=true`). A child that has already started is never auto-bg'd.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use one_core::agent::LlmProvider;
use one_core::error::Result;
use one_core::tool::{invalid_args, Tool, ToolCall, ToolDefinition, ToolOutput};
use one_tools::DEFAULT_MAX_BYTES;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use super::coordinator::{CoordinatorConfig, SpawnSpec, SubagentCoordinator};
use super::harness::{self, HarnessOptions, ParentReadGrants};
use super::jobs::{AgentJobRegistry, SpawnOptions};
use super::presets;
use crate::protocol::{
    error_code, normalize_agent_name, AgentSpec, CapabilityMode, IsolationMode, McpInheritance,
    ProtocolError, RunParent, RunRequest, RunResult, SessionMode, TaskExitStatus, ToolProfile,
    ToolsSpec,
};
use one_tools::PathPolicy;

/// Cap tool_result text so a long child summary does not flood the parent.
const SUMMARY_MAX_CHARS: usize = DEFAULT_MAX_BYTES;

/// Shared host state for all `task` tool instances on a parent runtime.
pub struct TaskToolHost {
    /// Bound by CLI after `ProviderSet::build` / model switch.
    provider: RwLock<Option<Arc<dyn LlmProvider>>>,
    opts: RwLock<HarnessOptions>,
    /// Same PathPolicy shell as main session (shares `dynamic` Arc with runtime).
    parent_path_policy: RwLock<PathPolicy>,
    /// Parent harness face (spawn_policy + agents table).
    parent_agent: RwLock<AgentSpec>,
    parent_run_id: RwLock<String>,
    parent_session_id: RwLock<Option<String>>,
    parent_depth: AtomicU32,
    /// Independent admission actor (Grok SubagentCoordinator).
    coordinator: Arc<SubagentCoordinator>,
    /// Background agent jobs (completion → notification queue).
    jobs: Arc<AgentJobRegistry>,
    /// Parent tool_call id → live job id (for TUI click-to-open while running).
    tool_jobs: Mutex<HashMap<String, String>>,
    /// Parent Langfuse sink — children fork under the open `task` tool span.
    langfuse: RwLock<Option<Arc<crate::langfuse::LangfuseTraceSink>>>,
    /// Inherit `--trace-full` for nested subagent I/O previews.
    langfuse_trace_full: std::sync::atomic::AtomicBool,
}

impl TaskToolHost {
    pub fn new(
        opts: HarnessOptions,
        parent_agent: AgentSpec,
        jobs: Arc<AgentJobRegistry>,
        parent_path_policy: PathPolicy,
    ) -> Arc<Self> {
        let max_c = parent_agent.spawn_policy.max_concurrent.max(1) as usize;
        let coordinator = SubagentCoordinator::new(jobs.clone(), CoordinatorConfig::from_env(max_c));
        Arc::new(Self {
            provider: RwLock::new(None),
            opts: RwLock::new(opts),
            parent_path_policy: RwLock::new(parent_path_policy),
            parent_agent: RwLock::new(parent_agent),
            parent_run_id: RwLock::new(format!("run_{}", uuid_simple())),
            parent_session_id: RwLock::new(None),
            parent_depth: AtomicU32::new(0),
            coordinator,
            jobs,
            tool_jobs: Mutex::new(HashMap::new()),
            langfuse: RwLock::new(None),
            langfuse_trace_full: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Attach the parent process Langfuse sink (called once from runtime build).
    pub async fn set_langfuse(
        &self,
        sink: Option<Arc<crate::langfuse::LangfuseTraceSink>>,
        trace_full: bool,
    ) {
        self.langfuse_trace_full
            .store(trace_full, Ordering::Relaxed);
        *self.langfuse.write().await = sink;
    }

    /// Build a nested child trace under the open `task` tool span (or parent root).
    pub async fn child_trace(
        &self,
        tool_call_id: &str,
        agent_name: &str,
        parent: &RunParent,
    ) -> (
        Option<one_core::SharedTrace>,
        Option<one_core::TraceRunMeta>,
    ) {
        let parent_sink = self.langfuse.read().await.clone();
        let Some(parent_sink) = parent_sink else {
            return (None, None);
        };
        // Prefer nesting under the live `task` tool span; fall back to agent root
        // (background jobs may race if tool already closed — rare for sync path).
        let parent_cx = parent_sink
            .context_for_tool(tool_call_id)
            .or_else(|| parent_sink.root_context());
        let Some(parent_cx) = parent_cx else {
            tracing::debug!(
                tool_call_id,
                "langfuse: no parent span for subagent; skipping child trace"
            );
            return (None, None);
        };
        let child = parent_sink.fork_child(parent_cx);
        let session_id = self.parent_session_id.read().await.clone();
        let trace_full = self.langfuse_trace_full.load(Ordering::Relaxed);
        let meta = one_core::TraceRunMeta {
            task_id: Some(format!("subagent:{agent_name}")),
            agent_version: Some(env!("CARGO_PKG_VERSION").into()),
            config: Some(json!({
                "subagent": true,
                "agent": agent_name,
                "parent_run_id": parent.run_id,
                "parent_tool_use_id": parent.tool_use_id,
                "depth": parent.depth,
            })),
            session_id,
            user_id: crate::langfuse::user_id_from_env(),
            trace_full,
        };
        let shared: one_core::SharedTrace = child;
        (Some(shared), Some(meta))
    }

    /// Record that parent `tool_call_id` is backed by live `job_id`.
    pub fn bind_tool_job(&self, tool_call_id: &str, job_id: &str) {
        if let Ok(mut m) = self.tool_jobs.lock() {
            m.insert(tool_call_id.to_string(), job_id.to_string());
        }
    }

    pub fn unbind_tool_job(&self, tool_call_id: &str) {
        if let Ok(mut m) = self.tool_jobs.lock() {
            m.remove(tool_call_id);
        }
    }

    /// Look up live job id for a parent tool call (if still bound).
    pub fn job_for_tool(&self, tool_call_id: &str) -> Option<String> {
        self.tool_jobs
            .lock()
            .ok()
            .and_then(|m| m.get(tool_call_id).cloned())
    }

    /// All active tool→job bindings (for TUI: refresh running task rows).
    pub fn tool_job_bindings(&self) -> Vec<(String, String)> {
        self.tool_jobs
            .lock()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// If plan↔act rebuild ever replaces policy shell, re-bind the same dynamic Arc.
    pub async fn set_parent_path_policy(&self, policy: PathPolicy) {
        *self.parent_path_policy.write().await = policy;
    }

    /// Snapshot parent grants into harness opts at spawn time.
    pub async fn child_harness_opts(&self) -> HarnessOptions {
        let mut opts = self.opts.read().await.clone();
        let exported = self.parent_path_policy.read().await.export_read_grants();
        opts.parent_read_grants = ParentReadGrants::from(exported);
        opts
    }

    pub fn jobs(&self) -> Arc<AgentJobRegistry> {
        self.jobs.clone()
    }

    pub async fn bind_provider(&self, provider: Arc<dyn LlmProvider>) {
        *self.provider.write().await = Some(provider);
    }

    pub async fn set_session_id(&self, id: Option<String>) {
        *self.parent_session_id.write().await = id;
    }

    pub async fn set_run_id(&self, id: impl Into<String>) {
        *self.parent_run_id.write().await = id.into();
    }

    pub async fn set_parent_agent(&self, agent: AgentSpec) {
        let max_c = agent.spawn_policy.max_concurrent.max(1) as usize;
        self.coordinator.resize(max_c);
        *self.parent_agent.write().await = agent;
    }

    pub fn coordinator(&self) -> Arc<SubagentCoordinator> {
        self.coordinator.clone()
    }

    pub async fn set_opts(&self, opts: HarnessOptions) {
        *self.opts.write().await = opts;
    }

    /// Refresh MCP / extension tools available to child harness runs (`tools.mcp`).
    pub async fn set_dynamic_tools(&self, tools: Vec<std::sync::Arc<dyn one_core::tool::Tool>>) {
        self.opts.write().await.dynamic_tools = tools;
    }

    pub fn set_depth(&self, depth: u32) {
        self.parent_depth.store(depth, Ordering::Relaxed);
    }

    pub fn can_spawn(&self) -> bool {
        // Sync read of spawn policy via try_lock — only for registration checks.
        // Fall back to true if locked (tool already running).
        match self.parent_agent.try_read() {
            Ok(a) => a.can_spawn(),
            Err(_) => true,
        }
    }
}

fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{t:x}")
}

/// Injectable harness for unit tests (FakeRunner) and production (DefaultHarness).
#[async_trait]
pub trait TaskHarness: Send + Sync {
    async fn run(&self, req: RunRequest) -> RunResult;
}

/// Production harness: `harness::run` with the host's bound provider.
pub struct DefaultTaskHarness {
    host: Arc<TaskToolHost>,
}

impl DefaultTaskHarness {
    pub fn new(host: Arc<TaskToolHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl TaskHarness for DefaultTaskHarness {
    async fn run(&self, req: RunRequest) -> RunResult {
        let provider = self.host.provider.read().await.clone();
        let Some(provider) = provider else {
            return RunResult::failure(
                ProtocolError::new(
                    error_code::INTERNAL,
                    "task tool has no LLM provider bound (call bind_provider)",
                ),
                0,
            );
        };
        let opts = self.host.child_harness_opts().await;
        harness::run(req, provider.as_ref(), &opts).await
    }
}

/// Meta-tool `task` — only registered on parent agents that may spawn.
pub struct TaskTool {
    harness: Arc<dyn TaskHarness>,
    host: Arc<TaskToolHost>,
}

impl TaskTool {
    pub fn new(host: Arc<TaskToolHost>) -> Self {
        Self {
            harness: Arc::new(DefaultTaskHarness::new(host.clone())),
            host,
        }
    }

    /// Test / alternate injection.
    pub fn with_harness(host: Arc<TaskToolHost>, harness: Arc<dyn TaskHarness>) -> Self {
        Self { harness, host }
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn definition(&self) -> ToolDefinition {
        let allowed = match self.host.parent_agent.try_read() {
            Ok(a) if !a.spawn_policy.allow.is_empty() => a.spawn_policy.allow.join(", "),
            _ => "explore".into(),
        };
        let child_blurb = match self.host.parent_agent.try_read() {
            Ok(a) => {
                let mut parts = Vec::new();
                for name in &a.spawn_policy.allow {
                    if name == "*" {
                        continue;
                    }
                    if let Some(child) = a.agents.get(name) {
                        if let Some(d) = &child.description {
                            parts.push(format!("{name}: {d}"));
                            continue;
                        }
                    }
                    parts.push(name.clone());
                }
                if parts.is_empty() {
                    String::new()
                } else {
                    format!(" Available agents — {}.", parts.join(" | "))
                }
            }
            Err(_) => String::new(),
        };
        ToolDefinition {
            name: "task".into(),
            description: format!(
                "Spawn a sub-agent (same harness as `one agent run`). Isolated context; \
returns a summary so this conversation stays small. Do not use for a single trivial file read. \
Default agent=explore when allowed. Types: explore (read-only research), plan (read-only plan), \
general / general-purpose (writable). \
**Do not use explore for git/VCS workflows** — explore has no bash. \
Allowed agent names: [{allowed}].{child_blurb} \
**background defaults to true** (Grok-aligned): returns status=started + job_id immediately; \
when done a [job completed] notice is injected (or poll with job_output / wait_tasks). \
Set background=false to **block until the child finishes**. A dedicated coordinator \
queues excess spawns (does not block the caller). Auto-background \
(details.backgrounded=true) only happens if the parent is still queued for a concurrent \
slot longer than ONE_TASK_FOREGROUND_BUDGET_MS (~1s) — not because the child is slow. \
resume_from=<job_id> continues a completed sibling's transcript. \
capability_mode=read-only|read-write|execute|all optionally filters tools. \
The sub-agent cannot ask the user questions; if it lacks info it ends with ERROR:."
            ),
            parameters: json!({
                "type": "object",
                "required": ["prompt"],
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Task for the sub-agent (be specific about scope / symbols)"
                    },
                    "description": {
                        "type": "string",
                        "description": "Short label for logs / TUI (3–8 words)"
                    },
                    "agent": {
                        "type": "string",
                        "description": format!(
                            "Preset / child name (one of: {allowed}; default explore if allowed)"
                        )
                    },
                    "subagent_type": {
                        "type": "string",
                        "description": "Grok-style alias for agent (explore | plan | general-purpose)"
                    },
                    "mode": {
                        "type": "string",
                        "description": "Alias for agent (Claude-style)"
                    },
                    "agent_spec": {
                        "type": "object",
                        "description": "Optional full AgentSpec JSON override (must still pass spawn_policy)"
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Default true: return immediately with job_id (queued if slots are full). False blocks until the child ends. Auto-bg only if still queued for a slot after ~1s."
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "Alias for background (Grok spawn_subagent name)"
                    },
                    "isolation": {
                        "type": "string",
                        "description": "none (default, shared cwd) | worktree (isolated git worktree; no auto-merge)"
                    },
                    "capability_mode": {
                        "type": "string",
                        "description": "Optional tool filter: read-only | read-write | execute | all"
                    },
                    "resume_from": {
                        "type": "string",
                        "description": "Completed job_id whose transcript/model/cwd to continue"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory for the child. Ignored when isolation=worktree or resume_from is set."
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model id override for this child"
                    }
                }
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let prompt = call
            .arguments
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("task", "missing non-empty `prompt`"))?
            .to_string();

        let description = call
            .arguments
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let agent_name = resolve_agent_name(&call.arguments);
        let background = parse_background(&call.arguments);
        let isolation_arg = call
            .arguments
            .get("isolation")
            .and_then(|v| v.as_str())
            .and_then(IsolationMode::parse);
        let capability_mode = call
            .arguments
            .get("capability_mode")
            .and_then(|v| v.as_str())
            .and_then(CapabilityMode::parse);
        let resume_from = call
            .arguments
            .get("resume_from")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let cwd_arg = call
            .arguments
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let model_override = call
            .arguments
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let parent_agent = self.host.parent_agent.read().await.clone();
        let parent_depth = self.host.parent_depth.load(Ordering::Relaxed);
        let child_depth = parent_depth + 1;

        let parent_meta = RunParent {
            run_id: self.host.parent_run_id.read().await.clone(),
            session_id: self.host.parent_session_id.read().await.clone(),
            tool_use_id: call.id.clone(),
            agent_name: Some(parent_agent.display_name().to_string()),
            depth: child_depth,
        };

        let mut req = match build_child_request(
            &parent_agent,
            &agent_name,
            &prompt,
            parent_meta.clone(),
            call.arguments.get("agent_spec"),
            &self.host.opts.read().await.cwd,
        ) {
            Ok(r) => r,
            Err(e) => {
                return Ok(format_task_output(
                    &agent_name,
                    description.as_deref(),
                    &RunResult::failure(e, 0).with_status(TaskExitStatus::RuntimeError),
                ));
            }
        };
        // M4: settings.memory.subagent may opt-in Index for children still at Off.
        apply_subagent_memory_settings(&mut req.agent);
        if let Some(mode) = capability_mode {
            apply_capability_mode(&mut req.agent, mode);
        }
        apply_mcp_inheritance(&mut req.agent);
        if let Some(ref mid) = model_override {
            req.agent.model.id = Some(mid.clone());
            req.agent.model.inherit = false;
        }
        if let Some(iso) = isolation_arg {
            req.agent.isolation = iso;
        }
        // resume_from: inherit transcript / cwd; isolation=worktree + explicit cwd ignored.
        if let Some(ref src_id) = resume_from {
            if let Err(e) = apply_resume_from(
                &self.host,
                &mut req,
                src_id,
                &parent_agent,
                &agent_name,
                &parent_meta,
            ) {
                return Ok(format_task_output(
                    &agent_name,
                    description.as_deref(),
                    &RunResult::failure(e, 0).with_status(TaskExitStatus::RuntimeError),
                ));
            }
        } else if let Some(ref cwd) = cwd_arg {
            if matches!(req.agent.isolation, IsolationMode::Worktree) {
                return Ok(format_task_output(
                    &agent_name,
                    description.as_deref(),
                    &RunResult::failure(
                        ProtocolError::new(
                            error_code::INVALID_REQUEST,
                            "cwd is mutually exclusive with isolation=worktree",
                        ),
                        0,
                    )
                    .with_status(TaskExitStatus::RuntimeError),
                ));
            }
            req.agent.cwd = Some(cwd.clone());
        }
        // Default: writable children in background → worktree when still none.
        if background
            && matches!(req.agent.isolation, IsolationMode::None)
            && child_tools_look_writable(&req.agent)
        {
            req.agent.isolation = IsolationMode::Worktree;
        }

        // Fail closed: read-only explore cannot inspect VCS via bash — do not burn
        // 10+ turns reading `.git/index` and return a fake success summary.
        if child_lacks_bash(&req.agent) && prompt_looks_like_vcs_workflow(&prompt) {
            let msg = "ERROR: need bash (explore/read-only subagent has no shell; \
parent must run `git status --short` / `git diff --stat` / stage+commit directly). \
Do not re-delegate git workflows to explore.";
            let mut rr = RunResult::success(msg, 0).with_status(TaskExitStatus::IncompleteInfo);
            rr.stop_reason = Some("incomplete_info".into());
            rr.error = Some(ProtocolError::new(error_code::INVALID_REQUEST, msg));
            return Ok(format_task_output(
                &agent_name,
                description.as_deref(),
                &rr,
            ));
        }

        // Validate tools can materialize before spending a slot on LLM work.
        if let Err(e) = validate_child_tools(&req.agent) {
            return Ok(format_task_output(
                &agent_name,
                description.as_deref(),
                &RunResult::failure(e, 0).with_status(TaskExitStatus::RuntimeError),
            ));
        }

        // Nest Langfuse under this `task` tool span (same OTEL trace as parent).
        let (child_trace, child_trace_meta) = self
            .host
            .child_trace(&call.id, &agent_name, &parent_meta)
            .await;

        // Bound provider → independent coordinator (Start / Enqueue / Reject).
        // Unbound provider (unit tests / FakeHarness) skips admission.
        if let Some(provider) = self.host.provider.read().await.clone() {
            let opts = self.host.child_harness_opts().await;
            let session_id = parent_meta.session_id.clone();
            let prompt_snap = req.prompt.text.clone();
            let cwd_snap = req.agent.cwd.clone();
            let reply = self
                .host
                .coordinator
                .spawn(SpawnSpec {
                    req,
                    provider,
                    opts,
                    agent_name: agent_name.clone(),
                    description: description.clone(),
                    background,
                    spawn_opts: SpawnOptions {
                        notify_completion: background,
                        apply_wall_timeout: true,
                        trace: child_trace,
                        trace_meta: child_trace_meta,
                        acquire_slot: None,
                    },
                    session_id,
                    prompt: prompt_snap,
                    cwd: cwd_snap,
                })
                .await;

            if reply.rejected {
                return Ok(format_task_output(
                    &agent_name,
                    description.as_deref(),
                    &reply.result.unwrap_or_else(|| {
                        RunResult::failure(
                            ProtocolError::new(error_code::SPAWN_NOT_ALLOWED, "admission rejected"),
                            0,
                        )
                        .with_status(TaskExitStatus::RuntimeError)
                    }),
                ));
            }

            if !reply.job_id.is_empty() {
                self.host.bind_tool_job(&call.id, &reply.job_id);
            }

            if let Some(result) = reply.result {
                self.host.unbind_tool_job(&call.id);
                let log_path = self
                    .host
                    .jobs
                    .get(&reply.job_id)
                    .and_then(|s| s.log_path.map(|p| p.display().to_string()));
                return Ok(stamp_job_output(
                    format_task_output(&agent_name, description.as_deref(), &result),
                    &reply.job_id,
                    &result,
                    log_path,
                ));
            }

            let log_path = self
                .host
                .jobs
                .get(&reply.job_id)
                .and_then(|s| s.log_path.map(|p| p.display().to_string()));
            return Ok(format_task_started(
                &agent_name,
                description.as_deref(),
                &reply.job_id,
                log_path.as_deref(),
                reply.backgrounded,
                reply.queued,
            ));
        }

        if background {
            return Ok(format_task_output(
                &agent_name,
                description.as_deref(),
                &RunResult::failure(
                    ProtocolError::new(
                        error_code::INTERNAL,
                        "task tool has no LLM provider bound (call bind_provider)",
                    ),
                    0,
                )
                .with_status(TaskExitStatus::RuntimeError),
            ));
        }

        // Sync fallback (tests / unbound provider): no admission.
        let result = self.harness.run(req).await;
        Ok(format_task_output(
            &agent_name,
            description.as_deref(),
            &result,
        ))
    }
}

fn resolve_agent_name(args: &Value) -> String {
    for key in ["subagent_type", "agent", "mode"] {
        if let Some(a) = args.get(key).and_then(|v| v.as_str()) {
            let t = a.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    "explore".into()
}

/// Grok-aligned default: background unless the caller explicitly waits.
fn parse_background(args: &Value) -> bool {
    args.get("background")
        .or_else(|| args.get("run_in_background"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn apply_capability_mode(agent: &mut AgentSpec, mode: CapabilityMode) {
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
            ];
            agent.tools.mcp = false;
            agent.mcp_inheritance = McpInheritance::None;
        }
        CapabilityMode::ReadWrite => {
            agent.tools.profile = ToolProfile::Coding;
            agent.tools.allow.clear();
            for n in ["bash", "bash_output", "bash_kill", "monitor", "ask_user"] {
                if !agent.tools.deny.iter().any(|d| d == n) {
                    agent.tools.deny.push(n.into());
                }
            }
        }
        CapabilityMode::Execute => {
            agent.tools.profile = ToolProfile::Coding;
            agent.tools.allow.clear();
            for n in ["write", "edit", "ask_user"] {
                if !agent.tools.deny.iter().any(|d| d == n) {
                    agent.tools.deny.push(n.into());
                }
            }
        }
        CapabilityMode::All => {
            agent.tools = ToolsSpec::coding();
            agent.tools.deny = vec!["ask_user".into(), "task".into()];
            agent.tools.mcp = true;
            agent.mcp_inheritance = McpInheritance::All;
        }
    }
}

fn apply_mcp_inheritance(agent: &mut AgentSpec) {
    match agent.mcp_inheritance {
        McpInheritance::All => agent.tools.mcp = true,
        McpInheritance::None => {
            // Leave explicit tools.mcp=true from capability_mode=all / general.
        }
    }
}

fn apply_resume_from(
    host: &TaskToolHost,
    req: &mut RunRequest,
    src_id: &str,
    parent_agent: &AgentSpec,
    agent_name: &str,
    parent_meta: &RunParent,
) -> std::result::Result<(), ProtocolError> {
    let src = host.jobs.resume_source(src_id).ok_or_else(|| {
        ProtocolError::new(
            error_code::INVALID_REQUEST,
            format!("resume_from `{src_id}` is unknown or still running"),
        )
    })?;
    let want = normalize_agent_name(agent_name);
    let have = normalize_agent_name(&src.agent);
    if want != have {
        return Err(ProtocolError::new(
            error_code::INVALID_REQUEST,
            format!(
                "resume_from type mismatch: source is `{}`, requested `{}`",
                src.agent, agent_name
            ),
        ));
    }
    if let (Some(parent_sid), Some(src_sid)) = (
        parent_meta.session_id.as_deref(),
        src.parent_session_id.as_deref(),
    ) {
        if parent_sid != src_sid {
            return Err(ProtocolError::new(
                error_code::INVALID_REQUEST,
                "resume_from job belongs to a different parent session",
            ));
        }
    }
    // Grok: isolation/cwd ignored on resume — inherit source cwd / worktree path.
    req.agent.isolation = IsolationMode::None;
    if let Some(path) = src.worktree_path.or(src.cwd) {
        req.agent.cwd = Some(path);
    }
    req.seed_messages = src.transcript;
    if req.seed_messages.is_empty() && (!src.prompt.is_empty() || !src.summary.is_empty()) {
        let fallback = format!(
            "[resumed from {src_id} · {}]\nPrevious task:\n{}\n\nPrevious summary:\n{}",
            src.agent, src.prompt, src.summary
        );
        req.seed_messages.push(serde_json::json!({
            "role": "user",
            "content": fallback,
        }));
    }
    let _ = parent_agent;
    Ok(())
}

fn stamp_job_output(
    mut out: ToolOutput,
    job_id: &str,
    result: &RunResult,
    log_path: Option<String>,
) -> ToolOutput {
    if let Some(details) = out.details.as_mut() {
        if let Some(obj) = details.as_object_mut() {
            obj.insert("job_id".into(), json!(job_id));
            if let Some(ref p) = log_path {
                obj.insert("log_path".into(), json!(p));
            }
        }
    }
    if let Some(first) = out.content.first_mut() {
        if let one_core::message::TextOrImage::Text { text } = first {
            if let Some(rest) = text.strip_prefix('[') {
                if let Some(end) = rest.find(']') {
                    let inside = &rest[..end];
                    if !inside.contains("id=") {
                        let stamped = format!("[{inside} · id={job_id}]{}", &rest[end + 1..]);
                        *text = stamped;
                    }
                }
            }
            if !result.ok {
                if let Some(ref p) = log_path {
                    if !text.contains("log_path:") {
                        text.push_str(&format!("\nlog_path: {p}"));
                    }
                }
            }
        }
    }
    out
}

/// True when the child agent will not get a `bash` tool (explore / read-only faces).
fn child_lacks_bash(spec: &AgentSpec) -> bool {
    use crate::protocol::ToolProfile;
    if spec.display_name() == "explore" {
        return true;
    }
    if spec.tools.deny.iter().any(|n| n == "bash") {
        return true;
    }
    if !spec.tools.allow.is_empty() {
        return !spec.tools.allow.iter().any(|n| n == "bash");
    }
    matches!(
        spec.tools.profile,
        ToolProfile::ReadOnly | ToolProfile::Plan | ToolProfile::None
    )
}

/// Heuristic: task needs live git/shell, which explore cannot provide.
pub(crate) fn prompt_looks_like_vcs_workflow(prompt: &str) -> bool {
    let p = prompt.to_ascii_lowercase();
    const KEYS: &[&str] = &[
        "git status",
        "git diff",
        "git commit",
        "git add",
        "git reset",
        "git log",
        "git stash",
        "git restore",
        "staged",
        "unstaged",
        "working tree",
        "uncommitted",
        "diff --stat",
        "diff --cached",
        "categorize repository",
        "categorize repo",
        "repository changes",
        "repo changes",
        "commit by feature",
        "by feature commit",
        "split commit",
        "split into commit",
        "prepare commit",
        "create commit",
        "make commit",
        "stage files",
        "commit message",
        "commit splitting",
    ];
    if KEYS.iter().any(|k| p.contains(k)) {
        return true;
    }
    // "commit" alone is too broad; require a staging/diff signal nearby.
    p.contains("commit")
        && (p.contains("stage")
            || p.contains("diff")
            || p.contains("change")
            || p.contains("feature")
            || p.contains("split"))
}

/// Apply `settings.memory.subagent` (default off). Only **upgrades** Off → Index
/// so an explicit AgentSpec `resources.memory: index` is never cleared.
///
/// No-op when feature `memory` is off (whole package disabled).
fn apply_subagent_memory_settings(agent: &mut AgentSpec) {
    use crate::protocol::MemoryResourceMode;
    use crate::runtime::features::{env_no_memory, FeatureState};
    if !matches!(agent.resources.memory, MemoryResourceMode::Off) {
        return;
    }
    let settings = crate::settings::load();
    let features =
        FeatureState::from_settings(&settings).with_process_overrides(false, env_no_memory());
    if !features.memory_enabled() {
        return;
    }
    let mode = settings.memory_or_default().subagent_mode();
    if mode.is_index() {
        agent.resources.memory = MemoryResourceMode::Index;
    }
}

fn child_tools_look_writable(spec: &AgentSpec) -> bool {
    use super::tool_materialize::resolve_names;
    let prefer = spec.display_name() == "explore"
        || (matches!(spec.tools.profile, crate::protocol::ToolProfile::ReadOnly)
            && spec.tools.allow.is_empty());
    let names = resolve_names(&spec.tools, prefer);
    names.iter().any(|n| {
        matches!(
            n.as_str(),
            "write" | "edit" | "bash" | "bash_output" | "bash_kill"
        )
    })
}

/// Ensure AgentSpec.tools resolve against the builtin registry.
/// MCP names are allowed when `tools.mcp` is true (filled at run from host dynamic_tools).
fn validate_child_tools(spec: &AgentSpec) -> std::result::Result<(), ProtocolError> {
    use super::tool_materialize::{harness_build_context, harness_registry, resolve_names};
    use one_tools::PathPolicy;
    let prefer_explore = spec.display_name() == "explore"
        || (matches!(spec.tools.profile, crate::protocol::ToolProfile::ReadOnly)
            && spec.tools.allow.is_empty());
    let names = resolve_names(&spec.tools, prefer_explore);
    let reg = harness_registry();
    let mut unknown = Vec::new();
    for n in &names {
        let is_mcp = n.contains("__");
        if is_mcp {
            if !spec.tools.mcp {
                unknown.push(format!("{n} (MCP tool but tools.mcp=false)"));
            }
            continue;
        }
        if n == "plan" || n == "exit_plan_mode" {
            // plan tools are main-session only unless registered as instances
            unknown.push(n.clone());
            continue;
        }
        if !reg.contains(n) {
            unknown.push(n.clone());
        }
    }
    if !unknown.is_empty() {
        return Err(ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            format!(
                "unknown tool(s) for agent `{}`: {}",
                spec.display_name(),
                unknown.join(", ")
            ),
        ));
    }
    // Dry-run materialize builtins (ensures factories work).
    let ctx = harness_build_context(PathPolicy::workspace(std::path::PathBuf::from(".")), true);
    let builtin_only: Vec<String> = names
        .into_iter()
        .filter(|n| !n.contains("__") && n != "plan" && n != "exit_plan_mode")
        .collect();
    reg.materialize(&builtin_only, &ctx).map_err(|e| {
        ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            format!("tool materialize failed: {e}"),
        )
    })?;
    Ok(())
}

fn build_child_request(
    parent_agent: &AgentSpec,
    agent_name: &str,
    prompt: &str,
    parent: RunParent,
    agent_spec: Option<&Value>,
    cwd: &std::path::Path,
) -> std::result::Result<RunRequest, ProtocolError> {
    if let Some(v) = agent_spec {
        if !v.is_null() {
            // Inline full AgentSpec: still must be allow-listed by name.
            if !parent_agent.spawn_allowed(agent_name) {
                return Err(ProtocolError::new(
                    error_code::SPAWN_NOT_ALLOWED,
                    format!("agent `{agent_name}` not in spawn_policy.allow"),
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
            let mut child: AgentSpec = serde_json::from_value(v.clone()).map_err(|e| {
                ProtocolError::new(
                    error_code::INVALID_AGENT_SPEC,
                    format!("invalid agent_spec: {e}"),
                )
            })?;
            if child.name.is_none() {
                child.name = Some(agent_name.to_string());
            }
            // Nested children never spawn further in MVP.
            child.spawn_policy = crate::protocol::SpawnPolicy::none();
            let mut req = RunRequest::new(child, prompt);
            req.session.mode = SessionMode::Ephemeral;
            req.parent = Some(parent);
            return Ok(req);
        }
    }

    // Preset / agents table path.
    if parent_agent.spawn_allowed(agent_name) {
        // Prefer parent's agents table / builtin alias.
        if let Ok(req) = RunRequest::child(parent_agent, agent_name, prompt, parent.clone()) {
            return Ok(req);
        }
    }

    // Fall back to loading preset from disk / builtin (still need allow).
    if !parent_agent.spawn_allowed(agent_name) {
        return Err(ProtocolError::new(
            error_code::SPAWN_NOT_ALLOWED,
            format!("agent `{agent_name}` not in spawn_policy.allow"),
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
    let child = presets::load_preset(agent_name, cwd)?;
    let mut req = RunRequest::new(child, prompt);
    req.session.mode = SessionMode::Ephemeral;
    req.parent = Some(parent);
    Ok(req)
}

/// Immediate ack for background spawn.
pub fn format_task_started(
    agent_name: &str,
    description: Option<&str>,
    job_id: &str,
    log_path: Option<&str>,
    backgrounded: bool,
    queued: bool,
) -> ToolOutput {
    let desc_part = description.map(|d| format!(" · {d}")).unwrap_or_default();
    let auto = if backgrounded {
        " Concurrent slot wait exceeded — handed off to background (child had not started)."
    } else if queued {
        " No free slot — queued; starts when another subagent finishes."
    } else {
        ""
    };
    let mut text = format!(
        "[task · {agent_name}{desc_part} · status=started · id={job_id}]\n\
         Background job started.{auto} Continue other work.\n\
         Result arrives as a [job completed] notice before the next LLM turn, \
         or poll with job_output(job_id=\"{job_id}\")."
    );
    if let Some(p) = log_path {
        text.push_str(&format!("\nlog_path: {p}"));
    }
    let details = json!({
        "ok": true,
        "status": "started",
        "job_id": job_id,
        "background": true,
        "backgrounded": backgrounded,
        "queued": queued,
        "agent": agent_name,
        "description": description,
        "log_path": log_path,
    });
    ToolOutput::text_with_details(text, details)
}

/// Format tool_result: summary + status trailer + structured details.
pub fn format_task_output(
    agent_name: &str,
    description: Option<&str>,
    result: &RunResult,
) -> ToolOutput {
    let status = result.status.unwrap_or(if result.ok {
        TaskExitStatus::Success
    } else {
        TaskExitStatus::RuntimeError
    });
    let status_s = status.as_str();
    let desc_part = description.map(|d| format!(" · {d}")).unwrap_or_default();
    let header = format!("[task · {agent_name}{desc_part} · status={status_s}]");

    let mut body = result.result.clone();
    if body.len() > SUMMARY_MAX_CHARS {
        body.truncate(SUMMARY_MAX_CHARS);
        body.push_str("\n…[truncated]");
    }

    let text = match status {
        TaskExitStatus::IncompleteInfo => format!(
            "{header}\n\
             Sub-agent could not finish without clarification (do not treat as a question to the user).\n\
             Partial findings:\n{body}"
        ),
        TaskExitStatus::TimedOut => {
            let err = result
                .error
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "wall time exceeded".into());
            if body.is_empty() {
                format!("{header}\nTimeout: {err}")
            } else {
                format!("{header}\nTimeout: {err}\nPartial:\n{body}")
            }
        }
        TaskExitStatus::Aborted => {
            let err = result
                .error
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "aborted".into());
            if body.is_empty() {
                format!("{header}\nAborted: {err}")
            } else {
                format!("{header}\nAborted: {err}\nPartial:\n{body}")
            }
        }
        TaskExitStatus::MaxTurnsExceeded => {
            if body.is_empty() {
                format!("{header}\nMax turns exceeded (partial empty).")
            } else {
                format!("{header}\nMax turns exceeded. Partial findings:\n{body}")
            }
        }
        _ if !result.ok || status.is_failure() => {
            let err = result
                .error
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown error".into());
            if body.is_empty() {
                format!("{header}\nError: {err}")
            } else {
                format!("{header}\nError: {err}\nPartial:\n{body}")
            }
        }
        _ if body.is_empty() => format!("{header}\n(no summary)"),
        _ => format!("{header}\n{body}"),
    };

    let details = json!({
        "ok": result.ok && status.is_ok(),
        "status": status_s,
        "agent": agent_name,
        "description": description,
        "duration_ms": result.duration_ms,
        "turns": result.turns,
        "stop_reason": result.stop_reason,
        "error": result.error,
        "usage": result.usage,
        "parent": result.parent,
        "worktree": result.worktree,
    });

    ToolOutput::text_with_details(text, details)
}

/// Build parent-facing tool text from a finished job snapshot (TUI / recovery).
pub fn format_task_output_from_job(snap: &super::jobs::JobSnapshot) -> (bool, String) {
    let status = snap.status.unwrap_or(if snap.ok {
        TaskExitStatus::Success
    } else {
        TaskExitStatus::RuntimeError
    });
    let is_error = !snap.ok || status.is_failure();
    let desc_part = snap
        .description
        .as_ref()
        .filter(|d| !d.is_empty())
        .map(|d| format!(" · {d}"))
        .unwrap_or_default();
    let header = format!(
        "[task · {}{desc_part} · status={} · id={}]",
        snap.agent,
        status.as_str(),
        snap.id
    );
    let mut body = snap.summary.clone();
    if body.len() > SUMMARY_MAX_CHARS {
        body.truncate(SUMMARY_MAX_CHARS);
        body.push_str("\n…[truncated]");
    }
    let text = if is_error {
        let err = snap.error.clone().unwrap_or_else(|| status.as_str().into());
        let kind = match status {
            TaskExitStatus::TimedOut => "Timeout",
            TaskExitStatus::Aborted => "Aborted",
            _ => "Error",
        };
        if body.is_empty() {
            format!("{header}\n{kind}: {err}")
        } else {
            format!("{header}\n{kind}: {err}\nPartial:\n{body}")
        }
    } else if body.is_empty() {
        format!("{header}\n(no summary)")
    } else {
        format!("{header}\n{body}")
    };
    (is_error, text)
}

/// One-liner for main agent system prompt.
pub const TASK_TOOL_PROMPT_HINT: &str = "\
- To delegate work to a sub-agent, use the `task` tool. `agent` / `subagent_type` / `mode` select the role (explore = read-only research; plan = read-only implementation plan; general / general-purpose = writable coding). Default agent is explore when allowed. Findings return as a summary so this conversation stays small. Do not use task for a trivial single-file read, and do not use task to implement code when you already have the design/context — edit/write yourself. **Never use explore/task for git workflows** — explore has no bash; run compact git via bash yourself. **background defaults to true** (returns status=started + job_id; completion is a [job completed] notice). Set background=false when you need the summary this turn — that **waits for the child to finish**. Auto-background only happens if all concurrent slots are busy for ~1s (admission), not because the child is slow. Use resume_from=<job_id> to continue a completed sibling. capability_mode=read-only|read-write|execute|all optionally filters tools. When you have spawned all background work and have nothing else to do, call wait_tasks (mode=all) with no wait_ms. job_output without wait_ms is a snapshot. job_output / job_kill accept both `job_*` and `bg_*` ids. For agents that write/edit/bash, prefer isolation=worktree (background writable children default to worktree).";

/// Build parent AgentSpec for the interactive / -p main agent.
///
/// Load order:
/// 1. `cwd/.one/agents/main.json` or `default.json`
/// 2. `~/.one/agent/agents/main.json` or `default.json`
/// 3. builtin main (spawn allow: explore only)
///
/// Then merges every other `*.json` under project/user agents dirs into
/// `agents` + `spawn_policy.allow` (so defining a new worker is drop-a-file).
pub fn main_parent_agent_spec() -> AgentSpec {
    main_parent_agent_spec_for_cwd(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Same as [`main_parent_agent_spec`] with an explicit project cwd.
pub fn main_parent_agent_spec_for_cwd(cwd: &std::path::Path) -> AgentSpec {
    let mut spec = if let Ok(s) = super::presets::load_preset("main", cwd) {
        s
    } else if let Ok(s) = super::presets::load_preset("default", cwd) {
        s
    } else {
        AgentSpec::builtin_main()
    };
    merge_disk_main(&mut spec);
    super::presets::merge_discovered_agents(&mut spec, cwd);
    spec
}

fn merge_disk_main(spec: &mut AgentSpec) {
    if spec.name.is_none() {
        spec.name = Some("main".into());
    }
    // Ensure builtin children exist when allow lists them but the table omits them.
    if spec.spawn_allowed("explore") && !spec.agents.contains_key("explore") {
        spec.agents
            .insert("explore".into(), AgentSpec::builtin_explore());
    }
    if spec.spawn_allowed("plan") && !spec.agents.contains_key("plan") {
        spec.agents.insert("plan".into(), AgentSpec::builtin_plan());
    }
    if spec.spawn_allowed("general") && !spec.agents.contains_key("general") {
        spec.agents
            .insert("general".into(), AgentSpec::builtin_general());
    }
}

/// HarnessOptions from runtime path policy fields.
pub fn harness_opts_from_policy(
    cwd: PathBuf,
    full_access: bool,
    add_dirs: Vec<PathBuf>,
    auto_approve: bool,
) -> HarnessOptions {
    HarnessOptions {
        cwd,
        full_access,
        add_dirs,
        auto_approve,
        dynamic_tools: vec![],
        parent_read_grants: ParentReadGrants::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct FakeHarness {
        summary: String,
        status: TaskExitStatus,
    }

    #[async_trait]
    impl TaskHarness for FakeHarness {
        async fn run(&self, req: RunRequest) -> RunResult {
            let mut rr = RunResult::success(format!("{}|{}", self.summary, req.prompt.text), 42)
                .with_status(self.status);
            rr.turns = Some(2);
            rr.parent = req.parent;
            if !self.status.is_ok() {
                rr.ok = false;
            }
            rr
        }
    }

    fn test_host() -> Arc<TaskToolHost> {
        let jobs = AgentJobRegistry::new(Arc::new(std::sync::Mutex::new(Vec::new())));
        TaskToolHost::new(
            HarnessOptions::from_cwd(std::env::temp_dir()),
            AgentSpec::builtin_main(),
            jobs,
            PathPolicy::workspace(std::env::temp_dir()),
        )
    }

    #[test]
    fn format_task_output_distinguishes_timeout() {
        let mut rr = RunResult::failure(
            ProtocolError::new(error_code::TIMEOUT, "job wall time exceeded (1000ms)"),
            1000,
        )
        .with_status(TaskExitStatus::TimedOut);
        rr.stop_reason = Some("wall_timeout".into());
        let out = format_task_output("explore", Some("scan"), &rr);
        let text = out.as_text();
        assert!(text.contains("status=timeout"), "{text}");
        assert!(text.contains("Timeout:"), "{text}");
        assert_eq!(
            out.details
                .as_ref()
                .and_then(|d| d.get("ok"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn format_task_output_from_job_marks_failures() {
        use super::super::jobs::{JobSnapshot, JobState};
        let snap = JobSnapshot {
            id: "job_x".into(),
            kind: "task",
            agent: "explore".into(),
            description: Some("scan".into()),
            state: JobState::Failed,
            status: Some(TaskExitStatus::RuntimeError),
            summary: String::new(),
            ok: false,
            duration_ms: 12,
            turns: Some(4),
            max_turns: Some(16),
            error: Some("provider_error: overloaded".into()),
            notified: true,
            activity: String::new(),
            event_lines: vec![],
            notify_completion: false,
            log_path: None,
        };
        let (is_error, text) = format_task_output_from_job(&snap);
        assert!(is_error);
        assert!(text.contains("status=runtime_error"), "{text}");
        assert!(text.contains("provider_error"), "{text}");
        assert!(text.contains("id=job_x"), "{text}");
    }

    #[tokio::test]
    async fn task_tool_formats_runner_summary() {
        let host = test_host();
        let tool = TaskTool::with_harness(
            host,
            Arc::new(FakeHarness {
                summary: "found auth".into(),
                status: TaskExitStatus::Success,
            }),
        );
        let out = tool
            .execute(&ToolCall {
                id: "call_1".into(),
                name: "task".into(),
                arguments: json!({"prompt": "find auth", "description": "auth map", "background": false}),
            })
            .await
            .unwrap();
        let text = out.as_text();
        assert!(text.contains("status=success"), "{text}");
        assert!(text.contains("found auth|find auth"), "{text}");
        assert!(text.contains("auth map"), "{text}");
        let details = out.details.as_ref().unwrap();
        assert_eq!(details["status"], "success");
        assert_eq!(details["duration_ms"], 42);
        assert_eq!(details["turns"], 2);
    }

    #[tokio::test]
    async fn task_tool_incomplete_info_envelope() {
        let host = test_host();
        let tool = TaskTool::with_harness(
            host,
            Arc::new(FakeHarness {
                summary: "ERROR: need path".into(),
                status: TaskExitStatus::IncompleteInfo,
            }),
        );
        let out = tool
            .execute(&ToolCall {
                id: "c2".into(),
                name: "task".into(),
                arguments: json!({"prompt": "x", "background": false}),
            })
            .await
            .unwrap();
        let text = out.as_text();
        assert!(text.contains("status=incomplete_info"), "{text}");
        assert!(text.contains("do not treat as a question"), "{text}");
    }

    #[tokio::test]
    async fn task_tool_rejects_empty_prompt() {
        let host = test_host();
        let tool = TaskTool::new(host);
        let err = tool
            .execute(&ToolCall {
                id: "c3".into(),
                name: "task".into(),
                arguments: json!({"prompt": "  "}),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("prompt") || format!("{err:?}").contains("prompt"));
    }

    #[tokio::test]
    async fn task_tool_rejects_explore_for_git_workflow() {
        let host = test_host();
        // Harness would succeed if reached — precheck must short-circuit first.
        let tool = TaskTool::with_harness(
            host,
            Arc::new(FakeHarness {
                summary: "should not run".into(),
                status: TaskExitStatus::Success,
            }),
        );
        let out = tool
            .execute(&ToolCall {
                id: "c_git".into(),
                name: "task".into(),
                arguments: json!({
                    "prompt": "Categorize repository changes and split commits by feature",
                    "agent": "explore",
                    "description": "categorize",
                    "background": false,
                }),
            })
            .await
            .unwrap();
        let text = out.as_text();
        assert!(text.contains("status=incomplete_info"), "{text}");
        assert!(text.contains("need bash") || text.contains("ERROR:"), "{text}");
        assert!(!text.contains("should not run"), "{text}");
    }

    #[test]
    fn vcs_prompt_heuristic() {
        assert!(prompt_looks_like_vcs_workflow(
            "Categorize repository changes for feature commits"
        ));
        assert!(prompt_looks_like_vcs_workflow("run git status and group diffs"));
        assert!(prompt_looks_like_vcs_workflow("split commit by feature"));
        assert!(!prompt_looks_like_vcs_workflow("find auth middleware callers"));
        assert!(!prompt_looks_like_vcs_workflow("how does Agent::run work?"));
    }

    #[tokio::test]
    async fn task_tool_mode_alias_to_explore() {
        let host = test_host();
        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        struct Capture {
            seen: Arc<Mutex<Option<String>>>,
        }
        #[async_trait]
        impl TaskHarness for Capture {
            async fn run(&self, req: RunRequest) -> RunResult {
                *self.seen.lock().unwrap() = Some(req.agent.display_name().to_string());
                RunResult::success("ok", 1)
            }
        }
        let tool = TaskTool::with_harness(host, Arc::new(Capture { seen: seen.clone() }));
        let _ = tool
            .execute(&ToolCall {
                id: "c4".into(),
                name: "task".into(),
                arguments: json!({"prompt": "go", "mode": "explore", "background": false}),
            })
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().as_deref(), Some("explore"));
    }

    #[tokio::test]
    async fn task_tool_background_started() {
        let host = test_host();
        host.bind_provider(Arc::new(one_ai::MockProvider::new()))
            .await;
        let tool = TaskTool::new(host.clone());
        let out = tool
            .execute(&ToolCall {
                id: "bg1".into(),
                name: "task".into(),
                arguments: json!({
                    "prompt": "research auth entrypoints",
                    "description": "auth",
                    "agent": "explore",
                    "background": true
                }),
            })
            .await
            .expect("bg task");
        let text = out.as_text();
        assert!(text.contains("status=started"), "{text}");
        let details = out.details.expect("details");
        assert_eq!(details["status"], "started");
        assert!(details["job_id"].as_str().unwrap().starts_with("job_"));
        assert_eq!(details["background"], true);

        // Wait for completion notification.
        let job_id = details["job_id"].as_str().unwrap().to_string();
        for _ in 0..100 {
            if let Some(s) = host.jobs().get(&job_id) {
                if s.state.is_terminal() {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let notes = host.jobs().notification_queue().lock().unwrap().clone();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("[job completed]") && n.contains(&job_id)),
            "notes={notes:?}"
        );
    }

    #[tokio::test]
    async fn task_tool_spawn_not_allowed() {
        let mut parent = AgentSpec::builtin_main();
        parent.spawn_policy = crate::protocol::SpawnPolicy::none();
        let jobs = AgentJobRegistry::new(Arc::new(std::sync::Mutex::new(Vec::new())));
        let host = TaskToolHost::new(
            HarnessOptions::from_cwd(std::env::temp_dir()),
            parent,
            jobs,
            PathPolicy::workspace(std::env::temp_dir()),
        );
        let tool = TaskTool::with_harness(
            host,
            Arc::new(FakeHarness {
                summary: "should not run".into(),
                status: TaskExitStatus::Success,
            }),
        );
        let out = tool
            .execute(&ToolCall {
                id: "c5".into(),
                name: "task".into(),
                arguments: json!({"prompt": "x", "agent": "explore", "background": false}),
            })
            .await
            .unwrap();
        let text = out.as_text();
        assert!(
            text.contains("status=runtime_error") || text.contains("not in spawn"),
            "{text}"
        );
    }

    #[test]
    fn format_trailer_stable() {
        let rr = RunResult::success("hello", 10);
        let out = format_task_output("explore", Some("scan"), &rr);
        assert!(out
            .as_text()
            .starts_with("[task · explore · scan · status=success]"));
    }

    #[tokio::test]
    async fn parent_abort_kills_background_jobs() {
        let host = test_host();
        host.bind_provider(Arc::new(one_ai::MockProvider::new()))
            .await;
        let tool = TaskTool::new(host.clone());
        let out = tool
            .execute(&ToolCall {
                id: "bg_abort".into(),
                name: "task".into(),
                arguments: json!({
                    "prompt": "research something long",
                    "background": true,
                    "agent": "explore"
                }),
            })
            .await
            .unwrap();
        let job_id = out.details.unwrap()["job_id"].as_str().unwrap().to_string();
        host.jobs().kill_all();
        let snap = host.jobs().get(&job_id).expect("job");
        assert!(
            snap.state.is_terminal(),
            "expected terminal after kill_all, got {:?}",
            snap.state
        );
    }

    /// Real harness path: TaskTool → harness::run → MockProvider (no nested parent LLM).
    #[tokio::test]
    async fn task_tool_harness_mock_end_to_end() {
        std::env::set_var("ONE_TASK_FOREGROUND_BUDGET_MS", "0");
        let host = test_host();
        host.bind_provider(Arc::new(one_ai::MockProvider::new()))
            .await;
        let tool = TaskTool::new(host);
        let out = tool
            .execute(&ToolCall {
                id: "call_e2e".into(),
                name: "task".into(),
                arguments: json!({
                    "prompt": "Summarize the auth module layout",
                    "description": "auth",
                    "agent": "explore",
                    "background": false
                }),
            })
            .await
            .expect("task execute");
        let text = out.as_text();
        assert!(
            text.contains("status=success") || text.contains("status=incomplete_info"),
            "unexpected trailer: {text}"
        );
        // Mock always returns some assistant text for free-form prompts.
        assert!(
            text.contains("mock") || text.contains("Thinking") || text.len() > 40,
            "expected child summary body: {text}"
        );
        let details = out.details.expect("details");
        assert!(details.get("duration_ms").is_some());
        assert_eq!(details["agent"], "explore");
    }

    #[tokio::test]
    async fn task_tool_inline_agent_spec() {
        std::env::set_var("ONE_TASK_FOREGROUND_BUDGET_MS", "0");
        let host = test_host();
        host.bind_provider(Arc::new(one_ai::MockProvider::new()))
            .await;
        let tool = TaskTool::new(host);
        let explore = AgentSpec::builtin_explore();
        let out = tool
            .execute(&ToolCall {
                id: "call_spec".into(),
                name: "task".into(),
                arguments: json!({
                    "prompt": "quick scan",
                    "agent": "explore",
                    "background": false,
                    "agent_spec": explore
                }),
            })
            .await
            .expect("task with agent_spec");
        let text = out.as_text();
        assert!(text.contains("[task · explore"), "{text}");
        assert!(
            !text.contains("status=runtime_error") || text.contains("status=success"),
            "{text}"
        );
    }

    /// Parent Agent has only `task`; scripted LLM emits one task call then final text.
    #[tokio::test]
    async fn parent_agent_invokes_task_tool() {
        use one_core::agent::{
            Agent, AgentConfig, CompletionRequest, CompletionResponse, LlmProvider, TokenUsage,
        };
        use one_core::message::{ContentBlock, StopReason};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ParentThenFinal {
            n: AtomicUsize,
        }

        #[async_trait]
        impl LlmProvider for ParentThenFinal {
            fn name(&self) -> &str {
                "scripted"
            }
            fn model(&self) -> &str {
                "script-v1"
            }
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> one_core::error::Result<CompletionResponse> {
                let step = self.n.fetch_add(1, Ordering::SeqCst);
                // After tool results, finish.
                let has_tool = request
                    .messages
                    .iter()
                    .any(|m| matches!(m, one_core::message::AgentMessage::ToolResult(_)));
                if has_tool || step >= 1 {
                    return Ok(CompletionResponse {
                        provider: "scripted".into(),
                        model: "script-v1".into(),
                        content: vec![ContentBlock::Text {
                            text: "Parent done with sub-agent findings.".into(),
                        }],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    });
                }
                Ok(CompletionResponse {
                    provider: "scripted".into(),
                    model: "script-v1".into(),
                    content: vec![ContentBlock::ToolCall {
                        id: "call_task_parent".into(),
                        name: "task".into(),
                        arguments: json!({
                            "prompt": "Locate authentication entrypoints",
                            "agent": "explore",
                            "description": "auth",
                            "background": false
                        }),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                    citations: Vec::new(),
                })
            }
        }

        let host = test_host();
        // Child harness uses mock; parent uses scripted.
        host.bind_provider(Arc::new(one_ai::MockProvider::new()))
            .await;
        let task = Arc::new(TaskTool::new(host)) as Arc<dyn Tool>;
        let mut agent = Agent::new(
            AgentConfig {
                system_prompt: "You are the parent.".into(),
                max_turns: 8,
                thinking_level: one_core::agent::ThinkingLevel::Off,
                server_search: false,
                empty_response_retries: one_core::agent::DEFAULT_EMPTY_RESPONSE_RETRIES,
            },
            vec![task],
        );
        std::env::set_var("ONE_TASK_FOREGROUND_BUDGET_MS", "0");
        let parent_llm = ParentThenFinal {
            n: AtomicUsize::new(0),
        };
        let out = agent
            .prompt(&parent_llm, "Research auth and summarize")
            .await
            .expect("parent prompt");
        assert!(
            out.contains("Parent done") || out.contains("findings"),
            "parent final: {out}"
        );
        // History should include a tool result from task.
        let has_task_result = agent.messages.iter().any(|m| {
            if let one_core::message::AgentMessage::ToolResult(tr) = m {
                tr.tool_name == "task"
                    && tr.content.iter().any(|c| match c {
                        one_core::message::TextOrImage::Text { text } => {
                            text.contains("[task · explore")
                        }
                        _ => false,
                    })
            } else {
                false
            }
        });
        assert!(has_task_result, "expected task tool_result in history");
    }

    #[tokio::test]
    async fn two_tasks_with_concurrency_one_complete() {
        // Physical LLM slots = 1; two concurrent task tools must queue, not deadlock.
        std::env::set_var("ONE_LLM_CONCURRENCY", "1");
        // Note: global semaphore is OnceLock — may already be init from other tests.
        // Still exercises logical slots + harness path.
        let host = test_host();
        host.bind_provider(Arc::new(one_ai::MockProvider::new()))
            .await;
        let tool = Arc::new(TaskTool::new(host));
        let t1 = {
            let tool = tool.clone();
            tokio::spawn(async move {
                tool.execute(&ToolCall {
                    id: "p1".into(),
                    name: "task".into(),
                    arguments: json!({"prompt": "task one research"}),
                })
                .await
            })
        };
        let t2 = {
            let tool = tool.clone();
            tokio::spawn(async move {
                tool.execute(&ToolCall {
                    id: "p2".into(),
                    name: "task".into(),
                    arguments: json!({"prompt": "task two research"}),
                })
                .await
            })
        };
        let (a, b) = tokio::join!(t1, t2);
        let a = a.unwrap().unwrap();
        let b = b.unwrap().unwrap();
        assert!(a.as_text().contains("status="), "{}", a.as_text());
        assert!(b.as_text().contains("status="), "{}", b.as_text());
    }

    #[tokio::test]
    async fn task_defaults_to_background() {
        let host = test_host();
        host.bind_provider(Arc::new(one_ai::MockProvider::new()))
            .await;
        let tool = TaskTool::new(host);
        let out = tool
            .execute(&ToolCall {
                id: "def_bg".into(),
                name: "task".into(),
                arguments: json!({"prompt": "research auth entrypoints", "agent": "explore"}),
            })
            .await
            .unwrap();
        let details = out.details.expect("details");
        assert_eq!(details["status"], "started");
        assert_eq!(details["background"], true);
        assert_eq!(details["backgrounded"], false);
        assert_eq!(details["queued"], false);
    }

    #[tokio::test]
    async fn background_false_waits_for_child_not_auto_bg() {
        // Regression: d934bfc1-d96 — explicit sync must not hand off after 1s
        // just because the child is still running. Slot is free → block for result.
        let host = test_host();
        host.bind_provider(Arc::new(one_ai::MockProvider::new()))
            .await;
        let tool = TaskTool::new(host);
        let out = tool
            .execute(&ToolCall {
                id: "sync_wait".into(),
                name: "task".into(),
                arguments: json!({
                    "prompt": "summarize the repo in one line",
                    "agent": "explore",
                    "background": false
                }),
            })
            .await
            .unwrap();
        let text = out.as_text();
        let details = out.details.expect("details");
        assert_ne!(
            details["status"], "started",
            "sync task with a free slot must not auto-bg: {text}"
        );
        assert!(
            details["status"] == "success"
                || details["status"] == "incomplete_info"
                || details["status"] == "max_turns_exceeded",
            "unexpected status: {} text={text}",
            details["status"],
        );
    }

    #[tokio::test]
    async fn subagent_type_alias_and_capability_read_only() {
        let host = test_host();
        let seen: Arc<Mutex<Option<AgentSpec>>> = Arc::new(Mutex::new(None));
        struct Capture {
            seen: Arc<Mutex<Option<AgentSpec>>>,
        }
        #[async_trait]
        impl TaskHarness for Capture {
            async fn run(&self, req: RunRequest) -> RunResult {
                *self.seen.lock().unwrap() = Some(req.agent.clone());
                RunResult::success("ok", 1)
            }
        }
        let tool = TaskTool::with_harness(host, Arc::new(Capture { seen: seen.clone() }));
        let _ = tool
            .execute(&ToolCall {
                id: "cap1".into(),
                name: "task".into(),
                arguments: json!({
                    "prompt": "go",
                    "subagent_type": "general-purpose",
                    "capability_mode": "read-only",
                    "background": false
                }),
            })
            .await
            .unwrap();
        let spec = seen.lock().unwrap().clone().expect("captured");
        assert_eq!(normalize_agent_name(spec.display_name()), "general");
        assert!(!child_tools_look_writable(&spec));
    }

    #[tokio::test]
    async fn resume_from_completed_job_seeds_transcript() {
        let host = test_host();
        host.bind_provider(Arc::new(one_ai::MockProvider::new()))
            .await;
        let tool = TaskTool::new(host.clone());
        let first = tool
            .execute(&ToolCall {
                id: "r1".into(),
                name: "task".into(),
                arguments: json!({
                    "prompt": "first research pass",
                    "agent": "explore",
                    "background": true
                }),
            })
            .await
            .unwrap();
        let job_id = first.details.unwrap()["job_id"].as_str().unwrap().to_string();
        for _ in 0..100 {
            if host
                .jobs()
                .get(&job_id)
                .is_some_and(|s| s.state.is_terminal())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            host.jobs().resume_source(&job_id).is_some(),
            "source should be resumeable"
        );

        std::env::set_var("ONE_TASK_FOREGROUND_BUDGET_MS", "0");
        let second = tool
            .execute(&ToolCall {
                id: "r2".into(),
                name: "task".into(),
                arguments: json!({
                    "prompt": "continue from prior findings",
                    "agent": "explore",
                    "resume_from": job_id,
                    "background": false
                }),
            })
            .await
            .unwrap();
        std::env::remove_var("ONE_TASK_FOREGROUND_BUDGET_MS");
        let text = second.as_text();
        assert!(
            text.contains("status=success")
                || text.contains("status=incomplete")
                || text.contains("status=started"),
            "{text}"
        );
    }
}
