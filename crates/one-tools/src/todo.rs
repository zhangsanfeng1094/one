//! Session-scoped todo list (Grok `todo_write` / Claude TodoWrite style).
//!
//! Keeps multi-step work visible in the conversation without flooding chat.
//! State lives in process memory for the agent session (not disk).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pending" | "todo" | "open" => Some(Self::Pending),
            "in_progress" | "in-progress" | "doing" | "active" => Some(Self::InProgress),
            "completed" | "done" | "complete" => Some(Self::Completed),
            "cancelled" | "canceled" | "dropped" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

/// Shared list for one agent session (wire the same Arc into the tool).
#[derive(Clone, Default)]
pub struct TodoListState {
    inner: Arc<Mutex<Vec<TodoItem>>>,
}

impl TodoListState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.inner.lock().expect("todo lock").clone()
    }

    /// Merge by id when `merge` is true; otherwise replace the whole list.
    pub fn apply(&self, items: Vec<TodoItem>, merge: bool) -> Vec<TodoItem> {
        let mut guard = self.inner.lock().expect("todo lock");
        if !merge {
            *guard = items;
        } else {
            for item in items {
                if let Some(existing) = guard.iter_mut().find(|t| t.id == item.id) {
                    if !item.content.is_empty() {
                        existing.content = item.content;
                    }
                    existing.status = item.status;
                } else {
                    guard.push(item);
                }
            }
        }
        guard.clone()
    }
}

pub struct TodoWriteTool {
    state: TodoListState,
}

impl TodoWriteTool {
    pub fn new(state: TodoListState) -> Self {
        Self { state }
    }

    pub fn with_shared(state: TodoListState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "todo_write".into(),
            description: "\
Create and manage a structured task list for multi-step work. The user sees this list live. \
Use for any task with 3+ steps; skip for trivial single-step work. \
Pass `todos` with id/content/status. When `merge` is true (default), update by id; \
when false, replace the whole list. Status: pending | in_progress | completed | cancelled."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["todos"],
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "Todo items to write (merge by id or replace)",
                        "items": {
                            "type": "object",
                            "required": ["id"],
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Stable id for merge updates"
                                },
                                "content": {
                                    "type": "string",
                                    "description": "Task description (required for new items)"
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed", "cancelled"],
                                    "description": "Item status (default pending for new items)"
                                }
                            }
                        }
                    },
                    "merge": {
                        "type": "boolean",
                        "description": "Merge by id (default true). False replaces the full list."
                    }
                }
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let merge = call
            .arguments
            .get("merge")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let raw = call
            .arguments
            .get("todos")
            .and_then(|v| v.as_array())
            .ok_or_else(|| invalid_args("todo_write", "missing `todos` array"))?;

        let mut items = Vec::with_capacity(raw.len());
        for (i, v) in raw.iter().enumerate() {
            let id = v
                .get("id")
                .and_then(|x| x.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid_args("todo_write", format!("todos[{i}]: missing id")))?
                .to_string();
            let content = v
                .get("content")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let status = v
                .get("status")
                .and_then(|x| x.as_str())
                .and_then(TodoStatus::parse)
                .unwrap_or(TodoStatus::Pending);
            if !merge && content.is_empty() {
                return Err(invalid_args(
                    "todo_write",
                    format!("todos[{i}]: content required when merge=false"),
                ));
            }
            if merge {
                // New id with empty content is not useful.
                let exists = self.state.snapshot().iter().any(|t| t.id == id);
                if !exists && content.is_empty() {
                    return Err(invalid_args(
                        "todo_write",
                        format!("todos[{i}]: content required for new id `{id}`"),
                    ));
                }
            }
            items.push(TodoItem {
                id,
                content,
                status,
            });
        }

        let list = self.state.apply(items, merge);
        let text = format_todo_list(&list);
        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "ok": true,
                "merge": merge,
                "count": list.len(),
                "todos": list.iter().map(|t| json!({
                    "id": t.id,
                    "content": t.content,
                    "status": t.status.as_str(),
                })).collect::<Vec<_>>(),
            }),
        ))
    }
}

fn format_todo_list(list: &[TodoItem]) -> String {
    if list.is_empty() {
        return "Todo list is empty.".into();
    }
    let mut out = String::from("Todo list:\n");
    for t in list {
        let mark = match t.status {
            TodoStatus::Completed => "[x]",
            TodoStatus::Cancelled => "[-]",
            TodoStatus::InProgress => "[>]",
            TodoStatus::Pending => "[ ]",
        };
        out.push_str(&format!(
            "- {mark} {} ({}) — {}\n",
            t.id,
            t.status.as_str(),
            t.content
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;
    use serde_json::json;

    #[tokio::test]
    async fn merge_and_replace() {
        let state = TodoListState::new();
        let tool = TodoWriteTool::new(state.clone());
        let call = ToolCall {
            id: "1".into(),
            name: "todo_write".into(),
            arguments: json!({
                "todos": [
                    {"id": "a", "content": "first", "status": "pending"},
                    {"id": "b", "content": "second", "status": "in_progress"},
                ]
            }),
        };
        let out = tool.execute(&call).await.unwrap();
        assert!(out.content.iter().any(|c| match c {
            one_core::message::TextOrImage::Text { text } => text.contains("first"),
            _ => false,
        }));

        let call2 = ToolCall {
            id: "2".into(),
            name: "todo_write".into(),
            arguments: json!({
                "merge": true,
                "todos": [
                    {"id": "a", "status": "completed"},
                ]
            }),
        };
        tool.execute(&call2).await.unwrap();
        let snap = state.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].status, TodoStatus::Completed);
        assert_eq!(snap[0].content, "first");
    }
}
