//! `ask_user` — structured human-in-the-loop clarifying questions.
//!
//! Schema aligns with Claude Code's AskUserQuestion (snake_case tool name).
//! The interactive bridge is injected from `one-cli` (TUI); non-interactive
//! builds fail closed.

use std::sync::Arc;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::{json, Value};

/// Async handler that presents questions to a human and returns answers.
#[async_trait]
pub trait AskUserHandler: Send + Sync {
    async fn ask(&self, questions: Vec<Value>) -> std::result::Result<Value, String>;
}

/// Fail-closed handler for print / json / rpc modes.
pub struct FailClosedAskUser;

#[async_trait]
impl AskUserHandler for FailClosedAskUser {
    async fn ask(&self, _questions: Vec<Value>) -> std::result::Result<Value, String> {
        Err(
            "ask_user requires interactive mode (TUI). \
             Re-run without -p / --mode print, or answer in a normal chat turn."
                .into(),
        )
    }
}

pub struct AskUserTool {
    handler: Arc<dyn AskUserHandler>,
}

impl AskUserTool {
    pub fn new(handler: Arc<dyn AskUserHandler>) -> Self {
        Self { handler }
    }

    pub fn fail_closed() -> Self {
        Self::new(Arc::new(FailClosedAskUser))
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ask_user".to_string(),
            description: "Ask the human clarifying questions with structured single-select \
                or multi-select options when requirements are ambiguous. Prefer this over \
                guessing. Supports 1-4 questions, each with 2-4 options. Users can always \
                type free-text via Other."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "1-4 clarifying questions",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {
                                    "type": "string",
                                    "description": "Full question text to display"
                                },
                                "header": {
                                    "type": "string",
                                    "description": "Short label (max ~12 chars)"
                                },
                                "options": {
                                    "type": "array",
                                    "description": "2-4 choices",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string" },
                                            "description": { "type": "string" }
                                        },
                                        "required": ["label"]
                                    }
                                },
                                "multi_select": {
                                    "type": "boolean",
                                    "description": "If true, user may pick multiple options"
                                }
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let questions = call
            .arguments
            .get("questions")
            .and_then(|v| v.as_array())
            .cloned()
            .ok_or_else(|| invalid_args("ask_user", "questions array required"))?;

        if questions.is_empty() || questions.len() > 4 {
            return Err(invalid_args(
                "ask_user",
                format!("need 1-4 questions, got {}", questions.len()),
            ));
        }

        for (i, q) in questions.iter().enumerate() {
            if q.get("question").and_then(|v| v.as_str()).is_none() {
                return Err(invalid_args(
                    "ask_user",
                    format!("questions[{i}].question required"),
                ));
            }
            let opts = q
                .get("options")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    invalid_args("ask_user", format!("questions[{i}].options required"))
                })?;
            if opts.len() < 2 || opts.len() > 4 {
                return Err(invalid_args(
                    "ask_user",
                    format!("questions[{i}] needs 2-4 options, got {}", opts.len()),
                ));
            }
        }

        match self.handler.ask(questions.clone()).await {
            Ok(answers) => {
                let text = format_answers_text(&questions, &answers);
                Ok(ToolOutput::text_with_details(
                    text,
                    json!({
                        "questions": questions,
                        "answers": answers,
                    }),
                ))
            }
            Err(msg) => Err(tool_error("ask_user", msg)),
        }
    }
}

fn format_answers_text(questions: &[Value], answers: &Value) -> String {
    let mut lines = Vec::new();
    for q in questions {
        let qtext = q
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let ans = answers.get(qtext);
        let rendered = match ans {
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "(no answer)".into(),
        };
        lines.push(format!("Q: {qtext}\nA: {rendered}"));
    }
    if lines.is_empty() {
        "User answered.".into()
    } else {
        lines.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct MockAsk;

    #[async_trait]
    impl AskUserHandler for MockAsk {
        async fn ask(&self, questions: Vec<Value>) -> std::result::Result<Value, String> {
            let mut answers = serde_json::Map::new();
            for q in questions {
                let qtext = q["question"].as_str().unwrap().to_string();
                let multi = q
                    .get("multi_select")
                    .or_else(|| q.get("multiSelect"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if multi {
                    answers.insert(qtext, json!(["A", "B"]));
                } else {
                    answers.insert(qtext, json!("A"));
                }
            }
            Ok(Value::Object(answers))
        }
    }

    #[tokio::test]
    async fn mock_single_and_multi() {
        let tool = AskUserTool::new(Arc::new(MockAsk));
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "ask_user".into(),
                arguments: json!({
                    "questions": [
                        {
                            "question": "Which test runner?",
                            "header": "Runner",
                            "options": [
                                {"label": "A", "description": "Jest"},
                                {"label": "B", "description": "Vitest"}
                            ],
                            "multi_select": false
                        },
                        {
                            "question": "Sections?",
                            "options": [
                                {"label": "A", "description": "Intro"},
                                {"label": "B", "description": "Outro"},
                                {"label": "C", "description": "Appendix"}
                            ],
                            "multi_select": true
                        }
                    ]
                }),
            })
            .await
            .unwrap();
        let text = out.as_text();
        assert!(text.contains("Which test runner?"), "{text}");
        assert!(text.contains("A"), "{text}");
        assert!(text.contains("Sections?"), "{text}");
    }

    #[tokio::test]
    async fn fail_closed() {
        let tool = AskUserTool::fail_closed();
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "ask_user".into(),
                arguments: json!({
                    "questions": [{
                        "question": "Ok?",
                        "options": [
                            {"label": "Yes"},
                            {"label": "No"}
                        ]
                    }]
                }),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("interactive"), "{err}");
    }

    #[tokio::test]
    async fn rejects_bad_counts() {
        let tool = AskUserTool::fail_closed();
        let err = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "ask_user".into(),
                arguments: json!({
                    "questions": [{
                        "question": "Only one option?",
                        "options": [{"label": "Only"}]
                    }]
                }),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("2-4"), "{err}");
    }
}
