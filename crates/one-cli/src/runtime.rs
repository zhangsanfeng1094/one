use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use one_core::agent::{
    Agent, AgentConfig, CompletionRequest, LlmProvider, ThinkingLevel,
};
use one_core::compaction::{
    compact_messages, is_context_overflow_error, should_compact_tokens, split_for_compaction,
    summarization_prompt, tokens_for_compaction, CompactionConfig,
};
use one_core::error::OneError;
use one_core::events::AgentEvent;
use one_core::message::AgentMessage;
use one_core::tool::Tool;
use one_ext::{discover_extensions, ExtensionContext, ExtensionEvent, ExtensionRuntime};
use one_resources::{skill_allowlist_roots, ResourceLoader};
use one_session::{agent_dir, SessionInfo, SessionManager};
use one_tools::{
    coding_tools_with_options, plan_mode_system_overlay, plan_mode_tools_with_policy,
    read_only_tools_with_ask, AskUserHandler, BackgroundTaskRegistry, OsSandbox, PathPolicy,
    PermissionRules, PlanExitState, SandboxMode, ToolBuildOptions,
};
use uuid::Uuid;

use crate::approval::PermissionGate;
use crate::cli::{Cli, RunMode};
use crate::hitl::{HitlChannel, InteractiveAskUser};

/// Agent operating mode (Build/Act vs Plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    /// Full coding tools — implement changes.
    #[default]
    Act,
    /// Explore + write plan only; no shell / app edits.
    Plan,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentMode::Act => "act",
            AgentMode::Plan => "plan",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentMode::Act => "Build",
            AgentMode::Plan => "Plan",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "act" | "build" | "agent" => Some(AgentMode::Act),
            "plan" => Some(AgentMode::Plan),
            _ => None,
        }
    }
}

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
    /// Workspace path boundary + add-dir roots (rebuilt into tools on mode switch).
    path_policy: PathPolicy,
    /// Interactive `-r`: open session picker on TUI start.
    pub open_session_picker: bool,
    /// Current agent mode (Plan vs Act/Build).
    mode: AgentMode,
    /// Path of the active plan markdown file (set while/after plan mode).
    plan_path: Option<PathBuf>,
    /// Shared exit_plan_mode signal.
    plan_exit: Arc<Mutex<PlanExitState>>,
    /// Shared background bash registry (reused when leaving plan mode).
    bg_registry: Arc<BackgroundTaskRegistry>,
    /// Base system prompt without plan-mode overlay.
    base_system_prompt: String,
    /// Shared permission gate (interactive ask / fail-closed / auto).
    pub permission_gate: Arc<PermissionGate>,
    /// Human-in-the-loop channel for `ask_user` select prompts.
    pub hitl: HitlChannel,
    ask_user_handler: Arc<dyn AskUserHandler>,
    /// Active model context window (tokens). 0 = unknown → fallback compact threshold.
    context_window: usize,
}

impl AppRuntime {
    pub async fn build(cli: &Cli) -> Result<Self, Box<dyn std::error::Error>> {
        let cwd = cli.cwd.canonicalize().unwrap_or_else(|_| cli.cwd.clone());
        let agent_dir = agent_dir();

        let mut resources = ResourceLoader::discover(&cwd, &agent_dir).await?;
        let extensions = discover_extensions(&agent_dir).await?;
        extensions
            .load_all(&ExtensionContext {
                cwd: &cwd,
                session_file: None,
            })
            .await?;

        let user_settings = crate::settings::load();
        // Codex-style skills enable/disable (settings.skills_config).
        resources.apply_skills_config(&user_settings.skills_config_entries());
        let auto_approve =
            cli.auto_approve || user_settings.auto_approve.unwrap_or(false);

        // agentskills.io: allowlist skill dirs so progressive disclosure `read` works
        // (Codex-compatible: ~/.agents/skills + client skill homes + package dirs).
        let path_policy = build_path_policy(&cwd, cli, &user_settings, &resources);
        // Optional settings kill-switch for OS bash sandbox.
        if user_settings.bash_sandbox == Some(false) {
            std::env::set_var("ONE_BASH_SANDBOX", "0");
        }

        let bg_registry = Arc::new(BackgroundTaskRegistry::new());
        // Apply OS sandbox to background tasks even before BashTool construction.
        bg_registry.set_os_sandbox(OsSandbox::from_policy(&path_policy));

        let interactive = matches!(cli.mode, RunMode::Interactive) && cli.print.is_none();
        let perm_rules = user_settings
            .permissions
            .clone()
            .unwrap_or_else(PermissionRules::default);
        let permission_gate =
            PermissionGate::with_auto_approve(perm_rules, auto_approve, interactive);

        let hitl = HitlChannel::new(interactive);
        let ask_user_handler: Arc<dyn AskUserHandler> =
            Arc::new(InteractiveAskUser::new(hitl.clone()));

        let start_plan = cli.plan && !cli.read_only;
        let plan_path = if start_plan {
            Some(new_plan_path())
        } else {
            None
        };
        let plan_exit = Arc::new(Mutex::new(PlanExitState::new(
            plan_path
                .clone()
                .unwrap_or_else(|| agent_dir.join("plans").join("_none.md")),
        )));

        let mut tools: Vec<Arc<dyn Tool>> = if cli.read_only {
            // No bash / background tools in read-only mode.
            read_only_tools_with_ask(path_policy.clone(), Some(ask_user_handler.clone()))
        } else if start_plan {
            plan_mode_tools_with_policy(
                path_policy.clone(),
                plan_path.clone().expect("plan path"),
                plan_exit.clone(),
                Some(ask_user_handler.clone()),
            )
        } else {
            coding_tools_with_options(ToolBuildOptions {
                policy: path_policy.clone(),
                auto_approve,
                registry: bg_registry.clone(),
                ask_user: Some(ask_user_handler.clone()),
            })
        };
        // Extension tools only in Act mode (may include write-capable tools).
        if !start_plan {
            tools.extend(extensions.tools());
        }

        let base_system_prompt =
            resources.build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);
        let system_prompt = if start_plan {
            let p = plan_path.as_ref().expect("plan path");
            format!("{base_system_prompt}{}", plan_mode_system_overlay(p))
        } else {
            base_system_prompt.clone()
        };
        let mut agent = Agent::new(
            AgentConfig {
                system_prompt,
                max_turns: 32,
                thinking_level: ThinkingLevel::Off,
            },
            tools,
        );
        agent.set_tool_gate(Some(permission_gate.clone()));
        // Claude-style: completed background bash → conversation notice (not TUI status bar).
        if !cli.read_only {
            agent.set_notification_queue(bg_registry.notification_queue());
        }

        // Interactive `-r` opens a picker in TUI — don't load a session yet.
        let pick_session = cli.resume
            && matches!(cli.mode, crate::cli::RunMode::Interactive)
            && cli.print.is_none()
            && cli.session.is_none();

        let mut session = if cli.no_session {
            None
        } else if let Some(path) = &cli.session {
            Some(SessionManager::open(path).await?)
        } else if pick_session {
            // Empty shell until user picks via /resume float.
            None
        } else if cli.r#continue || (cli.resume && !pick_session) {
            // `-c` always most-recent; non-interactive `-r` same.
            match SessionManager::continue_recent(&cwd).await {
                Ok(session) => Some(session),
                Err(_) => {
                    if matches!(cli.mode, crate::cli::RunMode::Interactive) {
                        Some(SessionManager::create(&cwd).await?)
                    } else {
                        None
                    }
                }
            }
        } else if matches!(cli.mode, crate::cli::RunMode::Interactive) && cli.print.is_none() {
            Some(SessionManager::create(&cwd).await?)
        } else {
            None
        };

        // Default thinking from settings before session override.
        if let Some(level) = user_settings
            .thinking
            .as_deref()
            .and_then(ThinkingLevel::parse)
        {
            agent.config.thinking_level = level;
        }

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

        let mut runtime = Self {
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            abort_flag,
            steering_queue,
            followup_queue,
            session,
            extensions,
            resources,
            auto_approve,
            cwd,
            read_only: cli.read_only,
            path_policy,
            open_session_picker: pick_session,
            mode: if start_plan {
                AgentMode::Plan
            } else {
                AgentMode::Act
            },
            plan_path,
            plan_exit,
            bg_registry,
            base_system_prompt,
            permission_gate,
            hitl,
            ask_user_handler,
            context_window: 0,
        };

        // Restore plan path + mode from session custom entry if present.
        if !cli.read_only {
            if let Some(path) = runtime.restore_plan_path_from_session() {
                runtime.plan_path = Some(path);
            }
            if !start_plan {
                if let Some(restored) = runtime.restore_mode_from_session() {
                    if restored == AgentMode::Plan {
                        let _ = runtime.enter_plan_mode().await;
                    }
                }
            }
        }
        if start_plan {
            let _ = runtime.persist_mode().await;
        }

        Ok(runtime)
    }

    pub fn mode(&self) -> AgentMode {
        self.mode
    }

    pub fn plan_path(&self) -> Option<&std::path::Path> {
        self.plan_path.as_deref()
    }

    /// True if the model called `exit_plan_mode` since the last clear.
    pub fn take_plan_exit_request(&self) -> bool {
        let mut state = self.plan_exit.lock().expect("plan exit lock");
        let requested = state.requested;
        state.clear();
        requested
    }

    /// Enter plan mode: hard tool gate + system overlay + plan file path.
    pub async fn enter_plan_mode(&mut self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        if self.read_only {
            return Err("already in --read-only; use full tools with /plan instead".into());
        }
        if self.mode == AgentMode::Plan {
            if let Some(path) = &self.plan_path {
                return Ok(path.clone());
            }
        }

        let path = self
            .plan_path
            .clone()
            .unwrap_or_else(new_plan_path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Seed empty plan file so path is stable and readable.
        if !path.exists() {
            tokio::fs::write(
                &path,
                "# Plan\n\n_Write the implementation plan here._\n",
            )
            .await?;
        }

        {
            let mut state = self.plan_exit.lock().expect("plan exit lock");
            state.plan_path = path.clone();
            state.clear();
        }

        let mut tools = plan_mode_tools_with_policy(
            self.path_policy.clone(),
            path.clone(),
            self.plan_exit.clone(),
            Some(self.ask_user_handler.clone()),
        );
        // Keep extension tools out of plan mode (may be write-capable).
        let _ = &mut tools;

        {
            let mut agent = self.agent.lock().await;
            agent.set_tools(tools);
            agent.config.system_prompt =
                format!("{}{}", self.base_system_prompt, plan_mode_system_overlay(&path));
        }

        self.mode = AgentMode::Plan;
        self.plan_path = Some(path.clone());
        self.persist_mode().await?;
        Ok(path)
    }

    /// Leave plan mode and restore full coding tools (no auto-implement).
    pub async fn leave_plan_mode(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.mode == AgentMode::Act {
            return Ok(());
        }
        self.apply_act_tools_and_prompt().await?;
        self.mode = AgentMode::Act;
        {
            let mut state = self.plan_exit.lock().expect("plan exit lock");
            state.clear();
        }
        self.persist_mode().await?;
        Ok(())
    }

    /// Approve plan and return a user prompt that starts implementation.
    pub async fn approve_plan_prompt(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let plan_path = self
            .plan_path
            .clone()
            .ok_or("no plan file — enter /plan first")?;
        let content = tokio::fs::read_to_string(&plan_path)
            .await
            .unwrap_or_default();
        if content.trim().is_empty()
            || content.trim() == "# Plan\n\n_Write the implementation plan here._"
        {
            return Err("plan file is empty — finish the plan before /act".into());
        }

        self.leave_plan_mode().await?;

        Ok(format!(
            "The plan below was approved. Implement it now. Follow the steps; \
             do not re-plan unless blocked.\n\n\
             Plan file: {}\n\n\
             # Approved plan\n\n\
             {content}",
            plan_path.display()
        ))
    }

    /// Read current plan file contents (if any).
    pub async fn read_plan(&self) -> Option<String> {
        let path = self.plan_path.as_ref()?;
        tokio::fs::read_to_string(path).await.ok()
    }

    async fn apply_act_tools_and_prompt(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Refresh base prompt from resources in case of /reload.
        self.base_system_prompt = self
            .resources
            .build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);

        let mut tools: Vec<Arc<dyn Tool>> = if self.read_only {
            read_only_tools_with_ask(
                self.path_policy.clone(),
                Some(self.ask_user_handler.clone()),
            )
        } else {
            coding_tools_with_options(ToolBuildOptions {
                policy: self.path_policy.clone(),
                auto_approve: self.auto_approve,
                registry: self.bg_registry.clone(),
                ask_user: Some(self.ask_user_handler.clone()),
            })
        };
        tools.extend(self.extensions.tools());

        let mut agent = self.agent.lock().await;
        agent.set_tools(tools);
        agent.config.system_prompt = self.base_system_prompt.clone();
        if !self.read_only {
            agent.set_notification_queue(self.bg_registry.notification_queue());
        }
        Ok(())
    }

    async fn persist_mode(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(session) = &mut self.session else {
            return Ok(());
        };
        let data = serde_json::json!({
            "mode": self.mode.as_str(),
            "plan_path": self.plan_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        });
        session.append_custom("agent_mode", data).await?;
        Ok(())
    }

    fn restore_mode_from_session(&self) -> Option<AgentMode> {
        let session = self.session.as_ref()?;
        // Walk entries newest-first for latest agent_mode custom.
        for entry in session.entries().iter().rev() {
            if let one_session::SessionEntry::Custom {
                custom_type, data, ..
            } = entry
            {
                if custom_type == "agent_mode" {
                    let mode = data
                        .get("mode")
                        .and_then(|v| v.as_str())
                        .and_then(AgentMode::parse)?;
                    return Some(mode);
                }
            }
        }
        None
    }

    fn restore_plan_path_from_session(&self) -> Option<PathBuf> {
        let session = self.session.as_ref()?;
        for entry in session.entries().iter().rev() {
            if let one_session::SessionEntry::Custom {
                custom_type, data, ..
            } = entry
            {
                if custom_type == "agent_mode" {
                    return data
                        .get("plan_path")
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from);
                }
            }
        }
        None
    }

    pub async fn subscribe_printer(&mut self, json: bool) {
        let mut agent = self.agent.lock().await;
        agent.subscribe(Box::new(move |event: &AgentEvent| match event {
            AgentEvent::ThinkingDelta { delta } if !json => {
                // Print-mode: stream reasoning to stderr so stdout stays clean for piping.
                eprint!("{delta}");
            }
            AgentEvent::ThinkingDelta { delta } if json => {
                let line = serde_json::json!({"type":"thinking_delta","delta":delta});
                println!("{line}");
            }
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

    /// Update the model context window used for auto-compact thresholds.
    pub fn set_context_window(&mut self, window: usize) {
        self.context_window = window;
    }

    /// Compact when over threshold, or when `force` (e.g. context overflow recovery).
    ///
    /// Threshold is ~70% of [`Self::context_window`] when known; otherwise 80k.
    /// Token pressure prefers last provider-reported prompt size over char/4 estimate.
    pub async fn maybe_compact(
        &mut self,
        provider: &dyn LlmProvider,
        force: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config = CompactionConfig::from_context_window(self.context_window);
        let (messages, last_prompt) = {
            let agent = self.agent.lock().await;
            (agent.messages.clone(), agent.last_prompt_tokens)
        };
        let observed = if last_prompt > 0 {
            Some(last_prompt)
        } else {
            None
        };
        let tokens = tokens_for_compaction(&messages, observed);

        if !force && !should_compact_tokens(tokens, &config) {
            return Ok(());
        }
        if split_for_compaction(&messages, &config).is_none() {
            return Ok(());
        }

        let tokens_before = tokens as u64;
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
        // After compact the buffer is much smaller; clear stale API size so the
        // next turn re-estimates until a new completion reports usage.
        agent.last_prompt_tokens = 0;
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
        let mut resources = ResourceLoader::discover(&self.cwd, &agent_dir).await?;
        let user_settings = crate::settings::load();
        resources.apply_skills_config(&user_settings.skills_config_entries());
        self.resources = resources;
        self.extensions = discover_extensions(&agent_dir).await?;
        self.extensions
            .load_all(&ExtensionContext {
                cwd: &self.cwd,
                session_file: self.session_path().as_deref(),
            })
            .await?;

        self.base_system_prompt = self
            .resources
            .build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);
        // Rebuild tools + prompt for current mode.
        match self.mode {
            AgentMode::Plan => {
                let path = self
                    .plan_path
                    .clone()
                    .unwrap_or_else(new_plan_path);
                self.plan_path = Some(path.clone());
                {
                    let mut state = self.plan_exit.lock().expect("plan exit lock");
                    state.plan_path = path.clone();
                }
                let tools = plan_mode_tools_with_policy(
                    self.path_policy.clone(),
                    path.clone(),
                    self.plan_exit.clone(),
                    Some(self.ask_user_handler.clone()),
                );
                let mut agent = self.agent.lock().await;
                agent.set_tools(tools);
                agent.config.system_prompt =
                    format!("{}{}", self.base_system_prompt, plan_mode_system_overlay(&path));
            }
            AgentMode::Act => {
                self.apply_act_tools_and_prompt().await?;
            }
        }
        Ok(self.extensions.names())
    }

    /// Re-apply skills enable/disable from settings and rebuild system prompt.
    pub async fn reapply_skills_config(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let user_settings = crate::settings::load();
        self.resources
            .apply_skills_config(&user_settings.skills_config_entries());
        self.base_system_prompt = self
            .resources
            .build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);
        match self.mode {
            AgentMode::Plan => {
                let path = self
                    .plan_path
                    .clone()
                    .unwrap_or_else(new_plan_path);
                let mut agent = self.agent.lock().await;
                agent.config.system_prompt =
                    format!("{}{}", self.base_system_prompt, plan_mode_system_overlay(&path));
            }
            AgentMode::Act => {
                let mut agent = self.agent.lock().await;
                agent.config.system_prompt = self.base_system_prompt.clone();
            }
        }
        Ok(())
    }

    /// Toggle a skill on/off (persists to settings.json), returns new enabled state.
    pub async fn set_skill_enabled(
        &mut self,
        path: &std::path::Path,
        enabled: bool,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let mut s = crate::settings::load();
        s.set_skill_enabled(path, enabled);
        crate::settings::save(&s)?;
        self.reapply_skills_config().await?;
        Ok(enabled)
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

        // Restore plan path / mode from session custom entries.
        if !self.read_only {
            if let Some(p) = self.restore_plan_path_from_session() {
                self.plan_path = Some(p);
            }
            match self.restore_mode_from_session().unwrap_or(AgentMode::Act) {
                AgentMode::Plan => {
                    let _ = self.enter_plan_mode().await;
                }
                AgentMode::Act => {
                    if self.mode == AgentMode::Plan {
                        let _ = self.leave_plan_mode().await;
                    }
                }
            }
        }
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

    /// Provider-reported cumulative usage (input/output) for this runtime.
    pub async fn token_usage(&self) -> one_core::TokenUsage {
        self.agent.lock().await.token_usage
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
                let name = s
                    .session_name()
                    .or_else(|| s.first_user_preview())
                    .unwrap_or_else(|| "—".into());
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

fn new_plan_path() -> PathBuf {
    agent_dir()
        .join("plans")
        .join(format!("{}.md", Uuid::new_v4()))
}

/// Build path policy from CLI + settings + discovered skills.
///
/// Priority: `--full-access` / CLI `--add-dir` override settings; settings fill gaps.
/// Skill discovery roots and package dirs are always readable (not writable) so
/// the model can load `SKILL.md` / bundled resources without `--add-dir`.
fn build_path_policy(
    cwd: &std::path::Path,
    cli: &Cli,
    settings: &crate::settings::Settings,
    resources: &ResourceLoader,
) -> PathPolicy {
    let mode = if cli.full_access {
        SandboxMode::FullAccess
    } else if let Some(s) = settings.sandbox.as_deref().and_then(SandboxMode::parse) {
        s
    } else {
        SandboxMode::WorkspaceWrite
    };

    let mut policy = PathPolicy::workspace(cwd.to_path_buf()).with_mode(mode);

    let mut extras: Vec<PathBuf> = cli.add_dir.clone();
    if let Some(dirs) = &settings.additional_directories {
        for d in dirs {
            extras.push(PathBuf::from(d));
        }
    }
    // Dedup while preserving order.
    let mut seen = std::collections::HashSet::new();
    extras.retain(|p| seen.insert(p.clone()));
    if !extras.is_empty() {
        policy = policy.with_additional_dirs(extras);
    }

    // Progressive disclosure allowlist (agentskills.io / Codex).
    let skill_roots =
        skill_allowlist_roots(cwd, &resources.agent_dir, resources.all_skills());
    policy = policy.with_readable_roots(skill_roots);

    policy
}
