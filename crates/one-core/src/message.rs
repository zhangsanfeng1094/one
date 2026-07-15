use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ContentBlock {
    Text { text: String },
    /// Extended reasoning / chain-of-thought (provider-agnostic).
    ///
    /// `signature` is an opaque multi-turn handoff blob (Anthropic thinking
    /// signature, OpenAI reasoning item id, redacted payload, …). Providers
    /// that require continuity must replay it on subsequent turns.
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// Safety-redacted thinking: body is a placeholder; `signature` holds
        /// the opaque encrypted payload for API replay.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn thinking(thinking: impl Into<String>) -> Self {
        Self::Thinking {
            thinking: thinking.into(),
            signature: None,
            redacted: false,
        }
    }

    pub fn thinking_with_signature(
        thinking: impl Into<String>,
        signature: impl Into<String>,
    ) -> Self {
        Self::Thinking {
            thinking: thinking.into(),
            signature: Some(signature.into()),
            redacted: false,
        }
    }

    pub fn redacted_thinking(signature: impl Into<String>) -> Self {
        Self::Thinking {
            thinking: "[Reasoning redacted]".into(),
            signature: Some(signature.into()),
            redacted: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum AgentMessage {
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: UserContent,
    #[serde(default = "default_timestamp")]
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<TextOrImage>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TextOrImage {
    Text { text: String },
    Image { data: String, mime_type: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub provider: String,
    pub model: String,
    pub stop_reason: StopReason,
    #[serde(default = "default_timestamp")]
    pub timestamp: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<TextOrImage>,
    pub is_error: bool,
    #[serde(default = "default_timestamp")]
    pub timestamp: u64,
}

fn default_timestamp() -> u64 {
    0
}

impl AgentMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        AgentMessage::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: now_ms(),
        })
    }

    /// User turn with mixed text/image blocks (vision / paste / `@image`).
    pub fn user_blocks(blocks: Vec<TextOrImage>) -> Self {
        AgentMessage::User(UserMessage {
            content: UserContent::Blocks(blocks),
            timestamp: now_ms(),
        })
    }

    /// Build a user message from optional text + image attachments `(mime, base64)`.
    pub fn user_with_images(
        text: impl Into<String>,
        images: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let text = text.into();
        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(TextOrImage::Text { text });
        }
        for (mime_type, data) in images {
            blocks.push(TextOrImage::Image { data, mime_type });
        }
        if blocks.is_empty() {
            return Self::user_text(String::new());
        }
        if blocks.len() == 1 {
            if let TextOrImage::Text { text } = &blocks[0] {
                return Self::user_text(text.clone());
            }
        }
        Self::user_blocks(blocks)
    }

    pub fn assistant_text(provider: &str, model: &str, text: impl Into<String>) -> Self {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            provider: provider.to_string(),
            model: model.to_string(),
            stop_reason: StopReason::Stop,
            timestamp: now_ms(),
        })
    }
}

impl UserContent {
    /// Flatten to display text for TUI (images become `[image · …]` labels).
    pub fn as_display_text(&self) -> String {
        match self {
            UserContent::Text(t) => t.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .map(TextOrImage::as_display_text)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// Plain text only (drops images).
    pub fn as_plain_text(&self) -> String {
        match self {
            UserContent::Text(t) => t.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    TextOrImage::Text { text } => Some(text.as_str()),
                    TextOrImage::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

impl TextOrImage {
    pub fn as_display_text(&self) -> String {
        match self {
            TextOrImage::Text { text } => text.clone(),
            TextOrImage::Image { data, mime_type } => {
                crate::image::image_label(mime_type, data)
            }
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}