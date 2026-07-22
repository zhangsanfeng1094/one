//! Prompt-cache helpers (Anthropic `cache_control` + session affinity headers).
//!
//! Anthropic / OpenRouter-Anthropic require explicit `cache_control` breakpoints
//! on content blocks. OpenAI automatic prefix caching needs no client markers.
//!
//! ## Stability rules (cache hits depend on byte-identical prefixes)
//!
//! 1. **Wire shape must not flip between turns.** Converting only the *last*
//!    message from `"text"` → `[{type:text,...}]` for `cache_control`, then
//!    sending it as a bare string once it is no longer last, invalidates the
//!    conversation prefix. When caching is on, **every** string `content` is
//!    normalized to a text-block array first.
//! 2. **`cache_control` only on eligible block types.** Thinking / redacted
//!    thinking blocks must not receive the marker (API rejects or ignores).
//! 3. **Do not rewrite message text** when attaching markers — only add the
//!    `cache_control` field.

use serde_json::{json, Map, Value};

/// Block `type` values that accept Anthropic `cache_control`.
const CACHEABLE_BLOCK_TYPES: &[&str] = &[
    "text",
    "image",
    "tool_use",
    "tool_result",
    "document",
    "image_url", // OpenRouter / OpenAI-shaped parts when proxying Anthropic
];

/// Build an Anthropic-style `cache_control` object.
///
/// When `long_retention` is true, requests a 1h TTL (models that only support
/// the default 5m window will reject — gate via `supports_long_cache_retention`).
pub fn anthropic_cache_control(long_retention: bool) -> Value {
    if long_retention {
        json!({ "type": "ephemeral", "ttl": "1h" })
    } else {
        json!({ "type": "ephemeral" })
    }
}

/// Convert string `content` → `[{ "type": "text", "text": ... }]` so the wire
/// shape is identical whether or not this message holds a cache breakpoint.
///
/// Arrays and other shapes are left unchanged (no content rewrite).
pub fn normalize_content_to_blocks(content: &mut Value) {
    if let Value::String(text) = content {
        let text = std::mem::take(text);
        *content = json!([{ "type": "text", "text": text }]);
    }
}

/// Normalize every message's string `content` to a text-block array.
///
/// Call this on the **full** history before placing any breakpoint so older
/// turns keep the same shape after they stop being "last".
///
/// Skips OpenAI `role: tool` messages — their `content` must remain a plain
/// string for Chat Completions (and most OpenAI-compat proxies).
pub fn stabilize_messages_for_cache(messages: &mut [Value]) {
    for message in messages.iter_mut() {
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "tool" {
            continue;
        }
        if let Some(content) = message.get_mut("content") {
            normalize_content_to_blocks(content);
        }
    }
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(|t| t.as_str())
}

fn is_cacheable_block(block: &Value) -> bool {
    match block_type(block) {
        Some(ty) => CACHEABLE_BLOCK_TYPES.contains(&ty),
        // Defensive: bare objects without type (should not appear after normalize)
        None => block.as_object().is_some(),
    }
}

/// Attach `cache_control` to the last **eligible** content block in `content`.
///
/// Returns `true` if a marker was placed. Does not rewrite text/image payloads.
pub fn attach_cache_control_to_content(content: &mut Value, cache: &Value) -> bool {
    normalize_content_to_blocks(content);
    let Value::Array(blocks) = content else {
        return false;
    };
    for block in blocks.iter_mut().rev() {
        if !is_cacheable_block(block) {
            continue;
        }
        if let Some(obj) = block.as_object_mut() {
            obj.insert("cache_control".into(), cache.clone());
            return true;
        }
    }
    false
}

/// Attach `cache_control` to a message object's `content` field.
pub fn attach_cache_control_to_message(message: &mut Value, cache: &Value) -> bool {
    let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
    // OpenAI tool results are string-only; converting them to blocks breaks the API
    // and is not a valid Anthropic `tool_result` block either (wrong envelope).
    if role == "tool" {
        return false;
    }
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    // Null content (assistant tool_calls-only on some OpenAI paths) cannot host a marker.
    if content.is_null() {
        return false;
    }
    attach_cache_control_to_content(content, cache)
}

/// Walk messages from the end; place one conversation breakpoint on the first
/// message that has an eligible content block (skips empty / thinking-only).
pub fn attach_cache_control_to_messages_suffix(messages: &mut [Value], cache: &Value) -> bool {
    for message in messages.iter_mut().rev() {
        if attach_cache_control_to_message(message, cache) {
            return true;
        }
    }
    false
}

/// System prompt as Anthropic content-block array with a trailing cache breakpoint.
pub fn anthropic_system_with_cache(system_prompt: &str, cache: &Value) -> Value {
    json!([{
        "type": "text",
        "text": system_prompt,
        "cache_control": cache,
    }])
}

/// Mark the last tool definition with `cache_control` (Anthropic multi-breakpoint).
pub fn attach_cache_control_to_last_tool(tools: &mut [Value], cache: &Value) {
    if let Some(last) = tools.last_mut() {
        if let Some(obj) = last.as_object_mut() {
            obj.insert("cache_control".into(), cache.clone());
        }
    }
}

/// Full Anthropic-style cache pass on an already-built messages array + tools.
///
/// 1. Stabilize every message content shape (string → text block).
/// 2. Breakpoint on last tool (if any).
/// 3. One conversation breakpoint on the last eligible message block.
pub fn apply_anthropic_message_cache(
    messages: &mut Vec<Value>,
    tools: &mut [Value],
    cache: &Value,
    cache_tools: bool,
) {
    stabilize_messages_for_cache(messages);
    if cache_tools && !tools.is_empty() {
        attach_cache_control_to_last_tool(tools, cache);
    }
    let _ = attach_cache_control_to_messages_suffix(messages, cache);
}

/// Stable-ish id for a provider instance (session affinity / sticky routing).
pub fn new_session_affinity_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("one-{t:x}-{n}")
}

// ── Debug dump (ONE_DEBUG_CACHE=1) ──────────────────────────────────────────

/// Whether cache debug logging is enabled.
///
/// **Default: on.** Writes under `~/.one/agent/cache-debug/`.
/// Set `ONE_DEBUG_CACHE=0` / `false` / `no` / `off` to disable.
pub fn debug_cache_enabled() -> bool {
    match std::env::var("ONE_DEBUG_CACHE") {
        Ok(v) => {
            let v = v.trim();
            !(v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no")
                || v.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    }
}

/// Directory for cache debug artifacts.
///
/// Override with `ONE_DEBUG_CACHE_DIR`. Default: `~/.one/agent/cache-debug/`.
pub fn debug_cache_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("ONE_DEBUG_CACHE_DIR") {
        let p = p.trim();
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".one/agent/cache-debug")
}

/// Path of the rolling latest snapshot (always overwritten).
pub fn debug_cache_latest_path() -> std::path::PathBuf {
    debug_cache_dir().join("latest.json")
}

/// Path of the append-only JSONL log.
pub fn debug_cache_jsonl_path() -> std::path::PathBuf {
    debug_cache_dir().join("log.jsonl")
}

fn count_cache_control(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            let self_hit = map.contains_key("cache_control") as usize;
            map.values().map(count_cache_control).sum::<usize>() + self_hit
        }
        Value::Array(arr) => arr.iter().map(count_cache_control).sum(),
        _ => 0,
    }
}

fn collect_cache_breakpoints(value: &Value, path: &str, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if let Some(cc) = map.get("cache_control") {
                out.push(json!({
                    "path": path,
                    "type": map.get("type").cloned().unwrap_or(Value::Null),
                    "cache_control": cc,
                    // Small text peek so we can see *what* was marked (not full body).
                    "text_preview": map.get("text").and_then(|t| t.as_str()).map(|s| {
                        let s = s.replace('\n', " ");
                        if s.chars().count() > 80 {
                            format!("{}…", s.chars().take(80).collect::<String>())
                        } else {
                            s
                        }
                    }),
                    "name": map.get("name").cloned(),
                }));
            }
            for (k, v) in map {
                let child = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                collect_cache_breakpoints(v, &child, out);
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                collect_cache_breakpoints(v, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

fn content_shape_summary(content: &Value) -> String {
    match content {
        Value::String(s) => format!("string(len={})", s.len()),
        Value::Array(blocks) => {
            let types: Vec<String> = blocks
                .iter()
                .map(|b| {
                    b.get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("?")
                        .to_string()
                })
                .collect();
            let has_cc = blocks.iter().any(|b| b.get("cache_control").is_some());
            format!(
                "array(n={}, types=[{}], cache_control={})",
                blocks.len(),
                types.join(","),
                has_cc
            )
        }
        Value::Null => "null".into(),
        other => format!("other({})", other),
    }
}

/// Build a redacted analysis of a provider request body for cache debugging.
pub fn analyze_request_body(body: &Value) -> Value {
    let mut breakpoints = Vec::new();
    collect_cache_breakpoints(body, "", &mut breakpoints);

    let mut message_shapes = Vec::new();
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for (i, m) in msgs.iter().enumerate() {
            message_shapes.push(json!({
                "i": i,
                "role": m.get("role"),
                "content": m.get("content").map(content_shape_summary),
            }));
        }
    }

    let system_shape = body.get("system").map(|s| match s {
        Value::String(t) => format!("string(len={})", t.len()),
        Value::Array(a) => format!(
            "array(n={}, cache_control={})",
            a.len(),
            a.iter().any(|b| b.get("cache_control").is_some())
        ),
        Value::Null => "null".into(),
        _ => "other".into(),
    });

    let tools_n = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let last_tool_cached = body
        .get("tools")
        .and_then(|t| t.as_array())
        .and_then(|a| a.last())
        .map(|t| t.get("cache_control").is_some())
        .unwrap_or(false);

    json!({
        "model": body.get("model"),
        "cache_control_count": count_cache_control(body),
        "breakpoints": breakpoints,
        "system_shape": system_shape,
        "tools_count": tools_n,
        "last_tool_has_cache_control": last_tool_cached,
        "messages_count": message_shapes.len(),
        "message_shapes": message_shapes,
    })
}

/// Record one LLM call for cache troubleshooting.
///
/// Writes:
/// - `~/.one/agent/cache-debug/latest.json` — last call only (easy to open)
/// - `~/.one/agent/cache-debug/log.jsonl` — append all calls
///
/// No-op unless [`debug_cache_enabled`]. Never panics.
pub fn record_cache_debug(
    provider: &str,
    phase: &str,
    body: Option<&Value>,
    usage: Option<&one_core::TokenUsage>,
    extra: Option<Value>,
) {
    if !debug_cache_enabled() {
        return;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let analysis = body.map(analyze_request_body);
    let usage_json = usage.map(|u| {
        json!({
            "input_tokens": u.input_tokens,
            "output_tokens": u.output_tokens,
            "cache_read_tokens": u.cache_read_tokens,
            "cache_write_tokens": u.cache_write_tokens,
            "total_io": u.total(),
            "prompt_expanded_anthropic_style": u.prompt_tokens_expanded(),
            "uncached_input_openai_style": u.uncached_input_tokens(),
            "cache_hit": u.cache_read_tokens > 0,
            "cache_write": u.cache_write_tokens > 0,
        })
    });

    // Optional: include a truncated wire body (no API keys — body shouldn't have them).
    let body_preview = body.map(|b| {
        let s = serde_json::to_string(b).unwrap_or_default();
        if s.len() > 32_000 {
            json!({
                "truncated": true,
                "chars": s.len(),
                "head": &s[..32_000],
            })
        } else {
            b.clone()
        }
    });

    let entry = json!({
        "ts_ms": ts,
        "provider": provider,
        "phase": phase,
        "analysis": analysis,
        "usage": usage_json,
        "extra": extra,
        "body": body_preview,
        "paths": {
            "latest": debug_cache_latest_path().display().to_string(),
            "log": debug_cache_jsonl_path().display().to_string(),
        },
        "hint": if usage.map(|u| u.cache_read_tokens > 0).unwrap_or(false) {
            "cache HIT (cache_read_tokens > 0)"
        } else if usage.map(|u| u.cache_write_tokens > 0).unwrap_or(false) {
            "cache WRITE this turn (expect HIT on next turn if prefix stable)"
        } else if analysis
            .as_ref()
            .and_then(|a| a.get("cache_control_count"))
            .and_then(|c| c.as_u64())
            .unwrap_or(0)
            > 0
        {
            "markers sent but no cache tokens in usage — too short, wrong model, or prefix changed"
        } else {
            "no cache_control markers in body (OpenAI auto-cache may still report cached_tokens)"
        },
    });

    let dir = debug_cache_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[one cache-debug] mkdir {}: {e}", dir.display());
        return;
    }

    // latest.json — pretty, easy to open
    let latest = debug_cache_latest_path();
    if let Ok(pretty) = serde_json::to_string_pretty(&entry) {
        if let Err(e) = std::fs::write(&latest, pretty) {
            eprintln!("[one cache-debug] write {}: {e}", latest.display());
        }
    }

    // log.jsonl — one line per event
    let log = debug_cache_jsonl_path();
    if let Ok(line) = serde_json::to_string(&entry) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Strip `cache_control` keys for comparing content equality in tests.
#[cfg(test)]
fn strip_cache_control(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if k == "cache_control" {
                    continue;
                }
                out.insert(k.clone(), strip_cache_control(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(strip_cache_control).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_string_content_without_losing_text() {
        let mut content = json!("hello");
        assert!(attach_cache_control_to_content(
            &mut content,
            &anthropic_cache_control(false)
        ));
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hello");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert!(content[0]["cache_control"].get("ttl").is_none());
    }

    #[test]
    fn long_retention_ttl() {
        let c = anthropic_cache_control(true);
        assert_eq!(c["ttl"], "1h");
    }

    #[test]
    fn marks_last_tool() {
        let mut tools = vec![json!({"name": "a"}), json!({"name": "b"})];
        attach_cache_control_to_last_tool(&mut tools, &anthropic_cache_control(false));
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn skips_thinking_blocks_for_cache_control() {
        let mut content = json!([
            { "type": "thinking", "thinking": "secret", "signature": "sig" },
            { "type": "text", "text": "visible" }
        ]);
        assert!(attach_cache_control_to_content(
            &mut content,
            &anthropic_cache_control(false)
        ));
        assert!(content[0].get("cache_control").is_none());
        assert_eq!(content[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(content[1]["text"], "visible");
    }

    #[test]
    fn history_shape_stable_across_turns() {
        // Turn 1: single user message becomes last (gets cache_control).
        let mut turn1 = vec![json!({
            "role": "user",
            "content": "hi"
        })];
        let cache = anthropic_cache_control(false);
        apply_anthropic_message_cache(&mut turn1, &mut [], &cache, false);

        // Turn 2: same user message is no longer last — must keep block shape
        // so the prefix matches turn1 (aside from cache_control placement).
        let mut turn2 = vec![
            json!({
                "role": "user",
                "content": "hi"
            }),
            json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": "hello" }]
            }),
        ];
        apply_anthropic_message_cache(&mut turn2, &mut [], &cache, false);

        // Content of message[0] without cache_control must match.
        let t1 = strip_cache_control(&turn1[0]["content"]);
        let t2 = strip_cache_control(&turn2[0]["content"]);
        assert_eq!(
            t1, t2,
            "user message wire shape must not flip string↔array between turns"
        );
        assert!(t1.is_array(), "expected stable text-block array, got {t1}");
        assert_eq!(t1[0]["text"], "hi");

        // Breakpoint moved to the new last message.
        assert!(turn2[1]["content"][0].get("cache_control").is_some());
        assert!(turn2[0]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn does_not_rewrite_existing_block_text() {
        let mut content = json!([
            { "type": "text", "text": "keep me" },
            { "type": "tool_use", "id": "1", "name": "read", "input": { "path": "a" } }
        ]);
        attach_cache_control_to_content(&mut content, &anthropic_cache_control(false));
        assert_eq!(content[0]["text"], "keep me");
        assert_eq!(content[1]["input"]["path"], "a");
        assert!(content[0].get("cache_control").is_none());
        assert!(content[1].get("cache_control").is_some());
    }
}
