//! Subprocess IPC Platform Adapter (Dynamic Process Connector).
//!
//! Spawns an external connector process (written in Rust, Go, Python, Node.js, etc.)
//! and exchanges JSON-RPC 2.0 messages over standard I/O (stdin/stdout).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tracing::{debug, info, warn};

use super::{PlatformAdapter, Result};
use crate::events::{
    ApprovalDecision, BotAttachment, BotInboundEvent, BotInboundMessage, BotOutboundMessage,
    BotSessionKey, MessageHandle, MessageTarget, PlatformCapabilities,
};

#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcNotification {
    jsonrpc: String,
    method: String,
    params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Option<u64>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

/// Out-of-process dynamic connector adapter communicating via stdio JSON-RPC.
pub struct ProcessConnectorAdapter {
    command: String,
    args: Vec<String>,
    platform_id: Arc<RwLock<String>>,
    capabilities: Arc<RwLock<PlatformCapabilities>>,
    stdin_writer: Arc<Mutex<Option<ChildStdin>>>,
    pending_requests: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value>>>>>,
    next_request_id: Arc<AtomicU64>,
    child_handle: Arc<Mutex<Option<Child>>>,
}

impl ProcessConnectorAdapter {
    pub fn new(
        command: impl Into<String>,
        args: Vec<String>,
        default_platform_id: Option<String>,
    ) -> Self {
        Self {
            command: command.into(),
            args,
            platform_id: Arc::new(RwLock::new(
                default_platform_id.unwrap_or_else(|| "dynamic_connector".to_string()),
            )),
            capabilities: Arc::new(RwLock::new(PlatformCapabilities::default())),
            stdin_writer: Arc::new(Mutex::new(None)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            next_request_id: Arc::new(AtomicU64::new(1)),
            child_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// Helper to send an RPC request and await response.
    async fn call_rpc(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };

        let line = serde_json::to_string(&req)? + "\n";
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(id, tx);
        }

        {
            let mut writer_guard = self.stdin_writer.lock().await;
            if let Some(stdin) = writer_guard.as_mut() {
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
            } else {
                let mut pending = self.pending_requests.lock().await;
                pending.remove(&id);
                return Err("Connector child process stdin not available".into());
            }
        }

        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err("RPC response channel closed unexpectedly".into()),
            Err(_) => {
                let mut pending = self.pending_requests.lock().await;
                pending.remove(&id);
                Err(format!("RPC call '{}' timed out after 30s", method).into())
            }
        }
    }
}

#[async_trait]
impl PlatformAdapter for ProcessConnectorAdapter {
    fn platform_id(&self) -> &str {
        // Fast synchronous check or fallback
        if let Ok(guard) = self.platform_id.try_read() {
            // Leak not needed if we return a known lifetime or store String
            // We use Box::leak on the string slice for the lifetime or convert trait
            Box::leak(guard.clone().into_boxed_str())
        } else {
            "dynamic_connector"
        }
    }

    fn capabilities(&self) -> PlatformCapabilities {
        if let Ok(guard) = self.capabilities.try_read() {
            guard.clone()
        } else {
            PlatformCapabilities::default()
        }
    }

    async fn start_listening(&self, event_tx: mpsc::Sender<BotInboundEvent>) -> Result<()> {
        info!(
            "Spawning dynamic process connector: {} {:?}",
            self.command, self.args
        );

        let mut child = Command::new(&self.command)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                format!(
                    "Failed to spawn connector process '{}': {}",
                    self.command, e
                )
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or("Failed to open child process stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Failed to open child process stdout")?;

        {
            let mut writer_guard = self.stdin_writer.lock().await;
            *writer_guard = Some(stdin);
        }

        let pending_map = self.pending_requests.clone();
        let platform_id_cell = self.platform_id.clone();
        let capabilities_cell = self.capabilities.clone();
        let cmd_name = self.command.clone();

        // Background reader for child process stdout
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();

            while let Ok(Some(line)) = reader.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                if let Ok(raw_json) = serde_json::from_str::<serde_json::Value>(line) {
                    // Check if it's a response (has "id")
                    if let Some(id) = raw_json.get("id").and_then(|v| v.as_u64()) {
                        let mut pending = pending_map.lock().await;
                        if let Some(tx) = pending.remove(&id) {
                            if let Some(err) = raw_json.get("error") {
                                let err_msg = err.to_string();
                                let _ = tx.send(Err(err_msg.into()));
                            } else {
                                let result = raw_json
                                    .get("result")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                let _ = tx.send(Ok(result));
                            }
                        }
                        continue;
                    }

                    // Check if it's an inbound notification/event from connector
                    if let Some(method) = raw_json.get("method").and_then(|v| v.as_str()) {
                        let params = raw_json
                            .get("params")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);

                        match method {
                            "inbound_message" => {
                                if let Ok(inbound) =
                                    serde_json::from_value::<BotInboundMessageDto>(params)
                                {
                                    let session_key = BotSessionKey::new(
                                        inbound.session_key.platform,
                                        inbound.session_key.channel_id,
                                        inbound.session_key.thread_id,
                                        inbound.session_key.user_id,
                                    );
                                    let target = MessageTarget::new(
                                        inbound.target.platform,
                                        inbound.target.channel_id,
                                        inbound.target.thread_id,
                                        inbound.target.user_id,
                                    );
                                    let attachments = inbound
                                        .attachments
                                        .into_iter()
                                        .map(|a| BotAttachment {
                                            mime_type: a.mime_type,
                                            file_name: a.file_name,
                                            local_path: a.local_path,
                                            is_image: a.is_image,
                                        })
                                        .collect();

                                    let msg = BotInboundMessage {
                                        session_key,
                                        target,
                                        user_name: inbound.user_name,
                                        text: inbound.text,
                                        reply_to_message_id: inbound.reply_to_message_id,
                                        attachments,
                                    };
                                    let _ = event_tx.send(BotInboundEvent::Message(msg)).await;
                                }
                            }
                            "approval_decision" => {
                                if let Ok(decision) =
                                    serde_json::from_value::<ApprovalDecisionDto>(params)
                                {
                                    let dec = ApprovalDecision {
                                        request_id: decision.request_id,
                                        user_id: decision.user_id,
                                        target: decision.target.map(|target| {
                                            MessageTarget::new(
                                                target.platform,
                                                target.channel_id,
                                                target.thread_id,
                                                target.user_id,
                                            )
                                        }),
                                        approved: decision.approved,
                                        always_allow: decision.always_allow.unwrap_or(false),
                                    };
                                    let _ =
                                        event_tx.send(BotInboundEvent::ApprovalDecision(dec)).await;
                                }
                            }
                            "handshake" => {
                                if let Some(pid) =
                                    params.get("platform_id").and_then(|v| v.as_str())
                                {
                                    let mut p_guard = platform_id_cell.write().await;
                                    *p_guard = pid.to_string();
                                    info!(
                                        "Connector '{}' completed handshake as platform: '{}'",
                                        cmd_name, pid
                                    );
                                }
                                if let Some(caps_val) = params.get("capabilities") {
                                    if let Ok(caps) = serde_json::from_value::<PlatformCapabilities>(
                                        caps_val.clone(),
                                    ) {
                                        let mut c_guard = capabilities_cell.write().await;
                                        *c_guard = caps;
                                    }
                                }
                            }
                            other => {
                                debug!(
                                    "Unknown notification from connector '{}': {}",
                                    cmd_name, other
                                );
                            }
                        }
                    }
                }
            }

            warn!(
                "Connector process '{}' standard output stream closed.",
                cmd_name
            );
        });

        {
            let mut child_guard = self.child_handle.lock().await;
            *child_guard = Some(child);
        }

        // Send initialization handshake
        match self
            .call_rpc("init", json!({"client": "one-bot", "version": "0.1.0"}))
            .await
        {
            Ok(res) => {
                if let Some(pid) = res.get("platform_id").and_then(|v| v.as_str()) {
                    let mut p_guard = self.platform_id.write().await;
                    *p_guard = pid.to_string();
                    info!(
                        "Connector '{}' initialized with platform: '{}'",
                        self.command, pid
                    );
                }
                if let Some(caps_val) = res.get("capabilities") {
                    if let Ok(caps) =
                        serde_json::from_value::<PlatformCapabilities>(caps_val.clone())
                    {
                        let mut c_guard = self.capabilities.write().await;
                        *c_guard = caps;
                        info!(
                            "Connector '{}' declared capabilities: max_len={}, streaming={}",
                            self.command,
                            c_guard.max_message_length,
                            c_guard.supports_streaming_edit
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Connector '{}' did not respond to init handshake: {}",
                    self.command, e
                );
            }
        }

        Ok(())
    }

    async fn send_message(
        &self,
        target: &MessageTarget,
        message: &BotOutboundMessage,
    ) -> Result<MessageHandle> {
        let params = json!({
            "target": target,
            "message": message,
        });

        let res = self.call_rpc("send_message", params).await?;
        let msg_id = res
            .get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .to_string();

        Ok(MessageHandle {
            target: target.clone(),
            message_id: msg_id,
        })
    }

    async fn edit_message(
        &self,
        handle: &MessageHandle,
        message: &BotOutboundMessage,
    ) -> Result<()> {
        let params = json!({
            "handle": handle,
            "message": message,
        });

        let _ = self.call_rpc("edit_message", params).await?;
        Ok(())
    }

    async fn send_typing(&self, target: &MessageTarget) -> Result<()> {
        let params = json!({
            "target": target,
        });

        let _ = self.call_rpc("send_typing", params).await?;
        Ok(())
    }
}

// Internal DTOs for IPC serialization
#[derive(Deserialize)]
struct BotSessionKeyDto {
    platform: String,
    channel_id: String,
    thread_id: Option<String>,
    user_id: String,
}

#[derive(Deserialize)]
struct MessageTargetDto {
    platform: String,
    channel_id: String,
    thread_id: Option<String>,
    user_id: Option<String>,
}

#[derive(Deserialize)]
struct BotAttachmentDto {
    mime_type: String,
    file_name: Option<String>,
    local_path: String,
    #[serde(default)]
    is_image: bool,
}

#[derive(Deserialize)]
struct BotInboundMessageDto {
    session_key: BotSessionKeyDto,
    target: MessageTargetDto,
    user_name: Option<String>,
    text: String,
    reply_to_message_id: Option<String>,
    #[serde(default)]
    attachments: Vec<BotAttachmentDto>,
}

#[derive(Deserialize)]
struct ApprovalDecisionDto {
    request_id: String,
    user_id: String,
    #[serde(default)]
    target: Option<MessageTargetDto>,
    approved: bool,
    always_allow: Option<bool>,
}
