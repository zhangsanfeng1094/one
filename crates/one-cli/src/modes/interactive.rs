use std::sync::{Arc, Mutex};

use one_core::agent::{LlmProvider, ThinkingLevel};
use one_core::error::OneError;
use one_core::events::AgentEvent;
use one_core::message::AgentMessage;
use one_tui::{App, ModelChoice, RunOutcome, TerminalSession};

use crate::provider::ProviderSet;
use crate::runtime::AppRuntime;
use one_session::export_html;

/// Short label for turn footers / compact chrome: just the model id.
fn format_mode_label(providers: &ProviderSet) -> String {
    providers.as_llm().model().to_string()
}

pub async fn run_interactive(
    runtime: &mut AppRuntime,
    providers: &mut ProviderSet,
    initial: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new("one");
    app.set_agent_label("Build");
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
    refresh_usage(&mut app, runtime).await;

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

    if let Some(text) = initial {
        run_turn_streaming(runtime, providers.as_arc(), &mut terminal, &mut app, &text).await?;
    }

    loop {
        match terminal
            .wait_action(&mut app)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> { e })?
        {
            RunOutcome::Quit => break,
            RunOutcome::Prompt(text) => {
                if let Some(reply) = handle_slash(runtime, providers, &mut app, &text).await? {
                    if !reply.is_empty() {
                        app.push_assistant(reply);
                    }
                    refresh_usage(&mut app, runtime).await;
                    terminal
                        .draw(&mut app)
                        .map_err(|e| -> Box<dyn std::error::Error> { e })?;
                    continue;
                }
                run_turn_streaming(runtime, providers.as_arc(), &mut terminal, &mut app, &text)
                    .await?;
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
            RunOutcome::CycleThinking => {
                let next = runtime.thinking_level().await.cycle_next();
                runtime.set_thinking_level(next).await?;
                app.set_thinking_level(next.as_str());
                app.set_notice(format!("thinking → {}", next.as_str()));
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
}

async fn run_turn_streaming(
    runtime: &mut AppRuntime,
    provider: std::sync::Arc<dyn LlmProvider>,
    terminal: &mut TerminalSession,
    app: &mut App,
    text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
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
        tokio::spawn(async move {
            let mut agent = agent.lock().await;
            agent.prompt(provider.as_ref(), &text).await
        })
    };

    let prompt_result = terminal
        .run_busy(
            app,
            |app| {
                drain_events(app, &events);
                if app.take_abort() {
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
        .await;

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
                    agent.prompt(provider2.as_ref(), &text2).await
                }
            });
            terminal
                .run_busy(
                    app,
                    |app| {
                        drain_events(app, &events2);
                        if app.take_abort() {
                            runtime.abort();
                        }
                    },
                    retry,
                )
                .await
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
        }
        Err(err) => {
            // Mid-transcript alert (UI only). Short notice remains for status strip.
            app.push_error_alert(format!("{err}"));
            app.set_notice(format!("error · see transcript"));
        }
    }

    refresh_usage(app, runtime).await;
    terminal
        .draw(app)
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;
    Ok(())
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
                let text = output.as_text();
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
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if !text.starts_with('/') {
        return Ok(None);
    }

    // Skill invocations are user prompts, not UI commands.
    if text.starts_with("/skill:") || text.starts_with("/skill ") {
        return Ok(None);
    }

    let parts: Vec<&str> = text.split_whitespace().collect();
    match parts.first().copied() {
        Some("/help") => {
            // Secondary float — not a toast dump.
            app.open_help_float();
            Ok(Some(String::new()))
        }
        Some("/clear") => {
            app.messages.clear();
            app.chat_scroll = 0;
            app.set_notice("chat cleared");
            Ok(Some(String::new()))
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
            Ok(Some(String::new()))
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
            Ok(Some(String::new()))
        }
        Some("/new") => {
            runtime.new_session().await?;
            app.messages.clear();
            app.chat_scroll = 0;
            app.set_notice("new session");
            refresh_usage(app, runtime).await;
            Ok(Some(String::new()))
        }
        Some("/resume") => {
            let sessions = runtime.list_sessions().await?;
            if sessions.is_empty() {
                app.set_notice("no sessions for this project");
                return Ok(Some(String::new()));
            }
            // Optional path/id argument → open directly.
            if let Some(spec) = parts.get(1) {
                let target = sessions.iter().find(|s| {
                    s.path.to_string_lossy().contains(spec)
                        || s.id.starts_with(spec)
                        || s.name.as_deref().is_some_and(|n| n == *spec)
                });
                if let Some(info) = target {
                    load_session_into_app(runtime, app, info).await?;
                } else {
                    app.set_notice(format!("session not found: {spec}"));
                }
                return Ok(Some(String::new()));
            }

            // Secondary float picker (newest first).
            let mut rows: Vec<(String, String, String, String)> = sessions
                .into_iter()
                .rev()
                .take(40)
                .map(|s| {
                    let id = s.path.to_string_lossy().to_string();
                    let label = s
                        .name
                        .clone()
                        .unwrap_or_else(|| s.id.chars().take(12).collect());
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
            Ok(Some(String::new()))
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
            Ok(Some(String::new()))
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
                }
                Err(err) => app.set_notice(format!("reload failed: {err}")),
            }
            Ok(Some(String::new()))
        }
        Some("/tree") => {
            let Some(session) = &mut runtime.session else {
                app.set_notice("no active session");
                return Ok(Some(String::new()));
            };

            if let Some(id) = parts.get(1) {
                match session.branch(id) {
                    Ok(()) => {
                        let mut agent = runtime.agent.lock().await;
                        agent.messages.clear();
                        session.load_messages_into(&mut agent.messages);
                        app.set_notice(format!("branched to {id}"));
                        refresh_usage(app, runtime).await;
                    }
                    Err(err) => app.set_notice(format!("branch failed: {err}")),
                }
                return Ok(Some(String::new()));
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
            Ok(Some(String::new()))
        }
        Some("/export") => {
            let Some(session) = &runtime.session else {
                app.set_notice("no session to export");
                return Ok(Some(String::new()));
            };
            let html = export_html(session);
            let path = parts
                .get(1)
                .map(|p| std::path::PathBuf::from(*p))
                .unwrap_or_else(|| std::path::PathBuf::from("session-export.html"));
            tokio::fs::write(&path, html).await?;
            app.set_notice(format!("exported {}", path.display()));
            Ok(Some(String::new()))
        }
        Some("/model") => {
            let Some(spec) = parts.get(1) else {
                app.open_model_picker();
                return Ok(Some(String::new()));
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
                    app.set_notice(format!(
                        "model → {} / {}",
                        providers.provider_id,
                        providers.as_llm().model(),
                    ));
                }
                Err(err) => app.set_notice(format!("model: {err}")),
            }
            Ok(Some(String::new()))
        }
        Some("/thinking") => {
            if parts.get(1).is_none() {
                // Bare /thinking → secondary level picker float.
                app.open_thinking_float();
                return Ok(Some(String::new()));
            }
            let level = parts
                .get(1)
                .and_then(|s| ThinkingLevel::parse(s))
                .unwrap_or(ThinkingLevel::Off);
            runtime.set_thinking_level(level).await?;
            app.set_thinking_level(level.as_str());
            app.set_notice(format!("thinking → {}", level.as_str()));
            Ok(Some(String::new()))
        }
        _ => {
            // Unknown /cmd — if it matches a prompt template, let agent handle via resolve.
            // Otherwise show hint only for bare unknown commands without template.
            if runtime.resources.resolve_input(text).text != text {
                return Ok(None); // expanded by resolve in prompt path — wait, handle_slash returns None means send as prompt
            }
            // If it's a known skill path already handled; treat other /foo as prompt template attempt.
            Ok(None)
        }
    }
}

/// Open a past session and mirror messages into the TUI transcript.
async fn load_session_into_app(
    runtime: &mut AppRuntime,
    app: &mut App,
    info: &one_session::SessionInfo,
) -> Result<(), Box<dyn std::error::Error>> {
    runtime.open_session_path(&info.path).await?;
    app.messages.clear();
    let msgs = runtime.agent.lock().await.messages.clone();
    for m in msgs {
        match m {
            AgentMessage::User(u) => {
                let t = match u.content {
                    one_core::message::UserContent::Text(t) => t,
                    one_core::message::UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            one_core::message::TextOrImage::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                app.push_user(t);
            }
            AgentMessage::Assistant(a) => {
                let t = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        one_core::message::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !t.is_empty() {
                    app.push_assistant(t);
                }
            }
            _ => {}
        }
    }
    app.set_thinking_level(runtime.thinking_level().await.as_str());
    refresh_usage(app, runtime).await;
    app.set_notice(format!(
        "resumed {}",
        info.name
            .clone()
            .unwrap_or_else(|| info.id.chars().take(8).collect())
    ));
    Ok(())
}
