//! JSONL RPC over stdin/stdout.
//!
//! Each request line:
//! ```json
//! {"id":"1","method":"prompt","params":{"text":"hello"}}
//! ```
//!
//! Response line:
//! ```json
//! {"id":"1","ok":true,"result":{…}}
//! ```
//!
//! Methods: `ping`, `prompt`, `abort`, `steer`, `follow_up`, `session`, `status`,
//! `thinking`, `compact`, `spawn`.

use std::io::{self, BufRead};

use one_core::agent::{LlmProvider, ThinkingLevel};
use serde::Deserialize;
use serde_json::json;

use crate::runtime::AppRuntime;
use one_core::tool::{Tool, ToolCall};

#[derive(Debug, Deserialize)]
struct RpcRequest {
    id: Option<String>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

async fn spawn_rpc(
    runtime: &AppRuntime,
    params: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let prompt = params
        .get("prompt")
        .or_else(|| params.get("text"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "params.prompt required".to_string())?;
    let host = runtime
        .task_host()
        .ok_or_else(|| "subagent spawn is not enabled on this runtime".to_string())?;
    let tool = crate::runtime::task_tool::TaskTool::new(host);
    let mut args = params.clone();
    if let Some(obj) = args.as_object_mut() {
        obj.insert("prompt".into(), serde_json::json!(prompt));
    }
    let out = tool
        .execute(&ToolCall {
            id: format!("rpc_spawn_{}", uuid_lite()),
            name: "task".into(),
            arguments: args,
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "text": out.as_text(),
        "details": out.details,
    }))
}

fn uuid_lite() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| format!("{:x}", d.as_nanos()))
        .unwrap_or_else(|_| "0".into())
}

pub async fn run_rpc(
    runtime: &mut AppRuntime,
    provider: &dyn LlmProvider,
) -> Result<(), Box<dyn std::error::Error>> {
    runtime.subscribe_printer(true).await;
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: RpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "id": null,
                        "ok": false,
                        "error": format!("invalid request json: {e}"),
                    }))?
                );
                continue;
            }
        };
        let response = handle(runtime, provider, &request).await;
        println!("{}", serde_json::to_string(&response)?);
    }
    Ok(())
}

async fn handle(
    runtime: &mut AppRuntime,
    provider: &dyn LlmProvider,
    request: &RpcRequest,
) -> serde_json::Value {
    let id = &request.id;
    match request.method.as_str() {
        "ping" => json!({"id": id, "ok": true, "result": "pong"}),

        "prompt" => {
            let prompt = request
                .params
                .get("text")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            runtime.clear_abort();
            match runtime.prompt(provider, prompt).await {
                Ok(text) => json!({"id": id, "ok": true, "result": {"text": text}}),
                Err(err) => json!({"id": id, "ok": false, "error": err.to_string()}),
            }
        }

        "abort" => {
            runtime.abort();
            json!({"id": id, "ok": true, "result": {"aborted": true}})
        }

        "steer" => {
            let text = request
                .params
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if text.is_empty() {
                json!({"id": id, "ok": false, "error": "params.text required"})
            } else {
                runtime.steer(text);
                json!({"id": id, "ok": true, "result": {"queued": "steer"}})
            }
        }

        "follow_up" | "followup" => {
            let text = request
                .params
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if text.is_empty() {
                json!({"id": id, "ok": false, "error": "params.text required"})
            } else {
                runtime.follow_up(text);
                json!({"id": id, "ok": true, "result": {"queued": "follow_up"}})
            }
        }

        "session" => json!({
            "id": id,
            "ok": true,
            "result": {
                "path": runtime.session_path().map(|p| p.display().to_string()),
                "summary": runtime.session_summary_line(),
            }
        }),

        "status" => {
            let usage = runtime.token_usage().await;
            let thinking = runtime.thinking_level().await;
            let (context_tokens, context_estimated) = runtime.context_tokens().await;
            let last_prompt = runtime.last_prompt_tokens().await;
            json!({
                "id": id,
                "ok": true,
                "result": {
                    "provider": provider.name(),
                    "model": provider.model(),
                    "mode": format!("{:?}", runtime.mode()),
                    "thinking": thinking.as_str(),
                    // Preferred: last provider prompt size; falls back to char/4.
                    "context_tokens": context_tokens,
                    "context_tokens_estimated": context_estimated,
                    "last_prompt_tokens": last_prompt,
                    // Kept for older clients (same as context_tokens).
                    "estimated_tokens": context_tokens,
                    "usage": {
                        "input_tokens": usage.input_tokens,
                        "output_tokens": usage.output_tokens,
                        "cache_read_tokens": usage.cache_read_tokens,
                        "cache_write_tokens": usage.cache_write_tokens,
                    },
                    "session": runtime.session_path().map(|p| p.display().to_string()),
                    "mcp": runtime.mcp_status_line(),
                }
            })
        }

        "thinking" => {
            // GET: no level → return current. SET: params.level = off|low|medium|high
            if let Some(level_str) = request.params.get("level").and_then(|v| v.as_str()) {
                match ThinkingLevel::parse(level_str) {
                    Some(level) => match runtime.set_thinking_level(level).await {
                        Ok(()) => json!({
                            "id": id,
                            "ok": true,
                            "result": {"thinking": level.as_str()}
                        }),
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    },
                    None => json!({
                        "id": id,
                        "ok": false,
                        "error": format!("invalid thinking level: {level_str}")
                    }),
                }
            } else {
                let level = runtime.thinking_level().await;
                json!({"id": id, "ok": true, "result": {"thinking": level.as_str()}})
            }
        }

        "compact" => {
            let instructions = request
                .params
                .get("instructions")
                .or_else(|| request.params.get("text"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            match runtime
                .maybe_compact(
                    provider,
                    one_core::compaction::CompactRequest::manual(instructions),
                )
                .await
            {
                Ok(_) => {
                    let (tokens, estimated) = runtime.context_tokens().await;
                    json!({
                        "id": id,
                        "ok": true,
                        "result": {
                            "estimated_tokens": tokens,
                            "context_tokens": tokens,
                            "context_tokens_estimated": estimated,
                        }
                    })
                }
                Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
            }
        }

        "spawn" => match spawn_rpc(runtime, &request.params).await {
            Ok(result) => json!({"id": id, "ok": true, "result": result}),
            Err(e) => json!({"id": id, "ok": false, "error": e}),
        },

        other => json!({
            "id": id,
            "ok": false,
            "error": format!(
                "unknown method: {other} (known: ping, prompt, abort, steer, follow_up, session, status, thinking, compact, spawn)"
            )
        }),
    }
}
