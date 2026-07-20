//! Extension runtime: registry + data + hooks dispatch.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use one_core::hooks::AgentHooks;
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
        let _ = self
            .runtime
            .emit(&ExtensionEvent::TurnStart { turn })
            .await;
    }

    async fn on_turn_end(&self, turn: usize) {
        let _ = self.runtime.emit(&ExtensionEvent::TurnEnd { turn }).await;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtensionRegistryBuilder;
    use one_core::tool_gate::AllowAllGate;
    use serde_json::json;

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
}
