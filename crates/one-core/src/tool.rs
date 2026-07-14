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