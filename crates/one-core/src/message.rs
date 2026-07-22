use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
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
    Text {
        text: String,
    },
    /// Local image file. Session stores path only; providers read → base64 at request time.
    Image {
        mime_type: String,
        /// Absolute path (`~/.one/agent/media/…` or workspace file).
        path: String,
    },
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

    /// Build a user message from text + local image files `(mime, path)`.
    ///
    /// Paths are stored as-is (no re-copy). Prefer media-store paths from paste.
    pub fn user_with_images(
        text: impl Into<String>,
        images: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let text = text.into();
        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(TextOrImage::Text { text });
        }
        for (mime_type, path) in images {
            blocks.push(TextOrImage::image_path(mime_type, path));
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

    /// True when this content carries at least one image block.
    pub fn has_images(&self) -> bool {
        match self {
            UserContent::Text(_) => false,
            UserContent::Blocks(blocks) => blocks
                .iter()
                .any(|b| matches!(b, TextOrImage::Image { .. })),
        }
    }

    /// Image attachments as `(mime_type, path)` in order.
    pub fn image_paths(&self) -> Vec<(String, String)> {
        match self {
            UserContent::Text(_) => Vec::new(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    TextOrImage::Image { mime_type, path } => {
                        Some((mime_type.clone(), path.clone()))
                    }
                    TextOrImage::Text { .. } => None,
                })
                .collect(),
        }
    }

    /// Resolve all images to `(mime, base64)` for providers / tests.
    pub fn images_base64(&self) -> Vec<(String, String)> {
        match self {
            UserContent::Text(_) => Vec::new(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| b.resolved_base64().ok())
                .collect(),
        }
    }

    /// Rebuild an editable prompt: text + `[图片.img]` chips + `(mime, path)` list.
    pub fn for_reedit(&self) -> (String, Vec<(String, String)>) {
        match self {
            UserContent::Text(t) => (t.clone(), Vec::new()),
            UserContent::Blocks(blocks) => {
                let mut parts: Vec<String> = Vec::new();
                let mut images: Vec<(String, String)> = Vec::new();
                let mut img_id = 1u32;
                for b in blocks {
                    match b {
                        TextOrImage::Text { text } => {
                            if !text.is_empty() {
                                parts.push(text.clone());
                            }
                        }
                        TextOrImage::Image { mime_type, path } => {
                            parts.push(crate::image::image_token(img_id));
                            images.push((mime_type.clone(), path.clone()));
                            img_id = img_id.saturating_add(1).max(2);
                        }
                    }
                }
                let text = parts.join(" ");
                (text, images)
            }
        }
    }

    /// True when this is plain text that looks like a lost multimodal turn
    /// (`as_display_text` of a prior image message was re-submitted).
    pub fn looks_like_image_placeholder_text(&self) -> bool {
        match self {
            UserContent::Text(t) => t.contains("[image ·"),
            UserContent::Blocks(_) => false,
        }
    }
}

impl TextOrImage {
    pub fn image_path(mime_type: impl Into<String>, path: impl Into<String>) -> Self {
        TextOrImage::Image {
            mime_type: mime_type.into(),
            path: path.into(),
        }
    }

    /// Store raw bytes into the media dir and return a path block.
    pub fn image_from_bytes(bytes: &[u8], mime_hint: Option<&str>) -> Result<Self, String> {
        let (path, mime) = crate::image::store_image_bytes(bytes, mime_hint)?;
        Ok(Self::image_path(mime, path.display().to_string()))
    }

    /// Decode base64 → media file → path block (for data-URI paste only).
    pub fn image_from_base64(data_b64: &str, mime_hint: Option<&str>) -> Result<Self, String> {
        let (path, mime) = crate::image::store_image_base64(data_b64, mime_hint)?;
        Ok(Self::image_path(mime, path.display().to_string()))
    }

    /// Read file and return `(mime_type, base64)` for API providers.
    pub fn resolved_base64(&self) -> Result<(String, String), String> {
        match self {
            TextOrImage::Text { .. } => Err("not an image".into()),
            TextOrImage::Image { mime_type, path } => {
                let p = std::path::Path::new(path);
                if !p.is_file() {
                    return Err(format!("image file missing: {path}"));
                }
                let (mime, b64) = crate::image::load_image_file(p)?;
                let mime = if mime.is_empty() {
                    mime_type.clone()
                } else {
                    mime
                };
                Ok((mime, b64))
            }
        }
    }

    pub fn as_display_text(&self) -> String {
        match self {
            TextOrImage::Text { text } => text.clone(),
            TextOrImage::Image { mime_type, path } => {
                crate::image::image_label_path(mime_type, std::path::Path::new(path))
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
