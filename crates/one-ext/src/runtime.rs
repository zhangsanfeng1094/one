//! Extension runtime: registry + data + hooks dispatch.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use one_core::hooks::{AgentHooks, StopDecision};
use one_core::tool::{Tool, ToolCall, ToolOutput};
use one_core::tool_gate::{ToolGate, ToolGateDecision};
use serde_json::Value;

use crate::data::ExtensionData;
use crate::events::{
    ExtensionCommand, ExtensionContext, ExtensionEvent, PreToolDecision, PromptFragment,
};
use crate::hooks::{self, HooksConfig};
use crate::registry::ExtensionRegistry;
use crate::traits::Extension;

/// Host runtime that owns installed extensions, session data, and external hooks.
pub struct ExtensionRuntime {
    registry: ExtensionRegistry,
    data: Arc<ExtensionData>,
    hooks: HooksConfig,
    cwd: PathBuf,
}

impl ExtensionRuntime {
    pub fn new(extensions: Vec<Arc<dyn Extension>>) -> Self {
        let mut builder = crate::registry::ExtensionRegistryBuilder::new();
        builder.install_all(extensions);
        Self::from_registry(builder.build(), HooksConfig::default(), PathBuf::from("."))
    }

    pub fn from_registry(registry: ExtensionRegistry, hooks: HooksConfig, cwd: PathBuf) -> Self {
        Self {
            registry,
            data: Arc::new(ExtensionData::new()),
            hooks,
            cwd,
        }
    }

    pub fn empty() -> Self {
        Self::from_registry(
            ExtensionRegistry::empty(),
            HooksConfig::default(),
            PathBuf::from("."),
        )
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    pub fn with_hooks(mut self, hooks: HooksConfig) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn data(&self) -> &Arc<ExtensionData> {
        &self.data
    }

    pub fn registry(&self) -> &ExtensionRegistry {
        &self.registry
    }

    pub fn hooks_config(&self) -> &HooksConfig {
        &self.hooks
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub async fn load_all(&self, ctx: &ExtensionContext<'_>) -> crate::Result<()> {
        for extension in self.registry.extensions() {
            extension.on_load(ctx).await?;
        }
        // True session lifecycle (once at process/extension load).
        let _ = self.emit(&ExtensionEvent::SessionStart).await;
        hooks::run_session_hooks(&self.hooks.session_start, "SessionStart", &self.cwd).await;
        Ok(())
    }

    pub async fn unload_all(&self, ctx: &ExtensionContext<'_>) -> crate::Result<()> {
        let _ = self.emit(&ExtensionEvent::SessionEnd).await;
        hooks::run_session_hooks(&self.hooks.session_end, "SessionEnd", &self.cwd).await;
        for extension in self.registry.extensions() {
            extension.on_unload(ctx).await?;
        }
        Ok(())
    }

    /// Fire session-start for conversation switches (`/new`, `/resume`) without reloading extensions.
    pub async fn notify_session_start(&self) {
        let _ = self.emit(&ExtensionEvent::SessionStart).await;
        hooks::run_session_hooks(&self.hooks.session_start, "SessionStart", &self.cwd).await;
    }

    /// Fire session-end before replacing the active conversation.
    pub async fn notify_session_end(&self) {
        let _ = self.emit(&ExtensionEvent::SessionEnd).await;
        hooks::run_session_hooks(&self.hooks.session_end, "SessionEnd", &self.cwd).await;
    }

    /// Fire PreCompact (extensions + hooks.json). `trigger` is `manual` or `auto`.
    pub async fn notify_pre_compact(&self, trigger: &str) {
        let _ = self
            .emit(&ExtensionEvent::PreCompact {
                trigger: trigger.to_string(),
            })
            .await;
        hooks::run_compact_hooks(&self.hooks.pre_compact, "PreCompact", trigger, &self.cwd).await;
    }

    /// Fire PostCompact (extensions + hooks.json). `trigger` is `manual` or `auto`.
    pub async fn notify_post_compact(&self, trigger: &str) {
        let _ = self
            .emit(&ExtensionEvent::PostCompact {
                trigger: trigger.to_string(),
            })
            .await;
        hooks::run_compact_hooks(&self.hooks.post_compact, "PostCompact", trigger, &self.cwd).await;
    }

    /// Fire UserPromptSubmit (extensions + hooks.json). Returns `Block` when a
    /// hook blocks the prompt (Grok Prompt gate); `Allow` otherwise.
    pub async fn notify_user_prompt_submit(&self, text: &str) -> hooks::PromptHookOutcome {
        let _ = self
            .emit(&ExtensionEvent::UserPromptSubmit {
                text: text.to_string(),
            })
            .await;
        hooks::run_user_prompt_hooks(&self.hooks.user_prompt_submit, text, &self.cwd).await
    }

    /// Fire SubagentStop (extensions + hooks.json) when a subagent job ends.
    pub async fn notify_subagent_stop(&self, job_id: &str, agent: &str, ok: bool, summary: &str) {
        let _ = self
            .emit(&ExtensionEvent::SubagentStop {
                job_id: job_id.to_string(),
                agent: agent.to_string(),
                ok,
                summary: summary.to_string(),
            })
            .await;
        hooks::run_subagent_stop_hooks(
            &self.hooks.subagent_stop,
            job_id,
            agent,
            ok,
            summary,
            &self.cwd,
        )
        .await;
    }

    pub fn make_context<'a>(
        &'a self,
        cwd: &'a Path,
        session_file: Option<&'a Path>,
    ) -> ExtensionContext<'a> {
        ExtensionContext {
            cwd,
            session_file,
            data: &self.data,
        }
    }

    pub async fn emit(&self, event: &ExtensionEvent) -> crate::Result<()> {
        for extension in self.registry.extensions() {
            if let Err(e) = extension.on_event(event).await {
                tracing::warn!(
                    extension = %extension.name(),
                    error = %e,
                    "extension on_event failed"
                );
            }
        }
        Ok(())
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.registry
            .extensions()
            .iter()
            .flat_map(|extension| extension.tools())
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.registry.names()
    }

    pub fn commands(&self) -> Vec<ExtensionCommand> {
        self.registry
            .extensions()
            .iter()
            .flat_map(|e| e.commands())
            .collect()
    }

    /// Merge all context fragments into a single system-prompt section.
    pub fn system_prompt_overlay(&self) -> Option<String> {
        let fragments: Vec<PromptFragment> = self
            .registry
            .extensions()
            .iter()
            .flat_map(|e| e.contribute_context())
            .collect();
        if fragments.is_empty() {
            return None;
        }
        let mut parts = vec!["# Extension context".to_string()];
        for f in fragments {
            parts.push(format!("## {}\n{}", f.source, f.text));
        }
        Some(parts.join("\n\n"))
    }

    pub fn custom_states(&self) -> Vec<(String, Value)> {
        self.registry
            .extensions()
            .iter()
            .filter_map(|extension| extension.custom_state())
            .collect()
    }

    pub fn restore_custom(&self, custom_type: &str, data: Value) {
        for extension in self.registry.extensions() {
            let _ = extension.restore_state(custom_type, &data);
        }
    }

    /// Run PreToolUse: Rust extensions first, then external hooks.
    pub async fn before_tool(&self, call: &ToolCall) -> PreToolDecision {
        let mut args = call.arguments.clone();
        let mut rewritten = false;

        for extension in self.registry.extensions() {
            let mut probe = call.clone();
            probe.arguments = args.clone();
            match extension.before_tool(&probe).await {
                Ok(PreToolDecision::Allow) => {}
                Ok(PreToolDecision::Rewrite { arguments }) => {
                    args = arguments;
                    rewritten = true;
                }
                Ok(PreToolDecision::Deny { message }) => {
                    return PreToolDecision::Deny { message };
                }
                Ok(PreToolDecision::Ask { message }) => {
                    return PreToolDecision::Ask { message };
                }
                Err(e) => {
                    tracing::warn!(
                        extension = %extension.name(),
                        error = %e,
                        "before_tool failed; treating as allow"
                    );
                }
            }
        }

        let mut probe = call.clone();
        probe.arguments = args.clone();
        match hooks::run_pre_tool_use(&self.hooks, &probe, &self.cwd).await {
            Ok(PreToolDecision::Allow) => {}
            Ok(PreToolDecision::Rewrite { arguments }) => {
                args = arguments;
                rewritten = true;
            }
            Ok(PreToolDecision::Deny { message }) => {
                return PreToolDecision::Deny { message };
            }
            Ok(PreToolDecision::Ask { message }) => {
                return PreToolDecision::Ask { message };
            }
            Err(e) => {
                tracing::warn!(error = %e, "pre_tool_use hooks failed");
            }
        }

        if rewritten {
            PreToolDecision::Rewrite { arguments: args }
        } else {
            PreToolDecision::Allow
        }
    }

    pub async fn after_tool(&self, call: &ToolCall, output: &ToolOutput, is_error: bool) {
        for extension in self.registry.extensions() {
            if let Err(e) = extension.after_tool(call, output, is_error).await {
                tracing::warn!(
                    extension = %extension.name(),
                    error = %e,
                    "after_tool failed"
                );
            }
        }
        hooks::run_post_tool_use(&self.hooks, call, output, is_error, &self.cwd).await;
        let _ = self
            .emit(&ExtensionEvent::ToolEnd {
                tool_call: call.clone(),
                output: output.clone(),
                is_error,
            })
            .await;
    }

    /// Bridge for `one_core::AgentHooks`.
    pub fn agent_hooks(self: &Arc<Self>) -> Arc<dyn AgentHooks> {
        Arc::new(RuntimeAgentHooks {
            runtime: Arc::clone(self),
        })
    }

    /// Composite tool gate: extension PreToolUse → inner permission gate → after hooks.
    pub fn tool_gate(self: &Arc<Self>, inner: Arc<dyn ToolGate>) -> Arc<dyn ToolGate> {
        Arc::new(ExtensionToolGate {
            runtime: Arc::clone(self),
            inner,
        })
    }
}

struct RuntimeAgentHooks {
    runtime: Arc<ExtensionRuntime>,
}

#[async_trait]
impl AgentHooks for RuntimeAgentHooks {
    // Agent start/end are *prompt* boundaries, not conversation sessions.
    // SessionStart/End are fired from load/unload and AppRuntime session open/new.
    async fn on_agent_start(&self) {}

    async fn on_agent_end(&self) {}

    async fn on_turn_start(&self, turn: usize) {
        let _ = self.runtime.emit(&ExtensionEvent::TurnStart { turn }).await;
    }

    async fn on_turn_end(&self, turn: usize) {
        let _ = self.runtime.emit(&ExtensionEvent::TurnEnd { turn }).await;
    }

    async fn on_stop(&self, turn: usize, last_assistant_message: Option<&str>) -> StopDecision {
        let _ = self
            .runtime
            .emit(&ExtensionEvent::Stop {
                turn,
                last_assistant_message: last_assistant_message.map(|s| s.to_string()),
            })
            .await;
        hooks::run_stop_hooks(
            self.runtime.hooks_config(),
            turn,
            last_assistant_message,
            self.runtime.cwd(),
        )
        .await
    }
}

struct ExtensionToolGate {
    runtime: Arc<ExtensionRuntime>,
    inner: Arc<dyn ToolGate>,
}

#[async_trait]
impl ToolGate for ExtensionToolGate {
    async fn check(&self, call: &ToolCall) -> ToolGateDecision {
        // 1) Extension + script PreToolUse
        let mut effective = call.clone();
        match self.runtime.before_tool(&effective).await {
            PreToolDecision::Allow => {}
            PreToolDecision::Rewrite { arguments } => {
                effective.arguments = arguments;
            }
            PreToolDecision::Deny { message } => {
                return ToolGateDecision::Deny { message };
            }
            PreToolDecision::Ask { message } => {
                // 2) Hook asked: route through the permission UI / approval flow
                //    before the regular rule check (Grok `ask` decision).
                let reason = if message.trim().is_empty() {
                    "PreToolUse hook requests user confirmation".to_string()
                } else {
                    message
                };
                match self.inner.confirm(&effective, &reason).await {
                    ToolGateDecision::Allow => {}
                    other => return other,
                }
            }
        }

        // 2) Permission / approval gate on (possibly rewritten) call
        let decision = self.inner.check(&effective).await;
        match decision {
            ToolGateDecision::Allow => {
                if effective.arguments != call.arguments {
                    ToolGateDecision::Rewrite {
                        arguments: effective.arguments,
                    }
                } else {
                    ToolGateDecision::Allow
                }
            }
            ToolGateDecision::Rewrite { arguments } => ToolGateDecision::Rewrite { arguments },
            deny @ ToolGateDecision::Deny { .. } => deny,
        }
    }

    async fn after_tool(&self, call: &ToolCall, output: &ToolOutput, is_error: bool) {
        self.runtime.after_tool(call, output, is_error).await;
        self.inner.after_tool(call, output, is_error).await;
    }

    async fn confirm(&self, call: &ToolCall, reason: &str) -> ToolGateDecision {
        self.inner.confirm(call, reason).await
    }

    fn release_permission_lease(&self, call: &ToolCall) {
        self.inner.release_permission_lease(call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtensionRegistryBuilder;
    use one_core::tool_gate::AllowAllGate;
    use serde_json::json;

    /// Inner gate that records `confirm` calls and returns a canned decision.
    struct AskRecordingGate {
        confirmed: std::sync::Mutex<Vec<String>>,
        allow: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl ToolGate for AskRecordingGate {
        async fn check(&self, _call: &ToolCall) -> ToolGateDecision {
            ToolGateDecision::Allow
        }

        async fn confirm(&self, _call: &ToolCall, reason: &str) -> ToolGateDecision {
            self.confirmed.lock().unwrap().push(reason.to_string());
            if self.allow.load(std::sync::atomic::Ordering::Relaxed) {
                ToolGateDecision::Allow
            } else {
                ToolGateDecision::Deny {
                    message: "user said no".into(),
                }
            }
        }
    }

    struct DenyBash;

    #[async_trait]
    impl Extension for DenyBash {
        fn name(&self) -> &str {
            "deny-bash"
        }

        async fn before_tool(&self, call: &ToolCall) -> crate::Result<PreToolDecision> {
            if call.name == "bash" {
                Ok(PreToolDecision::Deny {
                    message: "no bash".into(),
                })
            } else {
                Ok(PreToolDecision::Allow)
            }
        }
    }

    struct RewriteRead;

    #[async_trait]
    impl Extension for RewriteRead {
        fn name(&self) -> &str {
            "rewrite-read"
        }

        async fn before_tool(&self, call: &ToolCall) -> crate::Result<PreToolDecision> {
            if call.name == "read" {
                Ok(PreToolDecision::Rewrite {
                    arguments: json!({"path": "/safe/file.txt"}),
                })
            } else {
                Ok(PreToolDecision::Allow)
            }
        }
    }

    #[tokio::test]
    async fn gate_denies_via_extension() {
        let mut b = ExtensionRegistryBuilder::new();
        b.install(Arc::new(DenyBash));
        let rt = Arc::new(ExtensionRuntime::from_registry(
            b.build(),
            HooksConfig::default(),
            PathBuf::from("/tmp"),
        ));
        let gate = rt.tool_gate(Arc::new(AllowAllGate));
        let call = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
        };
        match gate.check(&call).await {
            ToolGateDecision::Deny { message } => assert!(message.contains("no bash")),
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gate_rewrites_args() {
        let mut b = ExtensionRegistryBuilder::new();
        b.install(Arc::new(RewriteRead));
        let rt = Arc::new(ExtensionRuntime::from_registry(
            b.build(),
            HooksConfig::default(),
            PathBuf::from("/tmp"),
        ));
        let gate = rt.tool_gate(Arc::new(AllowAllGate));
        let call = ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: json!({"path": "/etc/passwd"}),
        };
        match gate.check(&call).await {
            ToolGateDecision::Rewrite { arguments } => {
                assert_eq!(arguments["path"], "/safe/file.txt");
            }
            other => panic!("expected rewrite, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pre_tool_ask_routes_to_inner_confirm() {
        // Hook asks → inner.confirm() decides.
        let hooks_raw = r#"{
            "pre_tool_use": [{
                "matcher": "bash",
                "command": "echo '{\"permissionDecision\":\"ask\",\"systemMessage\":\"check this\"}'"
            }]
        }"#;
        let cfg: HooksConfig = serde_json::from_str(hooks_raw).unwrap();
        let rt = Arc::new(ExtensionRuntime::from_registry(
            ExtensionRegistryBuilder::new().build(),
            cfg,
            PathBuf::from("/tmp"),
        ));
        let inner = Arc::new(AskRecordingGate {
            confirmed: std::sync::Mutex::new(Vec::new()),
            allow: std::sync::atomic::AtomicBool::new(true),
        });
        let gate = rt.tool_gate(inner.clone() as Arc<dyn ToolGate>);
        let call = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
        };
        assert!(matches!(gate.check(&call).await, ToolGateDecision::Allow));
        assert_eq!(
            inner.confirmed.lock().unwrap().as_slice(),
            ["check this".to_string()]
        );

        // User denies → Deny flows back to the agent.
        let inner2 = Arc::new(AskRecordingGate {
            confirmed: std::sync::Mutex::new(Vec::new()),
            allow: std::sync::atomic::AtomicBool::new(false),
        });
        let gate2 = rt.tool_gate(inner2.clone() as Arc<dyn ToolGate>);
        match gate2.check(&call).await {
            ToolGateDecision::Deny { message } => assert_eq!(message, "user said no"),
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subagent_stop_hook_runs_on_notify() {
        let hooks_raw = r#"{
            "subagent_stop": [{ "command": "true" }]
        }"#;
        let cfg: HooksConfig = serde_json::from_str(hooks_raw).unwrap();
        let rt = ExtensionRuntime::from_registry(
            ExtensionRegistryBuilder::new().build(),
            cfg,
            PathBuf::from("/tmp"),
        );
        rt.notify_subagent_stop("job_1", "explore", true, "done")
            .await;
    }

    #[tokio::test]
    async fn stop_hook_exit_code_2_blocks() {
        let hooks_raw = r#"{
            "stop": [
                {
                    "name": "lint-check",
                    "command": "echo 'linter error on line 42' >&2; exit 2"
                }
            ]
        }"#;
        let cfg: HooksConfig = serde_json::from_str(hooks_raw).unwrap();
        let rt = Arc::new(ExtensionRuntime::from_registry(
            ExtensionRegistryBuilder::new().build(),
            cfg,
            PathBuf::from("/tmp"),
        ));
        let agent_hooks = rt.agent_hooks();
        let decision = agent_hooks.on_stop(1, Some("done")).await;
        match decision {
            StopDecision::Block { reason } => {
                assert!(reason.contains("linter error on line 42"));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stop_hook_json_block() {
        let hooks_raw = r#"{
            "stop": [
                {
                    "name": "quality-gate",
                    "command": "echo '{\"decision\": \"block\", \"reason\": \"unit tests missing\"}'"
                }
            ]
        }"#;
        let cfg: HooksConfig = serde_json::from_str(hooks_raw).unwrap();
        let rt = Arc::new(ExtensionRuntime::from_registry(
            ExtensionRegistryBuilder::new().build(),
            cfg,
            PathBuf::from("/tmp"),
        ));
        let agent_hooks = rt.agent_hooks();
        let decision = agent_hooks.on_stop(1, Some("done")).await;
        match decision {
            StopDecision::Block { reason } => {
                assert_eq!(reason, "unit tests missing");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }
}
