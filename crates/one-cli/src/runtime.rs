use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use one_core::agent::{
    Agent, AgentConfig, CompletionRequest, LlmProvider, ThinkingLevel,
};
use one_core::compaction::{
    compact_messages, is_context_overflow_error, should_compact, split_for_compaction,
    summarization_prompt, CompactionConfig,
};
use one_core::error::OneError;
use one_core::events::AgentEvent;
use one_core::message::AgentMessage;
use one_core::tool::Tool;
use one_ext::{discover_extensions, ExtensionContext, ExtensionEvent, ExtensionRuntime};
use one_resources::ResourceLoader;
use one_session::{agent_dir, SessionInfo, SessionManager};
use one_tools::{coding_tools_with_approve, read_only_tools};

use crate::cli::Cli;

pub struct AppRuntime {
    pub agent: Arc<tokio::sync::Mutex<Agent>>,
    abort_flag: Arc<AtomicBool>,
    steering_queue: Arc<std::sync::Mutex<Vec<String>>>,
    followup_queue: Arc<std::sync::Mutex<Vec<String>>>,
    pub session: Option<SessionManager>,
    pub extensions: ExtensionRuntime,
    pub resources: ResourceLoader,
    pub auto_approve: bool,
    pub cwd: PathBuf,
    read_only: bool,
}

impl AppRuntime {
    pub async fn build(cli: &Cli) -> Result<Self, Box<dyn std::error::Error>> {
        let cwd = cli.cwd.canonicalize().unwrap_or_else(|_| cli.cwd.clone());
        let agent_dir = agent_dir();

        let resources = ResourceLoader::discover(&cwd, &agent_dir).await?;
        let extensions = discover_extensions(&agent_dir).await?;
        extensions
            .load_all(&ExtensionContext {
                cwd: &cwd,
                session_file: None,
            })
            .await?;

        let mut tools: Vec<Arc<dyn Tool>> = if cli.read_only {
            read_only_tools(cwd.clone())
        } else {
            coding_tools_with_approve(cwd.clone(), cli.auto_approve)
        };
        tools.extend(extensions.tools());

        let system_prompt =
            resources.build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);
        let mut agent = Agent::new(
            AgentConfig {
                system_prompt,
                max_turns: 32,
                thinking_level: ThinkingLevel::Off,
            },
            tools,
        );

        let mut session = if cli.no_session {
            None
        } else if let Some(path) = &cli.session {
            Some(SessionManager::open(path).await?)
        } else if cli.resume {
            // Interactive resume: open most recent if any; TUI will offer picker.
            match SessionManager::continue_recent(&cwd).await {
                Ok(session) => Some(session),
                Err(_) => Some(SessionManager::create(&cwd).await?),
            }
        } else if cli.r#continue {
            match SessionManager::continue_recent(&cwd).await {
                Ok(session) => Some(session),
                Err(_) => Some(SessionManager::create(&cwd).await?),
            }
        } else if matches!(cli.mode, crate::cli::RunMode::Interactive) && cli.print.is_none() {
            Some(SessionManager::create(&cwd).await?)
        } else {
            None
        };

        if let Some(session) = &session {
            session.load_messages_into(&mut agent.messages);
            // Restore thinking level from session if present.
            if let Some(level) = session.build_session_context().thinking_level {
                if let Some(tl) = ThinkingLevel::parse(&level) {
                    agent.config.thinking_level = tl;
                }
            }
            load_extension_state(&extensions, session);
        }
        if let (Some(session), Some(name)) = (&mut session, &cli.name) {
            session.append_session_info(name).await?;
        }

        let steering_queue = agent.steering_queue_handle();
        let followup_queue = agent.followup_queue_handle();
        let abort_flag = agent.abort_handle();

        Ok(Self {
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            abort_flag,
            steering_queue,
            followup_queue,
            session,
            extensions,
            resources,
            auto_approve: cli.auto_approve,
            cwd,
            read_only: cli.read_only,
        })
    }

    pub async fn subscribe_printer(&mut self, json: bool) {
        let mut agent = self.agent.lock().await;
        agent.subscribe(Box::new(move |event: &AgentEvent| match event {
            AgentEvent::TextDelta { delta } if !json => print!("{delta}"),
            AgentEvent::TextDelta { delta } if json => {
                let line = serde_json::json!({"type":"text_delta","delta":delta});
                println!("{line}");
            }
            AgentEvent::ToolExecutionStart { tool_call } if !json => {
                eprintln!("\n[tool] {}({})", tool_call.name, tool_call.arguments);
            }
            AgentEvent::ToolExecutionStart { tool_call } if json => {
                let line = serde_json::json!({
                    "type":"tool_start",
                    "name": tool_call.name,
                    "arguments": tool_call.arguments,
                });
                println!("{line}");
            }
            AgentEvent::ToolExecutionEnd { is_error, .. } if !json && *is_error => {
                eprintln!("[tool] error");
            }
            AgentEvent::ToolExecutionEnd {
                tool_call,
                is_error,
                ..
            } if json => {
                let line = serde_json::json!({
                    "type":"tool_end",
                    "name": tool_call.name,
                    "is_error": is_error,
                });
                println!("{line}");
            }
            AgentEvent::AgentEnd { new_messages } if json => {
                let line = serde_json::json!({
                    "type":"agent_end",
                    "messages": new_messages.len(),
                });
                println!("{line}");
            }
            _ => {}
        }));
    }

    pub async fn subscribe_collector(&mut self, events: Arc<Mutex<Vec<AgentEvent>>>) {
        let mut agent = self.agent.lock().await;
        agent.clear_listeners();
        agent.subscribe(Box::new(move |event| {
            if let Ok(mut batch) = events.lock() {
                batch.push(event.clone());
            }
        }));
    }

    pub async fn prompt(
        &mut self,
        provider: &dyn LlmProvider,
        text: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let text = self.resources.resolve_prompt(text);
        self.maybe_compact(provider, false).await?;

        self.extensions
            .emit(&ExtensionEvent::AgentStart)
            .await?;

        let before = {
            let agent = self.agent.lock().await;
            agent.messages.len()
        };

        let output = match {
            let mut agent = self.agent.lock().await;
            agent.prompt(provider, &text).await
        } {
            Ok(out) => out,
            Err(err) if is_overflow_err(&err) => {
                // Force compact then retry once.
                drop(err);
                self.maybe_compact(provider, true).await?;
                let mut agent = self.agent.lock().await;
                // Drop the user message that was pushed before failure if present twice risk:
                // agent.prompt already pushed user text — on error messages may include it.
                // Retry by calling run() if last message is the user text, else re-prompt.
                if agent
                    .messages
                    .last()
                    .map(|m| matches!(m, AgentMessage::User(_)))
                    .unwrap_or(false)
                {
                    agent.run(provider).await?
                } else {
                    agent.prompt(provider, &text).await?
                }
            }
            Err(err) => return Err(err.into()),
        };

        if let Some(session) = &mut self.session {
            let messages = self.agent.lock().await.messages[before..].to_vec();
            for message in messages {
                session.append_message(message).await?;
            }
        }

        self.persist_extension_state().await?;
        self.extensions.emit(&ExtensionEvent::AgentEnd).await?;
        Ok(output)
    }

    /// Compact when over threshold, or when `force` (e.g. context overflow recovery).
    pub async fn maybe_compact(
        &mut self,
        provider: &dyn LlmProvider,
        force: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config = CompactionConfig::default();
        let messages = {
            let agent = self.agent.lock().await;
            agent.messages.clone()
        };

        if !force && !should_compact(&messages, &config) {
            return Ok(());
        }
        if split_for_compaction(&messages, &config).is_none() {
            return Ok(());
        }

        let tokens_before = one_core::estimate_tokens(&messages) as u64;
        let summary = self
            .summarize_for_compaction(provider, &messages, &config)
            .await;
        let (fallback, kept) = compact_messages(&messages, &config);
        let summary = summary.unwrap_or(fallback);
        if summary.is_empty() {
            return Ok(());
        }

        let first_kept = self
            .session
            .as_ref()
            .and_then(|s| s.get_leaf_id().map(|s| s.to_string()))
            .unwrap_or_else(|| "root".into());

        if let Some(session) = &mut self.session {
            session
                .append_compaction(&summary, first_kept, tokens_before)
                .await?;
        }

        let mut agent = self.agent.lock().await;
        agent.messages = kept;
        agent.messages.insert(
            0,
            AgentMessage::assistant_text(provider.name(), provider.model(), &summary),
        );
        Ok(())
    }

    async fn summarize_for_compaction(
        &self,
        provider: &dyn LlmProvider,
        messages: &[AgentMessage],
        config: &CompactionConfig,
    ) -> Option<String> {
        let (older, _) = split_for_compaction(messages, config)?;
        if older.is_empty() {
            return None;
        }
        let prompt = summarization_prompt(older, None);
        let request = CompletionRequest {
            system_prompt: "You summarize coding-agent conversations for context compaction."
                .into(),
            messages: vec![AgentMessage::user_text(prompt)],
            tools: Vec::new(),
            thinking_level: ThinkingLevel::Off,
        };
        match provider.complete(request).await {
            Ok(response) => {
                let text = one_core::agent::extract_text(&response.content);
                let text = text.trim().to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(format!(
                        "Earlier conversation summary ({} messages):\n{}",
                        older.len(),
                        text
                    ))
                }
            }
            Err(_) => None,
        }
    }

    pub async fn persist_extension_state(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let states = self.extensions.custom_states();
        if let Some(session) = &mut self.session {
            for (custom_type, data) in states {
                session.append_custom(custom_type, data).await?;
            }
        }
        Ok(())
    }

    pub async fn reload_extensions(&mut self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let agent_dir = agent_dir();
        // Reload resources (skills/prompts/AGENTS) + extensions.
        self.resources = ResourceLoader::discover(&self.cwd, &agent_dir).await?;
        self.extensions = discover_extensions(&agent_dir).await?;
        self.extensions
            .load_all(&ExtensionContext {
                cwd: &self.cwd,
                session_file: self.session_path().as_deref(),
            })
            .await?;

        let system_prompt = self
            .resources
            .build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);
        {
            let mut agent = self.agent.lock().await;
            agent.config.system_prompt = system_prompt;
            // Refresh tools from extensions (keep built-ins).
            let mut tools: Vec<Arc<dyn Tool>> = if self.read_only {
                read_only_tools(self.cwd.clone())
            } else {
                coding_tools_with_approve(self.cwd.clone(), self.auto_approve)
            };
            tools.extend(self.extensions.tools());
            // Agent tools field is private — re-create agent messages only via system prompt update.
            // tools are not publicly mutable; keep extension tools from initial load unless we add setter.
            let _ = tools;
        }
        Ok(self.extensions.names())
    }

    pub async fn new_session(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.session = Some(SessionManager::create(&self.cwd).await?);
        let mut agent = self.agent.lock().await;
        agent.messages.clear();
        Ok(())
    }

    pub async fn open_session_path(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let session = SessionManager::open(path).await?;
        {
            let mut agent = self.agent.lock().await;
            agent.messages.clear();
            session.load_messages_into(&mut agent.messages);
            if let Some(level) = session.build_session_context().thinking_level {
                if let Some(tl) = ThinkingLevel::parse(&level) {
                    agent.config.thinking_level = tl;
                }
            }
        }
        load_extension_state(&self.extensions, &session);
        self.session = Some(session);
        Ok(())
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>, Box<dyn std::error::Error>> {
        Ok(SessionManager::list(&self.cwd).await?)
    }

    pub async fn set_session_name(
        &mut self,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(session) = &mut self.session {
            session.append_session_info(name).await?;
        }
        Ok(())
    }

    pub async fn set_thinking_level(
        &mut self,
        level: ThinkingLevel,
    ) -> Result<(), Box<dyn std::error::Error>> {
        {
            let mut agent = self.agent.lock().await;
            agent.config.thinking_level = level;
        }
        if let Some(session) = &mut self.session {
            session
                .append_thinking_level_change(level.as_str())
                .await?;
        }
        Ok(())
    }

    pub async fn thinking_level(&self) -> ThinkingLevel {
        self.agent.lock().await.config.thinking_level
    }

    pub async fn estimated_tokens(&self) -> usize {
        let agent = self.agent.lock().await;
        one_core::estimate_tokens(&agent.messages)
    }

    pub fn session_path(&self) -> Option<PathBuf> {
        self.session
            .as_ref()
            .and_then(|session| session.session_file().map(|path| path.to_path_buf()))
    }

    pub fn session_summary_line(&self) -> String {
        match &self.session {
            None => "session: (ephemeral)".into(),
            Some(s) => {
                let path = s
                    .session_file()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(memory)".into());
                let name = s.session_name().unwrap_or_else(|| "—".into());
                let leaf = s.get_leaf_id().unwrap_or("root");
                format!(
                    "session {name} · {} msgs · leaf={leaf} · {path}",
                    s.message_count()
                )
            }
        }
    }

    pub fn steer(&self, text: impl Into<String>) {
        Agent::push_queue(&self.steering_queue, text);
    }

    pub fn follow_up(&self, text: impl Into<String>) {
        Agent::push_queue(&self.followup_queue, text);
    }

    pub fn steering_queue(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
        self.steering_queue.clone()
    }

    pub fn followup_queue(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
        self.followup_queue.clone()
    }

    pub fn clear_abort(&self) {
        self.abort_flag.store(false, Ordering::Relaxed);
    }

    pub fn abort(&self) {
        self.abort_flag.store(true, Ordering::Relaxed);
    }
}

fn is_overflow_err(err: &OneError) -> bool {
    match err {
        OneError::ContextOverflow(_) => true,
        OneError::Provider(msg) => is_context_overflow_error(msg),
        _ => false,
    }
}

fn load_extension_state(extensions: &ExtensionRuntime, session: &SessionManager) {
    for entry in session.entries() {
        if let one_session::SessionEntry::Custom {
            custom_type, data, ..
        } = entry
        {
            extensions.restore_custom(custom_type, data.clone());
        }
    }
}
