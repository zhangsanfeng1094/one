use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tracing::info;

use super::{PlatformAdapter, Result};
use crate::config::FeishuConfig;
use crate::events::{BotInboundEvent, BotOutboundMessage, MessageHandle, MessageTarget};

/// Feishu / Lark Bot Adapter (Interactive Card & Webhook support).
pub struct FeishuAdapter {
    config: FeishuConfig,
    client: reqwest::Client,
    tenant_access_token: Arc<RwLock<Option<String>>>,
}

impl FeishuAdapter {
    pub fn new(config: FeishuConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        Self {
            config,
            client,
            tenant_access_token: Arc::new(RwLock::new(None)),
        }
    }

    async fn get_access_token(&self) -> Result<String> {
        {
            let guard = self.tenant_access_token.read().await;
            if let Some(token) = &*guard {
                return Ok(token.clone());
            }
        }

        let url = "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal";
        let body = json!({
            "app_id": self.config.app_id,
            "app_secret": self.config.app_secret,
        });

        let resp = self.client.post(url).json(&body).send().await?;
        let res_json: serde_json::Value = resp.json().await?;

        if let Some(token) = res_json.get("tenant_access_token").and_then(|t| t.as_str()) {
            let mut guard = self.tenant_access_token.write().await;
            *guard = Some(token.to_string());
            Ok(token.to_string())
        } else {
            Err("Failed to obtain Feishu tenant_access_token".into())
        }
    }

    fn build_card_json(&self, message: &BotOutboundMessage) -> serde_json::Value {
        let mut elements = Vec::new();

        if let Some(tool_status) = &message.tool_status {
            if !tool_status.is_empty() {
                elements.push(json!({
                    "tag": "div",
                    "text": {
                        "tag": "lark_md",
                        "content": format!("**🔧 工具状态**: {}", tool_status)
                    }
                }));
            }
        }

        if let Some(thinking) = &message.thinking {
            if !thinking.is_empty() {
                elements.push(json!({
                    "tag": "div",
                    "text": {
                        "tag": "lark_md",
                        "content": format!("*💭 思考中*:\n> {}", thinking.replace('\n', "\n> "))
                    }
                }));
            }
        }

        if !message.text.is_empty() {
            elements.push(json!({
                "tag": "div",
                "text": {
                    "tag": "lark_md",
                    "content": message.text
                }
            }));
        }

        // Action buttons for HITL approval
        if !message.buttons.is_empty() {
            let mut actions = Vec::new();
            for row in &message.buttons {
                for btn in row {
                    actions.push(json!({
                        "tag": "button",
                        "text": {
                            "tag": "plain_text",
                            "content": btn.text
                        },
                        "type": "primary",
                        "value": {
                            "action": btn.callback_data
                        }
                    }));
                }
            }
            elements.push(json!({
                "tag": "action",
                "actions": actions
            }));
        }

        json!({
            "config": {
                "wide_screen_mode": true
            },
            "header": {
                "title": {
                    "tag": "plain_text",
                    "content": "🤖 One Agent"
                },
                "template": "blue"
            },
            "elements": elements
        })
    }
}

#[async_trait]
impl PlatformAdapter for FeishuAdapter {
    fn platform_id(&self) -> &'static str {
        "feishu"
    }

    async fn start_listening(&self, _event_tx: mpsc::Sender<BotInboundEvent>) -> Result<()> {
        info!("Feishu Adapter initialized.");
        Ok(())
    }

    async fn send_message(
        &self,
        target: &MessageTarget,
        message: &BotOutboundMessage,
    ) -> Result<MessageHandle> {
        let token = self.get_access_token().await?;
        let url = format!(
            "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type={}",
            if target.channel_id.starts_with("oc_") {
                "chat_id"
            } else {
                "open_id"
            }
        );

        let card = self.build_card_json(message);
        let body = json!({
            "receive_id": target.channel_id,
            "msg_type": "interactive",
            "content": serde_json::to_string(&card)?
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .json(&body)
            .send()
            .await?;

        let res_json: serde_json::Value = resp.json().await?;
        let msg_id = res_json
            .get("data")
            .and_then(|d| d.get("message_id"))
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
        let token = self.get_access_token().await?;
        let url = format!(
            "https://open.feishu.cn/open-apis/im/v1/messages/{}",
            handle.message_id
        );

        let card = self.build_card_json(message);
        let body = json!({
            "content": serde_json::to_string(&card)?
        });

        let _ = self
            .client
            .patch(&url)
            .header("Authorization", format!("Bearer {}", token))
            .json(&body)
            .send()
            .await;

        Ok(())
    }

    async fn send_typing(&self, _target: &MessageTarget) -> Result<()> {
        Ok(())
    }
}
