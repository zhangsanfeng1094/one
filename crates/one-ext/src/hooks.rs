//! External command and HTTP hooks (Grok & Codex compatible).
//!
//! Config: `~/.one/agent/hooks.json` or plugin-declared hook files.
//! Handlers run as subprocesses (stdin/stdout JSON) or HTTP endpoints (POST JSON).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use one_core::hooks::StopDecision;
use one_core::tool::{ToolCall, ToolOutput};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::events::PreToolDecision;

/// Top-level hooks config file supporting both flat and nested (`"hooks": { ... }`) layouts.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HooksConfig {
    #[serde(default)]
    pub pre_tool_use: Vec<HookHandler>,
    #[serde(default)]
    pub post_tool_use: Vec<HookHandler>,
    #[serde(default)]
    pub post_tool_use_failure: Vec<HookHandler>,
    #[serde(default)]
    pub stop: Vec<HookHandler>,
    #[serde(default)]
    pub subagent_stop: Vec<HookHandler>,
    #[serde(default)]
    pub session_start: Vec<HookHandler>,
    #[serde(default)]
    pub session_end: Vec<HookHandler>,
    #[serde(default)]
    pub user_prompt_submit: Vec<HookHandler>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookHandler {
    /// Optional regex / glob matched against tool name (Pre/PostToolUse).
    #[serde(default)]
    pub matcher: Option<String>,
    /// Execution type: "command" (default) or "http".
    #[serde(default, alias = "type")]
    pub hook_type: Option<String>,
    /// HTTP Webhook URL (for hook_type = "http").
    #[serde(default)]
    pub url: Option<String>,
    /// Command argv (first element is program, or full string command).
    #[serde(default, deserialize_with = "deserialize_command")]
    pub command: Vec<String>,
    /// Timeout seconds (default 30).
    #[serde(default = "default_timeout", alias = "timeout")]
    pub timeout_sec: u64,
    /// Human label for logs.
    #[serde(default)]
    pub name: Option<String>,
    /// Environment variables injected for command runner.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_timeout() -> u64 {
    30
}

fn deserialize_command<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum CmdOrList {
        List(Vec<String>),
        Str(String),
    }

    match Option::<CmdOrList>::deserialize(deserializer)? {
        Some(CmdOrList::List(l)) => Ok(l),
        Some(CmdOrList::Str(s)) => {
            if s.trim().is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec!["/bin/sh".into(), "-c".into(), s])
            }
        }
        None => Ok(Vec::new()),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawHookItem {
    Handler(HookHandler),
    Group(RawHookGroup),
}

#[derive(Debug, Clone, Deserialize)]
struct RawHookGroup {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<HookHandler>,
}

fn flatten_raw_hooks(items: Vec<RawHookItem>) -> Vec<HookHandler> {
    let mut out = Vec::new();
    for item in items {
        match item {
            RawHookItem::Handler(h) => out.push(h),
            RawHookItem::Group(g) => {
                for mut h in g.hooks {
                    if h.matcher.is_none() {
                        h.matcher = g.matcher.clone();
                    }
                    out.push(h);
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawHooksMap {
    #[serde(default, alias = "PreToolUse", alias = "preToolUse", alias = "pre_tool_use")]
    pre_tool_use: Vec<RawHookItem>,
    #[serde(default, alias = "PostToolUse", alias = "postToolUse", alias = "post_tool_use")]
    post_tool_use: Vec<RawHookItem>,
    #[serde(default, alias = "PostToolUseFailure", alias = "postToolUseFailure", alias = "post_tool_use_failure")]
    post_tool_use_failure: Vec<RawHookItem>,
    #[serde(default, alias = "Stop", alias = "stop")]
    stop: Vec<RawHookItem>,
    #[serde(default, alias = "SubagentStop", alias = "subagentStop", alias = "subagent_stop", alias = "SubagentEnd", alias = "subagent_end")]
    subagent_stop: Vec<RawHookItem>,
    #[serde(default, alias = "SessionStart", alias = "sessionStart", alias = "session_start")]
    session_start: Vec<RawHookItem>,
    #[serde(default, alias = "SessionEnd", alias = "sessionEnd", alias = "session_end")]
    session_end: Vec<RawHookItem>,
    #[serde(default, alias = "UserPromptSubmit", alias = "userPromptSubmit", alias = "user_prompt_submit", alias = "beforeSubmitPrompt")]
    user_prompt_submit: Vec<RawHookItem>,
}

impl<'de> Deserialize<'de> for HooksConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            hooks: Option<RawHooksMap>,
            #[serde(flatten)]
            flat: RawHooksMap,
        }

        let wrapper = Wrapper::deserialize(deserializer)?;
        let raw = if let Some(inner) = wrapper.hooks {
            inner
        } else {
            wrapper.flat
        };

        Ok(HooksConfig {
            pre_tool_use: flatten_raw_hooks(raw.pre_tool_use),
            post_tool_use: flatten_raw_hooks(raw.post_tool_use),
            post_tool_use_failure: flatten_raw_hooks(raw.post_tool_use_failure),
            stop: flatten_raw_hooks(raw.stop),
            subagent_stop: flatten_raw_hooks(raw.subagent_stop),
            session_start: flatten_raw_hooks(raw.session_start),
            session_end: flatten_raw_hooks(raw.session_end),
            user_prompt_submit: flatten_raw_hooks(raw.user_prompt_submit),
        })
    }
}

impl HooksConfig {
    pub fn load_file(path: &Path) -> crate::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            return toml_from_str(&raw);
        }
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn merge(mut self, other: Self) -> Self {
        self.pre_tool_use.extend(other.pre_tool_use);
        self.post_tool_use.extend(other.post_tool_use);
        self.post_tool_use_failure.extend(other.post_tool_use_failure);
        self.stop.extend(other.stop);
        self.subagent_stop.extend(other.subagent_stop);
        self.session_start.extend(other.session_start);
        self.session_end.extend(other.session_end);
        self.user_prompt_submit.extend(other.user_prompt_submit);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.post_tool_use_failure.is_empty()
            && self.stop.is_empty()
            && self.subagent_stop.is_empty()
            && self.session_start.is_empty()
            && self.session_end.is_empty()
            && self.user_prompt_submit.is_empty()
    }
}

fn toml_from_str(raw: &str) -> crate::Result<HooksConfig> {
    let _ = raw;
    Err(crate::ExtError::Toml(
        "hooks.toml is reserved; use hooks.json for now".into(),
    ))
}

/// Discover hooks from agent dir + optional extra files.
pub fn load_hooks(agent_dir: &Path, extra_files: &[PathBuf]) -> HooksConfig {
    let mut cfg = HooksConfig::default();
    for path in [
        agent_dir.join("hooks.json"),
        agent_dir.join("hooks").join("hooks.json"),
    ] {
        if path.is_file() {
            match HooksConfig::load_file(&path) {
                Ok(c) => cfg = cfg.merge(c),
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "hooks load failed"),
            }
        }
    }
    for path in extra_files {
        if path.is_file() {
            match HooksConfig::load_file(path) {
                Ok(c) => cfg = cfg.merge(c),
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "hooks load failed"),
            }
        }
    }
    cfg
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreToolUseRequest {
    hook_event_name: &'static str,
    tool_name: String,
    tool_input: Value,
    cwd: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreToolUseResponse {
    #[serde(default, alias = "decision")]
    permission_decision: Option<String>,
    #[serde(default)]
    updated_input: Option<Value>,
    #[serde(default, alias = "systemMessage", alias = "reason")]
    system_message: Option<String>,
    #[serde(default, rename = "continue")]
    continue_flag: Option<bool>,
    #[serde(default)]
    hook_specific_output: Option<PreToolHookSpecificOutput>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreToolHookSpecificOutput {
    #[serde(default)]
    updated_input: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PostToolUseRequest {
    hook_event_name: &'static str,
    tool_name: String,
    tool_input: Value,
    tool_output: String,
    is_error: bool,
    cwd: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StopRequest {
    hook_event_name: &'static str,
    turn: usize,
    last_assistant_message: Option<String>,
    cwd: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StopResponse {
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default, rename = "continue")]
    continue_flag: Option<bool>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    hook_specific_output: Option<StopHookSpecificOutput>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StopHookSpecificOutput {
    #[serde(default)]
    additional_context: Option<String>,
}

/// Run PreToolUse command hooks; first Deny wins; rewrites compose left-to-right.
pub async fn run_pre_tool_use(
    hooks: &HooksConfig,
    call: &ToolCall,
    cwd: &Path,
) -> crate::Result<PreToolDecision> {
    let mut decision = PreToolDecision::Allow;
    let mut args = call.arguments.clone();

    for handler in &hooks.pre_tool_use {
        if !matcher_hits(handler.matcher.as_deref(), &call.name) {
            continue;
        }
        let req = PreToolUseRequest {
            hook_event_name: "PreToolUse",
            tool_name: call.name.clone(),
            tool_input: args.clone(),
            cwd: cwd.display().to_string(),
        };
        let exec_res = match run_handler_raw(handler, &req, "PreToolUse", cwd).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    hook = %handler.name.as_deref().unwrap_or("pre_tool_use"),
                    error = %e,
                    "pre_tool_use hook failed; continuing"
                );
                continue;
            }
        };

        // Exit code 2 explicitly blocks with stderr feedback
        if exec_res.exit_code == Some(2) {
            let msg = if !exec_res.stderr.trim().is_empty() {
                exec_res.stderr.trim().to_string()
            } else {
                "blocked by PreToolUse hook (exit 2)".into()
            };
            return Ok(PreToolDecision::Deny { message: msg });
        }

        if exec_res.stdout.trim().is_empty() {
            continue;
        }
        let resp: PreToolUseResponse = match serde_json::from_str(&exec_res.stdout) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, body = %exec_res.stdout, "pre_tool_use invalid JSON");
                continue;
            }
        };
        if resp.continue_flag == Some(false) {
            let msg = resp
                .system_message
                .unwrap_or_else(|| "blocked by PreToolUse hook".into());
            return Ok(PreToolDecision::Deny { message: msg });
        }
        if let Some(dec) = resp.permission_decision.as_deref() {
            if dec.eq_ignore_ascii_case("deny") {
                let msg = resp
                    .system_message
                    .unwrap_or_else(|| "denied by PreToolUse hook".into());
                return Ok(PreToolDecision::Deny { message: msg });
            }
        }
        if let Some(updated) = resp
            .updated_input
            .or_else(|| resp.hook_specific_output.and_then(|h| h.updated_input))
        {
            args = updated;
            decision = PreToolDecision::Rewrite {
                arguments: args.clone(),
            };
        }
    }
    Ok(decision)
}

/// Fire-and-forget PostToolUse hooks (errors logged).
pub async fn run_post_tool_use(
    hooks: &HooksConfig,
    call: &ToolCall,
    output: &ToolOutput,
    is_error: bool,
    cwd: &Path,
) {
    let target_hooks = if is_error && !hooks.post_tool_use_failure.is_empty() {
        &hooks.post_tool_use_failure
    } else {
        &hooks.post_tool_use
    };

    for handler in target_hooks {
        if !matcher_hits(handler.matcher.as_deref(), &call.name) {
            continue;
        }
        let req = PostToolUseRequest {
            hook_event_name: if is_error {
                "PostToolUseFailure"
            } else {
                "PostToolUse"
            },
            tool_name: call.name.clone(),
            tool_input: call.arguments.clone(),
            tool_output: output.as_ui_text(),
            is_error,
            cwd: cwd.display().to_string(),
        };
        if let Err(e) = run_handler_raw(handler, &req, "PostToolUse", cwd).await {
            tracing::warn!(
                hook = %handler.name.as_deref().unwrap_or("post_tool_use"),
                error = %e,
                "post_tool_use hook failed"
            );
        }
    }
}

/// Run Stop hooks at the end of an agent turn.
pub async fn run_stop_hooks(
    hooks: &HooksConfig,
    turn: usize,
    last_assistant_message: Option<&str>,
    cwd: &Path,
) -> StopDecision {
    for handler in &hooks.stop {
        let req = StopRequest {
            hook_event_name: "Stop",
            turn,
            last_assistant_message: last_assistant_message.map(|s| s.to_string()),
            cwd: cwd.display().to_string(),
        };
        match run_handler_raw(handler, &req, "Stop", cwd).await {
            Ok(exec_res) => {
                // Exit code 2: block stop, feed stderr back
                if exec_res.exit_code == Some(2) {
                    let reason = if !exec_res.stderr.trim().is_empty() {
                        exec_res.stderr.trim().to_string()
                    } else {
                        "Stop blocked by hook with exit code 2".to_string()
                    };
                    return StopDecision::Block { reason };
                }
                if exec_res.stdout.trim().is_empty() {
                    continue;
                }
                if let Ok(resp) = serde_json::from_str::<StopResponse>(&exec_res.stdout) {
                    if let Some(dec) = resp.decision.as_deref() {
                        if dec.eq_ignore_ascii_case("block") {
                            let reason = resp.reason.unwrap_or_else(|| "Blocked by Stop hook".to_string());
                            return StopDecision::Block { reason };
                        }
                    }
                    if resp.continue_flag == Some(false) {
                        return StopDecision::ForceStop {
                            reason: resp.stop_reason.or(resp.reason),
                        };
                    }
                    if let Some(hook_out) = resp.hook_specific_output {
                        if let Some(ctx) = hook_out.additional_context {
                            return StopDecision::Block { reason: ctx };
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    hook = %handler.name.as_deref().unwrap_or("stop"),
                    error = %e,
                    "stop hook failed"
                );
            }
        }
    }
    StopDecision::Allow
}

pub async fn run_session_hooks(hooks: &[HookHandler], event: &str, cwd: &Path) {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Req {
        hook_event_name: String,
        cwd: String,
    }
    let req = Req {
        hook_event_name: event.into(),
        cwd: cwd.display().to_string(),
    };
    for handler in hooks {
        if let Err(e) = run_handler_raw(handler, &req, event, cwd).await {
            tracing::warn!(
                hook = %handler.name.as_deref().unwrap_or(event),
                error = %e,
                "session hook failed"
            );
        }
    }
}

pub fn matcher_hits(matcher: Option<&str>, tool_name: &str) -> bool {
    let Some(pat) = matcher else {
        return true;
    };
    if pat.is_empty() || pat == "*" {
        return true;
    }

    // Support pipe-separated matchers e.g. "Bash|Write|Edit"
    if pat.contains('|') {
        return pat
            .split('|')
            .any(|part| single_matcher_hits(part.trim(), tool_name));
    }

    single_matcher_hits(pat.trim(), tool_name)
}

fn single_matcher_hits(pat: &str, tool_name: &str) -> bool {
    if pat.is_empty() || pat == "*" {
        return true;
    }

    // Check alias maps
    if matches_alias(pat, tool_name) {
        return true;
    }

    // Simple glob: exact, prefix*, *suffix, *contains*
    if let Some(middle) = pat.strip_prefix('*').and_then(|p| p.strip_suffix('*')) {
        return tool_name.contains(middle);
    }
    if let Some(prefix) = pat.strip_suffix('*') {
        return tool_name.starts_with(prefix);
    }
    if let Some(suffix) = pat.strip_prefix('*') {
        return tool_name.ends_with(suffix);
    }
    tool_name.eq_ignore_ascii_case(pat)
}

fn matches_alias(pat: &str, tool_name: &str) -> bool {
    match pat.to_ascii_lowercase().as_str() {
        "bash" => matches!(tool_name, "bash" | "run_terminal_command"),
        "read" => matches!(tool_name, "read" | "read_file"),
        "edit" | "write" | "multiedit" => matches!(tool_name, "edit" | "write" | "search_replace"),
        "grep" => matches!(tool_name, "grep"),
        "glob" | "listdir" | "list_dir" | "listfiles" => {
            matches!(tool_name, "glob" | "listdir" | "list_dir" | "find" | "ls")
        }
        "task" => matches!(tool_name, "task" | "spawn_subagent"),
        _ => false,
    }
}

#[derive(Debug, Clone, Default)]
pub struct HookExecutionResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

async fn run_handler_raw<T: Serialize>(
    handler: &HookHandler,
    body: &T,
    event_name: &str,
    cwd: &Path,
) -> crate::Result<HookExecutionResult> {
    // HTTP Runner
    if handler.hook_type.as_deref() == Some("http")
        || (handler.command.is_empty() && handler.url.is_some())
    {
        let Some(url) = handler.url.as_deref() else {
            return Err(crate::ExtError::Hook {
                name: handler.name.clone().unwrap_or_default(),
                message: "HTTP hook missing 'url' field".into(),
            });
        };
        let timeout = Duration::from_secs(handler.timeout_sec.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| crate::ExtError::Hook {
                name: handler.name.clone().unwrap_or_else(|| url.to_string()),
                message: e.to_string(),
            })?;

        let resp = client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|e| crate::ExtError::Hook {
                name: handler.name.clone().unwrap_or_else(|| url.to_string()),
                message: e.to_string(),
            })?;

        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return Ok(HookExecutionResult {
            stdout: body_text,
            stderr: String::new(),
            exit_code: Some(if status.is_success() { 0 } else { 1 }),
        });
    }

    // Command Runner
    if handler.command.is_empty() {
        return Err(crate::ExtError::Hook {
            name: handler.name.clone().unwrap_or_default(),
            message: "empty command".into(),
        });
    }

    let program = &handler.command[0];
    let args = &handler.command[1..];
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("ONE_HOOK_EVENT", event_name)
        .env("ONE_CWD", cwd.display().to_string());

    if let Some(name) = &handler.name {
        cmd.env("ONE_HOOK_NAME", name);
    }
    for (k, v) in &handler.env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().map_err(|e| crate::ExtError::Hook {
        name: handler.name.clone().unwrap_or_else(|| program.clone()),
        message: e.to_string(),
    })?;

    let payload = serde_json::to_vec(body)?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&payload).await?;
        stdin.shutdown().await.ok();
    }

    let timeout = Duration::from_secs(handler.timeout_sec.max(1));
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| crate::ExtError::Hook {
            name: handler.name.clone().unwrap_or_else(|| program.clone()),
            message: format!("timeout after {}s", handler.timeout_sec),
        })?
        .map_err(|e| crate::ExtError::Hook {
            name: handler.name.clone().unwrap_or_else(|| program.clone()),
            message: e.to_string(),
        })?;

    Ok(HookExecutionResult {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher_glob() {
        assert!(matcher_hits(None, "bash"));
        assert!(matcher_hits(Some("bash"), "bash"));
        assert!(matcher_hits(Some("ba*"), "bash"));
        assert!(matcher_hits(Some("Bash"), "run_terminal_command"));
        assert!(matcher_hits(Some("Edit|Write"), "search_replace"));
        assert!(!matcher_hits(Some("write"), "bash"));
    }

    #[test]
    fn parse_hooks_json_flat_and_nested() {
        let raw_flat = r#"{
            "preToolUse": [{
                "matcher": "bash",
                "command": ["echo", "{}"]
            }],
            "stop": [{
                "command": "python3 check.py",
                "timeout": 15
            }]
        }"#;
        let cfg: HooksConfig = serde_json::from_str(raw_flat).unwrap();
        assert_eq!(cfg.pre_tool_use.len(), 1);
        assert_eq!(cfg.pre_tool_use[0].matcher.as_deref(), Some("bash"));
        assert_eq!(cfg.stop.len(), 1);
        assert_eq!(cfg.stop[0].timeout_sec, 15);

        let raw_nested = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash|Write",
                        "hooks": [
                            { "type": "command", "command": "bin/check.sh" }
                        ]
                    }
                ]
            }
        }"#;
        let cfg2: HooksConfig = serde_json::from_str(raw_nested).unwrap();
        assert_eq!(cfg2.pre_tool_use.len(), 1);
        assert_eq!(cfg2.pre_tool_use[0].matcher.as_deref(), Some("Bash|Write"));
    }
}
