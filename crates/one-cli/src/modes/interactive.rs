use std::sync::{Arc, Mutex};

use one_core::agent::{LlmProvider, ThinkingLevel};
use one_core::error::OneError;
use one_core::events::AgentEvent;
use one_core::message::AgentMessage;
use one_tui::{
    App, ApprovalAnswer, ApprovalPrompt, ForceQuit, ModelChoice, RunOutcome, TerminalSession,
};

use crate::approval::ApprovalChoice;
use crate::provider::ProviderSet;
use crate::runtime::{AgentMode, AppRuntime};
use one_session::export_html;

/// Short label for turn footers / compact chrome: just the model id.
fn format_mode_label(providers: &ProviderSet) -> String {
    providers.as_llm().model().to_string()
}

/// Result of handling a slash command.
enum SlashAction {
    /// Not a slash command / pass through as user prompt.
    Pass,
    /// Handled; no further action (optional empty assistant bubble suppressed).
    Consumed,
    /// Run a full agent turn with this text as the user prompt.
    Prompt(String),
}

pub async fn run_interactive(
    runtime: &mut AppRuntime,
    providers: &mut ProviderSet,
    initial: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new("one");
    app.set_agent_label(runtime.mode().label());
    app.set_mode_label(format_mode_label(providers));
    app.set_model_catalog(
        providers
            .registry
            .list()
            .iter()
            .map(|m| ModelChoice {
                provider: m.provider.clone(),
                id: m.id.clone(),
                name: m.name.clone(),
            })
            .collect(),
    );
    app.set_current_model(&providers.provider_id, providers.as_llm().model());
    app.set_thinking_level(runtime.thinking_level().await.as_str());
    app.set_context_window(providers.context_window());
    refresh_usage(&mut app, runtime).await;

    // Project-scoped ↑/↓ history (survives `/new` and process restart).
    // Seeds from past session user prompts when the history file is empty.
    let history = one_session::load_or_seed_prompt_history(&runtime.cwd).await;
    app.load_prompt_history(history);
    app.enable_prompt_history_persist(runtime.cwd.clone());

    if let Some(warn) = &providers.config_warning {
        app.set_notice(format!("config  {warn}"));
    }

    // Skills: model sees catalog only; force-load via /skill:name is optional.
    let visible = runtime.resources.model_visible_skills();
    if !visible.is_empty() {
        let names: Vec<_> = visible.iter().map(|s| s.name.as_str()).collect();
        app.set_notice(format!(
            "skills ready ({}) · agent auto-reads SKILL.md when relevant",
            names.join(", ")
        ));
    }

    let mut terminal = TerminalSession::enter().map_err(|e| -> Box<dyn std::error::Error> { e })?;

    // `-r` → open session picker immediately.
    if runtime.open_session_picker {
        let sessions = runtime.list_sessions().await.unwrap_or_default();
        if sessions.is_empty() {
            // No past sessions — create one so interactive works.
            runtime.new_session().await?;
            app.set_notice("no past sessions · started new");
        } else {
            let rows: Vec<(String, String, String, String)> = sessions
                .into_iter()
                .rev()
                .take(40)
                .map(|s| {
                    let id = s.path.to_string_lossy().to_string();
                    let label = s.display_label();
                    let file = s
                        .path
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or("?")
                        .to_string();
                    let detail = format!("{}  {}", s.modified.format("%Y-%m-%d %H:%M"), file);
                    let hint = s.id.chars().take(8).collect();
                    (id, label, detail, hint)
                })
                .collect();
            app.open_sessions_float(&rows);
            app.set_notice("pick a session · Esc for new");
        }
    }

    if let Some(text) = initial {
        app.push_prompt_history(&text);
        match run_turn_streaming(
            runtime,
            providers.as_arc(),
            &mut terminal,
            &mut app,
            &text,
            Vec::new(),
        )
        .await?
        {
            TurnEnd::Continue => {}
            TurnEnd::ForceQuit => {
                terminal
                    .leave()
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
                return Ok(());
            }
        }
    }

    loop {
        match terminal
            .wait_action(&mut app)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> { e })?
        {
            RunOutcome::Quit => break,
            RunOutcome::Prompt(text) => {
                match handle_slash(runtime, providers, &mut app, &text).await? {
                    SlashAction::Pass => {
                        let images = app.take_pending_images();
                        match run_turn_streaming(
                            runtime,
                            providers.as_arc(),
                            &mut terminal,
                            &mut app,
                            &text,
                            images,
                        )
                        .await?
                        {
                            TurnEnd::Continue => {}
                            TurnEnd::ForceQuit => break,
                        }
                    }
                    SlashAction::Consumed => {
                        refresh_usage(&mut app, runtime).await;
                        terminal
                            .draw(&mut app)
                            .map_err(|e| -> Box<dyn std::error::Error> { e })?;
                    }
                    SlashAction::Prompt(prompt) => {
                        match run_turn_streaming(
                            runtime,
                            providers.as_arc(),
                            &mut terminal,
                            &mut app,
                            &prompt,
                            Vec::new(),
                        )
                        .await?
                        {
                            TurnEnd::Continue => {}
                            TurnEnd::ForceQuit => break,
                        }
                    }
                }
            }
            RunOutcome::FollowUp(text) => {
                runtime.follow_up(text.clone());
                app.set_notice(format!("queued follow-up  {text}"));
                terminal
                    .draw(&mut app)
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            RunOutcome::Steer(text) => {
                runtime.steer(text.clone());
                app.set_notice(format!("queued steer  {text}"));
                terminal
                    .draw(&mut app)
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            RunOutcome::CycleAgentMode => {
                // Space on empty prompt: Plan ↔ Build (thinking lives in /settings).
                match runtime.mode() {
                    AgentMode::Act => match runtime.enter_plan_mode().await {
                        Ok(path) => {
                            app.set_agent_label(AgentMode::Plan.label());
                            app.set_notice(format!(
                                "plan mode · {} · Space or /act to leave",
                                path.display()
                            ));
                        }
                        Err(err) => app.set_notice(format!("plan: {err}")),
                    },
                    AgentMode::Plan => {
                        // Toggle off without auto-implement (use /act to approve + run).
                        runtime.leave_plan_mode().await?;
                        app.set_agent_label(AgentMode::Act.label());
                        app.set_notice("Build mode · /act to implement an approved plan");
                    }
                }
                terminal
                    .draw(&mut app)
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            RunOutcome::OpenRewind => {
                open_rewind_menu(runtime, &mut app)?;
                terminal
                    .draw(&mut app)
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            RunOutcome::Noop => {}
        }
    }

    terminal
        .leave()
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;
    Ok(())
}

async fn refresh_usage(app: &mut App, runtime: &AppRuntime) {
    let tokens = runtime.estimated_tokens().await;
    app.set_usage_tokens(tokens);
    let usage = runtime.token_usage().await;
    app.set_usage_io(usage.input_tokens, usage.output_tokens);
    // Rough blended cost (USD / 1M tokens) — good enough for a footer estimate.
    let cost = estimate_cost_usd(
        &app.current_provider,
        &app.current_model,
        usage.input_tokens,
        usage.output_tokens,
    );
    app.set_usage_cost_usd(cost);
}

/// Very rough public list prices (USD per 1M tokens). Zero when unknown.
fn estimate_cost_usd(provider: &str, model: &str, input: u64, output: u64) -> f64 {
    let (in_rate, out_rate) = match (provider, model) {
        ("openai", m) if m.contains("gpt-4o-mini") => (0.15, 0.60),
        ("openai", m) if m.contains("gpt-4o") => (2.50, 10.0),
        ("anthropic", _) => (3.0, 15.0),
        ("deepseek", m) if m.contains("reasoner") => (0.55, 2.19),
        ("deepseek", _) => (0.27, 1.10),
        ("gemini", m) if m.contains("pro") => (1.25, 10.0),
        ("gemini", _) => (0.15, 0.60),
        ("openrouter", _) => (0.0, 0.0),
        _ => (0.0, 0.0),
    };
    if in_rate == 0.0 && out_rate == 0.0 {
        return 0.0;
    }
    (input as f64 / 1_000_000.0) * in_rate + (output as f64 / 1_000_000.0) * out_rate
}

enum TurnEnd {
    Continue,
    ForceQuit,
}

async fn run_turn_streaming(
    runtime: &mut AppRuntime,
    provider: std::sync::Arc<dyn LlmProvider>,
    terminal: &mut TerminalSession,
    app: &mut App,
    text: &str,
    images: Vec<(String, String)>,
) -> Result<TurnEnd, Box<dyn std::error::Error>> {
    // After `-r` picker dismiss without pick, create a session on first turn.
    if runtime.session.is_none() {
        runtime.new_session().await?;
        runtime.open_session_picker = false;
    }

    app.begin_busy();
    runtime.clear_abort();

    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    runtime.subscribe_collector(events.clone()).await;

    let agent = runtime.agent.clone();
    let resolved = runtime.resources.resolve_input(text);
    if let Some(skill) = &resolved.skill {
        app.set_notice(format!("skill → {skill}"));
    }
    let text = resolved.text;
    runtime.maybe_compact(provider.as_ref(), false).await?;
    runtime
        .extensions
        .emit(&one_ext::ExtensionEvent::AgentStart)
        .await?;

    let before = agent.lock().await.messages.len();
    let steering = runtime.steering_queue();
    let followup = runtime.followup_queue();

    let prompt_handle = {
        let agent = agent.clone();
        let provider = provider.clone();
        let text = text.clone();
        let images = images.clone();
        tokio::spawn(async move {
            let mut agent = agent.lock().await;
            agent
                .prompt_with_images(provider.as_ref(), &text, images)
                .await
        })
    };

    let gate = runtime.permission_gate.clone();
    let prompt_result = match terminal
        .run_busy(
            app,
            |app| {
                drain_events(app, &events);
                // Surface interactive permission asks from the agent task.
                if let Some(req) = gate.poll_request() {
                    app.set_approval_prompt(ApprovalPrompt {
                        id: req.id,
                        tool: req.tool,
                        summary: req.summary,
                        reason: req.reason,
                    });
                }
                if let Some(answer) = app.take_approval_answer() {
                    let choice = match answer {
                        ApprovalAnswer::Once => ApprovalChoice::Once,
                        ApprovalAnswer::Session => ApprovalChoice::Session,
                        ApprovalAnswer::Deny => ApprovalChoice::Deny,
                    };
                    let _ = gate.respond(choice);
                }
                if app.take_abort() {
                    gate.cancel_pending();
                    runtime.abort();
                }
                if let Some(text) = app.take_steer() {
                    one_core::agent::Agent::push_queue(&steering, text.clone());
                    app.set_notice(format!("queued steer  {text}"));
                }
                if let Some(text) = app.take_followup() {
                    one_core::agent::Agent::push_queue(&followup, text.clone());
                    app.set_notice(format!("queued follow-up  {text}"));
                }
            },
            prompt_handle,
        )
        .await
    {
        Ok(result) => result,
        Err(ForceQuit) => {
            gate.cancel_pending();
            runtime.abort();
            app.end_busy();
            app.finish_stream_with_interrupted(true);
            return Ok(TurnEnd::ForceQuit);
        }
    };
    gate.cancel_pending();
    app.clear_approval_prompt();

    // Overflow recovery: force compact + retry once.
    let prompt_result = match prompt_result {
        Err(err) if is_overflow(&err) => {
            app.set_notice("context overflow · compacting…");
            let _ = terminal.draw(app);
            runtime.maybe_compact(provider.as_ref(), true).await?;
            let events2: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
            runtime.subscribe_collector(events2.clone()).await;
            let agent2 = agent.clone();
            let provider2 = provider.clone();
            let text2 = text.clone();
            let images2 = images.clone();
            let retry = tokio::spawn(async move {
                let mut agent = agent2.lock().await;
                // If last message is already the user turn from failed prompt, just run.
                if agent
                    .messages
                    .last()
                    .map(|m| matches!(m, AgentMessage::User(_)))
                    .unwrap_or(false)
                {
                    agent.run(provider2.as_ref()).await
                } else {
                    agent
                        .prompt_with_images(provider2.as_ref(), &text2, images2)
                        .await
                }
            });
            let gate2 = runtime.permission_gate.clone();
            match terminal
                .run_busy(
                    app,
                    |app| {
                        drain_events(app, &events2);
                        if let Some(req) = gate2.poll_request() {
                            app.set_approval_prompt(ApprovalPrompt {
                                id: req.id,
                                tool: req.tool,
                                summary: req.summary,
                                reason: req.reason,
                            });
                        }
                        if let Some(answer) = app.take_approval_answer() {
                            let choice = match answer {
                                ApprovalAnswer::Once => ApprovalChoice::Once,
                                ApprovalAnswer::Session => ApprovalChoice::Session,
                                ApprovalAnswer::Deny => ApprovalChoice::Deny,
                            };
                            let _ = gate2.respond(choice);
                        }
                        if app.take_abort() {
                            gate2.cancel_pending();
                            runtime.abort();
                        }
                    },
                    retry,
                )
                .await
            {
                Ok(result) => result,
                Err(ForceQuit) => {
                    gate2.cancel_pending();
                    runtime.abort();
                    app.end_busy();
                    app.finish_stream_with_interrupted(true);
                    return Ok(TurnEnd::ForceQuit);
                }
            }
        }
        other => other,
    };

    app.end_busy();

    match prompt_result {
        Ok(reply) => {
            if let Some(session) = &mut runtime.session {
                let messages = agent.lock().await.messages[before..].to_vec();
                for message in messages {
                    session.append_message(message).await?;
                }
            }
            runtime.persist_extension_state().await?;
            runtime
                .extensions
                .emit(&one_ext::ExtensionEvent::AgentEnd)
                .await?;

            if app.stream_buffer.is_empty() && !reply.is_empty() {
                app.push_assistant(reply);
            } else {
                app.finish_stream();
            }

            // Model finished planning → surface plan for review.
            if runtime.take_plan_exit_request() {
                notify_plan_ready(app, runtime).await;
            }
        }
        Err(OneError::Aborted) => {
            app.finish_stream_with_interrupted(true);
            app.set_notice("interrupted");
            if let Some(session) = &mut runtime.session {
                let messages = agent.lock().await.messages[before..].to_vec();
                for message in messages {
                    session.append_message(message).await?;
                }
            }
            runtime.persist_extension_state().await?;
            runtime
                .extensions
                .emit(&one_ext::ExtensionEvent::AgentEnd)
                .await?;
            let _ = runtime.take_plan_exit_request();
        }
        Err(err) => {
            // Mid-transcript alert (UI only). Short notice remains for status strip.
            app.push_error_alert(format!("{err}"));
            app.set_notice(format!("error · see transcript"));
            let _ = runtime.take_plan_exit_request();
        }
    }

    refresh_usage(app, runtime).await;
    terminal
        .draw(app)
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;
    Ok(TurnEnd::Continue)
}

fn is_overflow(err: &OneError) -> bool {
    match err {
        OneError::ContextOverflow(_) => true,
        OneError::Provider(msg) => one_core::is_context_overflow_error(msg),
        _ => false,
    }
}

fn drain_events(app: &mut App, events: &Arc<Mutex<Vec<AgentEvent>>>) {
    let mut batch = events.lock().expect("events lock");
    for event in batch.drain(..) {
        match event {
            AgentEvent::TextDelta { delta } => app.append_stream(&delta),
            AgentEvent::ThinkingDelta { delta } => app.append_thinking_stream(&delta),
            AgentEvent::ToolExecutionStart { tool_call } => {
                let args = match &tool_call.arguments {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                app.push_tool_call(&tool_call.name, args);
            }
            AgentEvent::ToolExecutionEnd {
                is_error,
                tool_call,
                output,
            } => {
                // Full `output` is already on the agent as ToolResult (LLM context).
                // TUI only keeps a truncated preview for mid-transcript display.
                // Prefer as_ui_text so image tool results show `[image · png · …]`.
                let text = output.as_ui_text();
                app.finish_tool_with_output(
                    &tool_call.name,
                    is_error,
                    if text.is_empty() { None } else { Some(text) },
                );
            }
            _ => {}
        }
    }
}

async fn handle_slash(
    runtime: &mut AppRuntime,
    providers: &mut ProviderSet,
    app: &mut App,
    text: &str,
) -> Result<SlashAction, Box<dyn std::error::Error>> {
    if !text.starts_with('/') {
        return Ok(SlashAction::Pass);
    }

    // Skill invocations are user prompts, not UI commands.
    if text.starts_with("/skill:") || text.starts_with("/skill ") {
        return Ok(SlashAction::Pass);
    }

    let parts: Vec<&str> = text.split_whitespace().collect();
    match parts.first().copied() {
        Some("/help") => {
            // Secondary float — not a toast dump.
            app.open_help_float();
            Ok(SlashAction::Consumed)
        }
        Some("/clear") => {
            app.messages.clear();
            app.chat_scroll = 0;
            app.set_notice("chat cleared");
            Ok(SlashAction::Consumed)
        }
        Some("/session") => {
            // Key/value info float.
            let summary = runtime.session_summary_line();
            let mut rows: Vec<(String, String)> = Vec::new();
            // Parse "key: value · key: value" style if present; else one row.
            if summary.contains('·') || summary.contains('|') {
                for part in summary.split(['·', '|']) {
                    let part = part.trim();
                    if let Some((k, v)) = part.split_once(':') {
                        rows.push((k.trim().to_string(), v.trim().to_string()));
                    } else if !part.is_empty() {
                        rows.push(("info".into(), part.to_string()));
                    }
                }
            }
            if rows.is_empty() {
                rows.push(("session".into(), summary));
            }
            if let Some(session) = &runtime.session {
                rows.push((
                    "messages".into(),
                    runtime.agent.lock().await.messages.len().to_string(),
                ));
                if let Some(name) = session.session_name() {
                    rows.push(("name".into(), name));
                }
                let _ = session;
            }
            app.open_info_float("Session", &rows);
            Ok(SlashAction::Consumed)
        }
        Some("/name") => {
            let name = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
            if name.is_empty() {
                let cur = runtime
                    .session
                    .as_ref()
                    .and_then(|s| s.session_name())
                    .unwrap_or_else(|| "(unnamed)".into());
                app.set_notice(format!("session name: {cur} · /name <new>"));
            } else {
                runtime.set_session_name(&name).await?;
                app.set_notice(format!("named session → {name}"));
            }
            Ok(SlashAction::Consumed)
        }
        Some("/new") => {
            runtime.new_session().await?;
            // New session defaults to Build; leave plan mode if active.
            if runtime.mode() == AgentMode::Plan {
                let _ = runtime.leave_plan_mode().await;
            }
            app.messages.clear();
            app.chat_scroll = 0;
            app.set_agent_label(runtime.mode().label());
            app.set_notice("new session");
            refresh_usage(app, runtime).await;
            Ok(SlashAction::Consumed)
        }
        Some("/resume") => {
            let sessions = runtime.list_sessions().await?;
            if sessions.is_empty() {
                app.set_notice("no sessions for this project");
                return Ok(SlashAction::Consumed);
            }
            // Optional path/id argument → open directly.
            if let Some(spec) = parts.get(1) {
                let target = sessions.iter().find(|s| {
                    s.path.to_string_lossy().contains(spec)
                        || s.id.starts_with(spec)
                        || s.name.as_deref().is_some_and(|n| n == *spec)
                        || s.preview.as_deref().is_some_and(|p| p == *spec || p.contains(spec))
                });
                if let Some(info) = target {
                    load_session_into_app(runtime, app, info).await?;
                } else {
                    app.set_notice(format!("session not found: {spec}"));
                }
                return Ok(SlashAction::Consumed);
            }

            // Secondary float picker (newest first).
            let mut rows: Vec<(String, String, String, String)> = sessions
                .into_iter()
                .rev()
                .take(40)
                .map(|s| {
                    let id = s.path.to_string_lossy().to_string();
                    // Prefer /name, else first user prompt (Codex-style), else short id.
                    let label = s.display_label();
                    let file = s
                        .path
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or("?")
                        .to_string();
                    let detail = format!(
                        "{}  {}",
                        s.modified.format("%Y-%m-%d %H:%M"),
                        file
                    );
                    let hint = s.id.chars().take(8).collect();
                    (id, label, detail, hint)
                })
                .collect();
            if rows.is_empty() {
                app.set_notice("no sessions");
            } else {
                app.open_sessions_float(&rows);
            }
            let _ = &mut rows;
            Ok(SlashAction::Consumed)
        }
        Some("/plan") => {
            match runtime.enter_plan_mode().await {
                Ok(path) => {
                    app.set_agent_label(AgentMode::Plan.label());
                    app.set_notice(format!(
                        "plan mode · write plan to {} · /act when ready",
                        path.display()
                    ));
                }
                Err(err) => app.set_notice(format!("plan: {err}")),
            }
            Ok(SlashAction::Consumed)
        }
        Some("/act") | Some("/build") => {
            if runtime.mode() == AgentMode::Act && runtime.plan_path().is_none() {
                app.set_agent_label(AgentMode::Act.label());
                app.set_notice("already in Build mode");
                return Ok(SlashAction::Consumed);
            }
            // Leave plan without implementing if user only wants tools back and no plan yet.
            if parts.get(1).is_some_and(|s| *s == "skip" || *s == "--no-run") {
                runtime.leave_plan_mode().await?;
                app.set_agent_label(AgentMode::Act.label());
                app.set_notice("Build mode (plan not auto-run)");
                return Ok(SlashAction::Consumed);
            }
            match runtime.approve_plan_prompt().await {
                Ok(prompt) => {
                    app.set_agent_label(AgentMode::Act.label());
                    app.set_notice("plan approved · implementing…");
                    Ok(SlashAction::Prompt(prompt))
                }
                Err(err) => {
                    // No plan file yet — just switch to Build tools.
                    if runtime.mode() == AgentMode::Plan {
                        runtime.leave_plan_mode().await?;
                        app.set_agent_label(AgentMode::Act.label());
                        app.set_notice(format!("Build mode · {err}"));
                        Ok(SlashAction::Consumed)
                    } else {
                        app.set_notice(format!("act: {err}"));
                        Ok(SlashAction::Consumed)
                    }
                }
            }
        }
        Some("/compact") => {
            let custom = parts.get(1..).map(|p| p.join(" "));
            // Temporarily lower threshold by force-compacting.
            if let Some(instr) = custom.filter(|s| !s.is_empty()) {
                app.set_notice(format!("compacting… ({instr})"));
            } else {
                app.set_notice("compacting…");
            }
            runtime
                .maybe_compact(providers.as_llm(), true)
                .await?;
            refresh_usage(app, runtime).await;
            app.set_notice(format!(
                "compacted · ~{} tokens",
                runtime.estimated_tokens().await
            ));
            Ok(SlashAction::Consumed)
        }
        Some("/reload") => {
            match runtime.reload_extensions().await {
                Ok(names) => {
                    let skills = runtime.resources.skill_names();
                    app.set_notice(format!(
                        "reloaded ext=[{}] skills=[{}]",
                        names.join(","),
                        skills.join(",")
                    ));
                    app.set_agent_label(runtime.mode().label());
                }
                Err(err) => app.set_notice(format!("reload failed: {err}")),
            }
            Ok(SlashAction::Consumed)
        }
        Some("/tree") => {
            let Some(session) = &mut runtime.session else {
                app.set_notice("no active session");
                return Ok(SlashAction::Consumed);
            };

            if let Some(id) = parts.get(1) {
                match session.branch(id) {
                    Ok(()) => {
                        let mut agent = runtime.agent.lock().await;
                        agent.messages.clear();
                        session.load_messages_into(&mut agent.messages);
                        rebuild_tui_from_agent(app, &agent.messages);
                        app.set_notice(format!("branched to {id}"));
                        refresh_usage(app, runtime).await;
                    }
                    Err(err) => app.set_notice(format!("branch failed: {err}")),
                }
                return Ok(SlashAction::Consumed);
            }

            // Secondary float: pick a branch entry.
            let leaf = session.get_leaf_id().unwrap_or("root").to_string();
            use one_session::SessionEntry;
            let entries: Vec<(String, String, String)> = session
                .entries()
                .iter()
                .rev()
                .take(60)
                .map(|e| {
                    let id = e.id().to_string();
                    let kind = match e {
                        SessionEntry::Message { .. } => "message",
                        SessionEntry::Compaction { .. } => "compaction",
                        SessionEntry::BranchSummary { .. } => "summary",
                        SessionEntry::SessionInfo { .. } => "info",
                        SessionEntry::ModelChange { .. } => "model",
                        SessionEntry::ThinkingLevelChange { .. } => "thinking",
                        SessionEntry::Label { .. } => "label",
                        SessionEntry::Custom { .. } | SessionEntry::CustomMessage { .. } => "custom",
                    };
                    let mark = if id == leaf { "●" } else { " " };
                    let label = format!("{mark} {kind}");
                    let detail = id.chars().take(16).collect();
                    (id, label, detail)
                })
                .collect();
            if entries.is_empty() {
                app.set_notice(format!("session tree empty · leaf={leaf}"));
            } else {
                app.open_tree_float(&entries);
            }
            Ok(SlashAction::Consumed)
        }
        Some("/rewind") => {
            // Bare /rewind or Esc Esc → menu; /rewind <id> restores conversation.
            if parts.get(1).is_none() {
                open_rewind_menu(runtime, app)?;
                return Ok(SlashAction::Consumed);
            }
            let id = parts[1];
            apply_rewind(runtime, app, id).await?;
            Ok(SlashAction::Consumed)
        }
        Some("/export") => {
            let Some(session) = &runtime.session else {
                app.set_notice("no session to export");
                return Ok(SlashAction::Consumed);
            };
            let html = export_html(session);
            let path = parts
                .get(1)
                .map(|p| std::path::PathBuf::from(*p))
                .unwrap_or_else(|| std::path::PathBuf::from("session-export.html"));
            tokio::fs::write(&path, html).await?;
            app.set_notice(format!("exported {}", path.display()));
            Ok(SlashAction::Consumed)
        }
        Some("/model") => {
            let Some(spec) = parts.get(1) else {
                app.open_model_picker();
                return Ok(SlashAction::Consumed);
            };

            let (provider_name, model) = if let Some((p, m)) = spec.split_once(':') {
                (p.to_string(), Some(m.to_string()))
            } else {
                ((*spec).to_string(), parts.get(2).map(|s| (*s).to_string()))
            };

            match providers.switch_named(&provider_name, model) {
                Ok(()) => {
                    app.set_mode_label(format_mode_label(providers));
                    app.set_current_model(&providers.provider_id, providers.as_llm().model());
                    if let Some(session) = &mut runtime.session {
                        session
                            .append_model_change(
                                providers.provider_id.clone(),
                                providers.as_llm().model(),
                            )
                            .await?;
                    }
                    app.set_context_window(providers.context_window());
                    app.set_notice(format!(
                        "model → {} / {}",
                        providers.provider_id,
                        providers.as_llm().model(),
                    ));
                }
                Err(err) => app.set_notice(format!("model: {err}")),
            }
            Ok(SlashAction::Consumed)
        }
        Some("/thinking") => {
            if parts.get(1).is_none() {
                // Bare /thinking → secondary level picker float.
                app.open_thinking_float();
                return Ok(SlashAction::Consumed);
            }
            let level = parts
                .get(1)
                .and_then(|s| ThinkingLevel::parse(s))
                .unwrap_or(ThinkingLevel::Off);
            runtime.set_thinking_level(level).await?;
            app.set_thinking_level(level.as_str());
            // Persist into settings.
            let mut s = crate::settings::load();
            s.thinking = Some(level.as_str().to_string());
            let _ = crate::settings::save(&s);
            app.set_notice(format!("thinking → {}", level.as_str()));
            Ok(SlashAction::Consumed)
        }
        Some("/settings") => {
            if parts.len() >= 3 {
                let key = parts[1];
                let value = parts[2..].join(" ");
                let mut s = crate::settings::load();
                match crate::settings::set_key(&mut s, key, &value) {
                    Ok(()) => {
                        crate::settings::save(&s)?;
                        // Apply live where possible.
                        if key.eq_ignore_ascii_case("thinking") {
                            if let Some(tl) = ThinkingLevel::parse(&value) {
                                runtime.set_thinking_level(tl).await?;
                                app.set_thinking_level(tl.as_str());
                            }
                        }
                        if key.eq_ignore_ascii_case("context_window")
                            || key.eq_ignore_ascii_case("context-window")
                            || key.eq_ignore_ascii_case("context")
                        {
                            if let Some(n) = s.context_window {
                                app.set_context_window(n);
                            }
                        }
                        if key.eq_ignore_ascii_case("provider")
                            || key.eq_ignore_ascii_case("model")
                        {
                            app.set_notice(format!(
                                "settings.{key} = {value} · use /model to apply live"
                            ));
                        } else {
                            app.set_notice(format!("settings.{key} = {value}"));
                        }
                    }
                    Err(err) => app.set_notice(format!("settings: {err}")),
                }
            } else {
                let s = crate::settings::load();
                let rows = crate::settings::rows(&s);
                app.open_info_float("Settings", &rows);
            }
            Ok(SlashAction::Consumed)
        }
        _ => {
            // Unknown /cmd — if it matches a prompt template, let agent handle via resolve.
            if runtime.resources.resolve_input(text).text != text {
                return Ok(SlashAction::Pass);
            }
            Ok(SlashAction::Pass)
        }
    }
}

/// After `exit_plan_mode`, show the plan path and how to approve.
async fn notify_plan_ready(app: &mut App, runtime: &AppRuntime) {
    let path = runtime
        .plan_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(plan file)".into());
    let preview = runtime
        .read_plan()
        .await
        .map(|c| {
            let trimmed = c.trim();
            let max = 1200usize;
            if trimmed.len() > max {
                format!("{}…", &trimmed[..max])
            } else {
                trimmed.to_string()
            }
        })
        .unwrap_or_default();
    if !preview.is_empty() {
        app.push_assistant(format!(
            "**Plan ready for review**\n\n\
             Path: `{path}`\n\n\
             {preview}\n\n\
             —\n\
             `/act` to implement · keep chatting to refine · edit the plan file directly"
        ));
    }
    app.set_notice(format!("plan ready · /act to implement · {path}"));
}

/// Open a past session and mirror messages into the TUI transcript.
async fn load_session_into_app(
    runtime: &mut AppRuntime,
    app: &mut App,
    info: &one_session::SessionInfo,
) -> Result<(), Box<dyn std::error::Error>> {
    runtime.open_session_path(&info.path).await?;
    let msgs = runtime.agent.lock().await.messages.clone();
    rebuild_tui_from_agent(app, &msgs);
    // ↑ history is project-scoped (loaded at startup); do not re-append on resume.
    app.set_thinking_level(runtime.thinking_level().await.as_str());
    app.set_agent_label(runtime.mode().label());
    refresh_usage(app, runtime).await;
    app.set_notice(format!("resumed {}", info.display_label()));
    Ok(())
}

/// Esc Esc / `/rewind` menu — list user prompts on the active branch.
fn open_rewind_menu(
    runtime: &AppRuntime,
    app: &mut App,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(session) = &runtime.session else {
        app.set_notice("no session · nothing to rewind");
        return Ok(());
    };
    let prompts = session.user_prompts_for_rewind();
    if prompts.is_empty() {
        app.set_notice("no prompts to rewind");
        return Ok(());
    }
    app.open_rewind_float(&prompts);
    Ok(())
}

/// Rewind conversation to before `entry_id` and restore that prompt into the input.
async fn apply_rewind(
    runtime: &mut AppRuntime,
    app: &mut App,
    entry_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(session) = &mut runtime.session else {
        app.set_notice("no session · nothing to rewind");
        return Ok(());
    };

    let prompt_text = session
        .user_prompt_text(entry_id)
        .ok_or_else(|| format!("rewind: entry not a user prompt: {entry_id}"))?;

    session.rewind_before(entry_id)?;

    {
        let mut agent = runtime.agent.lock().await;
        agent.messages.clear();
        session.load_messages_into(&mut agent.messages);
        rebuild_tui_from_agent(app, &agent.messages);
    }

    app.set_input_for_edit(prompt_text);
    refresh_usage(app, runtime).await;
    app.set_notice("rewound · edit prompt and Enter to re-send");
    Ok(())
}

/// Rebuild on-screen transcript from agent messages (user / thinking / assistant text).
fn rebuild_tui_from_agent(app: &mut App, messages: &[AgentMessage]) {
    app.messages.clear();
    app.chat_scroll = 0;
    app.stream_buffer.clear();
    app.thinking_buffer.clear();
    for m in messages {
        match m {
            AgentMessage::User(u) => {
                app.push_user(u.content.as_display_text());
            }
            AgentMessage::Assistant(a) => {
                for block in &a.content {
                    match block {
                        one_core::message::ContentBlock::Thinking {
                            thinking,
                            redacted,
                            ..
                        } => {
                            if !thinking.is_empty() {
                                let mut msg = one_tui::message::Message::thinking(thinking.clone());
                                msg.thinking_expanded = app.show_thinking && !redacted;
                                app.messages.push(msg);
                            }
                        }
                        one_core::message::ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                app.push_assistant(text);
                            }
                        }
                        one_core::message::ContentBlock::ToolCall { name, arguments, .. } => {
                            let args = match arguments {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            app.push_tool_call(name, args);
                            app.finish_tool_with_output(name, false, None);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
