//! Bot Mode for One (Hermes / Grok multi-channel bot gateway).

use std::sync::Arc;

use async_trait::async_trait;
use one_bot::{
    BotAgentExecutor, BotConfig, BotGateway, BotSessionKey, BotStreamSink, DiscordAdapter,
    FeishuAdapter, TelegramAdapter,
};
use one_core::events::AgentEvent;
use one_core::ToolGate;
use tokio::sync::Mutex;
use tracing::info;

use crate::cli::{BotCli, Cli};
use crate::provider::ProviderSet;
use crate::runtime::AppRuntime;

/// Bridge executor running One AgentLoop on behalf of the BotGateway.
pub struct CliBotExecutor {
    cli: Cli,
    providers: Arc<Mutex<ProviderSet>>,
    runtime: Arc<Mutex<AppRuntime>>,
}

impl CliBotExecutor {
    pub async fn new(cli: Cli) -> Result<Self, Box<dyn std::error::Error>> {
        let providers = ProviderSet::build(&cli)?;
        let runtime = AppRuntime::build(&cli).await?;

        Ok(Self {
            cli,
            providers: Arc::new(Mutex::new(providers)),
            runtime: Arc::new(Mutex::new(runtime)),
        })
    }
}

#[async_trait]
impl BotAgentExecutor for CliBotExecutor {
    async fn execute_prompt(
        &self,
        prompt: String,
        attachments: Vec<one_bot::BotAttachment>,
        _session_key: &BotSessionKey,
        model_override: Option<String>,
        tool_gate: Arc<dyn ToolGate>,
        sink: Arc<dyn BotStreamSink>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut runtime = self.runtime.lock().await;

        // 1. Process attachments: separate vision images vs other documents
        let mut images: Vec<(String, String)> = Vec::new();
        let mut file_notices = Vec::new();

        for att in attachments {
            if att.is_image {
                images.push((att.mime_type, att.local_path));
            } else {
                let fname = att.file_name.unwrap_or_else(|| "unnamed_file".to_string());
                file_notices.push(format!(
                    "[用户上传了文件: `{}` (本地已保存至 `{}`)]",
                    fname, att.local_path
                ));
            }
        }

        let effective_prompt = if !file_notices.is_empty() {
            format!("{}\n\n{}", file_notices.join("\n"), prompt)
        } else {
            prompt
        };

        // 2. Subscribe streaming event listener to forward events to IM Sink
        {
            let mut agent = runtime.agent.lock().await;
            agent.set_tool_gate(Some(tool_gate));

            let sink_clone = sink.clone();
            agent.subscribe(Box::new(move |event: &AgentEvent| {
                let sink_inner = sink_clone.clone();
                match event {
                    AgentEvent::TextDelta { delta } => {
                        let d = delta.clone();
                        tokio::spawn(async move {
                            sink_inner.on_text_delta(&d).await;
                        });
                    }
                    AgentEvent::ThinkingDelta { delta } => {
                        let d = delta.clone();
                        tokio::spawn(async move {
                            sink_inner.on_thinking_delta(&d).await;
                        });
                    }
                    AgentEvent::ToolExecutionStart { tool_call } => {
                        let name = tool_call.name.clone();
                        let args_preview = tool_call.arguments.to_string();
                        let preview = if args_preview.len() > 60 {
                            format!("{}...", &args_preview[..57])
                        } else {
                            args_preview
                        };
                        tokio::spawn(async move {
                            sink_inner.on_tool_call_start(&name, &preview).await;
                        });
                    }
                    AgentEvent::ToolExecutionEnd { tool_call, .. } => {
                        let name = tool_call.name.clone();
                        tokio::spawn(async move {
                            sink_inner.on_tool_call_done(&name).await;
                        });
                    }
                    _ => {}
                }
            }));
        }

        // 3. Resolve LLM provider (with model override if specified)
        let provider = {
            let providers_guard = self.providers.lock().await;
            if let Some(model) = model_override {
                if !model.is_empty() {
                    let mut cli_copy = self.cli.clone();
                    cli_copy.model = Some(model);
                    if let Ok(custom_set) = ProviderSet::build(&cli_copy) {
                        custom_set.as_arc()
                    } else {
                        providers_guard.as_arc()
                    }
                } else {
                    providers_guard.as_arc()
                }
            } else {
                providers_guard.as_arc()
            }
        };

        // 4. Run prompt loop (with images if any)
        if images.is_empty() {
            runtime
                .prompt(provider.as_ref(), &effective_prompt)
                .await
                .map_err(|e| format!("Agent execution failed: {}", e))?;
        } else {
            runtime
                .prompt_with_images(provider.as_ref(), &effective_prompt, images)
                .await
                .map_err(|e| format!("Agent execution failed: {}", e))?;
        }

        Ok(())
    }

    async fn reset_session(
        &self,
        _session_key: &BotSessionKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut runtime = self.runtime.lock().await;
        let _ = runtime.new_session().await;
        Ok(())
    }
}

/// Run the Multi-Channel Bot Gateway server.
pub async fn run_bot(cli: Cli, bot_cli: Option<BotCli>) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Load default config (from bot.toml / ~/.one/bot.toml / ENV)
    let mut config = BotConfig::load_default();

    // 2. Load explicit config from file if provided via CLI flag (-C / --config)
    if let Some(b) = &bot_cli {
        if let Some(config_path) = &b.config {
            if config_path.exists() {
                match BotConfig::load_from_path(config_path) {
                    Ok(cfg) => {
                        info!("Loaded bot configuration from: {}", config_path.display());
                        config.merge(cfg);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to parse config file {}: {}",
                            config_path.display(),
                            e
                        );
                    }
                }
            }
        }

        // 3. Override tokens from CLI flags
        if let Some(tg) = &b.telegram_token {
            config.telegram.enabled = true;
            config.telegram.bot_token = tg.clone();
        }
        if let Some(dc) = &b.discord_token {
            config.discord.enabled = true;
            config.discord.bot_token = dc.clone();
        }
        if let Some(app_id) = &b.feishu_app_id {
            config.feishu.enabled = true;
            config.feishu.app_id = app_id.clone();
        }
        if let Some(app_secret) = &b.feishu_app_secret {
            config.feishu.app_secret = app_secret.clone();
        }
    }

    config.security.workspace_root = cli.cwd.clone();

    let executor = Arc::new(CliBotExecutor::new(cli.clone()).await?);
    let gateway = BotGateway::new(config.clone(), executor);

    // Register active platform adapters
    if config.telegram.enabled && !config.telegram.bot_token.is_empty() {
        info!("Registering Telegram Adapter...");
        gateway
            .register_adapter(Arc::new(TelegramAdapter::new(config.telegram.clone())))
            .await;
    }

    if config.discord.enabled && !config.discord.bot_token.is_empty() {
        info!("Registering Discord Adapter...");
        gateway
            .register_adapter(Arc::new(DiscordAdapter::new(config.discord.clone())))
            .await;
    }

    if config.feishu.enabled && !config.feishu.app_id.is_empty() {
        info!("Registering Feishu Adapter...");
        gateway
            .register_adapter(Arc::new(FeishuAdapter::new(config.feishu.clone())))
            .await;
    }

    // Register dynamic subprocess connectors from config file
    for conn in &config.connectors {
        if let Err(e) =
            one_bot::ConnectorPluginManager::register_connector_config(&gateway, conn).await
        {
            tracing::warn!("Failed to load connector '{}': {}", conn.command, e);
        }
    }

    // Register dynamic subprocess connectors from CLI flags & directories
    if let Some(b) = &bot_cli {
        // 1. Explicit --connector flags
        for cmd in &b.connectors {
            if let Err(e) =
                one_bot::ConnectorPluginManager::register_connector_command(&gateway, cmd).await
            {
                tracing::warn!("Failed to load dynamic connector '{}': {}", cmd, e);
            }
        }

        // 2. Custom connectors directory from CLI
        if let Some(dir) = &b.connectors_dir {
            let loaded = one_bot::ConnectorPluginManager::load_from_dir(&gateway, dir).await;
            info!(
                "Loaded {} connector(s) from custom directory: {}",
                loaded,
                dir.display()
            );
        }
    }

    // 3. Connectors directory configured in config file
    if let Some(dir) = &config.connectors_dir {
        let loaded = one_bot::ConnectorPluginManager::load_from_dir(&gateway, dir).await;
        if loaded > 0 {
            info!(
                "Loaded {} dynamic connector(s) from config directory: {}",
                loaded,
                dir.display()
            );
        }
    }

    // 4. Default ~/.one/connectors directory
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        let default_dir = home.join(".one").join("connectors");
        if default_dir.exists() {
            let loaded =
                one_bot::ConnectorPluginManager::load_from_dir(&gateway, &default_dir).await;
            if loaded > 0 {
                info!(
                    "Loaded {} dynamic connector(s) from {}",
                    loaded,
                    default_dir.display()
                );
            }
        }
    }

    info!("Starting Bot Gateway...");
    gateway
        .run()
        .await
        .map_err(|e| format!("Bot gateway error: {}", e))?;

    Ok(())
}
