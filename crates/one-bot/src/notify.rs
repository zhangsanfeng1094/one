use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::adapters::PlatformAdapter;
use crate::events::{BotOutboundMessage, MessageTarget};

/// Proactive notification engine and event bus for broadcasting to IM channels.
#[derive(Clone)]
pub struct NotificationEngine {
    adapters: Arc<RwLock<HashMap<String, Arc<dyn PlatformAdapter>>>>,
}

impl NotificationEngine {
    pub fn new() -> Self {
        Self {
            adapters: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a platform adapter.
    pub async fn register_adapter(&self, adapter: Arc<dyn PlatformAdapter>) {
        let mut map = self.adapters.write().await;
        map.insert(adapter.platform_id().to_string(), adapter);
    }

    /// Push an alert or notification to a specific target channel.
    pub async fn notify(&self, target: &MessageTarget, title: &str, content: &str) -> bool {
        let guard = self.adapters.read().await;
        if let Some(adapter) = guard.get(&target.platform) {
            let msg = BotOutboundMessage {
                text: format!("📢 **[{}]**\n\n{}", title, content),
                thinking: None,
                tool_status: None,
                buttons: Vec::new(),
                is_final: true,
            };
            match adapter.send_message(target, &msg).await {
                Ok(_) => {
                    info!(
                        "Proactive notification sent to {}:{}",
                        target.platform, target.channel_id
                    );
                    true
                }
                Err(e) => {
                    warn!("Failed to send proactive notification: {}", e);
                    false
                }
            }
        } else {
            warn!("No adapter registered for platform {}", target.platform);
            false
        }
    }
}
