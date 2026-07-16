use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{OneError, Result};
use crate::message::TextOrImage;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolOutput {
    pub content: Vec<TextOrImage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![TextOrImage::Text {
                text: text.into(),
            }],
            details: None,
        }
    }

    pub fn text_with_details(text: impl Into<String>, details: Value) -> Self {
        Self {
            content: vec![TextOrImage::Text {
                text: text.into(),
            }],
            details: Some(details),
        }
    }

    /// Image tool result from a local path.
    pub fn image_path(mime_type: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            content: vec![TextOrImage::image_path(mime_type, path)],
            details: None,
        }
    }

    /// Image from an existing file path with tool details JSON.
    pub fn image_path_with_details(
        mime_type: impl Into<String>,
        path: impl Into<String>,
        details: Value,
    ) -> Self {
        Self {
            content: vec![TextOrImage::image_path(mime_type, path)],
            details: Some(details),
        }
    }

    /// Decode base64 → media file → path block (data-URI paste / tests).
    ///
    /// Panics if bytes are not a supported image (callers must pass valid raster data).
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        let data = data.into();
        let mime = mime_type.into();
        let block = TextOrImage::image_from_base64(&data, Some(&mime))
            .unwrap_or_else(|e| panic!("ToolOutput::image: {e}"));
        Self {
            content: vec![block],
            details: None,
        }
    }

    pub fn image_with_details(
        data: impl Into<String>,
        mime_type: impl Into<String>,
        details: Value,
    ) -> Self {
        let data = data.into();
        let mime = mime_type.into();
        let block = TextOrImage::image_from_base64(&data, Some(&mime))
            .unwrap_or_else(|e| panic!("ToolOutput::image_with_details: {e}"));
        Self {
            content: vec![block],
            details: Some(details),
        }
    }

    /// Plain text only (images dropped). Used for bash exit parsing etc.
    pub fn as_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                TextOrImage::Text { text } => Some(text.as_str()),
                TextOrImage::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// TUI / logs: text plus `[image · …]` labels for image blocks.
    pub fn as_ui_text(&self) -> String {
        self.content
            .iter()
            .map(TextOrImage::as_display_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn has_images(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, TextOrImage::Image { .. }))
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput>;
}

pub fn tool_error(tool: &str, message: impl Into<String>) -> OneError {
    OneError::Tool {
        tool: tool.to_string(),
        message: message.into(),
    }
}

pub fn invalid_args(tool: &str, message: impl Into<String>) -> OneError {
    OneError::InvalidToolArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}