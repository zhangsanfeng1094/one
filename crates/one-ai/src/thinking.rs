//! Provider-agnostic thinking / reasoning request helpers.
//!
//! Core carries a single [`ThinkingLevel`](one_core::agent::ThinkingLevel). Each
//! wire format maps that level differently; this module centralizes those maps
//! so providers stay thin and new OpenAI-compatible endpoints can opt in.

use one_core::agent::ThinkingLevel;
use one_core::message::ContentBlock;
use serde_json::{json, Value};

/// How to encode thinking level on an OpenAI-compatible chat/completions body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingWire {
    /// Do not send thinking/reasoning request fields.
    Off,
    /// Official OpenAI: top-level `reasoning_effort`.
    #[default]
    ReasoningEffort,
    /// OpenRouter: `reasoning: { effort }` (+ optional `include_reasoning`).
    OpenRouter,
    /// Best-effort: send both OpenAI and OpenRouter shapes (unknown proxies).
    Auto,
}

/// Apply thinking/reasoning fields to a chat/completions-style JSON body.
pub fn apply_chat_thinking(body: &mut Value, level: ThinkingLevel, wire: ThinkingWire) {
    let Some(effort) = level.effort() else {
        return;
    };
    let obj = match body.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    match wire {
        ThinkingWire::Off => {}
        ThinkingWire::ReasoningEffort => {
            obj.insert("reasoning_effort".into(), json!(effort));
        }
        ThinkingWire::OpenRouter => {
            obj.insert("reasoning".into(), json!({ "effort": effort }));
            // Ask OpenRouter to surface reasoning when the model supports it.
            obj.insert("include_reasoning".into(), json!(true));
        }
        ThinkingWire::Auto => {
            obj.insert("reasoning_effort".into(), json!(effort));
            obj.insert("reasoning".into(), json!({ "effort": effort }));
        }
    }
}

/// Apply thinking fields to an OpenAI Responses API body.
pub fn apply_responses_thinking(body: &mut Value, level: ThinkingLevel) {
    let Some(effort) = level.effort() else {
        return;
    };
    if let Some(obj) = body.as_object_mut() {
        // Responses API: reasoning.effort (o-series / GPT-5 family).
        obj.insert(
            "reasoning".into(),
            json!({ "effort": effort, "summary": "auto" }),
        );
    }
}

/// Anthropic Messages: enable budget-based extended thinking.
///
/// Returns the thinking budget when enabled so callers can raise `max_tokens`.
pub fn apply_anthropic_thinking(body: &mut Value, level: ThinkingLevel) -> Option<u32> {
    let budget = level.budget_tokens()?;
    let obj = body.as_object_mut()?;
    obj.insert(
        "thinking".into(),
        json!({ "type": "enabled", "budget_tokens": budget }),
    );
    // max_tokens must exceed thinking budget; keep room for tool/text output.
    let min_output = 4_096u32;
    let max_tokens = budget.saturating_add(min_output);
    obj.insert("max_tokens".into(), json!(max_tokens));
    Some(budget)
}

/// Ollama: enable model-native think channel when supported.
pub fn apply_ollama_thinking(body: &mut Value, level: ThinkingLevel) {
    if !level.is_enabled() {
        return;
    }
    if let Some(obj) = body.as_object_mut() {
        obj.insert("think".into(), json!(true));
    }
}

/// Collect plain thinking text from assistant content blocks.
pub fn thinking_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Thinking {
                thinking, redacted, ..
            } if !redacted => Some(thinking.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// First thinking signature (for providers that need a single id).
pub fn first_thinking_signature(content: &[ContentBlock]) -> Option<&str> {
    content.iter().find_map(|b| match b {
        ContentBlock::Thinking {
            signature: Some(s), ..
        } if !s.is_empty() => Some(s.as_str()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::agent::ThinkingLevel;

    #[test]
    fn chat_openai_wire() {
        let mut body = json!({ "model": "x" });
        apply_chat_thinking(
            &mut body,
            ThinkingLevel::High,
            ThinkingWire::ReasoningEffort,
        );
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn chat_openrouter_wire() {
        let mut body = json!({ "model": "x" });
        apply_chat_thinking(&mut body, ThinkingLevel::Medium, ThinkingWire::OpenRouter);
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["include_reasoning"], true);
    }

    #[test]
    fn off_is_noop() {
        let mut body = json!({ "model": "x" });
        apply_chat_thinking(&mut body, ThinkingLevel::Off, ThinkingWire::Auto);
        assert!(body.get("reasoning_effort").is_none());
        apply_anthropic_thinking(&mut body, ThinkingLevel::Off);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn anthropic_budget() {
        let mut body = json!({ "max_tokens": 100 });
        let b = apply_anthropic_thinking(&mut body, ThinkingLevel::Low).unwrap();
        assert_eq!(b, 2_048);
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
        assert!(body["max_tokens"].as_u64().unwrap() > 2048);
    }
}
