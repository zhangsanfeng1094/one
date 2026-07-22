//! `task` meta-tool: thin wrapper around [`super::harness::run`].
//!
//! Lives in **one-cli only** (not one-tools). Parent agents register this tool
//! when `spawn_policy` allows children; explore children never get it.
//!
//! Sync path returns tool_result. `background=true` returns `status=started` and
//! pushes `[job completed]` onto the shared notification queue when done.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use one_core::agent::LlmProvider;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use one_tools::DEFAULT_MAX_BYTES;
use serde_json::{json, Value};
use tokio::sync::{RwLock, Semaphore};

use super::harness::{self, HarnessOptions};
use super::jobs::AgentJobRegistry;
use super::presets;
use crate::protocol::{
    error_code, AgentSpec, ProtocolError, RunParent, RunRequest, RunResult, SessionMode,
    TaskExitStatus,
};

/// Cap tool_result text so a long child summary does not flood the parent.
const SUMMARY_MAX_CHARS: usize = DEFAULT_MAX_BYTES;

/// Shared host state for all `task` tool instances on a parent runtime.
pub struct TaskToolHost {
    /// Bound by CLI after `ProviderSet::build` / model switch.
    provider: RwLock<Option<Arc<dyn LlmProvider>>>,
    opts: RwLock<HarnessOptions>,
    /// Parent harness face (spawn_policy + agents table).
    parent_agent: RwLock<AgentSpec>,
    parent_run_id: RwLock<String>,
    parent_session_id: RwLock<Option<String>>,
    parent_depth: AtomicU32,
    /// Logical concurrent `task` slots (default 4); shared with background jobs.
    task_slots: Arc<Semaphore>,
    /// Background agent jobs (completion → notification queue).
    jobs: Arc<AgentJobRegistry>,
}

impl TaskToolHost {
    pub fn new(
        opts: HarnessOptions,
        parent_agent: AgentSpec,
        jobs: Arc<AgentJobRegistry>,
    ) -> Arc<Self> {
        let max_c = parent_agent.spawn_policy.max_concurrent.max(1) as usize;
        Arc::new(Self {
            provider: RwLock::new(None),
            opts: RwLock::new(opts),
            parent_agent: RwLock::new(parent_agent),
            parent_run_id: RwLock::new(format!("run_{}", uuid_simple())),
            parent_session_id: RwLock::new(None),
            parent_depth: AtomicU32::new(0),
            task_slots: Arc::new(Semaphore::new(max_c)),
            jobs,
        })
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
        // Resize semaphore only if larger; shrinking mid-flight is racy — keep max of both.
        let available = self.task_slots.available_permits();
        if max_c > available {
            self.task_slots.add_permits(max_c - available);
        }
        *self.parent_agent.write().await = agent;
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
        let opts = self.host.opts.read().await.clone();
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
                "Run a sub-agent via the same harness as `one agent run` (Agent ≡ Subagent). \
Default agent=explore when allowed. Returns a concise summary so this conversation stays small. \
Do not use for a single trivial file read. Allowed agent names: [{allowed}].{child_blurb} \
Set background=true for long work that should not block this turn: returns \
status=started + job_id immediately; when done a [job completed] notice is \
injected before the next LLM turn (or poll with job_output). \
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
                        "description": "Short label for logs / TUI (optional)"
                    },
                    "agent": {
                        "type": "string",
                        "description": format!(
                            "Preset / child name under spawn_policy.allow (one of: {allowed}; default explore if allowed)"
                        )
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
                        "description": "If true, return immediately with job_id; result arrives as [job completed] notification"
                    },
                    "isolation": {
                        "type": "string",
                        "description": "none (default, shared cwd) | worktree (isolated git worktree under .one/worktrees; no auto-merge). Prefer worktree for writable agents."
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
        let background = call
            .arguments
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let isolation_arg = call
            .arguments
            .get("isolation")
            .and_then(|v| v.as_str())
            .and_then(crate::protocol::IsolationMode::parse);

        // Logical concurrency (independent of physical LLM permit).
        let slot = self
            .host
            .task_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| tool_error("task", "task slot semaphore closed"))?;

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
            parent_meta,
            call.arguments.get("agent_spec"),
            &self.host.opts.read().await.cwd,
        ) {
            Ok(r) => r,
            Err(e) => {
                drop(slot);
                return Ok(format_task_output(
                    &agent_name,
                    description.as_deref(),
                    &RunResult::failure(e, 0).with_status(TaskExitStatus::RuntimeError),
                ));
            }
        };
        // task arg overrides AgentSpec.isolation when provided.
        if let Some(iso) = isolation_arg {
            req.agent.isolation = iso;
        }
        // Default: writable children in background → worktree when still none.
        if background
            && matches!(req.agent.isolation, crate::protocol::IsolationMode::None)
            && child_tools_look_writable(&req.agent)
        {
            req.agent.isolation = crate::protocol::IsolationMode::Worktree;
        }

        // Validate tools can materialize before spending a slot on LLM work.
        if let Err(e) = validate_child_tools(&req.agent) {
            drop(slot);
            return Ok(format_task_output(
                &agent_name,
                description.as_deref(),
                &RunResult::failure(e, 0).with_status(TaskExitStatus::RuntimeError),
            ));
        }

        if background {
            let provider = self.host.provider.read().await.clone();
            let Some(provider) = provider else {
                drop(slot);
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
            };
            let opts = self.host.opts.read().await.clone();
            let job_id = self.host.jobs.spawn(
                req,
                provider,
                opts,
                agent_name.clone(),
                description.clone(),
                Some(slot),
            );
            return Ok(format_task_started(
                &agent_name,
                description.as_deref(),
                &job_id,
            ));
        }

        // Sync: hold slot until harness returns.
        let result = self.harness.run(req).await;
        drop(slot);
        Ok(format_task_output(
            &agent_name,
            description.as_deref(),
            &result,
        ))
    }
}

fn resolve_agent_name(args: &Value) -> String {
    if let Some(a) = args.get("agent").and_then(|v| v.as_str()) {
        let t = a.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(m) = args.get("mode").and_then(|v| v.as_str()) {
        let t = m.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    "explore".into()
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
) -> ToolOutput {
    let desc_part = description.map(|d| format!(" · {d}")).unwrap_or_default();
    let text = format!(
        "[task · {agent_name}{desc_part} · status=started · id={job_id}]\n\
         Background job started. Continue other work.\n\
         Result arrives as a [job completed] notice before the next LLM turn, \
         or poll with job_output(job_id=\"{job_id}\")."
    );
    let details = json!({
        "ok": true,
        "status": "started",
        "job_id": job_id,
        "background": true,
        "agent": agent_name,
        "description": description,
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

    let text = if matches!(status, TaskExitStatus::IncompleteInfo) {
        format!(
            "{header}\n\
             Sub-agent could not finish without clarification (do not treat as a question to the user).\n\
             Partial findings:\n{body}"
        )
    } else if !result.ok {
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
    } else if body.is_empty() {
        format!("{header}\n(no summary)")
    } else {
        format!("{header}\n{body}")
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

/// One-liner for main agent system prompt.
pub const TASK_TOOL_PROMPT_HINT: &str = "\
- To delegate work to a sub-agent, use the `task` tool with `agent` set to a name allowed by spawn policy (default: explore for multi-file research). Findings return as a summary so this conversation stays small. Do not use task for a trivial single-file read. For long work that should not block this turn, use task(background=true); when you have spawned all background tasks and have nothing else to do, call wait_tasks (mode=all) to block until they finish — or wait_tasks(mode=any) for the next one. job_output polls without waiting. For agents that write/edit/bash, prefer isolation=worktree (or background, which defaults worktree for writable tools) so changes stay under .one/worktrees and are not auto-merged.";

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
    // Ensure explore child exists when allow lists explore but agents table omits it.
    if spec.spawn_allowed("explore") && !spec.agents.contains_key("explore") {
        spec.agents
            .insert("explore".into(), AgentSpec::builtin_explore());
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
        )
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
                arguments: json!({"prompt": "find auth", "description": "auth map"}),
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
                arguments: json!({"prompt": "x"}),
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
                arguments: json!({"prompt": "go", "mode": "explore"}),
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
        let host = TaskToolHost::new(HarnessOptions::from_cwd(std::env::temp_dir()), parent, jobs);
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
                arguments: json!({"prompt": "x", "agent": "explore"}),
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
                    "agent": "explore"
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
                            "description": "auth"
                        }),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
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
            },
            vec![task],
        );
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
}
