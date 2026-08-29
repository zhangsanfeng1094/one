//! Discord Platform Adapter (Hermes-aligned).
//!
//! Supports sending/editing messages, interactive ActionRow buttons (HITL),
//! uploading/downloading multimodal file attachments (images, code files, documents),
//! and directory/channel session routing.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::info;

use super::{PlatformAdapter, Result};
use crate::config::DiscordConfig;
use crate::events::{
    ApprovalDecision, BotAttachment, BotInboundEvent, BotInboundMessage, BotOutboundMessage,
    BotSessionKey, MessageHandle, MessageTarget, PlatformCapabilities,
};

/// Download an attachment from Discord CDN and save to `/tmp/one_bot_downloads/`.
pub async fn download_discord_attachment(
    client: &reqwest::Client,
    url: &str,
    suggested_name: &str,
    content_type: Option<&str>,
) -> Option<BotAttachment> {
    let resp = client.get(url).send().await.ok()?;
    let bytes = resp.bytes().await.ok()?;

    let cache_dir = std::env::temp_dir().join("one_bot_downloads");
    let _ = std::fs::create_dir_all(&cache_dir);

    let ext = Path::new(suggested_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let filename = format!(
        "{}_{}",
        &uuid::Uuid::new_v4().to_string()[..8],
        suggested_name
    );
    let target_path = cache_dir.join(&filename);
    std::fs::write(&target_path, &bytes).ok()?;

    let mime = if let Some(ct) = content_type {
        ct.to_string()
    } else {
        match ext.to_lowercase().as_str() {
            "jpg" | "jpeg" => "image/jpeg".to_string(),
            "png" => "image/png".to_string(),
            "webp" => "image/webp".to_string(),
            "gif" => "image/gif".to_string(),
            "pdf" => "application/pdf".to_string(),
            "json" => "application/json".to_string(),
            "rs" | "py" | "js" | "ts" | "txt" | "md" | "log" | "c" | "cpp" | "go" => {
                "text/plain".to_string()
            }
            _ => "application/octet-stream".to_string(),
        }
    };

    let is_image = mime.starts_with("image/")
        || matches!(
            ext.to_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "gif"
        );

    Some(BotAttachment {
        mime_type: mime,
        file_name: Some(suggested_name.to_string()),
        local_path: target_path.to_string_lossy().to_string(),
        is_image,
    })
}

/// Discord Bot Adapter (REST & Gateway interaction support).
pub struct DiscordAdapter {
    config: DiscordConfig,
    client: reqwest::Client,
}

impl DiscordAdapter {
    pub fn new(config: DiscordConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        Self { config, client }
    }

    fn auth_header(&self) -> String {
        format!("Bot {}", self.config.bot_token)
    }

    /// Upload a local file/document/image directly to Discord channel via multipart upload.
    pub async fn upload_file(
        &self,
        target: &MessageTarget,
        file_path: impl AsRef<Path>,
        comment: Option<&str>,
    ) -> Result<MessageHandle> {
        let path = file_path.as_ref();
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        let file_bytes = tokio::fs::read(path).await?;

        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            target.channel_id
        );

        let part = reqwest::multipart::Part::bytes(file_bytes).file_name(filename);
        let mut form = reqwest::multipart::Form::new().part("files[0]", part);

        if let Some(c) = comment {
            let payload = json!({ "content": c });
            form = form.text("payload_json", serde_json::to_string(&payload)?);
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .await?;

        let res_json: serde_json::Value = resp.json().await?;
        let msg_id = res_json
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or_default();

        Ok(MessageHandle {
            target: target.clone(),
            message_id: msg_id.to_string(),
        })
    }

    /// Process an inbound Discord Message JSON payload (from Gateway or Webhook).
    pub async fn handle_inbound_message_json(
        &self,
        msg: &serde_json::Value,
        event_tx: &mpsc::Sender<BotInboundEvent>,
    ) -> Result<()> {
        // Skip bot messages
        if msg
            .get("author")
            .and_then(|a| a.get("bot"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            return Ok(());
        }

        let channel_id = msg
            .get("channel_id")
            .and_then(|c| c.as_str())
            .unwrap_or_default();
        let thread_id = msg
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(|id| id.as_str())
            .map(|s| s.to_string());
        let author_id = msg
            .get("author")
            .and_then(|a| a.get("id"))
            .and_then(|id| id.as_str())
            .unwrap_or("unknown");
        let username = msg
            .get("author")
            .and_then(|a| a.get("username"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_string());
        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
        let msg_id = msg
            .get("id")
            .and_then(|id| id.as_str())
            .map(|s| s.to_string());

        let mut attachments = Vec::new();
        if let Some(att_array) = msg.get("attachments").and_then(|a| a.as_array()) {
            for item in att_array {
                if let (Some(url), Some(fname)) = (
                    item.get("url").and_then(|u| u.as_str()),
                    item.get("filename").and_then(|f| f.as_str()),
                ) {
                    let content_type = item.get("content_type").and_then(|ct| ct.as_str());
                    if let Some(att) =
                        download_discord_attachment(&self.client, url, fname, content_type).await
                    {
                        info!(
                            "Downloaded Discord attachment: {} ({}) -> {}",
                            fname, att.mime_type, att.local_path
                        );
                        attachments.push(att);
                    }
                }
            }
        }

        let session_key = BotSessionKey::new("discord", channel_id, thread_id.clone(), author_id);
        let target = MessageTarget::new(
            "discord",
            channel_id,
            thread_id,
            Some(author_id.to_string()),
        );

        let inbound = BotInboundMessage {
            session_key,
            target,
            user_name: username,
            text: content.to_string(),
            reply_to_message_id: msg_id,
            attachments,
        };

        let _ = event_tx.send(BotInboundEvent::Message(inbound)).await;
        Ok(())
    }

    /// Process an inbound Discord Interaction (e.g. Button click for HITL approvals).
    pub async fn handle_interaction_json(
        &self,
        interaction: &serde_json::Value,
        event_tx: &mpsc::Sender<BotInboundEvent>,
    ) -> Result<()> {
        let interaction_id = interaction
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or_default();
        let token = interaction
            .get("token")
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        let user_id = interaction
            .get("user")
            .or_else(|| interaction.get("member").and_then(|m| m.get("user")))
            .and_then(|u| u.get("id"))
            .and_then(|id| id.as_str())
            .unwrap_or("unknown");

        let custom_id = interaction
            .get("data")
            .and_then(|d| d.get("custom_id"))
            .and_then(|c| c.as_str())
            .unwrap_or("");

        // Acknowledge interaction to Discord
        let ack_url = format!(
            "https://discord.com/api/v10/interactions/{}/{}/callback",
            interaction_id, token
        );
        let _ = self
            .client
            .post(&ack_url)
            .json(&json!({
                "type": 6 // DEFERRED_UPDATE_MESSAGE
            }))
            .send()
            .await;

        if custom_id.starts_with("appr:") {
            let parts: Vec<&str> = custom_id.split(':').collect();
            if parts.len() >= 3 {
                let req_id = parts[1];
                let decision = parts[2];

                let (approved, always) = match decision {
                    "yes" => (true, false),
                    "always" => (true, true),
                    _ => (false, false),
                };

                let dec = ApprovalDecision {
                    request_id: req_id.to_string(),
                    user_id: user_id.to_string(),
                    approved,
                    always_allow: always,
                };

                let _ = event_tx.send(BotInboundEvent::ApprovalDecision(dec)).await;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl PlatformAdapter for DiscordAdapter {
    fn platform_id(&self) -> &'static str {
        "discord"
    }

    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            max_message_length: 2000,
            supports_streaming_edit: true,
            supports_reactions: true,
            supports_threads: true,
            supports_hitl_buttons: true,
            supports_images: true,
            supports_documents: true,
        }
    }

    async fn start_listening(&self, _event_tx: mpsc::Sender<BotInboundEvent>) -> Result<()> {
        info!("Discord Adapter started with multimodal attachment and HITL interaction support.");
        Ok(())
    }

    async fn send_message(
        &self,
        target: &MessageTarget,
        message: &BotOutboundMessage,
    ) -> Result<MessageHandle> {
        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            target.channel_id
        );

        let mut embeds = Vec::new();
        if let Some(thinking) = &message.thinking {
            if !thinking.is_empty() {
                embeds.push(json!({
                    "title": "💭 思考过程",
                    "description": thinking,
                    "color": 0x7289da,
                }));
            }
        }

        let mut body = json!({
            "content": if message.text.is_empty() { "..." } else { &message.text },
            "embeds": embeds,
        });

        // Convert buttons to ActionRow components
        if !message.buttons.is_empty() {
            let mut components = Vec::new();
            for row in &message.buttons {
                let row_buttons: Vec<serde_json::Value> = row
                    .iter()
                    .map(|btn| {
                        json!({
                            "type": 2, // Button
                            "style": 1, // Primary
                            "label": btn.text,
                            "custom_id": btn.callback_data,
                        })
                    })
                    .collect();
                components.push(json!({
                    "type": 1, // Action Row
                    "components": row_buttons,
                }));
            }
            body["components"] = json!(components);
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await?;

        let res_json: serde_json::Value = resp.json().await?;
        let msg_id = res_json
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or_default();

        Ok(MessageHandle {
            target: target.clone(),
            message_id: msg_id.to_string(),
        })
    }

    async fn edit_message(
        &self,
        handle: &MessageHandle,
        message: &BotOutboundMessage,
    ) -> Result<()> {
        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages/{}",
            handle.target.channel_id, handle.message_id
        );

        let mut embeds = Vec::new();
        if let Some(thinking) = &message.thinking {
            if !thinking.is_empty() {
                embeds.push(json!({
                    "title": "💭 思考过程",
                    "description": thinking,
                    "color": 0x7289da,
                }));
            }
        }

        let mut body = json!({
            "content": if message.text.is_empty() { "..." } else { &message.text },
            "embeds": embeds,
        });

        // Update components if final or removed
        if message.is_final && message.buttons.is_empty() {
            body["components"] = json!([]);
        }

        let _ = self
            .client
            .patch(&url)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await;

        Ok(())
    }

    async fn send_typing(&self, target: &MessageTarget) -> Result<()> {
        let url = format!(
            "https://discord.com/api/v10/channels/{}/typing",
            target.channel_id
        );
        let _ = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await;
        Ok(())
    }
}
