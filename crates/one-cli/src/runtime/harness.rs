//! Single entry: `run(RunRequest) → RunResult` (CLI + TaskTool / background jobs).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Instant;

use one_core::agent::{Agent, AgentConfig, LlmProvider, ThinkingLevel};
use one_core::error::OneError;
use one_tools::PathPolicy;

use super::tool_materialize::{
    harness_build_context, harness_registry, materialize_tools, resolve_names,
};
use crate::approval::{ApprovalMode, PermissionGate};
use crate::protocol::{
    error_code, AgentRunEcho, AgentSpec, RunParent, RunRequest, RunResult, SessionMode,
    TaskExitStatus, UsageSnapshot,
};

/// Options for a harness invocation (cwd, path policy roots, etc.).
#[derive(Clone)]
pub struct HarnessOptions {
    pub cwd: PathBuf,
    /// When true, skip path boundary (full access).
    pub full_access: bool,
    pub add_dirs: Vec<PathBuf>,
    pub auto_approve: bool,
    /// Optional pre-built tools (MCP / extensions) merged into the registry by name.
    /// Not used for explore-only; when set, `ToolsSpec` can allow those names.
    pub dynamic_tools: Vec<std::sync::Arc<dyn one_core::tool::Tool>>,
}

impl std::fmt::Debug for HarnessOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessOptions")
            .field("cwd", &self.cwd)
            .field("full_access", &self.full_access)
            .field("add_dirs", &self.add_dirs)
            .field("auto_approve", &self.auto_approve)
            .field("dynamic_tools", &self.dynamic_tools.len())
            .finish()
    }
}

impl HarnessOptions {
    pub fn from_cwd(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            full_access: false,
            add_dirs: vec![],
            auto_approve: true,
            dynamic_tools: vec![],
        }
    }
}

/// Optional controls for nested / background runs.
#[derive(Debug, Clone, Default)]
pub struct RunControl {
    /// Shared abort flag (job_kill / parent Esc).
    pub abort: Option<Arc<AtomicBool>>,
    /// 1-based turn progress for job_output while running.
    pub turn_progress: Option<Arc<AtomicU64>>,
}

/// Run one agent (root or sub) according to [`RunRequest`].
pub async fn run(req: RunRequest, provider: &dyn LlmProvider, opts: &HarnessOptions) -> RunResult {
    run_with_control(req, provider, opts, RunControl::default()).await
}

/// Like [`run`], with abort / progress hooks for background jobs.
pub async fn run_with_control(
    req: RunRequest,
    provider: &dyn LlmProvider,
    opts: &HarnessOptions,
    control: RunControl,
) -> RunResult {
    let t0 = Instant::now();
    let depth = req.parent.as_ref().map(|p| p.depth).unwrap_or(0);
    let agent_name = req.agent.display_name().to_string();

    // Optional git worktree isolation (no auto-merge).
    let mut req = req;
    let mut effective_opts = opts.clone();
    let wt_handle = match apply_isolation(&mut req, &mut effective_opts) {
        Ok(h) => h,
        Err(e) => {
            return RunResult::failure(e, t0.elapsed().as_millis() as u64).with_agent_echo(
                AgentRunEcho {
                    name: Some(agent_name),
                    depth: Some(depth),
                    ..Default::default()
                },
            );
        }
    };

    let tools = match build_tools(&req.agent, &effective_opts) {
        Ok(t) => t,
        Err(e) => {
            if let Some(ref h) = wt_handle {
                let _ = super::worktree::WorktreeManager::remove(h, true);
            }
            return RunResult::failure(e, t0.elapsed().as_millis() as u64).with_agent_echo(
                AgentRunEcho {
                    name: Some(agent_name),
                    depth: Some(depth),
                    ..Default::default()
                },
            );
        }
    };
    let tool_names: Vec<String> = tools.iter().map(|t| t.definition().name).collect();

    // Shared-cwd mutators serialize; worktree runs may write in parallel.
    let _write_permit =
        if wt_handle.is_none() && super::provider_limit::tools_need_write_lock(&tool_names) {
            Some(super::provider_limit::acquire_write_permit().await)
        } else {
            None
        };

    let system_prompt = resolve_system_prompt(&req.agent);
    let max_turns = req.agent.max_turns.unwrap_or(16);
    let thinking = resolve_thinking(&req.agent);

    let config = AgentConfig {
        system_prompt,
        max_turns,
        thinking_level: thinking,
    };

    let mut agent = Agent::new(config, tools);
    if let Some(flag) = control.abort {
        agent.set_abort_flag(flag);
    }
    if let Some(progress) = control.turn_progress {
        agent.set_turn_progress(Some(progress));
    }

    // Fail-closed permissions for non-interactive harness (subagent / agent run).
    // Map permission_mode when present; else fall back to opts.auto_approve.
    let gate = PermissionGate::new(
        one_tools::PermissionRules::default(),
        resolve_approval_mode(&req.agent, effective_opts.auto_approve),
    );
    agent.set_tool_gate(Some(gate));

    // Serialize LLM calls through global permit (nested tasks share the pool).
    // Whole-run hold is safe for leaf agents (no spawn). Agents that can spawn
    // must not hold the permit across the whole run or child harness::run would
    // deadlock when ONE_LLM_CONCURRENCY=1.
    let _permit = if req.agent.can_spawn() {
        None
    } else {
        Some(super::provider_limit::acquire_llm_permit().await)
    };

    let prompt_text = req.prompt.text.clone();
    let run_outcome = agent.prompt(provider, &prompt_text).await;
    let duration_ms = t0.elapsed().as_millis() as u64;
    let usage = UsageSnapshot {
        input_tokens: agent.token_usage.input_tokens,
        output_tokens: agent.token_usage.output_tokens,
        cache_read_tokens: agent.token_usage.cache_read_tokens,
        cache_write_tokens: agent.token_usage.cache_write_tokens,
        estimated_cost_usd: None,
    };
    let echo = AgentRunEcho {
        name: Some(agent_name),
        tools: tool_names,
        model: Some(crate::protocol::ModelSpec {
            provider: Some(provider.name().to_string()),
            id: Some(provider.model().to_string()),
            thinking: Some(thinking.as_str().to_string()),
            inherit: req.agent.model.inherit,
        }),
        max_turns: Some(max_turns),
        permission_mode: req.agent.permission_mode.clone(),
        depth: Some(depth),
    };

    let mut result = match run_outcome {
        Ok(text) => {
            let status = classify_success_text(&text);
            let mut rr = RunResult::success(text.clone(), duration_ms).with_status(status);
            if matches!(status, TaskExitStatus::IncompleteInfo) {
                rr.ok = false;
                rr.stop_reason = Some("incomplete_info".into());
            }
            let turns = agent
                .messages
                .iter()
                .filter(|m| matches!(m, one_core::message::AgentMessage::Assistant(_)))
                .count() as u64;
            rr.turns = Some(turns);
            rr
        }
        Err(OneError::MaxTurns { max }) => {
            let partial = last_assistant_text(&agent).unwrap_or_default();
            let mut rr = RunResult::success(partial, duration_ms)
                .with_status(TaskExitStatus::MaxTurnsExceeded);
            rr.stop_reason = Some("max_turns".into());
            rr.turns = Some(max as u64);
            rr.error = Some(crate::protocol::ProtocolError::new(
                error_code::MAX_TURNS,
                format!("max turns ({max}) exceeded"),
            ));
            rr
        }
        Err(OneError::Aborted) => {
            let mut rr = RunResult::failure(
                crate::protocol::ProtocolError::new(error_code::ABORTED, "aborted"),
                duration_ms,
            )
            .with_status(TaskExitStatus::Aborted);
            rr.stop_reason = Some("aborted".into());
            rr
        }
        Err(e) => {
            let mut rr = RunResult::failure(
                crate::protocol::ProtocolError::new(error_code::PROVIDER_ERROR, e.to_string()),
                duration_ms,
            )
            .with_status(TaskExitStatus::RuntimeError);
            rr.stop_reason = Some("error".into());
            rr
        }
    };

    result = result.with_agent_echo(echo);
    result.usage = Some(usage);
    result.parent = req.parent.clone();

    if let Some(ref h) = wt_handle {
        let kept = super::worktree::WorktreeManager::cleanup_after_run(h, result.ok);
        result = result.with_worktree(h.to_info(kept));
        if kept {
            // Surface path in text so hosts without JSON still see it.
            if !result.result.contains(&h.path.display().to_string()) {
                result.result.push_str(&format!(
                    "\n\n[worktree kept · {} · branch {}]",
                    h.path.display(),
                    h.branch
                ));
            }
        }
    }

    result
}

/// Apply `AgentSpec.isolation`; mutates req (cwd + system append) and opts.cwd.
fn apply_isolation(
    req: &mut RunRequest,
    opts: &mut HarnessOptions,
) -> Result<Option<super::worktree::WorktreeHandle>, crate::protocol::ProtocolError> {
    use crate::protocol::IsolationMode;
    if !matches!(req.agent.isolation, IsolationMode::Worktree) {
        return Ok(None);
    }
    let job_id = req
        .run_id
        .clone()
        .or_else(|| {
            req.parent
                .as_ref()
                .map(|p| format!("{}_{}", p.tool_use_id, p.depth))
        })
        .unwrap_or_else(|| {
            format!(
                "run_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            )
        });
    let base = req
        .agent
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| opts.cwd.clone());
    let handle = super::worktree::WorktreeManager::create(&base, &job_id)?;
    opts.cwd = handle.path.clone();
    req.agent.cwd = Some(handle.path.display().to_string());
    let note = format!(
        "You are running in an isolated git worktree at {} (branch {}). \
         Do not assume files outside this cwd; changes are not auto-merged into the parent tree.",
        handle.path.display(),
        handle.branch
    );
    match &mut req.agent.append_system_prompt {
        Some(a) if !a.is_empty() => {
            a.push_str("\n\n");
            a.push_str(&note);
        }
        _ => req.agent.append_system_prompt = Some(note),
    }
    Ok(Some(handle))
}

fn classify_success_text(text: &str) -> TaskExitStatus {
    let trimmed = text.trim_start();
    if trimmed.starts_with("ERROR:") || trimmed.starts_with("ERROR ") {
        TaskExitStatus::IncompleteInfo
    } else {
        TaskExitStatus::Success
    }
}

fn last_assistant_text(agent: &Agent) -> Option<String> {
    for m in agent.messages.iter().rev() {
        if let one_core::message::AgentMessage::Assistant(a) = m {
            let mut s = String::new();
            for b in &a.content {
                if let one_core::message::ContentBlock::Text { text } = b {
                    s.push_str(text);
                }
            }
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn resolve_system_prompt(spec: &AgentSpec) -> String {
    let mut base = spec
        .system_prompt
        .clone()
        .unwrap_or_else(|| one_core::agent::DEFAULT_SYSTEM_PROMPT.to_string());
    if let Some(append) = &spec.append_system_prompt {
        if !append.is_empty() {
            base.push_str("\n\n");
            base.push_str(append);
        }
    }
    base
}

fn resolve_thinking(spec: &AgentSpec) -> ThinkingLevel {
    if let Some(t) = &spec.model.thinking {
        return ThinkingLevel::parse(t).unwrap_or(ThinkingLevel::Off);
    }
    ThinkingLevel::Off
}

fn resolve_approval_mode(spec: &AgentSpec, auto_approve: bool) -> ApprovalMode {
    match spec.permission_mode.as_deref() {
        Some("bypass") | Some("accept_edits") => ApprovalMode::Auto,
        Some("dont_ask") | Some("plan") => ApprovalMode::FailClosed,
        Some("default") => {
            if auto_approve {
                ApprovalMode::Auto
            } else {
                ApprovalMode::FailClosed
            }
        }
        _ => {
            if auto_approve {
                ApprovalMode::Auto
            } else {
                ApprovalMode::FailClosed
            }
        }
    }
}

fn build_tools(
    spec: &AgentSpec,
    opts: &HarnessOptions,
) -> Result<Vec<std::sync::Arc<dyn one_core::tool::Tool>>, crate::protocol::ProtocolError> {
    let cwd = resolve_cwd(spec, opts);
    let policy = build_policy(&cwd, opts, spec);
    let mut registry = harness_registry();
    if !opts.dynamic_tools.is_empty() {
        registry.register_instances(opts.dynamic_tools.iter().cloned());
    }

    // Named explore (or builtin research face): force Explore catalog when tools
    // look like default read_only — keeps hard whitelist even if deny list varies.
    let prefer_explore = spec.display_name() == "explore"
        || (matches!(spec.tools.profile, crate::protocol::ToolProfile::ReadOnly)
            && spec.tools.allow.is_empty());

    let ctx = harness_build_context(policy, opts.auto_approve);
    materialize_tools(&spec.tools, &registry, &ctx, prefer_explore)
}

fn resolve_cwd(spec: &AgentSpec, opts: &HarnessOptions) -> PathBuf {
    if let Some(c) = &spec.cwd {
        return PathBuf::from(c);
    }
    opts.cwd.clone()
}

fn build_policy(cwd: &Path, opts: &HarnessOptions, spec: &AgentSpec) -> PathPolicy {
    if opts.full_access || spec.sandbox.as_deref() == Some("full-access") {
        return PathPolicy::full_access(cwd.to_path_buf());
    }
    // read_only sandbox: still PathPolicy workspace (file tools may write if
    // registered); callers should use ToolsSpec deny for write tools.
    let mut dirs: Vec<PathBuf> = opts.add_dirs.clone();
    for d in &spec.add_dirs {
        dirs.push(PathBuf::from(d));
    }
    PathPolicy::workspace(cwd.to_path_buf()).with_additional_dirs(dirs)
}

/// Convenience: run a named preset with a user prompt.
pub async fn run_preset(
    preset: &str,
    prompt: &str,
    provider: &dyn LlmProvider,
    opts: &HarnessOptions,
) -> RunResult {
    match super::presets::load_preset(preset, &opts.cwd) {
        Ok(spec) => {
            let mut req = RunRequest::new(spec, prompt);
            req.session.mode = SessionMode::Ephemeral;
            run(req, provider, opts).await
        }
        Err(e) => RunResult::failure(e, 0),
    }
}

/// Build a child RunRequest for TaskTool (later).
#[allow(dead_code)]
pub fn child_request(
    parent_spec: &AgentSpec,
    child_name: &str,
    prompt: impl Into<String>,
    parent: RunParent,
) -> Result<RunRequest, crate::protocol::ProtocolError> {
    RunRequest::child(parent_spec, child_name, prompt, parent)
}

/// Resolved tool names for inspect/dump without constructing tools.
pub fn preview_tool_names(spec: &AgentSpec) -> Vec<String> {
    let prefer_explore = spec.display_name() == "explore"
        || (matches!(spec.tools.profile, crate::protocol::ToolProfile::ReadOnly)
            && spec.tools.allow.is_empty());
    resolve_names(&spec.tools, prefer_explore)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AgentSpec;

    #[test]
    fn explore_tool_names_via_registry() {
        let s = AgentSpec::builtin_explore();
        let names = preview_tool_names(&s);
        assert!(names.contains(&"read".to_string()));
        assert!(!names.iter().any(|x| x == "bash"));
        assert!(!names.iter().any(|x| x == "ask_user"));
    }

    #[test]
    fn coding_allow_list_materializes() {
        let mut s = AgentSpec::builtin_explore();
        s.name = Some("implementer".into());
        s.tools = crate::protocol::ToolsSpec {
            profile: crate::protocol::ToolProfile::None,
            allow: vec!["read".into(), "write".into(), "edit".into(), "bash".into()],
            deny: vec![],
            mcp: false,
            ..Default::default()
        };
        let opts = HarnessOptions::from_cwd("/tmp");
        let tools = build_tools(&s, &opts).expect("coding allow list");
        let names: Vec<_> = tools.iter().map(|t| t.definition().name).collect();
        assert!(names.contains(&"write".to_string()));
        assert!(names.contains(&"bash".to_string()));
    }

    #[test]
    fn resolve_explore_prompt_uses_spec() {
        let s = AgentSpec::builtin_explore();
        let p = resolve_system_prompt(&s);
        assert!(p.contains("read-only") || p.contains("sub-agent"));
    }
}
