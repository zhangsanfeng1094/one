use std::io::{self, BufRead};

use one_core::agent::LlmProvider;
use serde::Deserialize;
use serde_json::json;

use crate::runtime::AppRuntime;

#[derive(Debug, Deserialize)]
struct RpcRequest {
    id: Option<String>,
    method: String,
    params: serde_json::Value,
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
        let request: RpcRequest = serde_json::from_str(&line)?;
        let response = match request.method.as_str() {
            "prompt" => {
                let prompt = request
                    .params
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                match runtime.prompt(provider, prompt).await {
                    Ok(text) => json!({"id": request.id, "ok": true, "result": {"text": text}}),
                    Err(err) => json!({"id": request.id, "ok": false, "error": err.to_string()}),
                }
            }
            "session" => json!({
                "id": request.id,
                "ok": true,
                "result": {
                    "path": runtime.session_path(),
                }
            }),
            "ping" => json!({"id": request.id, "ok": true, "result": "pong"}),
            other => json!({"id": request.id, "ok": false, "error": format!("unknown method: {other}")}),
        };
        println!("{}", serde_json::to_string(&response)?);
    }
    Ok(())
}