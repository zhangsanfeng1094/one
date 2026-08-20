//! Optional async lifecycle hooks for the agent loop.
//!
//! Used by `one-ext` (via a bridge in `one-cli`) so Core stays free of extension
//! types while still firing session / turn boundaries inside the loop.

use async_trait::async_trait;

/// Decision returned by Stop hooks at the end of an agent turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDecision {
    /// Allow the turn to finish normally.
    Allow,
    /// Block the turn from finishing, feed reason back to the agent for another continuation loop.
    Block { reason: String },
    /// Force turn completion overriding any blocks.
    ForceStop { reason: Option<String> },
}

/// Async hooks invoked from [`crate::agent::Agent::run`].
///
/// All methods have default no-ops so implementors only override what they need.
#[async_trait]
pub trait AgentHooks: Send + Sync {
    async fn on_agent_start(&self) {}
    async fn on_agent_end(&self) {}
    async fn on_turn_start(&self, _turn: usize) {}
    async fn on_turn_end(&self, _turn: usize) {}
    async fn on_stop(&self, _turn: usize, _last_assistant_message: Option<&str>) -> StopDecision {
        StopDecision::Allow
    }
}

/// No-op hooks (tests / default).
pub struct NoopHooks;

#[async_trait]
impl AgentHooks for NoopHooks {}
