use std::sync::{Arc, Mutex};

use one_core::agent::{LlmProvider, ThinkingLevel};
use one_core::error::OneError;
use one_core::events::AgentEvent;
use one_core::message::AgentMessage;
use one_tui::{
    App, ApprovalAnswer, ApprovalPrompt, ConfigOp, ForceQuit, ModelChoice, RunOutcome, SelectKind,
    TerminalSession,
};

use crate::approval::{ApprovalChoice, PermissionGate};
use crate::hitl::HitlChannel;
use crate::provider::ProviderSet;
use crate::runtime::{AgentMode, AppRuntime};
use one_session::export_html;

/// Poll permission gate + ask_user HITL and feed TUI answers back.
///
/// **Order matters**: deliver answers *before* re-surfacing pending prompts.
/// `set_approval_prompt` / `set_select_prompt` clear stored answers; if we
/// re-open the dock first, Enter looks like a no-op (result wiped, UI stays).
fn drain_hitl(app: &mut App, gate: &PermissionGate, hitl: &HitlChannel) {
    if let Some(answer) = app.take_approval_answer() {
        let choice = match answer {
            ApprovalAnswer::Always => ApprovalChoice::Always,
            ApprovalAnswer::Once => ApprovalChoice::Once,
            ApprovalAnswer::Session => ApprovalChoice::Session,
            ApprovalAnswer::Deny { feedback } => ApprovalChoice::Deny { feedback },
        };
        let _ = gate.respond(choice);
    }
    if let Some((kind, result)) = app.take_select_result() {
        if matches!(kind, SelectKind::AskUser { .. }) {
            let _ = hitl.respond(result);
        }
    }

    // Only open a dock when the UI is not already showing one.
    if app.select_kind().is_none() {
        if let Some(req) = gate.poll_request() {
            app.set_approval_prompt(ApprovalPrompt {
                id: req.id,
                tool: req.tool,
                summary: req.summary,
                reason: req.reason,
            });
        } else if let Some(req) = hitl.poll_request() {
            app.set_select_prompt(SelectKind::AskUser { id: req.id }, req.prompt);
        }
    }
}

fn cancel_hitl(app: &mut App, gate: &PermissionGate, hitl: &HitlChannel) {
    gate.cancel_pending();
    hitl.cancel_pending();
    app.clear_approval_prompt();
    app.clear_select_prompt();
    let _ = app.take_select_result();
}

/// Short label for turn footers / compact chrome: just the model id.
fn format_mode_label(providers: &ProviderSet) -> String {
    providers.as_llm().model().to_string()
}

/// Refresh model picker + Settings lists after models.json CRUD.
fn refresh_model_catalog(app: &mut App, providers: &ProviderSet) {
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
    app.set_settings_catalog(
        providers.providers_rows(),
        providers.models_rows(None),
        providers.provider_field_rows(),
    );
}

async fn apply_switch_model(
    runtime: &mut AppRuntime,
    providers: &mut ProviderSet,
    app: &mut App,
    provider_name: &str,
    model: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    match providers.switch_named(provider_name, model) {
        Ok(()) => {
            app.set_mode_label(format_mode_label(providers));
            app.set_current_model(&providers.provider_id, providers.as_llm().model());
            if let Some(session) = &mut runtime.session {
                session
                    .append_model_change(providers.provider_id.clone(), providers.as_llm().model())
                    .await?;
            }
            let ctx = providers.context_window();
            app.set_context_window(ctx);
            runtime.set_context_window(ctx);
            // Keep nested task harness on the same provider after model switch.
            runtime.bind_task_provider(providers.as_arc()).await;
            app.set_notice(format!(
                "model → {} / {}",
                providers.provider_id,
                providers.as_llm().model(),
            ));
        }
        Err(err) => app.set_notice(format!("model: {err}")),
    }
    Ok(())
}

fn refresh_skills_rows(app: &mut App, runtime: &AppRuntime) {
    let rows: Vec<(String, String, String, bool)> = runtime
        .resources
        .all_skills()
        .iter()
        .map(|s| {
            let path = s.location.to_string_lossy().to_string();
            let label = s.name.clone();
            let mut detail = s.description.clone();
            if detail.chars().count() > 80 {
                detail = detail.chars().take(77).collect::<String>() + "…";
            }
            if s.disable_model_invocation {
                detail = format!("[manual only] {detail}");
            }
            (path, label, detail, s.enabled)
        })
        .collect();
    app.set_skills_rows(rows);
}

fn refresh_mcp_rows(app: &mut App, runtime: &AppRuntime) {
    // Keep the panel high-level: name + coarse status only (no source/URL/errors).
    let rows: Vec<(String, String, String, bool)> = runtime
        .mcp
        .server_rows()
        .into_iter()
        .map(|r| {
            let label = r.name.clone();
            let detail = match r.status.as_str() {
                "ok" => r.detail, // e.g. "3 tools"
                "off" => "off".into(),
                "error" => "unavailable".into(),
                "…" | "loading" => "starting…".into(),
                _ => r.detail,
            };
            (r.name, label, detail, r.enabled)
        })
        .collect();
    app.set_mcp_rows(rows, runtime.mcp.settings_summary());
    refresh_mcp_chip(app, runtime);
}

fn refresh_mcp_import_rows(app: &mut App, runtime: &AppRuntime) {
    let rows: Vec<(String, String, String, bool)> = match runtime.mcp.list_import_candidates(&runtime.cwd)
    {
        Ok(cands) => cands
            .into_iter()
            .map(|c| {
                let transport = if c.server.is_http() { "http" } else { "stdio" };
                let detail = format!("{} · {transport}", c.source.as_str());
                let label = c.name.clone();
                (c.name, label, detail, c.already_owned)
            })
            .collect(),
        Err(e) => {
            app.set_notice(format!("mcp scan: {e}"));
            Vec::new()
        }
    };
    app.set_mcp_import_rows(rows);
}

/// Update the status-bar / prompt-meta chip (`MCP 4/5…`) from live manager state.
fn refresh_mcp_chip(app: &mut App, runtime: &AppRuntime) {
    match runtime.mcp.progress_handle().chip() {
        Some(chip) => {
            let kind = match chip.kind {
                one_mcp::McpChipKind::Loading => 1,
                one_mcp::McpChipKind::Ok => 2,
                one_mcp::McpChipKind::Partial => 3,
                one_mcp::McpChipKind::Error => 4,
            };
            app.set_mcp_chip(chip.text, kind);
        }
        None => app.clear_mcp_chip(),
    }
}

async fn apply_config_op(
    runtime: &mut AppRuntime,
    providers: &mut ProviderSet,
    app: &mut App,
    op: ConfigOp,
) {
    match op {
        ConfigOp::ProviderAdd { id, base_url } => {
            // Create provider in Settings; set base_url later on provider detail (in-float).
            let mut kv = Vec::new();
            if let Some(url) = base_url.filter(|u| !u.is_empty()) {
                kv.push(("base_url".into(), url));
            }
            match providers.provider_add(&id, &kv) {
                Ok(msg) => {
                    refresh_model_catalog(app, providers);
                    app.set_notice(msg);
                    app.settings_provider_focus = id.clone();
                    app.reopen_settings_provider_detail();
                }
                Err(err) => app.set_notice(format!("provider: {err}")),
            }
        }
        ConfigOp::ProviderSet { id, key, value } => {
            match providers.provider_set(&id, &key, &value) {
                Ok(msg) => {
                    refresh_model_catalog(app, providers);
                    app.set_notice(msg);
                    app.settings_provider_focus = id;
                    app.reopen_settings_provider_detail();
                }
                Err(err) => app.set_notice(format!("provider: {err}")),
            }
        }
        ConfigOp::ProviderRm { id } => match providers.provider_rm(&id) {
            Ok(msg) => {
                refresh_model_catalog(app, providers);
                app.set_notice(msg);
                app.settings_provider_focus.clear();
                app.open_settings_providers(&app.settings_provider_rows.clone());
            }
            Err(err) => app.set_notice(format!("provider: {err}")),
        },
        ConfigOp::ProviderFetchModels { id } => {
            // Toast may already say "fetching…" (painted before await); refresh if not.
            let already = app
                .toast_active()
                .is_some_and(|t| t.text.contains("fetching"));
            if !already {
                app.set_notice(format!("fetching models for `{id}` · GET /models…"));
            }
            match providers.remote_model_rows(&id).await {
                Ok(rows) => {
                    let fetched = rows.len();
                    // Batch-import into models.json (single write) — no one-by-one Enter.
                    match providers.model_add_batch(&id, &rows) {
                        Ok(msg) => {
                            refresh_model_catalog(app, providers);
                            app.settings_provider_focus = id.clone();
                            app.open_settings_models_for_provider(&id);
                            app.set_notice(format!("fetched {fetched} · {msg}"));
                        }
                        Err(err) => {
                            app.settings_provider_focus = id;
                            app.set_notice(format!("models import: {err}"));
                        }
                    }
                }
                Err(err) => {
                    // Stay on the current Settings screen (Models / provider detail) so
                    // the user can fix base_url / api_key without losing their place.
                    app.settings_provider_focus = id;
                    app.set_notice(format!("models: {err}"));
                }
            }
        }
        ConfigOp::ModelAdd {
            spec,
            name,
            context_window,
        } => {
            let mut kv = Vec::new();
            if let Some(n) = name {
                kv.push(("name".into(), n));
            }
            if let Some(ctx) = context_window {
                kv.push(("ctx".into(), ctx.to_string()));
            }
            match providers.model_add(&spec, &kv) {
                Ok(msg) => {
                    refresh_model_catalog(app, providers);
                    app.set_notice(msg);
                    app.model_draft = None;
                    app.settings_form_edit = None;
                    let provider = spec
                        .split_once(':')
                        .map(|(p, _)| p.to_string())
                        .unwrap_or_else(|| app.settings_provider_focus.clone());
                    app.open_settings_models_for_provider(&provider);
                }
                Err(err) => app.set_notice(format!("model: {err}")),
            }
        }
        ConfigOp::ModelSet { spec, key, value } => match providers.model_set(&spec, &key, &value) {
            Ok(msg) => {
                refresh_model_catalog(app, providers);
                app.set_notice(msg);
                let provider = spec
                    .split_once(':')
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_else(|| app.settings_provider_focus.clone());
                app.open_settings_models_for_provider(&provider);
            }
            Err(err) => app.set_notice(format!("model: {err}")),
        },
        ConfigOp::ModelRm { spec } => match providers.model_rm(&spec) {
            Ok(msg) => {
                refresh_model_catalog(app, providers);
                app.set_notice(msg);
                let provider = spec
                    .split_once(':')
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_else(|| app.settings_provider_focus.clone());
                app.open_settings_models_for_provider(&provider);
            }
            Err(err) => app.set_notice(format!("model: {err}")),
        },
        ConfigOp::SettingSet { key, value } => {
            let mut s = crate::settings::load();
            let apply_value = match (key.as_str(), value.as_str()) {
                ("auto_approve", "toggle") => {
                    let cur = s.auto_approve.unwrap_or(false);
                    if cur { "false" } else { "true" }.to_string()
                }
                ("sandbox", "cycle") => {
                    let cur = s.sandbox.as_deref().unwrap_or("workspace-write");
                    if cur == "full-access" {
                        "workspace-write".to_string()
                    } else {
                        "full-access".to_string()
                    }
                }
                (_, v) => v.to_string(),
            };
            match crate::settings::set_key(&mut s, &key, &apply_value) {
                Ok(()) => {
                    let _ = crate::settings::save(&s);
                    app.set_notice(format!("settings.{key} = {apply_value}"));
                    app.open_settings_float();
                }
                Err(err) => app.set_notice(format!("settings: {err}")),
            }
        }
        ConfigOp::SkillToggle { path } => {
            let path_buf = std::path::PathBuf::from(&path);
            let current = runtime
                .resources
                .find_skill_by_path(&path_buf)
                .map(|s| s.enabled)
                .unwrap_or(true);
            let new_enabled = !current;
            match runtime.set_skill_enabled(&path_buf, new_enabled).await {
                Ok(_) => {
                    refresh_skills_rows(app, runtime);
                    let name = runtime
                        .resources
                        .find_skill_by_path(&path_buf)
                        .map(|s| s.name.as_str())
                        .unwrap_or("skill");
                    app.set_notice(format!(
                        "skill `{name}` → {}",
                        if new_enabled { "enabled" } else { "disabled" }
                    ));
                    app.reopen_skills_float();
                }
                Err(err) => app.set_notice(format!("skill: {err}")),
            }
        }
        ConfigOp::McpToggle { name } => {
            let currently_on = runtime.mcp.is_server_enabled(&name);
            let new_enabled = !currently_on;
            match runtime.set_mcp_server_enabled(&name, new_enabled).await {
                Ok(()) => {
                    refresh_mcp_rows(app, runtime);
                    app.set_notice(format!(
                        "mcp `{name}` → {}",
                        if new_enabled { "enabled" } else { "disabled" }
                    ));
                    app.reopen_mcp_float();
                }
                Err(err) => app.set_notice(format!("mcp: {err}")),
            }
        }
        ConfigOp::McpImport { names, force } => {
            match runtime
                .import_mcp_from_agents(&names, None, force)
                .await
            {
                Ok(report) => {
                    refresh_mcp_rows(app, runtime);
                    let mut parts = Vec::new();
                    if !report.imported.is_empty() {
                        parts.push(format!("imported {}", report.imported.join(", ")));
                    }
                    if !report.replaced.is_empty() {
                        parts.push(format!("replaced {}", report.replaced.join(", ")));
                    }
                    if !report.skipped_existing.is_empty() && parts.is_empty() {
                        parts.push(format!(
                            "skipped {} (already in One · Enter again to force)",
                            report.skipped_existing.len()
                        ));
                    }
                    if parts.is_empty() {
                        app.set_notice("mcp import: nothing to do");
                    } else {
                        app.set_notice(format!("mcp {}", parts.join(" · ")));
                    }
                    // Stay on MCP manager after import.
                    app.open_mcp_float();
                }
                Err(err) => app.set_notice(format!("mcp import: {err}")),
            }
        }
    }
}

/// Result of handling a slash command.
enum SlashAction {
    /// Not a slash command / pass through as user prompt.
    Pass,
    /// Handled; no further action (optional empty assistant bubble suppressed).
    Consumed,
    /// Run a full agent turn with this text as the user prompt.
    Prompt(String),
    /// OAuth login/logout — needs TerminalSession suspend (see main loop).
    LoginLogout { is_login: bool, args: Vec<String> },
}

pub async fn run_interactive(
    runtime: &mut AppRuntime,
    providers: &mut ProviderSet,
    initial: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new("one");
    app.set_agent_label(runtime.mode().label());
    app.set_mode_label(format_mode_label(providers));
    refresh_model_catalog(&mut app, providers);
    app.set_current_model(&providers.provider_id, providers.as_llm().model());
    app.set_thinking_level(runtime.thinking_level().await.as_str());
    let ctx = providers.context_window();
    app.set_context_window(ctx);
    runtime.set_context_window(ctx);
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
    refresh_skills_rows(&mut app, runtime);
    refresh_mcp_rows(&mut app, runtime);
    let visible = runtime.resources.model_visible_skills();
    let total = runtime.resources.all_skills().len();
    let disabled = total.saturating_sub(visible.len());
    if total > 0 {
        let names: Vec<_> = visible.iter().map(|s| s.name.as_str()).collect();
        let note = if disabled > 0 {
            format!(
                "skills ready ({}/{} on) · /skills to manage · {}",
                visible.len(),
                total,
                names.join(", ")
            )
        } else {
            format!(
                "skills ready ({}) · /skills to manage · agent auto-reads when relevant",
                names.join(", ")
            )
        };
        app.set_notice(note);
    }

    if one_ai::cache::debug_cache_enabled() {
        // Quiet footer-style notice once; dump always goes to disk by default.
        app.set_notice(format!(
            "cache-debug · {}",
            one_ai::cache::debug_cache_latest_path().display()
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
            .wait_action_with(&mut app, |app| {
                // Live MCP 4/5 chip while servers connect in the background.
                refresh_mcp_chip(app, runtime);
            })
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
                    SlashAction::LoginLogout { is_login, args } => {
                        if is_login {
                            handle_login_slash(
                                runtime,
                                providers,
                                &mut app,
                                &mut terminal,
                                &args,
                            )
                            .await?;
                        } else {
                            handle_logout_slash(providers, &mut app, &args).await?;
                        }
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
                // Shift+Tab: Plan ↔ Build (thinking lives in /settings).
                match runtime.mode() {
                    AgentMode::Act => match runtime.enter_plan_mode().await {
                        Ok(path) => {
                            app.set_agent_label(AgentMode::Plan.label());
                            app.set_notice(format!(
                                "plan mode · {} · S-Tab or /act to leave",
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
            RunOutcome::SwitchModel { provider, model } => {
                apply_switch_model(runtime, providers, &mut app, &provider, model).await?;
                terminal
                    .draw(&mut app)
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            RunOutcome::OpenMcpPanel => {
                refresh_mcp_rows(&mut app, runtime);
                app.open_mcp_float();
                terminal
                    .draw(&mut app)
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            RunOutcome::OpenMcpImportPanel => {
                refresh_mcp_import_rows(&mut app, runtime);
                app.open_mcp_import_float();
                terminal
                    .draw(&mut app)
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            RunOutcome::ConfigOp(op) => {
                // Paint "fetching…" before the network await so Ctrl+F feels responsive.
                if let ConfigOp::ProviderFetchModels { ref id } = op {
                    app.set_notice(format!("fetching models for `{id}` · GET /models…"));
                    terminal
                        .draw(&mut app)
                        .map_err(|e| -> Box<dyn std::error::Error> { e })?;
                }
                apply_config_op(runtime, providers, &mut app, op).await;
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
    app.set_usage_cache(usage.cache_read_tokens, usage.cache_write_tokens);
    // Rough blended cost (USD / 1M tokens) — cache read/write discounted when known.
    let cost = estimate_cost_usd(
        &app.current_provider,
        &app.current_model,
        &usage,
    );
    app.set_usage_cost_usd(cost);
}

/// Very rough public list prices (USD per 1M tokens). Zero when unknown.
///
/// Accounting:
/// - **Anthropic-style** (`cache_write > 0` or pure Anthropic provider): cache fields are
///   *disjoint* from `input_tokens` — bill input + 1.25× write + 0.1× read + output.
/// - **OpenAI-style**: `cache_read` is a *subset* of `input` — bill uncached at full rate,
///   cached at ~50% (OpenAI automatic prompt cache discount).
fn estimate_cost_usd(provider: &str, model: &str, usage: &one_core::TokenUsage) -> f64 {
    let (in_rate, out_rate) = match (provider, model) {
        ("openai", m) if m.contains("gpt-4o-mini") => (0.15, 0.60),
        ("openai", m) if m.contains("gpt-4o") => (2.50, 10.0),
        ("anthropic", _) => (3.0, 15.0),
        ("deepseek", m) if m.contains("reasoner") => (0.55, 2.19),
        ("deepseek", _) => (0.27, 1.10),
        ("gemini", m) if m.contains("pro") => (1.25, 10.0),
        ("gemini", _) => (0.15, 0.60),
        ("openrouter", m) if m.contains("anthropic/") || m.contains("claude") => (3.0, 15.0),
        ("openrouter", _) => (0.0, 0.0),
        _ => (0.0, 0.0),
    };
    if in_rate == 0.0 && out_rate == 0.0 {
        return 0.0;
    }
    let per_m = 1_000_000.0;
    let out_cost = (usage.output_tokens as f64 / per_m) * out_rate;

    // Prefer Anthropic disjoint accounting when we saw cache writes, or native Anthropic /
    // OpenRouter Claude (we inject cache_control so creation tokens appear).
    let anthropic_style = provider == "anthropic"
        || usage.cache_write_tokens > 0
        || (provider == "openrouter"
            && (model.contains("anthropic/") || model.contains("claude")));

    let in_cost = if anthropic_style {
        let write_rate = in_rate * 1.25;
        let read_rate = in_rate * 0.10;
        (usage.input_tokens as f64 / per_m) * in_rate
            + (usage.cache_write_tokens as f64 / per_m) * write_rate
            + (usage.cache_read_tokens as f64 / per_m) * read_rate
    } else {
        // OpenAI: prompt_tokens includes cached_tokens.
        let uncached = usage.uncached_input_tokens();
        let cached = usage.cache_read_tokens;
        let cache_rate = in_rate * 0.50;
        (uncached as f64 / per_m) * in_rate + (cached as f64 / per_m) * cache_rate
    };
    in_cost + out_cost
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

    // Attach MCP tools that finished loading since the last turn (same as print path).
    if runtime.mcp.is_loading() && runtime.mcp.tool_count() == 0 {
        app.set_notice("waiting for MCP…");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(45);
        while runtime.mcp.is_loading()
            && runtime.mcp.tool_count() == 0
            && tokio::time::Instant::now() < deadline
        {
            refresh_mcp_chip(app, runtime);
            let _ = terminal.draw(app);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    runtime.sync_mcp_tools().await?;
    refresh_mcp_chip(app, runtime);

    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    runtime.subscribe_collector(events.clone()).await;

    let agent = runtime.agent.clone();
    let resolved = runtime.resources.resolve_input(text);
    if let Some(skill) = &resolved.skill {
        app.set_notice(format!("skill → {skill}"));
    }
    let text = resolved.text;
    runtime.maybe_compact(provider.as_ref(), false).await?;
    let _ = runtime
        .extensions
        .emit(&one_ext::ExtensionEvent::UserPromptSubmit {
            text: text.clone(),
        })
        .await;

    let mut before = agent.lock().await.messages.len();
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
    let hitl = runtime.hitl.clone();
    let prompt_result = match terminal
        .run_busy(
            app,
            |app| {
                drain_events(app, &events);
                drain_hitl(app, &gate, &hitl);
                refresh_mcp_chip(app, runtime);
                if app.take_abort() {
                    cancel_hitl(app, &gate, &hitl);
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
            cancel_hitl(app, &gate, &hitl);
            runtime.abort();
            app.end_busy();
            app.finish_stream_with_interrupted(true);
            return Ok(TurnEnd::ForceQuit);
        }
    };
    cancel_hitl(app, &gate, &hitl);

    // Overflow recovery: force compact + retry once.
    let prompt_result = match prompt_result {
        Err(err) if is_overflow(&err) => {
            app.set_notice("context overflow · compacting…");
            let _ = terminal.draw(app);
            runtime.maybe_compact(provider.as_ref(), true).await?;
            // Buffer shrank: avoid `[before..]` panic; keep the in-flight user turn
            // for session append without re-writing already-persisted kept history.
            before = {
                let guard = agent.lock().await;
                guard
                    .messages
                    .iter()
                    .rposition(|m| matches!(m, AgentMessage::User(_)))
                    .unwrap_or(guard.messages.len())
            };
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
            let hitl2 = runtime.hitl.clone();
            match terminal
                .run_busy(
                    app,
                    |app| {
                        drain_events(app, &events2);
                        drain_hitl(app, &gate2, &hitl2);
                        if app.take_abort() {
                            cancel_hitl(app, &gate2, &hitl2);
                            runtime.abort();
                        }
                    },
                    retry,
                )
                .await
            {
                Ok(result) => result,
                Err(ForceQuit) => {
                    cancel_hitl(app, &gate2, &hitl2);
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

    // Always persist new messages — including error / aborted partial turns —
    // so tool results and the user prompt are not lost on crash or resume.
    if let Err(e) = runtime.append_session_delta(before).await {
        tracing::warn!(error = %e, "failed to append session messages after turn");
    }
    if let Err(e) = runtime.persist_extension_state().await {
        tracing::warn!(error = %e, "failed to persist extension state after turn");
    }

    match prompt_result {
        Ok(reply) => {
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
            let _ = runtime.take_plan_exit_request();
        }
        Err(err) => {
            // Mid-transcript alert (UI only). Short notice remains for status strip.
            app.push_error_alert(format!("{err}"));
            app.set_notice("error · see transcript".to_string());
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
            // MCP pool is kept; tools may still be loading in background.
            let notice = match runtime.mcp_status_line() {
                Some(mcp) => format!("new session · {mcp}"),
                None => "new session".into(),
            };
            app.set_notice(notice);
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
                        || s.preview
                            .as_deref()
                            .is_some_and(|p| p == *spec || p.contains(spec))
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
                    let detail = format!("{}  {}", s.modified.format("%Y-%m-%d %H:%M"), file);
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
            if parts
                .get(1)
                .is_some_and(|s| *s == "skip" || *s == "--no-run")
            {
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
            runtime.maybe_compact(providers.as_llm(), true).await?;
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
                    refresh_skills_rows(app, runtime);
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
        Some("/skills") => {
            refresh_skills_rows(app, runtime);
            let sub = parts.get(1).copied().unwrap_or("");
            match sub {
                "" | "list" => {
                    if parts.get(1).copied() == Some("list") {
                        let rows: Vec<(String, String)> = runtime
                            .resources
                            .all_skills()
                            .iter()
                            .map(|s| {
                                let status = if s.enabled { "on" } else { "off" };
                                (
                                    format!("{status} · {}", s.name),
                                    s.location.display().to_string(),
                                )
                            })
                            .collect();
                        if rows.is_empty() {
                            app.set_notice("no skills discovered");
                        } else {
                            app.open_info_float("Skills", &rows);
                        }
                    } else {
                        app.open_skills_float();
                    }
                    Ok(SlashAction::Consumed)
                }
                "enable" | "on" | "disable" | "off" => {
                    let name = parts.get(2..).map(|p| p.join(" ")).unwrap_or_default();
                    if name.is_empty() {
                        app.set_notice(format!("usage: /skills {sub} <name>"));
                        return Ok(SlashAction::Consumed);
                    }
                    let Some(skill) = runtime.resources.find_skill(&name) else {
                        app.set_notice(format!("unknown skill `{name}` · /skills list"));
                        return Ok(SlashAction::Consumed);
                    };
                    let path = skill.location.clone();
                    let enable = matches!(sub, "enable" | "on");
                    match runtime.set_skill_enabled(&path, enable).await {
                        Ok(_) => {
                            refresh_skills_rows(app, runtime);
                            app.set_notice(format!(
                                "skill `{name}` → {}",
                                if enable { "enabled" } else { "disabled" }
                            ));
                        }
                        Err(err) => app.set_notice(format!("skill: {err}")),
                    }
                    Ok(SlashAction::Consumed)
                }
                other => {
                    app.set_notice(format!(
                        "unknown /skills {other} · try: /skills | enable|disable <name> | list"
                    ));
                    Ok(SlashAction::Consumed)
                }
            }
        }
        Some("/mcp") => {
            refresh_mcp_rows(app, runtime);
            let sub = parts.get(1).copied().unwrap_or("");
            match sub {
                "" | "list" | "status" => {
                    if matches!(sub, "list" | "status") {
                        let rows: Vec<(String, String)> = runtime
                            .mcp
                            .server_rows()
                            .into_iter()
                            .map(|r| {
                                let status = match r.status.as_str() {
                                    "ok" => "ok",
                                    "off" => "off",
                                    "error" => "error",
                                    _ => "…",
                                };
                                (r.name, status.into())
                            })
                            .collect();
                        if rows.is_empty() {
                            app.set_notice("no MCP servers · /mcp");
                        } else {
                            app.open_info_float("MCP", &rows);
                        }
                    } else {
                        app.open_mcp_float();
                    }
                    Ok(SlashAction::Consumed)
                }
                "enable" | "on" | "disable" | "off" => {
                    let name = parts.get(2..).map(|p| p.join(" ")).unwrap_or_default();
                    if name.is_empty() {
                        app.set_notice(format!("usage: /mcp {sub} <server>"));
                        return Ok(SlashAction::Consumed);
                    }
                    let enable = matches!(sub, "enable" | "on");
                    match runtime.set_mcp_server_enabled(&name, enable).await {
                        Ok(()) => {
                            refresh_mcp_rows(app, runtime);
                            app.set_notice(format!(
                                "mcp `{name}` → {}",
                                if enable { "enabled" } else { "disabled" }
                            ));
                        }
                        Err(err) => app.set_notice(format!("mcp: {err}")),
                    }
                    Ok(SlashAction::Consumed)
                }
                "import" => {
                    // `/mcp import` → panel; `/mcp import all` → import all
                    let rest = parts.get(2).copied().unwrap_or("");
                    if rest == "all" {
                        match runtime.import_mcp_from_agents(&[], None, false).await {
                            Ok(report) => {
                                refresh_mcp_rows(app, runtime);
                                app.set_notice(format!(
                                    "mcp imported {} · skipped {}",
                                    report.imported.len(),
                                    report.skipped_existing.len()
                                ));
                            }
                            Err(err) => app.set_notice(format!("mcp import: {err}")),
                        }
                    } else {
                        refresh_mcp_import_rows(app, runtime);
                        app.open_mcp_import_float();
                    }
                    Ok(SlashAction::Consumed)
                }
                other => {
                    app.set_notice(format!(
                        "unknown /mcp {other} · try: /mcp | import | enable|disable <name> | list"
                    ));
                    Ok(SlashAction::Consumed)
                }
            }
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
                        SessionEntry::Custom { .. } | SessionEntry::CustomMessage { .. } => {
                            "custom"
                        }
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
                // Bare /model → docked select (same as Ctrl+L).
                app.open_model_select();
                return Ok(SlashAction::Consumed);
            };

            let (provider_name, model) = if let Some((p, m)) = spec.split_once(':') {
                (p.to_string(), Some(m.to_string()))
            } else {
                ((*spec).to_string(), parts.get(2).map(|s| (*s).to_string()))
            };

            apply_switch_model(runtime, providers, app, &provider_name, model).await?;
            Ok(SlashAction::Consumed)
        }
        Some("/login") => {
            let args: Vec<String> = parts.iter().skip(1).map(|s| (*s).to_string()).collect();
            // Bare `/login` (or only flags) → TUI float picker. With provider id → suspend login.
            let has_provider = args.iter().any(|a| !a.starts_with('-'));
            if !has_provider {
                app.open_login_float(&login_provider_rows());
                return Ok(SlashAction::Consumed);
            }
            Ok(SlashAction::LoginLogout {
                is_login: true,
                args,
            })
        }
        Some("/logout") => {
            let args: Vec<String> = parts.iter().skip(1).map(|s| (*s).to_string()).collect();
            if args.is_empty() {
                app.open_logout_float(&logout_provider_rows());
                return Ok(SlashAction::Consumed);
            }
            Ok(SlashAction::LoginLogout {
                is_login: false,
                args,
            })
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
                            // Re-resolve via provider (settings override > model registry).
                            let n = providers.context_window();
                            app.set_context_window(n);
                            runtime.set_context_window(n);
                        }
                        if key.eq_ignore_ascii_case("provider") || key.eq_ignore_ascii_case("model")
                        {
                            app.set_notice(format!(
                                "settings.{key} = {value} · Ctrl+L to switch live"
                            ));
                        } else {
                            app.set_notice(format!("settings.{key} = {value}"));
                        }
                    }
                    Err(err) => app.set_notice(format!("settings: {err}")),
                }
            } else {
                // Bare /settings → center Settings panel (same as Ctrl+G).
                app.open_settings_float();
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
fn open_rewind_menu(runtime: &AppRuntime, app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
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

    // Prefer structured restore so images stay as vision bytes, not
    // `[image · png · NKB]` labels (which the model cannot see).
    let (prompt_text, images) = session
        .user_prompt_for_edit(entry_id)
        .ok_or_else(|| format!("rewind: entry not a user prompt: {entry_id}"))?;

    session.rewind_before(entry_id)?;

    {
        let mut agent = runtime.agent.lock().await;
        agent.messages.clear();
        session.load_messages_into(&mut agent.messages);
        rebuild_tui_from_agent(app, &agent.messages);
    }

    app.set_input_for_edit_with_images(prompt_text, images);
    refresh_usage(app, runtime).await;
    let n = app.pending_images.len();
    if n > 0 {
        app.set_notice(format!(
            "rewound · {n} image(s) restored · edit and Enter to re-send"
        ));
    } else {
        app.set_notice("rewound · edit prompt and Enter to re-send");
    }
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
                            thinking, redacted, ..
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
                        one_core::message::ContentBlock::ToolCall {
                            name, arguments, ..
                        } => {
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

/// Rows for TUI login float: `(id, label, detail, logged_in)`.
fn login_provider_rows() -> Vec<(String, String, String, bool)> {
    let storage = one_ai::AuthStorage::create().ok();
    one_ai::oauth_provider_catalog()
        .iter()
        .map(|p| {
            let logged_in = storage
                .as_ref()
                .map(|s| s.has_auth(p.id))
                .unwrap_or(false);
            (
                p.id.to_string(),
                p.name.to_string(),
                p.description.to_string(),
                logged_in,
            )
        })
        .collect()
}

/// Rows for TUI logout float: `(id, label, detail)`.
fn logout_provider_rows() -> Vec<(String, String, String)> {
    let Ok(storage) = one_ai::AuthStorage::create() else {
        return Vec::new();
    };
    storage
        .list()
        .into_iter()
        .map(|id| {
            let status = storage.get_auth_status(&id);
            let kind = status.label.unwrap_or_else(|| "stored".into());
            let name = one_ai::oauth_provider_catalog()
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.to_string())
                .unwrap_or_else(|| id.clone());
            (id, name, format!("type={kind}"))
        })
        .collect()
}

async fn handle_login_slash(
    runtime: &mut AppRuntime,
    providers: &mut ProviderSet,
    app: &mut App,
    terminal: &mut TerminalSession,
    args: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut prefer_device = false;
    let mut prefer_browser = false;
    // None → interactive catalog picker (Codex / OpenCode Zen / Go …).
    let mut provider: Option<String> = None;
    for p in args {
        match p.as_str() {
            "--device-code" | "device" | "device-code" => prefer_device = true,
            "--browser" | "browser" => prefer_browser = true,
            "openai-codex" | "codex" | "chatgpt" => {
                provider = Some(one_ai::PROVIDER_OPENAI_CODEX.to_string());
            }
            "opencode" | "zen" | "opencode-zen" => {
                provider = Some(one_ai::PROVIDER_OPENCODE.to_string());
            }
            "opencode-go" | "go" => {
                provider = Some(one_ai::PROVIDER_OPENCODE_GO.to_string());
            }
            "xai" | "grok" | "supergrok" | "xai-oauth" => {
                provider = Some(one_ai::PROVIDER_XAI.to_string());
            }
            other if !other.starts_with('-') => provider = Some(other.to_string()),
            _ => {}
        }
    }

    // Suspend TUI: OAuth / API-key paste need a normal terminal.
    // Running login inside the alternate screen freezes the UI (no redraw / input).
    terminal
        .suspend()
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;

    eprintln!();
    eprintln!("── one login ──────────────────────────────────────────");
    eprintln!("  Suspended TUI for login. Complete the flow below.");
    if let Some(ref p) = provider {
        eprintln!("  Provider: {p}");
    } else {
        eprintln!("  Provider: (pick from list)");
    }
    eprintln!("───────────────────────────────────────────────────────");
    eprintln!();

    let login = crate::cli::LoginCli {
        provider,
        device_code: prefer_device,
        browser: prefer_browser,
    };
    let login_result = crate::auth_cmd::run_login(login).await;

    // Always resume TUI, even on failure.
    if let Err(e) = terminal.resume() {
        if let Err(login_err) = &login_result {
            eprintln!("login also failed: {login_err}");
        }
        return Err(format!("failed to resume TUI after login: {e}").into());
    }

    match login_result {
        Ok(provider) => {
            // run_login already seeded models.json; reload in-memory catalog.
            providers.reload_models_config();
            refresh_model_catalog(app, providers);
            let default = match provider.as_str() {
                one_ai::PROVIDER_OPENCODE => one_ai::OPENCODE_ZEN_DEFAULT_MODEL,
                one_ai::PROVIDER_OPENCODE_GO => one_ai::OPENCODE_GO_DEFAULT_MODEL,
                one_ai::PROVIDER_XAI => one_ai::XAI_DEFAULT_MODEL,
                _ => one_ai::OPENAI_CODEX_DEFAULT_MODEL,
            };
            apply_switch_model(
                runtime,
                providers,
                app,
                &provider,
                Some(default.to_string()),
            )
            .await?;
            app.set_notice(format!(
                "✓ logged in · {} / {}",
                providers.provider_id,
                providers.as_llm().model()
            ));
        }
        Err(e) => {
            app.set_notice(format!("login failed: {e}"));
        }
    }
    Ok(())
}

async fn handle_logout_slash(
    _providers: &mut ProviderSet,
    app: &mut App,
    args: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let storage = one_ai::AuthStorage::create()?;
    let arg = args
        .first()
        .map(|s| s.as_str())
        .unwrap_or(one_ai::PROVIDER_OPENAI_CODEX);
    if arg == "all" {
        let list = storage.list();
        if list.is_empty() {
            app.set_notice("no stored credentials");
            return Ok(());
        }
        for p in list {
            let _ = storage.logout(&p);
        }
        app.set_notice("logged out all providers");
        return Ok(());
    }
    let provider = match arg {
        "codex" | "chatgpt" | "openai-codex" => one_ai::PROVIDER_OPENAI_CODEX,
        "zen" | "opencode-zen" => one_ai::PROVIDER_OPENCODE,
        "go" => one_ai::PROVIDER_OPENCODE_GO,
        "grok" | "supergrok" | "xai-oauth" => one_ai::PROVIDER_XAI,
        other => other,
    };
    if storage.logout(provider)? {
        app.set_notice(format!("logged out `{provider}`"));
    } else {
        app.set_notice(format!("no credential for `{provider}`"));
    }
    Ok(())
}
