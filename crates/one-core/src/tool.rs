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

/// Map common model-hallucinated / cross-agent tool names to One builtins.
///
/// Keeps the agent loop resilient when models trained on Claude Code, Cursor,
/// Codex, or OpenCode emit alternate names (`read_file`, `search_replace`, …).
pub fn resolve_tool_name(name: &str) -> &str {
    match name {
        // read
        "read_file" | "Read" | "ReadFile" | "readFile" | "view" | "View" | "open_file" => "read",
        // write
        "write_file" | "Write" | "WriteFile" | "writeFile" | "create_file" => "write",
        // edit
        "search_replace"
        | "str_replace"
        | "StrReplace"
        | "Edit"
        | "ApplyPatch"
        | "replace_in_file"
        | "multi_edit" => "edit",
        // bash
        "shell" | "Bash" | "Shell" | "run_terminal_cmd" | "run_command" | "execute" | "terminal" => {
            "bash"
        }
        // find / glob
        "Glob" | "glob" | "glob_file_search" | "find_files" | "list_files_glob" => "find",
        // grep
        "Grep" | "rg" | "search_codebase" | "codebase_search" => "grep",
        // ls
        "LS" | "list_dir" | "list_files" | "list_directory" => "ls",
        // web
        "WebFetch" | "webfetch" | "fetch_url" | "fetch" => "web_fetch",
        "WebSearch" | "websearch" | "search_web" => "web_search",
        // ask user
        "AskUserQuestion" | "ask" | "question" | "ask_user_question" => "ask_user",
        other => other,
    }
}

#[cfg(test)]
mod tool_name_tests {
    use super::resolve_tool_name;

    #[test]
    fn aliases_common_hallucinations() {
        assert_eq!(resolve_tool_name("read_file"), "read");
        assert_eq!(resolve_tool_name("search_replace"), "edit");
        assert_eq!(resolve_tool_name("str_replace"), "edit");
        assert_eq!(resolve_tool_name("shell"), "bash");
        assert_eq!(resolve_tool_name("Glob"), "find");
        assert_eq!(resolve_tool_name("read"), "read");
        assert_eq!(resolve_tool_name("mcp__x"), "mcp__x");
    }
}