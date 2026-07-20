//! Single entry: `run(RunRequest) → RunResult` (CLI + TaskTool / background jobs).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Instant;

use one_core::agent::{Agent, AgentConfig, LlmProvider, ThinkingLevel};
use one_core::error::OneError;
use one_tools::{FailClosedAskUser, PathPolicy};

use super::explore_tools::explore_tools;
use super::provider_limit::acquire_llm_permit;
use crate::approval::{ApprovalMode, PermissionGate};
use crate::protocol::{
    error_code, AgentRunEcho, AgentSpec, RunParent, RunRequest, RunResult, SessionMode,
    TaskExitStatus, ToolsSpec, UsageSnapshot,
};

/// Options for a harness invocation (cwd, path policy roots, etc.).
#[derive(Debug, Clone)]
pub struct HarnessOptions {
    pub cwd: PathBuf,
    /// When true, skip path boundary (full access).
    pub full_access: bool,
    pub add_dirs: Vec<PathBuf>,
    pub auto_approve: bool,
}

impl HarnessOptions {
    pub fn from_cwd(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            full_access: false,
            add_dirs: vec![],
            auto_approve: true,
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
pub async fn run(
    req: RunRequest,
    provider: &dyn LlmProvider,
    opts: &HarnessOptions,
) -> RunResult {
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

    let tools = match build_tools(&req.agent, opts) {
        Ok(t) => t,
        Err(e) => {
            return RunResult::failure(e, t0.elapsed().as_millis() as u64)
                .with_agent_echo(AgentRunEcho {
                    name: Some(agent_name),
                    depth: Some(depth),
                    ..Default::default()
                });
        }
    };
    let tool_names: Vec<String> = tools
        .iter()
        .map(|t| t.definition().name)
        .collect();

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
    let gate = PermissionGate::new(
        one_tools::PermissionRules::default(),
        if opts.auto_approve {
            ApprovalMode::Auto
        } else {
            ApprovalMode::FailClosed
        },
    );
    agent.set_tool_gate(Some(gate));

    // Serialize LLM calls through global permit (nested tasks share the pool).
    // Whole-run hold is safe for leaf agents (no spawn). Agents that can spawn
    // must not hold the permit across the whole run or child harness::run would
    // deadlock when ONE_LLM_CONCURRENCY=1.
    let _permit = if req.agent.can_spawn() {
        None
    } else {
        Some(acquire_llm_permit().await)
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
            // Count turns roughly: assistant messages in history
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
    result
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

fn build_tools(
    spec: &AgentSpec,
    opts: &HarnessOptions,
) -> Result<Vec<std::sync::Arc<dyn one_core::tool::Tool>>, crate::protocol::ProtocolError> {
    let cwd = resolve_cwd(spec, opts);
    let policy = build_policy(&cwd, opts, spec);

    let name = spec.display_name();
    // Explore preset / profile: hard whitelist only.
    if name == "explore"
        || matches!(
            spec.tools.profile,
            crate::protocol::ToolProfile::ReadOnly
        )
            && spec.tools.allow.is_empty()
            && !spec.tools.mcp
    {
        // If explicitly named explore OR read_only profile without custom allow → whitelist
        if name == "explore" || is_default_explore_tools(&spec.tools) {
            return Ok(explore_tools(policy));
        }
    }

    // Custom allow list on read_only profile
    if !spec.tools.allow.is_empty() {
        return build_from_allow_list(&spec.tools.allow, policy);
    }

    // Fallback: explore whitelist for safety when spawn-less research specs
    if matches!(spec.tools.profile, crate::protocol::ToolProfile::ReadOnly) {
        return Ok(explore_tools(policy));
    }

    // Coding profile for general (v1.1) — MVP maps to read_only for safety if not explore
    Err(crate::protocol::ProtocolError::new(
        error_code::INVALID_AGENT_SPEC,
        format!(
            "harness MVP only supports explore / read_only tool profiles (got name={name}, profile={:?})",
            spec.tools.profile
        ),
    ))
}

fn is_default_explore_tools(tools: &ToolsSpec) -> bool {
    tools.allow.is_empty() && !tools.mcp && tools.extra.is_empty()
}

fn build_from_allow_list(
    allow: &[String],
    policy: PathPolicy,
) -> Result<Vec<std::sync::Arc<dyn one_core::tool::Tool>>, crate::protocol::ProtocolError> {
    use one_tools::{FindTool, GrepTool, LsTool, ReadTool};
    let mut out = Vec::new();
    for name in allow {
        match name.as_str() {
            "read" => out.push(std::sync::Arc::new(ReadTool::with_policy(policy.clone()))
                as std::sync::Arc<dyn one_core::tool::Tool>),
            "grep" => out.push(std::sync::Arc::new(GrepTool::with_policy(policy.clone()))
                as std::sync::Arc<dyn one_core::tool::Tool>),
            "find" => out.push(std::sync::Arc::new(FindTool::with_policy(policy.clone()))
                as std::sync::Arc<dyn one_core::tool::Tool>),
            "ls" => out.push(std::sync::Arc::new(LsTool::with_policy(policy.clone()))
                as std::sync::Arc<dyn one_core::tool::Tool>),
            #[cfg(feature = "network")]
            "web_search" => out.push(std::sync::Arc::new(one_tools::WebSearchTool::new())
                as std::sync::Arc<dyn one_core::tool::Tool>),
            #[cfg(feature = "network")]
            "web_fetch" => out.push(std::sync::Arc::new(one_tools::WebFetchTool::new())
                as std::sync::Arc<dyn one_core::tool::Tool>),
            other => {
                return Err(crate::protocol::ProtocolError::new(
                    error_code::INVALID_AGENT_SPEC,
                    format!("tool `{other}` not allowed in harness MVP allow-list"),
                ));
            }
        }
    }
    let _ = FailClosedAskUser; // ensure fail-closed path available; ask_user not registered
    Ok(out)
}

fn resolve_cwd(spec: &AgentSpec, opts: &HarnessOptions) -> PathBuf {
    if let Some(c) = &spec.cwd {
        return PathBuf::from(c);
    }
    opts.cwd.clone()
}

fn build_policy(cwd: &Path, opts: &HarnessOptions, spec: &AgentSpec) -> PathPolicy {
    if opts.full_access
        || spec.sandbox.as_deref() == Some("full-access")
    {
        return PathPolicy::full_access(cwd.to_path_buf());
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AgentSpec;
    use super::super::explore_tools::explore_tool_names;

    #[test]
    fn explore_tool_names_stable() {
        let n = explore_tool_names();
        assert!(n.contains(&"read".to_string()));
        assert!(!n.iter().any(|x| x == "bash"));
    }

    #[test]
    fn resolve_explore_prompt_uses_spec() {
        let s = AgentSpec::builtin_explore();
        let p = resolve_system_prompt(&s);
        assert!(p.contains("read-only") || p.contains("read-only") || p.contains("sub-agent"));
    }
}
