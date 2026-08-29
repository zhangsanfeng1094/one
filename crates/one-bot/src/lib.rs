pub mod adapters;
pub mod chunker;
pub mod config;
pub mod events;
pub mod gateway;
pub mod hitl;
pub mod notify;
pub mod plugin;
pub mod session_map;
pub mod throttler;

pub use adapters::{
    discord::DiscordAdapter, feishu::FeishuAdapter, process::ProcessConnectorAdapter,
    telegram::TelegramAdapter, PlatformAdapter,
};
pub use chunker::chunk_markdown;
pub use config::{
    BotConfig, ConnectorConfig, DiscordConfig, FeishuConfig, SecurityConfig, SlackConfig,
    TelegramConfig,
};
pub use events::{
    ApprovalDecision, ApprovalRequest, BotAttachment, BotInboundEvent, BotInboundMessage,
    BotOutboundMessage, BotSessionKey, InlineButton, MessageHandle, MessageTarget,
    PlatformCapabilities,
};
pub use gateway::{BotAgentExecutor, BotGateway, BotStreamSink, ThrottlerSink};
pub use hitl::{BotApprovalManager, BotToolGate};
pub use notify::NotificationEngine;
pub use plugin::ConnectorPluginManager;
pub use session_map::SessionMapper;
pub use throttler::StreamThrottler;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_key_format() {
        let key = BotSessionKey::new("telegram", "-100123456", Some("99".to_string()), "user1");
        assert_eq!(key.to_string_key(), "telegram:-100123456:99:user1");

        let key_no_thread = BotSessionKey::new("discord", "chan1", None, "user2");
        assert_eq!(key_no_thread.to_string_key(), "discord:chan1:user2");
    }

    #[test]
    fn test_security_config_acl() {
        let mut sec = SecurityConfig::default();
        sec.admin_users = vec!["admin1".to_string(), "admin2".to_string()];
        sec.allowed_channels = vec!["chan_a".to_string()];

        assert!(sec.is_user_admin("admin1"));
        assert!(!sec.is_user_admin("stranger"));
        assert!(sec.is_channel_allowed("chan_a"));
        assert!(!sec.is_channel_allowed("chan_b"));
    }

    #[test]
    fn test_telegram_formatter() {
        let adapter = TelegramAdapter::new(TelegramConfig::default());
        let msg = BotOutboundMessage {
            text: "Hello world".to_string(),
            thinking: Some("Let me analyze".to_string()),
            tool_status: Some("Reading file".to_string()),
            buttons: Vec::new(),
            is_final: true,
        };

        let formatted = adapter.format_text(&msg);
        assert!(formatted.contains("🔧 Reading file"));
        assert!(formatted.contains("💭 思考中:"));
        assert!(formatted.contains("Hello world"));
    }
}
