pub mod discord;
pub mod feishu;
pub mod process;
pub mod telegram;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::events::{
    BotInboundEvent, BotOutboundMessage, MessageHandle, MessageTarget, PlatformCapabilities,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Unified Platform Adapter Trait for IM Channels (Hermes-aligned).
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    /// Identifier of the platform ("telegram", "discord", "feishu", "slack", "wecom", etc.).
    fn platform_id(&self) -> &str;

    /// Declared capabilities (max message length, streaming edit support, HITL buttons, etc.).
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities::default()
    }

    /// Start polling or listening for inbound events, feeding into `event_tx`.
    async fn start_listening(&self, event_tx: mpsc::Sender<BotInboundEvent>) -> Result<()>;

    /// Send a new message to the target channel / user.
    async fn send_message(
        &self,
        target: &MessageTarget,
        message: &BotOutboundMessage,
    ) -> Result<MessageHandle>;

    /// Edit an existing message (used for streaming throttled updates).
    async fn edit_message(
        &self,
        handle: &MessageHandle,
        message: &BotOutboundMessage,
    ) -> Result<()>;

    /// Send a typing indicator ("Bot is typing...").
    async fn send_typing(&self, target: &MessageTarget) -> Result<()>;
}
