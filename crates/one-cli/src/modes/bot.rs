//! Bot Mode for One (Hermes / Grok multi-channel bot gateway).
//!
//! Each IM session owns an independent [`AppRuntime`]. This is intentional:
//! an Agent contains mutable conversation history, tool gates, listeners, and
//! cancellation state, so sharing one runtime across chat users leaks context
//! and serializes otherwise unrelated conversations.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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

use crate::cli::{BotCli, Cli, RunMode};
use crate::provider::ProviderSet;
use crate::runtime::AppRuntime;

type SharedRuntime = Arc<Mutex<AppRuntime>>;

/// Bridge executor running a separate One AgentLoop for every bot session.
pub struct CliBotExecutor {
    cli: Cli,
    providers: Arc<Mutex<ProviderSet>>,
    runtimes: Arc<Mutex<HashMap<String, SharedRuntime>>>,
    active_turns: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl CliBotExecutor {
    pub async fn new(cli: Cli) -> Result<Self, Box<dyn std::error::Error>> {
        let providers = ProviderSet::build(&cli)?;
        Ok(Self {
            cli,
            providers: Arc::new(Mutex::new(providers)),
            runtimes: Arc::new(Mutex::new(HashMap::new())),
            active_turns: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    async fn runtime_for(
        &self,
        session_key: &BotSessionKey,
    ) -> Result<SharedRuntime, Box<dyn std::error::Error + Send + Sync>> {
        let key = session_key.to_string_key();
        if let Some(runtime) = self.runtimes.lock().await.get(&key).cloned() {
            return Ok(runtime);
        }

        // Bot mode does not need a TUI shell or an initial implicit session.
        // `new_session` below creates the durable session after the runtime is
        // inserted into its actual platform/user route.
        let mut runtime_cli = self.cli.clone();
        runtime_cli.mode = RunMode::Print;
        runtime_cli.print = None;
        let mut runtime = AppRuntime::build(&runtime_cli)
            .await
            .map_err(|e| format!("failed to initialize bot session runtime: {e}"))?;
        runtime
            .new_session()
            .await
            .map_err(|e| format!("failed to create bot session: {e}"))?;
        let runtime = Arc::new(Mutex::new(runtime));

        let mut runtimes = self.runtimes.lock().await;
        // The gateway's per-session active-turn guard prevents a normal race,
        // but retaining the pre-existing runtime makes this safe if an adapter
        // dispatches duplicate events during startup.
        Ok(runtimes
            .entry(key)
            .or_insert_with(|| runtime.clone())
            .clone())
    }

    async fn provider_for(
        &self,
        model_override: Option<String>,
    ) -> Arc<dyn one_core::agent::LlmProvider> {
        if let Some(model) = model_override.filter(|model| !model.is_empty()) {
            let mut cli_copy = self.cli.clone();
            cli_copy.model = Some(model);
            if let Ok(provider_set) = ProviderSet::build(&cli_copy) {
                return provider_set.as_arc();
            }
        }
        self.providers.lock().await.as_arc()
    }
}

#[async_trait(?Send)]
impl BotAgentExecutor for CliBotExecutor {
    async fn ensure_session(
        &self,
        session_key: &BotSessionKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = self.runtime_for(session_key).await?;
        Ok(())
    }

    async fn execute_prompt(
        &self,
        prompt: String,
        attachments: Vec<one_bot::BotAttachment>,
        session_key: &BotSessionKey,
        model_override: Option<String>,
        tool_gate: Arc<dyn ToolGate>,
        sink: Arc<dyn BotStreamSink>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let runtime = self
            .runtimes
            .lock()
            .await
            .get(&session_key.to_string_key())
            .cloned()
            .ok_or_else(|| {
                format!(
                    "bot session {} was not initialized before execution",
                    session_key.to_string_key()
                )
            })?;
        let mut runtime = runtime.lock().await;

        // Separate vision images from ordinary documents. The latter remain
        // available in the session workspace for tools to inspect.
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
        let effective_prompt = if file_notices.is_empty() {
            prompt
        } else {
            format!("{}\n\n{}", file_notices.join("\n"), prompt)
        };

        // A runtime belongs to exactly one session, so clearing before every
        // turn avoids retaining stale stream closures while preserving its
        // conversation history. Listener callbacks intentionally only enqueue
        // to the sink; they never hold the Agent lock.
        {
            let mut agent = runtime.agent.lock().await;
            agent.set_tool_gate(Some(tool_gate));
            agent.clear_listeners();

            let sink_clone = sink.clone();
            agent.subscribe(Box::new(move |event: &AgentEvent| {
                let sink = sink_clone.clone();
                match event {
                    AgentEvent::TextDelta { delta } => {
                        let delta = delta.clone();
                        tokio::spawn(async move { sink.on_text_delta(&delta).await });
                    }
                    AgentEvent::ThinkingDelta { delta } => {
                        let delta = delta.clone();
                        tokio::spawn(async move { sink.on_thinking_delta(&delta).await });
                    }
                    AgentEvent::ToolExecutionStart { tool_call } => {
                        let name = tool_call.name.clone();
                        let args = tool_call.arguments.to_string();
                        let preview = if args.len() > 60 {
                            format!("{}...", &args[..57])
                        } else {
                            args
                        };
                        tokio::spawn(async move { sink.on_tool_call_start(&name, &preview).await });
                    }
                    AgentEvent::ToolExecutionEnd { tool_call, .. } => {
                        let name = tool_call.name.clone();
                        tokio::spawn(async move { sink.on_tool_call_done(&name).await });
                    }
                    _ => {}
                }
            }));
        }

        let provider = self.provider_for(model_override).await;
        let abort_flag = runtime.abort_handle();
        abort_flag.store(false, Ordering::Relaxed);
        self.active_turns
            .lock()
            .await
            .insert(session_key.to_string_key(), abort_flag);

        let result = if images.is_empty() {
            runtime.prompt(provider.as_ref(), &effective_prompt).await
        } else {
            runtime
                .prompt_with_images(provider.as_ref(), &effective_prompt, images)
                .await
        };

        // Do not retain an outbound sink after this turn. Apart from avoiding
        // leaks, this guarantees a future turn cannot render into an earlier
        // platform message.
        runtime.agent.lock().await.clear_listeners();
        self.active_turns
            .lock()
            .await
            .remove(&session_key.to_string_key());
        result.map(|_| ()).map_err(|e| {
            format!(
                "Agent execution failed for {}: {e}",
                session_key.to_string_key()
            )
            .into()
        })
    }

    async fn reset_session(
        &self,
        session_key: &BotSessionKey,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let runtime = self.runtime_for(session_key).await?;
        let mut runtime = runtime.lock().await;
        runtime
            .new_session()
            .await
            .map_err(|e| format!("failed to reset bot session: {e}").into())
    }

    async fn abort_session(
        &self,
        session_key: &BotSessionKey,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let abort_flag = self
            .active_turns
            .lock()
            .await
            .get(&session_key.to_string_key())
            .cloned();
        let Some(abort_flag) = abort_flag else {
            return Ok(false);
        };
        if abort_flag.swap(true, Ordering::Relaxed) {
            return Ok(false);
        }
        Ok(true)
    }
}

/// Run the Multi-Channel Bot Gateway server.
pub async fn run_bot(cli: Cli, bot_cli: Option<BotCli>) -> Result<(), Box<dyn std::error::Error>> {
    // 0. Check if `one bot init` was requested
    let is_init = bot_cli.as_ref().map_or(false, |b| {
        matches!(b.action, Some(crate::cli::BotAction::Init))
    });

    if is_init {
        let saved_path = crate::modes::bot_setup::run_interactive_setup()?;
        println!(
            "Setup complete. You can start the gateway with:\n  one bot -C {}\n",
            saved_path.display()
        );
        return Ok(());
    }

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

    // 4. Reject configurations that advertise an adapter unavailable in this build.
    config
        .validate()
        .map_err(|err| format!("invalid bot configuration: {err}"))?;

    // 5. If no adapters are configured and we are in an interactive TTY, offer setup wizard
    if crate::modes::bot_setup::should_offer_interactive_setup(&config) {
        println!(
            "\nℹ️  No active chat platform (Telegram, Discord, Feishu, or connector) configured."
        );
        print!("Would you like to run the interactive setup wizard now? [Y/n]: ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_ok() {
            let trimmed = answer.trim().to_lowercase();
            if trimmed.is_empty() || trimmed == "y" || trimmed == "yes" {
                let saved_path = crate::modes::bot_setup::run_interactive_setup()?;
                if let Ok(cfg) = BotConfig::load_from_path(&saved_path) {
                    config.merge(cfg);
                }
            }
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
        for cmd in &b.connectors {
            if let Err(e) =
                one_bot::ConnectorPluginManager::register_connector_command(&gateway, cmd).await
            {
                tracing::warn!("Failed to load dynamic connector '{}': {}", cmd, e);
            }
        }

        if let Some(dir) = &b.connectors_dir {
            let loaded = one_bot::ConnectorPluginManager::load_from_dir(&gateway, dir).await;
            info!(
                "Loaded {} connector(s) from custom directory: {}",
                loaded,
                dir.display()
            );
        }
    }

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
