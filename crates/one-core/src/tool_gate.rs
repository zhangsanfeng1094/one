//! Pre-tool permission gate (allow / deny / ask → resolve).
//!
//! Implementations live in higher layers (`one-tools` rules + `one-cli` interactive
//! approver). The agent only depends on this trait.

use async_trait::async_trait;

use crate::tool::ToolCall;

/// Result of a pre-tool permission check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolGateDecision {
    /// Run the tool.
    Allow,
    /// Do not run; message becomes the tool error result for the model.
    Deny { message: String },
}

/// Called by [`crate::agent::Agent`] immediately before each tool execution.
#[async_trait]
pub trait ToolGate: Send + Sync {
    async fn check(&self, call: &ToolCall) -> ToolGateDecision;
}

/// Always-allow gate (tests / full automation with other sandboxes).
pub struct AllowAllGate;

#[async_trait]
impl ToolGate for AllowAllGate {
    async fn check(&self, _call: &ToolCall) -> ToolGateDecision {
        ToolGateDecision::Allow
    }
}
