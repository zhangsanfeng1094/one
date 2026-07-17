//! Extension facade trait — Codex contributor surface collapsed into one trait
//! with default methods (simpler for Rust dyn-dispatch while covering the same
//! capability points).

use std::sync::Arc;

use async_trait::async_trait;
use one_core::tool::{Tool, ToolCall, ToolOutput};
use serde_json::Value;

use crate::events::{
    ExtensionCommand, ExtensionContext, ExtensionEvent, PreToolDecision, PromptFragment,
};

/// Rust-native extension (tools + lifecycle + context + interceptors).
///
/// Aligns with Codex contributor roles:
/// - tools → `ToolContributor`
/// - context → `ContextContributor`
/// - session/turn → `ThreadLifecycle` / `TurnLifecycle`
/// - before/after tool → `ToolLifecycle` + PreToolUse intercept
/// - commands → slash handlers
/// - custom_state → session persistence
#[async_trait]
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;

    /// Called once when the extension is loaded into a runtime.
    async fn on_load(&self, _ctx: &ExtensionContext<'_>) -> crate::Result<()> {
        Ok(())
    }

    /// Called on `/reload` or process shutdown of the extension set.
    async fn on_unload(&self, _ctx: &ExtensionContext<'_>) -> crate::Result<()> {
        Ok(())
    }

    /// Observe lifecycle events (session / turn / tool / compact).
    async fn on_event(&self, _event: &ExtensionEvent) -> crate::Result<()> {
        Ok(())
    }

    /// Register extra tools for the agent (Act mode).
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    /// System-prompt fragments (stable for the session).
    fn contribute_context(&self) -> Vec<PromptFragment> {
        Vec::new()
    }

    /// Slash commands (e.g. `/status`).
    fn commands(&self) -> Vec<ExtensionCommand> {
        Vec::new()
    }

    /// PreToolUse intercept: allow / deny / rewrite arguments.
    ///
    /// Runs **before** the permission gate so extensions can soft-block or
    /// sanitize inputs. Deny short-circuits; Rewrite feeds the next gate check.
    async fn before_tool(&self, _call: &ToolCall) -> crate::Result<PreToolDecision> {
        Ok(PreToolDecision::Allow)
    }

    /// PostToolUse observe (after execution, including soft failures).
    async fn after_tool(
        &self,
        _call: &ToolCall,
        _output: &ToolOutput,
        _is_error: bool,
    ) -> crate::Result<()> {
        Ok(())
    }

    /// Optional persistent state stored as session `custom` entries.
    fn custom_state(&self) -> Option<(String, Value)> {
        None
    }

    fn restore_state(&self, _custom_type: &str, _data: &Value) -> crate::Result<()> {
        Ok(())
    }
}
