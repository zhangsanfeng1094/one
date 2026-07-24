use std::sync::{Arc, Mutex};

use one_core::agent::{LlmProvider, ThinkingLevel};
use one_core::error::OneError;
use one_core::events::AgentEvent;
use one_core::message::AgentMessage;
use one_tui::{
    App, ApprovalAnswer, ApprovalPrompt, ConfigOp, FloatKind, FloatMenu, ForceQuit, ModelChoice,
    RunOutcome, SelectKind, TerminalSession,
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
            ApprovalAnswer::Prefix => ApprovalChoice::Prefix,
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
                suggested_prefix: req.suggested_prefix,
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

fn refresh_agents_rows(app: &mut App, runtime: &AppRuntime) {
    use crate::runtime::presets::{list_agents, project_agents_dir, user_agents_dir};
    let project = project_agents_dir(&runtime.cwd);
    let user = user_agents_dir();
    let project_s = project.display().to_string();
    let user_s = user.display().to_string();
    let rows: Vec<(String, String, String, String, String)> = list_agents(&runtime.cwd)
        .into_iter()
        .map(|e| {
            let path = e
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "builtin".into());
            let mut detail = if e.description.is_empty() {
                e.tools_preview.clone()
            } else {
                format!("{} · {}", e.description, e.tools_preview)
            };
            if detail.chars().count() > 90 {
                detail = detail.chars().take(87).collect::<String>() + "…";
            }
            let turns = e
                .max_turns
                .map(|t| format!(" · turns={t}"))
                .unwrap_or_default();
            let label = format!("{} [{}]{turns}", e.name, e.source.as_str());
            (e.name, label, detail, path, e.source.as_str().to_string())
        })
        .collect();
    app.set_agents_rows(rows, project_s, user_s);
}

fn refresh_features_rows(app: &mut App, runtime: &AppRuntime) {
    // Show desired settings state (includes pending toggles not yet applied).
    let s = crate::settings::load();
    let desired = crate::runtime::FeatureState::from_settings(&s);
    let mut rows = desired.rows();
    if runtime.features_pending() {
        for row in &mut rows {
            // detail already has description; append pending hint when applied differs.
            let applied_on = runtime.applied_features().is_enabled(&row.0);
            if applied_on != row.3 {
                row.2 = format!("{} · pending /new", row.2);
            }
        }
    }
    app.set_features_rows(rows);
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
    refresh_bg_chip(app, runtime);
}

fn refresh_mcp_import_rows(app: &mut App, runtime: &AppRuntime) {
    let rows: Vec<(String, String, String, bool)> =
        match runtime.mcp.list_import_candidates(&runtime.cwd) {
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

/// Live chip for background bash + agent jobs (`bg:1 · cargo…`). Open `/ps` for detail.
fn refresh_bg_chip(app: &mut App, runtime: &AppRuntime) {
    // Lightweight meta — no stdout/stderr clone on every tick.
    let bash = runtime.bg_registry().list_meta();
    let jobs = runtime
        .agent_jobs()
        .map(|j| j.list())
        .unwrap_or_default();

    let bash_running = bash
        .iter()
        .filter(|t| t.state == one_tools::TaskState::Running)
        .count();
    let jobs_running = jobs
        .iter()
        .filter(|j| j.state == crate::runtime::jobs::JobState::Running)
        .count();
    let running = bash_running + jobs_running;

    // Fail signal: real failures only — not intentional kill.
    let bash_failed = bash.iter().any(|t| {
        matches!(
            t.state,
            one_tools::TaskState::Failed | one_tools::TaskState::TimedOut
        ) || (t.state == one_tools::TaskState::Completed && t.exit_code.unwrap_or(0) != 0)
    });
    let jobs_failed = jobs.iter().any(|j| {
        matches!(j.state, crate::runtime::jobs::JobState::Failed)
            || (matches!(j.state, crate::runtime::jobs::JobState::Completed) && !j.ok)
    });

    if running == 0 && bash.is_empty() && jobs.is_empty() {
        app.clear_bg_chip();
        return;
    }

    if running == 0 {
        // No active work: hide chip unless something failed recently still listed.
        if bash_failed || jobs_failed {
            let n_fail = bash
                .iter()
                .filter(|t| {
                    matches!(
                        t.state,
                        one_tools::TaskState::Failed | one_tools::TaskState::TimedOut
                    ) || (t.state == one_tools::TaskState::Completed
                        && t.exit_code.unwrap_or(0) != 0)
                })
                .count()
                + jobs
                    .iter()
                    .filter(|j| {
                        matches!(j.state, crate::runtime::jobs::JobState::Failed)
                            || (matches!(j.state, crate::runtime::jobs::JobState::Completed)
                                && !j.ok)
                    })
                    .count();
            app.set_bg_chip(format!("bg:{n_fail} · fail · /ps"), 4);
        } else {
            // Completed / killed successfully — clear so chrome stays quiet.
            app.clear_bg_chip();
        }
        return;
    }

    // Prefer the newest running bash command as the label.
    let label = bash
        .iter()
        .filter(|t| t.state == one_tools::TaskState::Running)
        .max_by_key(|t| t.seq)
        .map(|t| {
            let cmd = t.command.trim();
            if cmd.chars().count() > 18 {
                format!("{}…", cmd.chars().take(17).collect::<String>())
            } else {
                cmd.to_string()
            }
        })
        .or_else(|| {
            jobs.iter()
                .filter(|j| j.state == crate::runtime::jobs::JobState::Running)
                .map(|j| {
                    j.description
                        .clone()
                        .unwrap_or_else(|| format!("task·{}", j.agent))
                })
                .next()
        })
        .unwrap_or_else(|| "running".into());

    let kind = if bash_failed || jobs_failed { 3 } else { 1 };
    app.set_bg_chip(format!("bg:{running} · {label}"), kind);
}

/// Selectable `/ps` list rows: `(id, label, detail, hint)`.
///
/// Columns: **status · command · time · short id** (id helps with `bash_output`).
fn background_ps_list_rows(runtime: &AppRuntime) -> Vec<(String, String, String, String)> {
    let bash = runtime.bg_registry().list_meta();
    let jobs = runtime
        .agent_jobs()
        .map(|j| j.list())
        .unwrap_or_default();

    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    for t in &bash {
        rows.push((
            t.id.clone(),
            bash_meta_status_label(t),
            truncate_cmd(&t.command, 48),
            format!("{} · {}", human_elapsed(t.elapsed_ms), short_task_id(&t.id)),
        ));
    }
    for j in &jobs {
        let what = j
            .description
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(j.agent.as_str());
        let progress = match (j.turns, j.max_turns) {
            (Some(t), Some(m)) => format!("{t}/{m}"),
            (Some(t), None) => format!("t{t}"),
            _ => String::new(),
        };
        let time = if progress.is_empty() {
            human_elapsed(j.duration_ms)
        } else {
            format!("{} · {}", human_elapsed(j.duration_ms), progress)
        };
        rows.push((
            j.id.clone(),
            job_status_label(j),
            truncate_cmd(what, 48),
            format!("{} · {}", time, short_task_id(&j.id)),
        ));
    }
    rows
}

/// Detail: title = command, section = status line, body = log lines (one float row each).
fn background_ps_detail(
    runtime: &AppRuntime,
    id: &str,
) -> (String, String, Vec<(String, String)>) {
    if let Some(t) = runtime.bg_registry().get(id) {
        let title = truncate_cmd(&t.command, 56);
        let section = format!("{} · {}", bash_status_line(&t), short_task_id(&t.id));
        let rows = log_lines_as_rows(&t.stdout, &t.stderr, 40);
        return (title, section, rows);
    }
    if let Some(jobs) = runtime.agent_jobs() {
        if let Some(j) = jobs.get(id) {
            let title = j
                .description
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("{} · {}", j.kind, j.agent));
            let section = format!("{} · {}", job_status_line(&j), short_task_id(&j.id));
            let mut rows = Vec::new();
            if let Some(err) = &j.error {
                if !err.is_empty() {
                    rows.push(("!".into(), truncate_cmd(err, 80)));
                }
            }
            if !j.summary.is_empty() {
                rows.extend(text_lines_as_rows(&j.summary, 40));
            }
            if rows.is_empty() {
                rows.push((String::new(), "(no summary yet)".into()));
            }
            return (truncate_cmd(&title, 56), section, rows);
        }
    }
    (
        id.to_string(),
        "not found".into(),
        vec![(String::new(), format!("unknown task `{id}`"))],
    )
}

fn open_background_list_panel(app: &mut App, runtime: &AppRuntime) {
    refresh_bg_chip(app, runtime);
    let rows = background_ps_list_rows(runtime);
    app.open_background_float(&rows);
}

/// Live-refresh an already-open `/ps` list (preserve selection by id).
fn refresh_background_list_if_open(app: &mut App, runtime: &AppRuntime) {
    let kind = app.float.as_ref().map(|f| f.kind);
    if kind != Some(FloatKind::Background) {
        return;
    }
    let sel_id = app
        .float
        .as_ref()
        .and_then(|f| f.selected_entry())
        .map(|e| e.item.id);
    let rows = background_ps_list_rows(runtime);
    app.bg_ps_list = rows.clone();
    app.float = Some(FloatMenu::background_picker(&rows));
    if let (Some(id), Some(f)) = (sel_id, app.float.as_mut()) {
        if let Some(idx) = f
            .filtered_entries()
            .iter()
            .position(|e| e.item.id == id)
        {
            f.selected = idx;
        }
    }
    refresh_bg_chip(app, runtime);
}

/// Live-refresh an already-open `/ps` task detail (status + log tail).
///
/// Follows the tail when the user was already on the last line; otherwise keeps
/// their scroll index so ↑/↓ browsing is not yanked away by new output.
fn refresh_background_detail_if_open(app: &mut App, runtime: &AppRuntime) {
    let kind = app.float.as_ref().map(|f| f.kind);
    if kind != Some(FloatKind::BackgroundDetail) {
        return;
    }
    let Some(id) = app.bg_ps_detail_id.clone() else {
        return;
    };
    let (follow_tail, sel) = app
        .float
        .as_ref()
        .map(|f| {
            let n = f.filtered_entries().len();
            let sel = f.selected;
            (n == 0 || sel + 1 >= n, sel)
        })
        .unwrap_or((true, 0));

    // Keep list cache warm for Esc-back.
    app.bg_ps_list = background_ps_list_rows(runtime);
    let (title, section, rows) = background_ps_detail(runtime, &id);
    app.open_background_detail_float(id, title, &section, &rows);
    // `open_background_detail_float` pins to the last line; restore if scrolled up.
    if !follow_tail {
        if let Some(f) = app.float.as_mut() {
            let n = f.filtered_entries().len();
            if n > 0 {
                f.selected = sel.min(n - 1);
            }
        }
    }
    refresh_bg_chip(app, runtime);
}

/// Apply UI outcomes queued mid-turn (Enter on `/ps`, kill from `/ps` panel, …).
///
/// Avoids anything that needs `agent.lock()` — the streaming task already holds it.
fn apply_busy_ui_outcome(app: &mut App, runtime: &AppRuntime, outcome: RunOutcome) {
    match outcome {
        RunOutcome::OpenBackgroundList => {
            open_background_list_panel(app, runtime);
        }
        RunOutcome::OpenBackgroundDetail { id } => {
            open_background_detail_panel(app, runtime, &id);
        }
        RunOutcome::KillBackground { id } => {
            // Sync kill so list refresh sees Killed immediately (no spawn race).
            kill_background_task_sync(app, runtime, &id);
        }
        RunOutcome::OpenMcpPanel => {
            refresh_mcp_rows(app, runtime);
            app.open_mcp_float();
        }
        RunOutcome::OpenMcpImportPanel => {
            refresh_mcp_import_rows(app, runtime);
            app.open_mcp_import_float();
        }
        RunOutcome::Prompt(text) => {
            // Safe mid-turn subset — no agent.lock / no nested prompt turn.
            let cmd = text.split_whitespace().next().unwrap_or(text.as_str());
            match cmd {
                "/ps" | "/jobs" => {
                    let id = text.split_whitespace().nth(1);
                    if let Some(id) = id {
                        open_background_detail_panel(app, runtime, id);
                    } else {
                        open_background_list_panel(app, runtime);
                    }
                }
                "/login" => {
                    app.open_login_float(&login_provider_rows());
                }
                "/logout" => {
                    app.open_logout_float(&logout_provider_rows());
                }
                "/mcp" => {
                    refresh_mcp_rows(app, runtime);
                    app.open_mcp_float();
                }
                _ => {
                    app.set_notice(format!("busy · `{cmd}` after this turn"));
                }
            }
        }
        _ => {}
    }
}

fn open_background_detail_panel(app: &mut App, runtime: &AppRuntime, id: &str) {
    refresh_bg_chip(app, runtime);
    // Always refresh list cache so Esc-back (if any) is not completely stale.
    app.bg_ps_list = background_ps_list_rows(runtime);
    let (title, section, rows) = background_ps_detail(runtime, id);
    app.open_background_detail_float(id, title, &section, &rows);
}

/// Kill bash task or agent job by id, then refresh `/ps` list (async path).
async fn kill_background_task(app: &mut App, runtime: &AppRuntime, id: &str) {
    kill_background_task_sync(app, runtime, id);
    // Yield so process-group reapers can settle.
    tokio::task::yield_now().await;
}

/// Sync kill + list refresh (safe from busy TUI tick).
fn kill_background_task_sync(app: &mut App, runtime: &AppRuntime, id: &str) {
    let bash = runtime.bg_registry();
    if bash.get(id).is_some() {
        match bash.kill_sync(id) {
            Ok(snap) => {
                app.set_notice(format!("killed {} · {}", snap.id, snap.state.as_str()));
            }
            Err(e) => {
                app.set_notice(format!("kill failed: {e}"));
            }
        }
        open_background_list_panel(app, runtime);
        return;
    }
    if let Some(jobs) = runtime.agent_jobs() {
        match jobs.kill(id) {
            Ok(snap) => {
                app.set_notice(format!("killed {} · {}", snap.id, snap.state.as_str()));
            }
            Err(e) => {
                app.set_notice(format!("kill failed: {e}"));
            }
        }
        open_background_list_panel(app, runtime);
        return;
    }
    app.set_notice(format!("unknown task `{id}`"));
    open_background_list_panel(app, runtime);
}

fn bash_meta_status_label(t: &one_tools::TaskMeta) -> String {
    match t.state {
        one_tools::TaskState::Running => "● run".into(),
        one_tools::TaskState::Completed => match t.exit_code {
            Some(0) => "✓ ok".into(),
            Some(c) => format!("✗ {c}"),
            None => "✓ done".into(),
        },
        one_tools::TaskState::TimedOut => "⏱ out".into(),
        one_tools::TaskState::Killed => "■ kill".into(),
        one_tools::TaskState::Failed => "✗ fail".into(),
    }
}

fn bash_status_line(t: &one_tools::TaskSnapshot) -> String {
    let time = human_elapsed(t.elapsed_ms);
    match t.state {
        one_tools::TaskState::Running => format!("running · {time}"),
        one_tools::TaskState::Completed => match t.exit_code {
            Some(0) => format!("ok · {time}"),
            Some(c) => format!("exit {c} · {time}"),
            None => format!("done · {time}"),
        },
        one_tools::TaskState::TimedOut => format!("timed out · {time}"),
        one_tools::TaskState::Killed => format!("killed · {time}"),
        one_tools::TaskState::Failed => {
            if let Some(err) = &t.error {
                format!("failed · {time} · {}", truncate_cmd(err, 40))
            } else {
                format!("failed · {time}")
            }
        }
    }
}

/// Short id for list hint / detail (strip `bg_` / `job_` prefix when present).
fn short_task_id(id: &str) -> String {
    let bare = id
        .strip_prefix("bg_")
        .or_else(|| id.strip_prefix("job_"))
        .unwrap_or(id);
    if bare.chars().count() > 12 {
        format!("{}…", bare.chars().take(11).collect::<String>())
    } else {
        bare.to_string()
    }
}

fn job_status_label(j: &crate::runtime::jobs::JobSnapshot) -> String {
    match j.state {
        crate::runtime::jobs::JobState::Running => "● job".into(),
        crate::runtime::jobs::JobState::Completed if j.ok => "✓ job".into(),
        crate::runtime::jobs::JobState::Completed => "✗ job".into(),
        crate::runtime::jobs::JobState::Aborted => "■ job".into(),
        crate::runtime::jobs::JobState::Failed => "✗ job".into(),
    }
}

fn job_status_line(j: &crate::runtime::jobs::JobSnapshot) -> String {
    let time = human_elapsed(j.duration_ms);
    let progress = match (j.turns, j.max_turns) {
        (Some(t), Some(m)) => format!(" · turn {t}/{m}"),
        (Some(t), None) => format!(" · turn {t}"),
        _ => String::new(),
    };
    match j.state {
        crate::runtime::jobs::JobState::Running => format!("running{progress} · {time}"),
        crate::runtime::jobs::JobState::Completed if j.ok => format!("ok{progress} · {time}"),
        crate::runtime::jobs::JobState::Completed => format!("failed{progress} · {time}"),
        crate::runtime::jobs::JobState::Aborted => format!("aborted · {time}"),
        crate::runtime::jobs::JobState::Failed => format!("failed · {time}"),
    }
}

/// One float row per log line (float UI is single-line; multi-line blobs are useless).
///
/// Stderr lines get a `!` label so errors stay visible when mixed with stdout.
fn log_lines_as_rows(
    stdout: &str,
    stderr: &str,
    max_lines: usize,
) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    for line in stdout.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        rows.push((String::new(), truncate_cmd(line, 96)));
    }
    for line in stderr.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        rows.push(("!".into(), truncate_cmd(line, 94)));
    }
    if rows.is_empty() {
        return vec![(String::new(), "(no output yet)".into())];
    }
    let start = rows.len().saturating_sub(max_lines);
    rows[start..].to_vec()
}

fn text_lines_as_rows(text: &str, max_lines: usize) -> Vec<(String, String)> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return vec![(String::new(), "(no output yet)".into())];
    }
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .map(|line| {
            // Empty label → full width for the log line in the detail column.
            (String::new(), truncate_cmd(line, 96))
        })
        .collect()
}

fn human_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        let s = ms as f64 / 1000.0;
        if s < 10.0 {
            format!("{s:.1}s")
        } else {
            format!("{}s", ms / 1000)
        }
    } else {
        let m = ms / 60_000;
        let s = (ms / 1000) % 60;
        format!("{m}m{s:02}s")
    }
}

fn truncate_cmd(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
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
            // Feature flags go through runtime so pending-/new policy applies.
            if key.starts_with("feature.") || key.starts_with("features.") {
                let id = key
                    .split_once('.')
                    .map(|(_, rest)| rest.trim())
                    .unwrap_or("")
                    .to_string();
                let current = runtime.applied_features().is_enabled(&id);
                let enabled = match value.as_str() {
                    "toggle" => !current,
                    other => match crate::runtime::features::parse_bool_token(other, current) {
                        Ok(b) => b,
                        Err(err) => {
                            app.set_notice(format!("settings: {err}"));
                            return;
                        }
                    },
                };
                match runtime.set_feature_enabled(&id, enabled).await {
                    Ok((on, applied)) => {
                        refresh_features_rows(app, runtime);
                        if applied {
                            app.set_notice(format!(
                                "feature `{id}` → {}",
                                if on { "on" } else { "off" }
                            ));
                        } else {
                            app.set_notice(format!(
                                "feature `{id}` → {} · takes effect on /new",
                                if on { "on" } else { "off" }
                            ));
                        }
                        app.open_features_float();
                    }
                    Err(err) => app.set_notice(format!("feature: {err}")),
                }
                return;
            }
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
                    // set_key already applies tool_output limits for those keys.
                    let lim = one_tools::tool_output_limits();
                    app.set_tool_output_limits(lim.max_lines, lim.max_bytes);
                    sync_compaction_settings(app, &s);
                    app.set_notice(format!("settings.{key} = {apply_value}"));
                    if key.contains("tool_output") {
                        app.open_settings_tool_output();
                    } else if key.contains("compaction") {
                        app.open_settings_compaction();
                    } else {
                        app.open_settings_float();
                    }
                }
                Err(err) => app.set_notice(format!("settings: {err}")),
            }
        }
        ConfigOp::FeatureToggle { id } => {
            // Toggle from settings desired state so pending double-toggles reverse cleanly.
            let s = crate::settings::load();
            let current = crate::runtime::FeatureState::from_settings(&s).is_enabled(&id);
            let new_enabled = !current;
            match runtime.set_feature_enabled(&id, new_enabled).await {
                Ok((on, applied)) => {
                    refresh_features_rows(app, runtime);
                    if applied {
                        app.set_notice(format!(
                            "feature `{id}` → {}",
                            if on { "on" } else { "off" }
                        ));
                    } else {
                        app.set_notice(format!(
                            "feature `{id}` → {} · takes effect on /new",
                            if on { "on" } else { "off" }
                        ));
                    }
                    app.reopen_features_float();
                }
                Err(err) => app.set_notice(format!("feature: {err}")),
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
            match runtime.import_mcp_from_agents(&names, None, force).await {
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
    {
        let lim = one_tools::tool_output_limits();
        app.set_tool_output_limits(lim.max_lines, lim.max_bytes);
        let s = crate::settings::load();
        sync_compaction_settings(&mut app, &s);
    }
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
    refresh_features_rows(&mut app, runtime);
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
            // `list` is already newest-first — do not reverse.
            let rows: Vec<(String, String, String, String)> = sessions
                .into_iter()
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
                // Live bg:N chip for background bash / agent jobs (/ps for detail).
                refresh_bg_chip(app, runtime);
                // Keep open `/ps` list / detail elapsed + log tail current.
                refresh_background_list_if_open(app, runtime);
                refresh_background_detail_if_open(app, runtime);
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
                            handle_login_slash(runtime, providers, &mut app, &mut terminal, &args)
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
            RunOutcome::OpenBackgroundList => {
                open_background_list_panel(&mut app, runtime);
                terminal
                    .draw(&mut app)
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            RunOutcome::OpenBackgroundDetail { id } => {
                open_background_detail_panel(&mut app, runtime, &id);
                terminal
                    .draw(&mut app)
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            RunOutcome::KillBackground { id } => {
                kill_background_task(&mut app, runtime, &id).await;
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
    // Context %: last provider-reported prompt size (OpenCode-style), else char/4.
    let (tokens, estimated) = runtime.context_tokens().await;
    app.set_usage_tokens(tokens);
    app.set_usage_tokens_estimated(estimated);
    // ↑↓ / cost: session-cumulative provider usage (billing semantics).
    let usage = runtime.token_usage().await;
    app.set_usage_io(usage.input_tokens, usage.output_tokens);
    app.set_usage_cache(usage.cache_read_tokens, usage.cache_write_tokens);
    // Rough blended cost (USD / 1M tokens) — cache read/write discounted when known.
    let cost = estimate_cost_usd(&app.current_provider, &app.current_model, &usage);
    app.set_usage_cost_usd(cost);
}

fn sync_compaction_settings(app: &mut App, s: &crate::settings::Settings) {
    let c = s.compaction_or_default();
    app.set_compaction_settings(
        c.auto.unwrap_or(true),
        c.ratio.unwrap_or(one_core::DEFAULT_COMPACT_RATIO),
        c.threshold,
        c.keep_recent.unwrap_or(12),
        c.prune.unwrap_or(false),
        c.prune_protect_tokens
            .unwrap_or(one_core::DEFAULT_PRUNE_PROTECT_TOKENS),
        c.prune_max_chars
            .unwrap_or(one_core::DEFAULT_PRUNE_MAX_CHARS),
    );
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
        || (provider == "openrouter" && (model.contains("anthropic/") || model.contains("claude")));

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
            refresh_bg_chip(app, runtime);
            let _ = terminal.draw(app);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    runtime.sync_mcp_tools().await?;
    refresh_mcp_chip(app, runtime);
    refresh_bg_chip(app, runtime);

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
        .emit(&one_ext::ExtensionEvent::UserPromptSubmit { text: text.clone() })
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
                refresh_bg_chip(app, runtime);
                refresh_background_list_if_open(app, runtime);
                refresh_background_detail_if_open(app, runtime);
                // UI slash / `/ps` while streaming — open floats without waiting for turn end.
                while let Some(outcome) = app.take_busy_ui() {
                    apply_busy_ui_outcome(app, runtime, outcome);
                }
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

/// Build TUI preview text from a tool result.
///
/// Uses `details.patch` when present (edit success) so the transcript can show a
/// real hunk while the model only received the short `content` summary.
fn tool_output_for_ui(output: &one_core::tool::ToolOutput) -> String {
    let summary = output.as_ui_text();
    if let Some(patch) = output
        .details
        .as_ref()
        .and_then(|d| d.get("patch"))
        .and_then(|v| v.as_str())
        .filter(|p| !p.is_empty())
    {
        if summary.is_empty() {
            return patch.to_string();
        }
        return format!("{summary}\n{patch}");
    }
    summary
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
                // Agent ToolResult stores `content` only (model context).
                // `details` (e.g. edit patch) is UI-only and never entered the LLM.
                // Prefer details.patch for edit/write-style diffs when present.
                let text = tool_output_for_ui(&output);
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
        // Codex-style process list: selectable panel → Enter detail.
        Some("/ps") | Some("/jobs") => {
            if let Some(id) = parts.get(1).copied() {
                open_background_detail_panel(app, runtime, id);
            } else {
                open_background_list_panel(app, runtime);
            }
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
            refresh_features_rows(app, runtime);
            // MCP pool is kept; tools may still be loading in background.
            let mut notice = match runtime.mcp_status_line() {
                Some(mcp) => format!("new session · {mcp}"),
                None => "new session".into(),
            };
            notice.push_str(&format!(
                " · features {}",
                runtime.applied_features().fingerprint()
            ));
            app.set_notice(notice);
            refresh_usage(app, runtime).await;
            Ok(SlashAction::Consumed)
        }
        Some("/resume") => {
            // Optional path/id argument → open directly (picker selection re-enters
            // here with the full session path — never re-list all files for that).
            if parts.len() > 1 {
                let spec = parts[1..].join(" ");
                if let Some(info) = resolve_resume_target(runtime, &spec).await? {
                    load_session_into_app(runtime, app, &info).await?;
                } else {
                    app.set_notice(format!("session not found: {spec}"));
                }
                return Ok(SlashAction::Consumed);
            }

            // Secondary float picker (newest first). Lightweight list — no full
            // open of every JSONL (that used to freeze the TUI for seconds).
            app.set_notice("loading sessions…");
            let sessions = runtime.list_sessions().await?;
            if sessions.is_empty() {
                app.set_notice("no sessions for this project");
                return Ok(SlashAction::Consumed);
            }

            let rows: Vec<(String, String, String, String)> = sessions
                .into_iter()
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
                app.set_notice(format!("{} recent · enter to resume", rows.len()));
            }
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
            let (toks, estimated) = runtime.context_tokens().await;
            app.set_notice(format!(
                "compacted · {}{} tokens",
                if estimated { "~" } else { "" },
                toks
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
        Some("/agents") => {
            refresh_agents_rows(app, runtime);
            let sub = parts.get(1).copied().unwrap_or("");
            match sub {
                "" => {
                    app.open_agents_float();
                    Ok(SlashAction::Consumed)
                }
                "list" => {
                    use crate::runtime::presets::list_agents;
                    let rows: Vec<(String, String)> = list_agents(&runtime.cwd)
                        .into_iter()
                        .map(|e| {
                            let path = e
                                .path
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "builtin".into());
                            (format!("{} [{}]", e.name, e.source.as_str()), path)
                        })
                        .collect();
                    if rows.is_empty() {
                        app.set_notice("no agents · add .one/agents/<name>.json");
                    } else {
                        app.open_info_float("Agents", &rows);
                    }
                    Ok(SlashAction::Consumed)
                }
                "path" | "inspect" => {
                    let name = parts.get(2..).map(|p| p.join(" ")).unwrap_or_default();
                    if name.is_empty() {
                        app.set_notice(format!("usage: /agents {sub} <name>"));
                        return Ok(SlashAction::Consumed);
                    }
                    use crate::runtime::presets::{list_agents, resolve_agent_path};
                    refresh_agents_rows(app, runtime);
                    if let Some(p) = resolve_agent_path(&name, &runtime.cwd) {
                        app.set_notice(format!("{name} · {}", p.display()));
                    } else if list_agents(&runtime.cwd).iter().any(|e| e.name == name) {
                        app.set_notice(format!("{name} · builtin (no disk file)"));
                    } else {
                        app.set_notice(format!("unknown agent `{name}` · /agents"));
                        return Ok(SlashAction::Consumed);
                    }
                    if sub == "inspect" {
                        app.open_agent_detail_float(&name);
                    }
                    Ok(SlashAction::Consumed)
                }
                "dirs" => {
                    use crate::runtime::presets::{project_agents_dir, user_agents_dir};
                    let rows = vec![
                        (
                            "project".into(),
                            project_agents_dir(&runtime.cwd).display().to_string(),
                        ),
                        ("user".into(), user_agents_dir().display().to_string()),
                    ];
                    app.open_info_float("Agent directories", &rows);
                    Ok(SlashAction::Consumed)
                }
                other => {
                    app.set_notice(format!(
                        "unknown /agents {other} · try: /agents | list | path|inspect <name> | dirs"
                    ));
                    Ok(SlashAction::Consumed)
                }
            }
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
            // /settings features — list effective + pending
            if parts.get(1).is_some_and(|p| {
                p.eq_ignore_ascii_case("features") || p.eq_ignore_ascii_case("feature")
            }) && parts.len() == 2
            {
                refresh_features_rows(app, runtime);
                app.open_features_float();
                if let Some(n) = runtime.features_pending_notice() {
                    app.set_notice(n);
                }
                return Ok(SlashAction::Consumed);
            }
            // /settings feature <id> <on|off|toggle>
            if parts
                .get(1)
                .is_some_and(|p| p.eq_ignore_ascii_case("feature"))
                && parts.len() >= 4
            {
                let id = parts[2];
                let value = parts[3..].join(" ");
                let s = crate::settings::load();
                let current = crate::runtime::FeatureState::from_settings(&s).is_enabled(id);
                match crate::runtime::features::parse_bool_token(&value, current) {
                    Ok(enabled) => match runtime.set_feature_enabled(id, enabled).await {
                        Ok((on, applied)) => {
                            refresh_features_rows(app, runtime);
                            if applied {
                                app.set_notice(format!(
                                    "feature `{id}` → {}",
                                    if on { "on" } else { "off" }
                                ));
                            } else {
                                app.set_notice(format!(
                                    "feature `{id}` → {} · takes effect on /new",
                                    if on { "on" } else { "off" }
                                ));
                            }
                        }
                        Err(err) => app.set_notice(format!("feature: {err}")),
                    },
                    Err(err) => app.set_notice(format!("settings: {err}")),
                }
                return Ok(SlashAction::Consumed);
            }
            if parts.len() >= 3 {
                let key = parts[1];
                let value = parts[2..].join(" ");
                // feature.<id> / features.<id> → runtime policy
                if key.to_ascii_lowercase().starts_with("feature.")
                    || key.to_ascii_lowercase().starts_with("features.")
                {
                    let id = key
                        .split_once('.')
                        .map(|(_, rest)| rest.trim())
                        .unwrap_or("");
                    let s = crate::settings::load();
                    let current = crate::runtime::FeatureState::from_settings(&s).is_enabled(id);
                    match crate::runtime::features::parse_bool_token(&value, current) {
                        Ok(enabled) => match runtime.set_feature_enabled(id, enabled).await {
                            Ok((on, applied)) => {
                                refresh_features_rows(app, runtime);
                                if applied {
                                    app.set_notice(format!(
                                        "feature `{id}` → {}",
                                        if on { "on" } else { "off" }
                                    ));
                                } else {
                                    app.set_notice(format!(
                                        "feature `{id}` → {} · takes effect on /new",
                                        if on { "on" } else { "off" }
                                    ));
                                }
                            }
                            Err(err) => app.set_notice(format!("feature: {err}")),
                        },
                        Err(err) => app.set_notice(format!("settings: {err}")),
                    }
                    return Ok(SlashAction::Consumed);
                }
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
                        if key.contains("tool_output") {
                            let lim = one_tools::tool_output_limits();
                            app.set_tool_output_limits(lim.max_lines, lim.max_bytes);
                        }
                        if key.contains("compaction") {
                            sync_compaction_settings(app, &s);
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
                refresh_features_rows(app, runtime);
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

/// Resolve `/resume <spec>` without listing every session when `spec` is a path.
async fn resolve_resume_target(
    runtime: &AppRuntime,
    spec: &str,
) -> Result<Option<one_session::SessionInfo>, Box<dyn std::error::Error>> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(None);
    }

    // Picker selection passes the absolute JSONL path as the item id.
    let as_path = std::path::Path::new(spec);
    if as_path.is_file() {
        if let Some(info) = one_session::SessionManager::list_info(as_path).await {
            return Ok(Some(info));
        }
        // File exists but header unreadable — still try open via synthetic info.
        return Ok(Some(one_session::SessionInfo {
            path: as_path.to_path_buf(),
            id: as_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(spec)
                .to_string(),
            cwd: runtime.cwd.display().to_string(),
            name: None,
            preview: None,
            modified: chrono::Utc::now(),
        }));
    }

    // Fuzzy match against project sessions (id prefix / name / preview / path).
    let sessions = runtime.list_sessions().await?;
    Ok(sessions.into_iter().find(|s| {
        s.path.to_string_lossy().contains(spec)
            || s.id.starts_with(spec)
            || s.name.as_deref().is_some_and(|n| n == spec)
            || s.preview
                .as_deref()
                .is_some_and(|p| p == spec || p.contains(spec))
    }))
}

/// Open a past session and mirror messages into the TUI transcript.
async fn load_session_into_app(
    runtime: &mut AppRuntime,
    app: &mut App,
    info: &one_session::SessionInfo,
) -> Result<(), Box<dyn std::error::Error>> {
    app.set_notice(format!("resuming {}…", info.display_label()));
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
            let logged_in = storage.as_ref().map(|s| s.has_auth(p.id)).unwrap_or(false);
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
