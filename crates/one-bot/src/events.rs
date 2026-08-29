use serde::{Deserialize, Serialize};

/// Target destination in an IM platform.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageTarget {
    pub platform: String,
    pub channel_id: String,
    pub thread_id: Option<String>,
    pub user_id: Option<String>,
}

impl MessageTarget {
    pub fn new(
        platform: impl Into<String>,
        channel_id: impl Into<String>,
        thread_id: Option<String>,
        user_id: Option<String>,
    ) -> Self {
        Self {
            platform: platform.into(),
            channel_id: channel_id.into(),
            thread_id,
            user_id,
        }
    }
}

/// Unique handle identifying a message sent on an IM platform.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageHandle {
    pub target: MessageTarget,
    pub message_id: String,
}

/// Unique session key for multi-tenant isolation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BotSessionKey {
    pub platform: String,
    pub channel_id: String,
    pub thread_id: Option<String>,
    pub user_id: String,
}

impl BotSessionKey {
    pub fn new(
        platform: impl Into<String>,
        channel_id: impl Into<String>,
        thread_id: Option<String>,
        user_id: impl Into<String>,
    ) -> Self {
        Self {
            platform: platform.into(),
            channel_id: channel_id.into(),
            thread_id,
            user_id: user_id.into(),
        }
    }

    pub fn to_string_key(&self) -> String {
        match &self.thread_id {
            Some(th) => format!(
                "{}:{}:{}:{}",
                self.platform, self.channel_id, th, self.user_id
            ),
            None => format!("{}:{}:{}", self.platform, self.channel_id, self.user_id),
        }
    }
}

/// Inbound event from an IM platform.
#[derive(Debug, Clone)]
pub enum BotInboundEvent {
    /// A normal chat message from user.
    Message(BotInboundMessage),
    /// An approval / rejection decision from an interactive button.
    ApprovalDecision(ApprovalDecision),
}

/// Attached media or document uploaded by the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotAttachment {
    pub mime_type: String,
    pub file_name: Option<String>,
    pub local_path: String,
    pub is_image: bool,
}

/// Inbound user message payload.
#[derive(Debug, Clone)]
pub struct BotInboundMessage {
    pub session_key: BotSessionKey,
    pub target: MessageTarget,
    pub user_name: Option<String>,
    pub text: String,
    pub reply_to_message_id: Option<String>,
    pub attachments: Vec<BotAttachment>,
}

/// Outbound message payload to be delivered to an IM platform.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BotOutboundMessage {
    pub text: String,
    pub thinking: Option<String>,
    pub tool_status: Option<String>,
    pub buttons: Vec<Vec<InlineButton>>,
    pub is_final: bool,
}

/// Interactive button for HITL approval or quick actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineButton {
    pub text: String,
    pub callback_data: String,
}

/// Capabilities declared by an IM platform or connector (Hermes-aligned).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    pub max_message_length: usize,
    pub supports_streaming_edit: bool,
    pub supports_reactions: bool,
    pub supports_threads: bool,
    pub supports_hitl_buttons: bool,
    pub supports_images: bool,
    pub supports_documents: bool,
}

impl Default for PlatformCapabilities {
    fn default() -> Self {
        Self {
            max_message_length: 4000,
            supports_streaming_edit: true,
            supports_reactions: false,
            supports_threads: false,
            supports_hitl_buttons: true,
            supports_images: true,
            supports_documents: true,
        }
    }
}

/// Request for Human-in-the-Loop permission gate approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub session_key: BotSessionKey,
    pub target: MessageTarget,
    pub tool_name: String,
    pub tool_args: serde_json::Value,
    pub explanation: Option<String>,
}

/// User's decision on an approval request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalDecision {
    pub request_id: String,
    pub user_id: String,
    pub approved: bool,
    pub always_allow: bool,
}
