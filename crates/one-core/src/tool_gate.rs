//! Pre-tool permission / extension gate (allow / rewrite / deny) + post-tool hooks.
//!
//! Implementations live in higher layers (`one-tools` rules + `one-cli` interactive
//! approver + `one-ext` interceptors). The agent only depends on this trait.

use async_trait::async_trait;
use serde_json::Value;

use crate::tool::{ToolCall, ToolOutput};

/// Result of a pre-tool permission / extension check.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolGateDecision {
    /// Run the tool with the original arguments.
    Allow,
    /// Run the tool with rewritten arguments (extension PreToolUse).
    Rewrite { arguments: Value },
    /// Do not run; message becomes the tool error result for the model.
    Deny { message: String },
}

/// Called by [`crate::agent::Agent`] around each tool execution.
#[async_trait]
pub trait ToolGate: Send + Sync {
    /// Pre-tool check. May allow, deny, or rewrite arguments.
    async fn check(&self, call: &ToolCall) -> ToolGateDecision;

    /// Ask the human to confirm this call (hook/extension `Ask` decision).
    ///
    /// Returns the final gate decision for the call: `Allow` (user approved),
    /// `Rewrite` (e.g. sandbox escalation applied), or `Deny` (rejected).
    /// Default: interactive gates await their pending-approval flow; other
    /// gates (tests / `AllowAllGate`) allow.
    async fn confirm(&self, _call: &ToolCall, _reason: &str) -> ToolGateDecision {
        ToolGateDecision::Allow
    }

    /// Post-tool observe hook (audit, metrics, extension after_tool). Default no-op.
    async fn after_tool(&self, _call: &ToolCall, _output: &ToolOutput, _is_error: bool) {}

    /// Release once-use permission leases for this call.
    ///
    /// Must be sync and independent of [`Self::after_tool`]: observational
    /// hooks are fire-and-forget with a timeout, but once tokens still have
    /// to be revoked on success, failure, cancel, and gated-but-not-run.
    fn release_permission_lease(&self, _call: &ToolCall) {}
}

/// Always-allow gate (tests / full automation with other sandboxes).
pub struct AllowAllGate;

#[async_trait]
impl ToolGate for AllowAllGate {
    async fn check(&self, _call: &ToolCall) -> ToolGateDecision {
        ToolGateDecision::Allow
    }
}
