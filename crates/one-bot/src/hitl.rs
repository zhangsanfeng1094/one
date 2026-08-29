use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use one_core::{ToolCall, ToolGate, ToolGateDecision};
use tokio::sync::{oneshot, Mutex};
use tracing::{info, warn};

use crate::adapters::PlatformAdapter;
use crate::events::{
    ApprovalDecision, BotOutboundMessage, BotSessionKey, InlineButton, MessageTarget,
};

/// Approval Manager managing pending HITL requests across IM channels.
#[derive(Clone)]
pub struct BotApprovalManager {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>,
}

impl Default for BotApprovalManager {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl BotApprovalManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit a user decision from an inline button callback.
    pub async fn submit_decision(&self, decision: ApprovalDecision) -> bool {
        let mut map = self.pending.lock().await;
        if let Some(tx) = map.remove(&decision.request_id) {
            let _ = tx.send(decision);
            true
        } else {
            false
        }
    }

    /// Request interactive approval on the IM platform and wait for user's decision.
    pub async fn request_approval(
        &self,
        adapter: &Arc<dyn PlatformAdapter>,
        target: &MessageTarget,
        _session_key: &BotSessionKey,
        tool_name: &str,
        tool_args: &serde_json::Value,
        timeout: Duration,
    ) -> bool {
        let request_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let (tx, rx) = oneshot::channel();

        {
            let mut map = self.pending.lock().await;
            map.insert(request_id.clone(), tx);
        }

        let args_str = serde_json::to_string_pretty(tool_args).unwrap_or_default();
        let short_args = if args_str.len() > 500 {
            format!("{}...", &args_str[..497])
        } else {
            args_str
        };

        let card = BotOutboundMessage {
            text: format!(
                "⚠️ **[安全确认] Agent 申请执行操作**\n\n🔧 工具: `{}`\n📋 参数:\n```json\n{}\n```\n请选择是否批准该操作：",
                tool_name, short_args
            ),
            thinking: None,
            tool_status: None,
            buttons: vec![vec![
                InlineButton {
                    text: "✅ 批准执行".to_string(),
                    callback_data: format!("approve:{}", request_id),
                },
                InlineButton {
                    text: "❌ 拒绝操作".to_string(),
                    callback_data: format!("deny:{}", request_id),
                },
            ]],
            is_final: true,
        };

        let handle = match adapter.send_message(target, &card).await {
            Ok(h) => h,
            Err(e) => {
                warn!("Failed to send approval card to IM: {}", e);
                let mut map = self.pending.lock().await;
                map.remove(&request_id);
                return false;
            }
        };

        let approved = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(decision)) => decision.approved,
            Ok(Err(_)) => false,
            Err(_) => {
                info!("Approval request {} timed out", request_id);
                let mut map = self.pending.lock().await;
                map.remove(&request_id);
                false
            }
        };

        // Update card to remove buttons and show final decision result
        let result_card = BotOutboundMessage {
            text: if approved {
                format!("✅ **[已批准]** 工具 `{}` 已授权并开始执行。", tool_name)
            } else {
                format!("❌ **[已拒绝]** 工具 `{}` 执行已被驳回或超时。", tool_name)
            },
            thinking: None,
            tool_status: None,
            buttons: Vec::new(),
            is_final: true,
        };
        let _ = adapter.edit_message(&handle, &result_card).await;

        approved
    }
}

/// ToolGate implementation that delegates risky tool calls to the IM Approval Manager.
pub struct BotToolGate {
    approval_manager: BotApprovalManager,
    adapter: Arc<dyn PlatformAdapter>,
    target: MessageTarget,
    session_key: BotSessionKey,
    require_approval_for_bash: bool,
}

impl BotToolGate {
    pub fn new(
        approval_manager: BotApprovalManager,
        adapter: Arc<dyn PlatformAdapter>,
        target: MessageTarget,
        session_key: BotSessionKey,
        require_approval_for_bash: bool,
    ) -> Self {
        Self {
            approval_manager,
            adapter,
            target,
            session_key,
            require_approval_for_bash,
        }
    }
}

#[async_trait]
impl ToolGate for BotToolGate {
    async fn check(&self, call: &ToolCall) -> ToolGateDecision {
        let is_risky = self.require_approval_for_bash && call.name == "bash";

        if is_risky {
            let approved = self
                .approval_manager
                .request_approval(
                    &self.adapter,
                    &self.target,
                    &self.session_key,
                    &call.name,
                    &call.arguments,
                    Duration::from_secs(300), // 5 minutes timeout
                )
                .await;

            if approved {
                ToolGateDecision::Allow
            } else {
                ToolGateDecision::Deny {
                    message: "User denied the execution of this tool call or approval timed out."
                        .to_string(),
                }
            }
        } else {
            ToolGateDecision::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_approval_manager_flow() {
        let manager = BotApprovalManager::new();

        let decision = ApprovalDecision {
            request_id: "req_123".to_string(),
            user_id: "user1".to_string(),
            approved: true,
            always_allow: false,
        };

        // Submitting to non-existent request should return false
        assert!(!manager.submit_decision(decision.clone()).await);
    }
}
