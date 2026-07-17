//! External command hooks (Codex-style PreToolUse / PostToolUse scripts).
//!
//! Config: `~/.one/agent/hooks.json` or plugin-declared hook files.
//! Each handler runs as a subprocess with JSON on stdin; JSON on stdout may
//! deny / rewrite / inject context.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use one_core::tool::{ToolCall, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::events::PreToolDecision;

/// Top-level hooks config file.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HooksConfig {
    #[serde(default)]
    pub pre_tool_use: Vec<HookHandler>,
    #[serde(default)]
    pub post_tool_use: Vec<HookHandler>,
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
    /// Optional regex matched against tool name (Pre/PostToolUse).
    #[serde(default)]
    pub matcher: Option<String>,
    /// Command argv (first element is program).
    pub command: Vec<String>,
    /// Timeout seconds (default 30).
    #[serde(default = "default_timeout")]
    pub timeout_sec: u64,
    /// Human label for logs.
    #[serde(default)]
    pub name: Option<String>,
}

fn default_timeout() -> u64 {
    30
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
        self.session_start.extend(other.session_start);
        self.session_end.extend(other.session_end);
        self.user_prompt_submit.extend(other.user_prompt_submit);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.session_start.is_empty()
            && self.session_end.is_empty()
            && self.user_prompt_submit.is_empty()
    }
}

fn toml_from_str(raw: &str) -> crate::Result<HooksConfig> {
    // Minimal TOML support without adding toml dep: reject with clear error,
    // prefer JSON. (Can add `toml` crate later.)
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
    #[serde(default)]
    permission_decision: Option<String>,
    #[serde(default)]
    updated_input: Option<Value>,
    #[serde(default)]
    #[serde(alias = "systemMessage")]
    system_message: Option<String>,
    #[serde(default)]
    #[serde(rename = "continue")]
    continue_flag: Option<bool>,
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
        let raw = match run_handler(handler, &req).await {
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
        if raw.trim().is_empty() {
            continue;
        }
        let resp: PreToolUseResponse = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, body = %raw, "pre_tool_use invalid JSON");
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
        if let Some(updated) = resp.updated_input {
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
    for handler in &hooks.post_tool_use {
        if !matcher_hits(handler.matcher.as_deref(), &call.name) {
            continue;
        }
        let req = PostToolUseRequest {
            hook_event_name: "PostToolUse",
            tool_name: call.name.clone(),
            tool_input: call.arguments.clone(),
            tool_output: output.as_ui_text(),
            is_error,
            cwd: cwd.display().to_string(),
        };
        if let Err(e) = run_handler(handler, &req).await {
            tracing::warn!(
                hook = %handler.name.as_deref().unwrap_or("post_tool_use"),
                error = %e,
                "post_tool_use hook failed"
            );
        }
    }
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
        if let Err(e) = run_handler(handler, &req).await {
            tracing::warn!(
                hook = %handler.name.as_deref().unwrap_or(event),
                error = %e,
                "session hook failed"
            );
        }
    }
}

fn matcher_hits(matcher: Option<&str>, tool_name: &str) -> bool {
    let Some(pat) = matcher else {
        return true;
    };
    if pat.is_empty() || pat == "*" {
        return true;
    }
    // Simple glob: exact, prefix*, or *suffix, or contains.
    if let Some(prefix) = pat.strip_suffix('*') {
        return tool_name.starts_with(prefix);
    }
    if let Some(suffix) = pat.strip_prefix('*') {
        return tool_name.ends_with(suffix);
    }
    tool_name == pat
}

async fn run_handler<T: Serialize>(handler: &HookHandler, body: &T) -> crate::Result<String> {
    if handler.command.is_empty() {
        return Err(crate::ExtError::Hook {
            name: handler.name.clone().unwrap_or_default(),
            message: "empty command".into(),
        });
    }
    let program = &handler.command[0];
    let args = &handler.command[1..];
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| crate::ExtError::Hook {
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

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(crate::ExtError::Hook {
            name: handler.name.clone().unwrap_or_else(|| program.clone()),
            message: format!("exit {:?}: {stderr}", output.status.code()),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher_glob() {
        assert!(matcher_hits(None, "bash"));
        assert!(matcher_hits(Some("bash"), "bash"));
        assert!(matcher_hits(Some("ba*"), "bash"));
        assert!(!matcher_hits(Some("write"), "bash"));
    }

    #[test]
    fn parse_hooks_json() {
        let raw = r#"{
            "preToolUse": [{
                "matcher": "bash",
                "command": ["echo", "{}"]
            }]
        }"#;
        let cfg: HooksConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.pre_tool_use.len(), 1);
        assert_eq!(cfg.pre_tool_use[0].matcher.as_deref(), Some("bash"));
    }
}
