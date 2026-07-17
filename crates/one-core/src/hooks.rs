//! Optional async lifecycle hooks for the agent loop.
//!
//! Used by `one-ext` (via a bridge in `one-cli`) so Core stays free of extension
//! types while still firing session / turn boundaries inside the loop.

use async_trait::async_trait;

/// Async hooks invoked from [`crate::agent::Agent::run`].
///
/// All methods have default no-ops so implementors only override what they need.
#[async_trait]
pub trait AgentHooks: Send + Sync {
    async fn on_agent_start(&self) {}
    async fn on_agent_end(&self) {}
    async fn on_turn_start(&self, _turn: usize) {}
    async fn on_turn_end(&self, _turn: usize) {}
}

/// No-op hooks (tests / default).
pub struct NoopHooks;

#[async_trait]
impl AgentHooks for NoopHooks {}
