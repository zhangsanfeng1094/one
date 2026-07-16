//! Shared multimodal content mapping for providers.
//!
//! Image blocks store a local **path** only. API payloads need base64 / data-URLs —
//! resolve at the edge via [`TextOrImage::resolved_base64`].

use one_core::message::{TextOrImage, UserContent, UserMessage};
use serde_json::{json, Value};

/// Anthropic user content: string or array of text/image blocks.
pub fn anthropic_user_content(user: &UserMessage) -> Value {
    match &user.content {
        UserContent::Text(text) => json!(text),
        UserContent::Blocks(blocks) => {
            if blocks.iter().all(|b| matches!(b, TextOrImage::Text { .. })) {
                json!(user.content.as_plain_text())
            } else {
                json!(blocks
                    .iter()
                    .filter_map(anthropic_content_block)
                    .collect::<Vec<_>>())
            }
        }
    }
}

/// Anthropic tool_result `content`: string or array of text/image blocks.
pub fn anthropic_tool_result_content(blocks: &[TextOrImage]) -> Value {
    let has_image = blocks.iter().any(|b| matches!(b, TextOrImage::Image { .. }));
    if !has_image {
        let text = blocks
            .iter()
            .filter_map(|b| match b {
                TextOrImage::Text { text } => Some(text.as_str()),
                TextOrImage::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return json!(text);
    }
    json!(blocks
        .iter()
        .filter_map(anthropic_content_block)
        .collect::<Vec<_>>())
}

fn anthropic_content_block(block: &TextOrImage) -> Option<Value> {
    match block {
        TextOrImage::Text { text } => {
            if text.is_empty() {
                None
            } else {
                Some(json!({ "type": "text", "text": text }))
            }
        }
        TextOrImage::Image { .. } => {
            let (mime_type, data) = block.resolved_base64().ok()?;
            Some(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type,
                    "data": data,
                }
            }))
        }
    }
}

/// OpenAI Chat Completions user `content` (string or multimodal array).
pub fn openai_chat_user_content(user: &UserMessage) -> Value {
    match &user.content {
        UserContent::Text(text) => json!(text),
        UserContent::Blocks(blocks) => {
            if !blocks.iter().any(|b| matches!(b, TextOrImage::Image { .. })) {
                return json!(user.content.as_plain_text());
            }
            let parts: Vec<Value> = blocks.iter().filter_map(openai_chat_part).collect();
            if parts.is_empty() {
                json!("")
            } else {
                json!(parts)
            }
        }
    }
}

fn openai_chat_part(block: &TextOrImage) -> Option<Value> {
    match block {
        TextOrImage::Text { text } => {
            if text.is_empty() {
                None
            } else {
                Some(json!({ "type": "text", "text": text }))
            }
        }
        TextOrImage::Image { .. } => {
            let (mime_type, data) = block.resolved_base64().ok()?;
            Some(json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{mime_type};base64,{data}")
                }
            }))
        }
    }
}

/// OpenAI Responses API user content parts.
pub fn openai_responses_user_content(user: &UserMessage) -> Value {
    match &user.content {
        UserContent::Text(text) => json!([{
            "type": "input_text",
            "text": text,
        }]),
        UserContent::Blocks(blocks) => {
            let parts: Vec<Value> = blocks
                .iter()
                .filter_map(|b| match b {
                    TextOrImage::Text { text } if !text.is_empty() => Some(json!({
                        "type": "input_text",
                        "text": text,
                    })),
                    TextOrImage::Image { .. } => {
                        let (mime_type, data) = b.resolved_base64().ok()?;
                        Some(json!({
                            "type": "input_image",
                            "image_url": format!("data:{mime_type};base64,{data}"),
                        }))
                    }
                    _ => None,
                })
                .collect();
            if parts.is_empty() {
                json!([{ "type": "input_text", "text": "" }])
            } else {
                json!(parts)
            }
        }
    }
}

/// Flatten tool result to plain text (image → label) for APIs that only accept strings.
pub fn tool_result_plain(blocks: &[TextOrImage]) -> String {
    blocks
        .iter()
        .map(TextOrImage::as_display_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collect image blocks as owned `(mime, base64)` (resolves paths).
pub fn collect_images(blocks: &[TextOrImage]) -> Vec<(String, String)> {
    blocks
        .iter()
        .filter_map(|b| b.resolved_base64().ok())
        .collect()
}

/// Ollama: text content + optional `images` base64 array on the message object.
pub fn ollama_user_message(user: &UserMessage) -> Value {
    match &user.content {
        UserContent::Text(text) => json!({ "role": "user", "content": text }),
        UserContent::Blocks(blocks) => {
            let text = user.content.as_plain_text();
            let images: Vec<String> = blocks
                .iter()
                .filter_map(|b| b.resolved_base64().ok().map(|(_, d)| d))
                .collect();
            if images.is_empty() {
                json!({ "role": "user", "content": text })
            } else {
                let mut content = text;
                if content.is_empty() {
                    content = blocks
                        .iter()
                        .map(TextOrImage::as_display_text)
                        .collect::<Vec<_>>()
                        .join("\n");
                }
                json!({
                    "role": "user",
                    "content": content,
                    "images": images,
                })
            }
        }
    }
}
