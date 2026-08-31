use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use one_core::ToolGate;
use tokio::sync::{mpsc, Mutex, RwLock, Semaphore};
use tokio::task::LocalSet;
use tracing::{debug, error, info, warn};

use crate::adapters::PlatformAdapter;
use crate::config::BotConfig;
use crate::events::{
    ApprovalDecision, BotInboundEvent, BotInboundMessage, BotOutboundMessage, BotSessionKey,
};
use crate::hitl::{BotApprovalManager, BotToolGate};
use crate::notify::NotificationEngine;
use crate::session_map::SessionMapper;
use crate::throttler::StreamThrottler;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Sink interface to receive streaming agent events and feed into the IM throttler.
#[async_trait]
pub trait BotStreamSink: Send + Sync {
    async fn on_text_delta(&self, text: &str);
    async fn on_thinking_delta(&self, thinking: &str);
    async fn on_tool_call_start(&self, name: &str, args_preview: &str);
    async fn on_tool_call_done(&self, name: &str);
}

pub struct ThrottlerSink {
    throttler: Arc<StreamThrottler>,
}

impl ThrottlerSink {
    pub fn new(throttler: Arc<StreamThrottler>) -> Self {
        Self { throttler }
    }
}

#[async_trait]
impl BotStreamSink for ThrottlerSink {
    async fn on_text_delta(&self, text: &str) {
        self.throttler.push_text_delta(text).await;
    }

    async fn on_thinking_delta(&self, thinking: &str) {
        self.throttler.push_thinking_delta(thinking).await;
    }

    async fn on_tool_call_start(&self, name: &str, args_preview: &str) {
        let status = if args_preview.is_empty() {
            format!("正在执行工具: `{}`", name)
        } else {
            format!("正在执行工具: `{}` ({})", name, args_preview)
        };
        self.throttler.set_tool_status(Some(status)).await;
    }

    async fn on_tool_call_done(&self, _name: &str) {
        self.throttler.set_tool_status(None).await;
    }
}

/// Agent execution hook invoked by the Bot Gateway to run turns against one-core.
#[async_trait(?Send)]
pub trait BotAgentExecutor: Send + Sync {
    /// Ensure an isolated runtime exists before a turn is spawned. Keeping
    /// cold-start construction outside `tokio::spawn` avoids carrying the
    /// resource-loader's deeply nested future into the Send task type.
    async fn ensure_session(&self, _session_key: &BotSessionKey) -> Result<()> {
        Ok(())
    }

    async fn execute_prompt(
        &self,
        prompt: String,
        attachments: Vec<crate::events::BotAttachment>,
        session_key: &BotSessionKey,
        model_override: Option<String>,
        tool_gate: Arc<dyn ToolGate>,
        sink: Arc<dyn BotStreamSink>,
    ) -> Result<()>;

    async fn reset_session(&self, _session_key: &BotSessionKey) -> Result<()> {
        Ok(())
    }

    /// Interrupt an active turn for this IM session. `/stop` is routed outside
    /// the normal per-session work guard so it stays responsive.
    async fn abort_session(&self, _session_key: &BotSessionKey) -> Result<bool> {
        Ok(false)
    }
}

/// Central Gateway orchestrating multi-channel bots.
pub struct BotGateway {
    config: BotConfig,
    adapters: Arc<RwLock<HashMap<String, Arc<dyn PlatformAdapter>>>>,
    session_mapper: SessionMapper,
    approval_manager: BotApprovalManager,
    notification_engine: NotificationEngine,
    executor: Arc<dyn BotAgentExecutor>,
    active_sessions: Arc<Mutex<HashSet<String>>>,
    session_limit: Arc<Semaphore>,
}

impl BotGateway {
    pub fn new(config: BotConfig, executor: Arc<dyn BotAgentExecutor>) -> Self {
        let default_workspace = config.security.workspace_root.clone();
        let default_model = config.default_model.clone();
        let session_mapper = SessionMapper::new(default_workspace, default_model);
        let approval_manager = BotApprovalManager::new();
        let notification_engine = NotificationEngine::new();

        let session_limit = config.max_concurrent_sessions.max(1);
        Self {
            config,
            adapters: Arc::new(RwLock::new(HashMap::new())),
            session_mapper,
            approval_manager,
            notification_engine,
            executor,
            active_sessions: Arc::new(Mutex::new(HashSet::new())),
            session_limit: Arc::new(Semaphore::new(session_limit)),
        }
    }

    pub fn notification_engine(&self) -> NotificationEngine {
        self.notification_engine.clone()
    }

    pub fn approval_manager(&self) -> BotApprovalManager {
        self.approval_manager.clone()
    }

    pub async fn register_adapter(&self, adapter: Arc<dyn PlatformAdapter>) {
        let platform_id = adapter.platform_id().to_string();
        self.notification_engine
            .register_adapter(adapter.clone())
            .await;
        let mut map = self.adapters.write().await;
        map.insert(platform_id, adapter);
    }

    /// Run the main gateway loop listening for inbound events.
    pub async fn run(&self) -> Result<()> {
        let (tx, mut rx) = mpsc::channel::<BotInboundEvent>(100);

        // Start listening on all registered adapters
        {
            let adapters_guard = self.adapters.read().await;
            for (name, adapter) in adapters_guard.iter() {
                info!("Starting platform adapter: {}", name);
                if let Err(e) = adapter.start_listening(tx.clone()).await {
                    warn!("Failed to start adapter {}: {}", name, e);
                }
            }
        }

        info!("🤖 One Bot Gateway is running and ready to process messages.");

        // `AppRuntime::build` contains resource-loading futures that are not
        // Send. Hermes-style per-session runtimes therefore execute on this
        // dedicated local task set, while platform pollers remain regular
        // Tokio tasks and communicate through the bounded event channel.
        let local = LocalSet::new();
        local
            .run_until(async {
                while let Some(event) = rx.recv().await {
                    match event {
                        BotInboundEvent::Message(msg) => {
                            self.handle_inbound_message(msg).await;
                        }
                        BotInboundEvent::ApprovalDecision(decision) => {
                            self.handle_approval_decision(decision).await;
                        }
                    }
                }
            })
            .await;

        Ok(())
    }

    async fn handle_approval_decision(&self, decision: ApprovalDecision) {
        let request_id = decision.request_id.clone();
        let user_id = decision.user_id.clone();
        let approved = decision.approved;
        let accepted = self.approval_manager.submit_decision(decision).await;
        if accepted {
            info!(
                request_id = %request_id,
                user_id = %user_id,
                approved,
                "accepted bot approval decision"
            );
        } else {
            warn!(
                request_id = %request_id,
                user_id = %user_id,
                "ignored unknown or unauthorized bot approval decision"
            );
        }
    }

    async fn handle_inbound_message(&self, msg: BotInboundMessage) {
        let platform = msg.session_key.platform.clone();
        let target = msg.target.clone();

        // 1. ACL Check
        if !self.config.security.is_channel_allowed(&target.channel_id) {
            debug!(channel_id = %target.channel_id, "ignored bot message from unauthorized channel");
            return;
        }
        if !self
            .config
            .security
            .is_user_allowed(&msg.session_key.user_id)
        {
            debug!(user_id = %msg.session_key.user_id, "ignored bot message from unauthorized user");
            return;
        }

        let adapter = {
            let guard = self.adapters.read().await;
            guard.get(&platform).cloned()
        };

        let adapter = match adapter {
            Some(a) => a,
            None => {
                warn!("No adapter found for platform {}", platform);
                return;
            }
        };

        let text = msg.text.trim();

        // 2. Handle built-in Slash commands
        let first_word = text.split_whitespace().next().unwrap_or("");
        let base_cmd = first_word.split('@').next().unwrap_or(first_word);
        let cmd_args = text.strip_prefix(first_word).unwrap_or("").trim();

        if base_cmd == "/start" || base_cmd == "/help" {
            let help_msg = BotOutboundMessage {
                text: "👋 你好！我是 **One Agent** (Grok/Hermes 驱动的自主编码与协作助手)。\n\n\
                常用指令：\n\
                • `/new` 或 `/reset` 或 `/clear` - 开启新对话，清空上下文\n\
                • `/model <name>` - 切换底座模型 (如 `grok-beta`, `claude-3-7-sonnet`)\n\
                • `/status` - 查看当前状态与工作区\n\
                • 直接发送任何问题或编程任务即可开始！"
                    .to_string(),
                thinking: None,
                tool_status: None,
                buttons: Vec::new(),
                is_final: true,
            };
            let _ = adapter.send_message(&target, &help_msg).await;
            return;
        }

        if base_cmd == "/stop" || base_cmd == "/cancel" {
            let stopped = self
                .executor
                .abort_session(&msg.session_key)
                .await
                .unwrap_or(false);
            let reply = BotOutboundMessage {
                text: if stopped {
                    "⏹️ 已请求停止当前任务。".to_string()
                } else {
                    "当前会话没有正在执行的任务。".to_string()
                },
                thinking: None,
                tool_status: None,
                buttons: Vec::new(),
                is_final: true,
            };
            let _ = adapter.send_message(&target, &reply).await;
            return;
        }

        if base_cmd == "/new" || base_cmd == "/reset" || base_cmd == "/clear" {
            let _ = self.executor.abort_session(&msg.session_key).await;
            self.session_mapper.reset_session(&msg.session_key).await;
            if let Err(err) = self.executor.reset_session(&msg.session_key).await {
                error!(session = %msg.session_key.to_string_key(), error = %err, "failed to reset bot session");
                let reply = BotOutboundMessage {
                    text: "❌ 无法重置会话，请稍后重试。".to_string(),
                    thinking: None,
                    tool_status: None,
                    buttons: Vec::new(),
                    is_final: true,
                };
                let _ = adapter.send_message(&target, &reply).await;
                return;
            }
            let reply = BotOutboundMessage {
                text: "✨ 会话已重置，历史上下文已清空。您可以开始新的任务。".to_string(),
                thinking: None,
                tool_status: None,
                buttons: Vec::new(),
                is_final: true,
            };
            let _ = adapter.send_message(&target, &reply).await;
            return;
        }

        if base_cmd == "/model" {
            if !self.config.security.is_user_admin(&msg.session_key.user_id) {
                let reply = BotOutboundMessage {
                    text: "🔒 只有管理员可以切换模型。".to_string(),
                    thinking: None,
                    tool_status: None,
                    buttons: Vec::new(),
                    is_final: true,
                };
                let _ = adapter.send_message(&target, &reply).await;
                return;
            }
            if !cmd_args.is_empty() {
                let model_name = cmd_args.to_string();
                self.session_mapper
                    .set_model(&msg.session_key, model_name.clone())
                    .await;
                let reply = BotOutboundMessage {
                    text: format!("🔄 当前会话模型已切换为: `{}`", model_name),
                    thinking: None,
                    tool_status: None,
                    buttons: Vec::new(),
                    is_final: true,
                };
                let _ = adapter.send_message(&target, &reply).await;
            } else {
                let current_model = self.session_mapper.get_model(&msg.session_key).await;
                let reply = BotOutboundMessage {
                    text: format!("🤖 当前模型: `{}`\n用法: `/model <模型名>` (例如 `/model grok-beta` 或 `/model claude-3-7-sonnet`)", current_model),
                    thinking: None,
                    tool_status: None,
                    buttons: Vec::new(),
                    is_final: true,
                };
                let _ = adapter.send_message(&target, &reply).await;
            }
            return;
        }

        if base_cmd == "/status" {
            let current_model = self.session_mapper.get_model(&msg.session_key).await;
            let reply = BotOutboundMessage {
                text: format!(
                    "📊 **One Agent 运行状态**\n\n\
                    • **平台**: `{}`\n\
                    • **会话标识**: `{}`\n\
                    • **当前模型**: `{}`\n\
                    • **工作区根目录**: `{}`\n\
                    • **Bash HITL 审批**: `{}`",
                    platform,
                    msg.session_key.to_string_key(),
                    current_model,
                    self.config.security.workspace_root.display(),
                    if self.config.security.require_approval_for_bash {
                        "开启 (高危需按钮确认)"
                    } else {
                        "关闭"
                    }
                ),
                thinking: None,
                tool_status: None,
                buttons: Vec::new(),
                is_final: true,
            };
            let _ = adapter.send_message(&target, &reply).await;
            return;
        }

        // 3. Normal Agent Prompt Execution. A session only has one active
        // turn, so its agent history and streaming listener cannot cross-talk.
        let session_id = msg.session_key.to_string_key();
        {
            let mut active = self.active_sessions.lock().await;
            if !active.insert(session_id.clone()) {
                let reply = BotOutboundMessage {
                    text: "⏳ 当前会话仍在执行。发送 `/stop` 取消，或等待完成后继续。".to_string(),
                    thinking: None,
                    tool_status: None,
                    buttons: Vec::new(),
                    is_final: true,
                };
                let _ = adapter.send_message(&target, &reply).await;
                return;
            }
        }

        let permit = match self.session_limit.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.active_sessions.lock().await.remove(&session_id);
                let reply = BotOutboundMessage {
                    text: "⏳ Bot 正在处理过多会话，请稍后重试。".to_string(),
                    thinking: None,
                    tool_status: None,
                    buttons: Vec::new(),
                    is_final: true,
                };
                let _ = adapter.send_message(&target, &reply).await;
                return;
            }
        };

        let edit_interval = self.config.telegram.stream_edit_interval_ms;
        let throttler = Arc::new(StreamThrottler::new(
            adapter.clone(),
            target.clone(),
            edit_interval,
        ));
        let sink: Arc<dyn BotStreamSink> = Arc::new(ThrottlerSink::new(throttler.clone()));

        let tool_gate = Arc::new(BotToolGate::new(
            self.approval_manager.clone(),
            adapter.clone(),
            target.clone(),
            msg.session_key.clone(),
            self.config.security.require_approval_for_bash,
        ));

        if let Err(err) = self.executor.ensure_session(&msg.session_key).await {
            error!(
                session = %msg.session_key.to_string_key(),
                error = %err,
                "failed to initialize bot session"
            );
            self.active_sessions.lock().await.remove(&session_id);
            let reply = BotOutboundMessage {
                text: "❌ 无法初始化会话，请稍后重试。".to_string(),
                thinking: None,
                tool_status: None,
                buttons: Vec::new(),
                is_final: true,
            };
            let _ = adapter.send_message(&target, &reply).await;
            return;
        }

        let current_model = self.session_mapper.get_model(&msg.session_key).await;
        let prompt = msg.text.clone();
        let attachments = msg.attachments.clone();
        let session_key = msg.session_key.clone();
        let executor = self.executor.clone();
        let active_sessions = self.active_sessions.clone();

        // Send typing indicator before the potentially expensive session build.
        let _ = adapter.send_typing(&target).await;

        tokio::task::spawn_local(async move {
            // Keep the owned semaphore permit until the agent turn and final
            // stream flush both finish.
            let _permit = permit;
            if let Err(err) = executor
                .execute_prompt(
                    prompt,
                    attachments,
                    &session_key,
                    Some(current_model),
                    tool_gate,
                    sink,
                )
                .await
            {
                error!(
                    "Agent execution error for {}: {}",
                    session_key.to_string_key(),
                    err
                );
                throttler
                    .push_text_delta(&format!("\n\n❌ 执行出错: {}", err))
                    .await;
            }
            throttler.finish().await;
            active_sessions.lock().await.remove(&session_id);
        });
    }
}
