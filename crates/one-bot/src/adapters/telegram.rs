use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info};

use super::{PlatformAdapter, Result};
use crate::config::TelegramConfig;
use crate::events::{
    ApprovalDecision, BotInboundEvent, BotInboundMessage, BotOutboundMessage, BotSessionKey,
    MessageHandle, MessageTarget,
};

async fn download_telegram_file(
    client: &reqwest::Client,
    api_base: &str,
    token: &str,
    file_id: &str,
    suggested_name: Option<&str>,
) -> Option<(String, String)> {
    let get_file_url = format!(
        "{}/bot{}/getFile?file_id={}",
        api_base.trim_end_matches('/'),
        token,
        file_id
    );

    let resp = client.get(&get_file_url).send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let file_path = json.get("result")?.get("file_path")?.as_str()?;

    let download_url = if api_base.contains("api.telegram.org") {
        format!("https://api.telegram.org/file/bot{}/{}", token, file_path)
    } else {
        format!(
            "{}/file/bot{}/{}",
            api_base.trim_end_matches('/'),
            token,
            file_path
        )
    };

    let file_bytes = client
        .get(&download_url)
        .send()
        .await
        .ok()?
        .bytes()
        .await
        .ok()?;

    let cache_dir = std::env::temp_dir().join("one_bot_downloads");
    let _ = std::fs::create_dir_all(&cache_dir);

    let ext = std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let filename = if let Some(name) = suggested_name {
        format!("{}_{}", &uuid::Uuid::new_v4().to_string()[..8], name)
    } else {
        format!(
            "{}_{}.{}",
            &uuid::Uuid::new_v4().to_string()[..8],
            "media",
            ext
        )
    };

    let target_path = cache_dir.join(filename);
    std::fs::write(&target_path, &file_bytes).ok()?;

    let local_path_str = target_path.to_string_lossy().to_string();
    let mime = match ext.to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "rs" | "py" | "js" | "ts" | "txt" | "md" | "log" => "text/plain",
        _ => "application/octet-stream",
    };

    Some((local_path_str, mime.to_string()))
}

/// Telegram Bot Adapter supporting Long Polling, streaming updates, and HITL inline buttons.
pub struct TelegramAdapter {
    config: TelegramConfig,
    client: reqwest::Client,
    last_update_id: Arc<AtomicI64>,
}

impl TelegramAdapter {
    pub fn new(config: TelegramConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .unwrap_or_default();

        Self {
            config,
            client,
            last_update_id: Arc::new(AtomicI64::new(0)),
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!(
            "{}/bot{}/{}",
            self.config.api_base.trim_end_matches('/'),
            self.config.bot_token,
            method
        )
    }

    /// Format an outbound message with thinking and tool status.
    pub fn format_text(&self, message: &BotOutboundMessage) -> String {
        let mut parts = Vec::new();

        if let Some(tool_status) = &message.tool_status {
            if !tool_status.is_empty() {
                parts.push(format!("🔧 {}\n", tool_status));
            }
        }

        if let Some(thinking) = &message.thinking {
            if !thinking.is_empty() {
                // Shorten or format thinking block
                let preview = if thinking.len() > 300 {
                    format!("{}...", &thinking[..297])
                } else {
                    thinking.clone()
                };
                parts.push(format!("💭 思考中:\n> {}\n", preview.replace('\n', "\n> ")));
            }
        }

        if !message.text.is_empty() {
            parts.push(message.text.clone());
        } else if parts.is_empty() {
            parts.push("...".to_string());
        }

        let full_text = parts.join("\n");
        // Telegram message text limit is 4096 characters
        if full_text.len() > 4000 {
            format!(
                "{}...\n\n[内容超出 Telegram 限制已截断]",
                &full_text[..3900]
            )
        } else {
            full_text
        }
    }

    /// Convert buttons to Telegram inline_keyboard JSON structure.
    fn build_reply_markup(&self, message: &BotOutboundMessage) -> Option<serde_json::Value> {
        if message.buttons.is_empty() {
            return None;
        }

        let keyboard: Vec<Vec<serde_json::Value>> = message
            .buttons
            .iter()
            .map(|row| {
                row.iter()
                    .map(|btn| {
                        json!({
                            "text": btn.text,
                            "callback_data": btn.callback_data,
                        })
                    })
                    .collect()
            })
            .collect();

        Some(json!({ "inline_keyboard": keyboard }))
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    fn platform_id(&self) -> &'static str {
        "telegram"
    }

    async fn start_listening(&self, event_tx: mpsc::Sender<BotInboundEvent>) -> Result<()> {
        info!("Starting Telegram Long Polling listener...");

        // Fetch bot profile info
        let get_me_url = self.api_url("getMe");
        if let Ok(resp) = self.client.get(&get_me_url).send().await {
            if let Ok(data) = resp.json::<serde_json::Value>().await {
                if let Some(user) = data.get("result") {
                    let username = user
                        .get("username")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let first_name = user
                        .get("first_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("One Bot");
                    info!("🤖 Telegram Bot connected: {} (@{})", first_name, username);
                }
            }
        }

        let api_base = self.api_url("getUpdates");
        let ack_base = self.api_url("answerCallbackQuery");
        let client = self.client.clone();
        let last_update_id = self.last_update_id.clone();
        let token = self.config.bot_token.clone();
        let server_api_base = self.config.api_base.clone();

        tokio::spawn(async move {
            loop {
                let offset = last_update_id.load(Ordering::Relaxed);
                let poll_url = format!("{}?offset={}&timeout=25", api_base, offset);

                match client.get(&poll_url).send().await {
                    Ok(resp) => {
                        if let Ok(data) = resp.json::<serde_json::Value>().await {
                            if let Some(ok) = data.get("ok").and_then(|v| v.as_bool()) {
                                if ok {
                                    if let Some(updates) =
                                        data.get("result").and_then(|v| v.as_array())
                                    {
                                        for update in updates {
                                            if let Some(uid) =
                                                update.get("update_id").and_then(|v| v.as_i64())
                                            {
                                                last_update_id.store(uid + 1, Ordering::Relaxed);
                                            }

                                            // 1. Handle normal message
                                            if let Some(msg) = update.get("message") {
                                                let chat_id = msg
                                                    .get("chat")
                                                    .and_then(|c| c.get("id"))
                                                    .map(|id| id.to_string())
                                                    .unwrap_or_default();
                                                let user_id = msg
                                                    .get("from")
                                                    .and_then(|u| u.get("id"))
                                                    .map(|id| id.to_string())
                                                    .unwrap_or_else(|| chat_id.clone());
                                                let user_name = msg
                                                    .get("from")
                                                    .and_then(|u| {
                                                        u.get("username")
                                                            .or_else(|| u.get("first_name"))
                                                    })
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string());
                                                let mut text = msg
                                                    .get("text")
                                                    .and_then(|t| t.as_str())
                                                    .unwrap_or_default()
                                                    .to_string();
                                                if text.is_empty() {
                                                    if let Some(caption) =
                                                        msg.get("caption").and_then(|c| c.as_str())
                                                    {
                                                        text = caption.to_string();
                                                    }
                                                }
                                                let thread_id = msg
                                                    .get("message_thread_id")
                                                    .map(|t| t.to_string());
                                                let reply_to = msg
                                                    .get("reply_to_message")
                                                    .and_then(|r| r.get("message_id"))
                                                    .map(|id| id.to_string());

                                                // Check for photo attachments
                                                let mut attachments = Vec::new();
                                                if let Some(photos) =
                                                    msg.get("photo").and_then(|p| p.as_array())
                                                {
                                                    if let Some(last) = photos.last() {
                                                        if let Some(fid) = last
                                                            .get("file_id")
                                                            .and_then(|v| v.as_str())
                                                        {
                                                            if let Some((path, mime)) =
                                                                download_telegram_file(
                                                                    &client,
                                                                    &server_api_base,
                                                                    &token,
                                                                    fid,
                                                                    Some("photo.jpg"),
                                                                )
                                                                .await
                                                            {
                                                                attachments.push(
                                                                    crate::events::BotAttachment {
                                                                        mime_type: mime,
                                                                        file_name: Some(
                                                                            "photo.jpg".to_string(),
                                                                        ),
                                                                        local_path: path,
                                                                        is_image: true,
                                                                    },
                                                                );
                                                            }
                                                        }
                                                    }
                                                }

                                                // Check for document attachments
                                                if let Some(doc) = msg.get("document") {
                                                    if let Some(fid) =
                                                        doc.get("file_id").and_then(|v| v.as_str())
                                                    {
                                                        let name = doc
                                                            .get("file_name")
                                                            .and_then(|v| v.as_str())
                                                            .unwrap_or("document");
                                                        let mime = doc
                                                            .get("mime_type")
                                                            .and_then(|v| v.as_str())
                                                            .unwrap_or("application/octet-stream");
                                                        let is_img = mime.starts_with("image/")
                                                            || name.ends_with(".png")
                                                            || name.ends_with(".jpg")
                                                            || name.ends_with(".jpeg");

                                                        if let Some((path, inferred_mime)) =
                                                            download_telegram_file(
                                                                &client,
                                                                &server_api_base,
                                                                &token,
                                                                fid,
                                                                Some(name),
                                                            )
                                                            .await
                                                        {
                                                            let final_mime = if mime
                                                                != "application/octet-stream"
                                                            {
                                                                mime.to_string()
                                                            } else {
                                                                inferred_mime
                                                            };
                                                            attachments.push(
                                                                crate::events::BotAttachment {
                                                                    mime_type: final_mime,
                                                                    file_name: Some(
                                                                        name.to_string(),
                                                                    ),
                                                                    local_path: path,
                                                                    is_image: is_img,
                                                                },
                                                            );
                                                        }
                                                    }
                                                }

                                                if text.is_empty() && !attachments.is_empty() {
                                                    text =
                                                        "请查看并分析我发送的文件/图片".to_string();
                                                }

                                                if (!text.is_empty() || !attachments.is_empty())
                                                    && !chat_id.is_empty()
                                                {
                                                    let session_key = BotSessionKey::new(
                                                        "telegram",
                                                        &chat_id,
                                                        thread_id.clone(),
                                                        &user_id,
                                                    );
                                                    let target = MessageTarget::new(
                                                        "telegram",
                                                        &chat_id,
                                                        thread_id,
                                                        Some(user_id),
                                                    );

                                                    let inbound = BotInboundMessage {
                                                        session_key,
                                                        target,
                                                        user_name,
                                                        text,
                                                        reply_to_message_id: reply_to,
                                                        attachments,
                                                    };
                                                    let _ = event_tx
                                                        .send(BotInboundEvent::Message(inbound))
                                                        .await;
                                                }
                                            }

                                            // 2. Handle callback query (inline button click for approval)
                                            if let Some(cb) = update.get("callback_query") {
                                                let cb_id = cb
                                                    .get("id")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or_default();
                                                let from_id = cb
                                                    .get("from")
                                                    .and_then(|u| u.get("id"))
                                                    .map(|id| id.to_string())
                                                    .unwrap_or_default();
                                                let data_str = cb
                                                    .get("data")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or_default();

                                                // Format: approve:<req_id> / deny:<req_id> / allow_always:<req_id>
                                                let mut approved = false;
                                                let mut always = false;
                                                let mut req_id = "";

                                                if let Some(id) = data_str.strip_prefix("approve:")
                                                {
                                                    approved = true;
                                                    req_id = id;
                                                } else if let Some(id) =
                                                    data_str.strip_prefix("deny:")
                                                {
                                                    approved = false;
                                                    req_id = id;
                                                } else if let Some(id) =
                                                    data_str.strip_prefix("allow_always:")
                                                {
                                                    approved = true;
                                                    always = true;
                                                    req_id = id;
                                                }

                                                if !req_id.is_empty() {
                                                    let target = cb
                                                        .get("message")
                                                        .and_then(|m| m.get("chat"))
                                                        .and_then(|c| c.get("id"))
                                                        .map(|chat_id| {
                                                            MessageTarget::new(
                                                                "telegram",
                                                                chat_id.to_string(),
                                                                cb.get("message")
                                                                    .and_then(|m| {
                                                                        m.get("message_thread_id")
                                                                    })
                                                                    .map(|id| id.to_string()),
                                                                Some(from_id.clone()),
                                                            )
                                                        });
                                                    let decision = ApprovalDecision {
                                                        request_id: req_id.to_string(),
                                                        user_id: from_id,
                                                        target,
                                                        approved,
                                                        always_allow: always,
                                                    };
                                                    let _ = event_tx
                                                        .send(BotInboundEvent::ApprovalDecision(
                                                            decision,
                                                        ))
                                                        .await;
                                                }

                                                // Acknowledge callback query immediately
                                                let _ = client.post(&ack_base)
                                                    .json(&json!({
                                                        "callback_query_id": cb_id,
                                                        "text": if approved { "✅ 已批准操作" } else { "❌ 已拒绝操作" },
                                                    }))
                                                    .send()
                                                    .await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        debug!("Telegram polling error: {}, retrying in 3s...", e);
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                }
            }
        });

        Ok(())
    }

    async fn send_message(
        &self,
        target: &MessageTarget,
        message: &BotOutboundMessage,
    ) -> Result<MessageHandle> {
        let url = self.api_url("sendMessage");
        let text = self.format_text(message);

        let mut payload = json!({
            "chat_id": target.channel_id,
            "text": text,
            "link_preview_options": { "is_disabled": true },
        });

        if let Some(thread_id) = &target.thread_id {
            if let Ok(tid) = thread_id.parse::<i64>() {
                payload["message_thread_id"] = json!(tid);
            }
        }

        if let Some(markup) = self.build_reply_markup(message) {
            payload["reply_markup"] = markup;
        }

        let resp = self.client.post(&url).json(&payload).send().await?;
        let res_json: serde_json::Value = resp.json().await?;

        if let Some(msg_id) = res_json.get("result").and_then(|r| r.get("message_id")) {
            Ok(MessageHandle {
                target: target.clone(),
                message_id: msg_id.to_string(),
            })
        } else {
            let desc = res_json
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("unknown error");
            Err(format!("Telegram send_message failed: {}", desc).into())
        }
    }

    async fn edit_message(
        &self,
        handle: &MessageHandle,
        message: &BotOutboundMessage,
    ) -> Result<()> {
        let url = self.api_url("editMessageText");
        let text = self.format_text(message);

        let mut payload = json!({
            "chat_id": handle.target.channel_id,
            "message_id": handle.message_id.parse::<i64>().unwrap_or_default(),
            "text": text,
            "link_preview_options": { "is_disabled": true },
        });

        if let Some(markup) = self.build_reply_markup(message) {
            payload["reply_markup"] = markup;
        }

        let resp = self.client.post(&url).json(&payload).send().await?;
        let res_json: serde_json::Value = resp.json().await?;

        if let Some(ok) = res_json.get("ok").and_then(|v| v.as_bool()) {
            if ok {
                return Ok(());
            }
        }

        // Telegram returns "message is not modified" if contents are unchanged - safe to ignore
        let desc = res_json
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        if desc.contains("message is not modified") {
            return Ok(());
        }

        debug!("Telegram edit_message response: {}", desc);
        Ok(())
    }

    async fn send_typing(&self, target: &MessageTarget) -> Result<()> {
        let url = self.api_url("sendChatAction");
        let mut payload = json!({
            "chat_id": target.channel_id,
            "action": "typing",
        });
        if let Some(thread_id) = &target.thread_id {
            if let Ok(tid) = thread_id.parse::<i64>() {
                payload["message_thread_id"] = json!(tid);
            }
        }
        let _ = self.client.post(&url).json(&payload).send().await;
        Ok(())
    }
}
