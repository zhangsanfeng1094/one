#![recursion_limit = "1024"]

mod agent_cmd;
mod approval;
mod auth_cmd;
mod bench_cmd;
mod cli;
mod config_studio;
mod control_cmd;
mod governance;
mod hitl;
mod langfuse;
mod learn_cmd;
mod mcp_cmd;
mod modes;
mod preferences;
mod prompt_config;
mod protocol;
mod provider;
mod runtime;
mod session_cmd;
mod settings;

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches};
use one_session::export_html;
use tracing_subscriber::EnvFilter;

use crate::cli::{AcpCli, Cli, Commands, ResumeCli, RunMode};
use crate::protocol::{RunResult, UsageSnapshot};
use crate::provider::ProviderSet;
use crate::runtime::AppRuntime;
use one_session::{SessionError, SessionManager};
use std::process::ExitCode;
use std::time::Instant;

/// Map `one resume [SPEC] [--list]` onto existing `--session` / `--resume` flags.
async fn apply_resume_cli(
    cli: &mut Cli,
    resume: ResumeCli,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = cli.cwd.canonicalize().unwrap_or_else(|_| cli.cwd.clone());

    if resume.list {
        let sessions = SessionManager::list(&cwd).await?;
        if sessions.is_empty() {
            eprintln!("no sessions for {}", cwd.display());
            std::process::exit(1);
        }
        println!(
            "{:<12}  {:<20}  {:<36}  {}",
            "modified", "id", "label", "path"
        );
        for s in sessions.iter().take(40) {
            let id: String = s.id.chars().take(12).collect();
            let label = s.display_label();
            let label = if label.chars().count() > 36 {
                format!("{}…", label.chars().take(35).collect::<String>())
            } else {
                label
            };
            println!(
                "{}  {:<12}  {:<36}  {}",
                s.modified.format("%Y-%m-%d %H:%M"),
                id,
                label,
                s.path.display()
            );
        }
        std::process::exit(0);
    }

    match resume
        .spec
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => {
            // Same as `one -r`: TUI picker, or most-recent in print mode.
            cli.resume = true;
            cli.r#continue = false;
            cli.session = None;
            cli.no_session = false;
        }
        Some(spec) => match SessionManager::resolve(&cwd, spec).await {
            Ok(info) => {
                eprintln!("resume: {} · {}", info.display_label(), info.path.display());
                cli.session = Some(info.path);
                cli.resume = false;
                cli.r#continue = false;
                cli.no_session = false;
            }
            Err(SessionError::Ambiguous { spec, candidates }) => {
                eprintln!("error: ambiguous session `{spec}` — refine the query:");
                for c in candidates {
                    eprintln!("  · {c}");
                }
                std::process::exit(2);
            }
            Err(SessionError::NoSessions) => {
                return Err(format!("no sessions for {}", cwd.display()).into());
            }
            Err(SessionError::NotFound(msg)) => {
                return Err(msg.into());
            }
            Err(err) => return Err(err.into()),
        },
    }
    Ok(())
}

/// Resolve run mode.
///
/// | invocation | mode |
/// |------------|------|
/// | `one` | Interactive |
/// | `one "…"` | Interactive, first turn = positional prompt |
/// | `one exec "…"` | Print/headless |
/// | `one -p "…"` | Print (legacy compat) |
/// | `one --mode interactive -p "…"` | Interactive, first turn = prompt |
/// | `one --tui -p "…"` | Interactive, first turn = prompt |
/// | `one --mode print -p "…"` | Print |
fn resolve_run_mode(cli: &Cli, matches: &clap::ArgMatches) -> RunMode {
    if cli.tui {
        return RunMode::Interactive;
    }
    let mode_explicit = matches.value_source("mode") == Some(ValueSource::CommandLine);
    if mode_explicit {
        return cli.mode.clone();
    }
    // Default mode is Interactive; bare `-p` historically means print/scripts.
    if cli.print.is_some() {
        return RunMode::Print;
    }
    cli.mode.clone()
}

/// Normalize convenience CLI forms without changing the legacy `-p` contract.
///
/// - `one exec PROMPT` becomes explicit print/headless mode.
/// - `--yolo` is the Codex-style convenience for always-approve + full-access.
fn normalize_cli_invocation(cli: &mut Cli) -> Option<RunMode> {
    if cli.yolo {
        cli.always_approve = true;
        cli.auto_approve = true;
        cli.full_access = true;
    }

    if matches!(&cli.command, Some(Commands::Exec(_))) {
        let Some(Commands::Exec(exec)) = cli.command.take() else {
            unreachable!();
        };
        cli.print = Some(exec.prompt);
        if let Some(max_turns) = exec.max_turns {
            cli.max_turns = max_turns;
        }
        if exec.output_format.is_some() {
            cli.output_format = exec.output_format;
        }
        cli.trace |= exec.trace;
        cli.trace_full |= exec.trace_full;
        // `one exec` persists its session by default so exec/TUI can resume it
        // (`one resume <id>` / `one exec --resume <id> ...`). Explicit
        // `--no-session` still wins. Legacy bare `-p` keeps its old
        // non-persisting behavior.
        cli.exec_session = !cli.no_session;
        if let Some(spec) = exec.resume {
            cli.exec_resume = Some(spec);
        }
        if exec.continue_session {
            cli.exec_resume = Some(String::new());
        }
        cli.mode = RunMode::Print;
        return Some(RunMode::Print);
    }
    None
}

fn initial_prompt(cli: &Cli) -> Option<String> {
    cli.prompt.clone().or_else(|| cli.print.clone())
}

fn init_tracing(interactive_tui: bool) {
    // Default to `warn` so panics/errors leave a trail without RUST_LOG; override
    // with RUST_LOG / ONE_LOG (ONE_LOG is accepted as an alias for RUST_LOG).
    if std::env::var_os("RUST_LOG").is_none() {
        if let Ok(one_log) = std::env::var("ONE_LOG") {
            std::env::set_var("RUST_LOG", one_log);
        }
    }
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    if interactive_tui {
        let log_dir = one_session::agent_dir().join("logs");
        let _ = std::fs::create_dir_all(&log_dir);
        let path = log_dir.join("one.log");
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::sync::Mutex::new(file))
                .with_target(true)
                .with_ansi(false)
                .init();
            return;
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();
}

/// Install a panic hook that restores the terminal and writes a panic log.
///
/// Without this, a panic inside the TUI alternate screen often looks like a
/// silent flash-exit: raw mode is cleared by Drop, but the panic text is gone.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Leave alt screen / raw mode first so the following eprintln is visible.
        one_tui::emergency_restore_terminal();

        let log_dir = one_session::agent_dir().join("logs");
        let _ = std::fs::create_dir_all(&log_dir);
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = log_dir.join(format!("panic-{stamp}.log"));

        let mut body = String::new();
        body.push_str(&format!("one panicked at {stamp}\n"));
        body.push_str(&format!("{info}\n"));
        if let Some(loc) = info.location() {
            body.push_str(&format!("location: {loc}\n"));
        }
        // Backtrace only when the user asked for it (or always capture a short one).
        let bt = std::backtrace::Backtrace::force_capture();
        body.push_str(&format!("\nbacktrace:\n{bt}\n"));

        let written = std::fs::write(&path, &body).is_ok();
        // Also append a one-liner to the rolling log when possible.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("one.log"))
        {
            use std::io::Write;
            let _ = writeln!(f, "PANIC {stamp}: see {}", path.display());
        }

        eprintln!();
        eprintln!("one crashed (panic).");
        if written {
            eprintln!("panic log: {}", path.display());
        } else {
            eprintln!("(could not write panic log under {})", log_dir.display());
            eprintln!("{info}");
        }
        eprintln!("set RUST_BACKTRACE=1 for a fuller trace on the next run.");
        eprintln!();

        default_hook(info);
    }));
}

/// Load `.env` files without overriding variables already present in the process.
///
/// Priority (highest → lowest):
/// 1. process env (shell `export`)
/// 2. cwd / parent `.env` (project you are working in)
/// 3. **debug only**: walk up from the binary path (workspace `.env` next to
///    `target/debug/one` — so `cd ~/other-app && path/to/one` still loads keys)
/// 4. `~/.one/agent/.env` then `~/.one/.env` (global; use this for release/`PATH` installs)
///
/// Debug builds also default `LANGFUSE_TRACING_ENVIRONMENT=dev` when neither that
/// nor `ONE_ENV` is set, so local traces show under the Langfuse **dev** environment.
fn load_env_files() {
    // Project you are editing (cwd and parents — first file wins via dotenvy).
    let _ = dotenvy::dotenv();

    // Dev: binary-adjacent workspace `.env` (fills only still-unset keys).
    #[cfg(debug_assertions)]
    load_env_from_exe_ancestors();

    // Global One config fallbacks for still-unset keys only.
    let agent = one_session::agent_dir();
    let _ = dotenvy::from_path(agent.join(".env"));
    if let Some(one_home) = agent.parent() {
        let _ = dotenvy::from_path(one_home.join(".env"));
    }

    #[cfg(debug_assertions)]
    {
        if std::env::var_os("LANGFUSE_TRACING_ENVIRONMENT").is_none()
            && std::env::var_os("ONE_ENV").is_none()
        {
            // Local debug → Langfuse environment filter "dev".
            // Override in `.env` with LANGFUSE_TRACING_ENVIRONMENT=… if needed.
            std::env::set_var("LANGFUSE_TRACING_ENVIRONMENT", "dev");
        }
    }
}

/// Walk parents of `current_exe()` and load any `.env` found (no override).
///
/// Typical path: `…/one/target/debug/one` → finds `…/one/.env`.
#[cfg(debug_assertions)]
fn load_env_from_exe_ancestors() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(mut dir) = exe.parent().map(|p| p.to_path_buf()) else {
        return;
    };
    for _ in 0..16 {
        let candidate = dir.join(".env");
        if candidate.is_file() {
            let _ = dotenvy::from_path(&candidate);
        }
        if !dir.pop() {
            break;
        }
    }
}

#[tokio::main]
async fn main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    load_env_files();
    install_panic_hook();
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let forced_mode = normalize_cli_invocation(&mut cli);
    let mut run_mode = forced_mode.unwrap_or_else(|| resolve_run_mode(&cli, &matches));
    cli.mode = run_mode.clone();

    // Interactive TUI owns the terminal — never print tracing to stderr
    // (MCP background connect would otherwise corrupt the alternate screen).
    // ACP reserves stdout for JSON-RPC; keep tracing on stderr.
    let session_tui = matches!(
        &cli.command,
        Some(Commands::Session(crate::cli::SessionCli {
            action: crate::cli::SessionAction::Tui { .. }
        }))
    );
    let interactive_tui = session_tui
        || (matches!(run_mode, RunMode::Interactive)
            && cli.command.is_none()
            && !cli.list_models
            && !cli.list_providers);
    init_tracing(interactive_tui);

    if matches!(&cli.command, Some(Commands::Control(_))) {
        if let Some(Commands::Control(control)) = cli.command.take() {
            return Ok(control_cmd::run_control(control).await);
        }
    }

    let mut _session_entry_lock = None;
    if matches!(&cli.command, Some(Commands::Session(_))) {
        let Some(Commands::Session(session)) = cli.command.take() else {
            unreachable!()
        };
        match session_cmd::apply(&mut cli, session).await {
            Ok(session_cmd::SessionDisposition::Complete(code)) => return Ok(code),
            Ok(session_cmd::SessionDisposition::Enter(lock)) => {
                _session_entry_lock = Some(lock);
                run_mode = cli.mode;
            }
            Err(message) => {
                eprintln!("one session: {message}");
                return Ok(ExitCode::FAILURE);
            }
        }
    }

    // `one acp` — Agent Client Protocol over stdio (IDE embedding).
    if matches!(&cli.command, Some(Commands::Acp(_))) {
        if let Some(Commands::Acp(acp)) = cli.command.take() {
            return run_acp_command(cli, acp).await;
        }
    }

    // `one web` — Web UI server (ACP over WebSocket).
    if matches!(&cli.command, Some(Commands::Web(_))) {
        if let Some(Commands::Web(web)) = cli.command.take() {
            return run_web_command(cli, web).await;
        }
    }

    // `one config` — 本地配置控制台（不依赖模型/认证可用）。
    if matches!(&cli.command, Some(Commands::Config(_))) {
        if let Some(Commands::Config(config)) = cli.command.take() {
            return run_config_command(cli, config).await;
        }
    }

    // `one bot` — Multi-channel Bot Gateway.
    if matches!(&cli.command, Some(Commands::Bot(_))) {
        if let Some(Commands::Bot(bot)) = cli.command.take() {
            modes::run_bot(cli, Some(bot)).await?;
            return Ok(ExitCode::SUCCESS);
        }
    }

    if let Some(Commands::Mcp(mcp)) = cli.command {
        mcp_cmd::run_mcp(mcp).await?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(Commands::Login(login)) = cli.command {
        auth_cmd::run_login(login).await?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(Commands::Logout(logout)) = cli.command {
        auth_cmd::run_logout(logout).await?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(Commands::Bench(bench)) = cli.command {
        bench_cmd::run_bench(bench).await?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(Commands::Learn(learn)) = cli.command {
        let cwd = cli.cwd.canonicalize().unwrap_or_else(|_| cli.cwd.clone());
        learn_cmd::run_learn(learn, &cwd).await?;
        return Ok(ExitCode::SUCCESS);
    }
    // `one resume …` rewrites into the normal agent path (session open + TUI/print).
    if matches!(&cli.command, Some(Commands::Resume(_))) {
        if let Some(Commands::Resume(resume)) = cli.command.take() {
            apply_resume_cli(&mut cli, resume).await?;
        }
    }
    // `one exec --resume <spec> PROMPT` / `one exec -c PROMPT`: resolve the spec
    // onto the standard `--session` path so the runtime opens (not creates) the
    // same session file — history stays continuous, id/path unchanged.
    // Empty spec (from `exec -c`) resolves to the most recent session.
    if let Some(spec) = cli.exec_resume.take() {
        let cwd = cli.cwd.canonicalize().unwrap_or_else(|_| cli.cwd.clone());
        if spec.trim().is_empty() {
            match SessionManager::continue_recent(&cwd).await {
                Ok(session) => {
                    eprintln!(
                        "exec resume: {}",
                        session
                            .session_file()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    );
                    cli.session = session.session_file().map(|p| p.to_path_buf());
                    cli.r#continue = false;
                    cli.resume = false;
                }
                Err(SessionError::NoSessions) => {
                    // No prior session: fall through and create a fresh one.
                }
                Err(err) => return Err(err.into()),
            }
        } else {
            match SessionManager::resolve(&cwd, &spec).await {
                Ok(info) => {
                    eprintln!(
                        "exec resume: {} · {}",
                        info.display_label(),
                        info.path.display()
                    );
                    cli.session = Some(info.path);
                    cli.r#continue = false;
                    cli.resume = false;
                }
                Err(SessionError::Ambiguous { spec, candidates }) => {
                    eprintln!("error: ambiguous session `{spec}` — refine the query:");
                    for c in candidates {
                        eprintln!("  · {c}");
                    }
                    std::process::exit(2);
                }
                Err(SessionError::NoSessions) => {
                    return Err(format!("no sessions for {}", cwd.display()).into());
                }
                Err(SessionError::NotFound(msg)) => {
                    return Err(msg.into());
                }
                Err(err) => return Err(err.into()),
            }
        }
        // Resuming implies persistence even if defaults changed elsewhere.
        cli.exec_session = !cli.no_session;
    }
    // Take subcommand first so remaining `cli` can be borrowed (Agent/Run need global flags).
    if matches!(
        &cli.command,
        Some(Commands::Agent(_)) | Some(Commands::Run(_))
    ) {
        match cli.command.take() {
            Some(Commands::Agent(agent)) => {
                return agent_cmd::run_agent_command(agent.command, &cli).await;
            }
            Some(Commands::Run(run)) => {
                return agent_cmd::run_run_cli(run, &cli).await;
            }
            _ => unreachable!(),
        }
    }

    if cli.list_providers {
        let set = ProviderSet::build(&cli)?;
        println!("{:<14} {:<36} {}", "provider", "description", "auth");
        println!("{}", "-".repeat(72));
        for (id, desc, auth) in one_ai::ModelRegistry::builtin_provider_catalog() {
            println!("{id:<14} {desc:<36} {auth}");
        }
        // Extra providers from models.json not in builtins.
        let builtins: std::collections::HashSet<&str> =
            one_ai::ModelRegistry::builtin_provider_catalog()
                .iter()
                .map(|(id, _, _)| *id)
                .collect();
        for id in set.available_providers() {
            if !builtins.contains(id.as_str()) {
                println!("{id:<14} {:<36} models.json", "custom");
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    if cli.list_models {
        let set = ProviderSet::build(&cli)?;
        for model in set.registry.list() {
            let ctx = model
                .context_window
                .map(|n| format!("  ctx={n}"))
                .unwrap_or_default();
            println!("{}:{} — {}{ctx}", model.provider, model.id, model.name);
        }
        return Ok(ExitCode::SUCCESS);
    }

    // ACP manages its own sessions — skip default AppRuntime assembly.
    if matches!(run_mode, RunMode::Acp) {
        modes::run_acp(cli).await?;
        return Ok(ExitCode::SUCCESS);
    }

    // Web mode runs ACP server over WebSocket.
    if matches!(run_mode, RunMode::Web) {
        modes::run_web_server(cli, "127.0.0.1", 3000, false).await?;
        return Ok(ExitCode::SUCCESS);
    }

    // Bot mode runs Bot Gateway.
    if matches!(run_mode, RunMode::Bot) {
        modes::run_bot(cli, None).await?;
        return Ok(ExitCode::SUCCESS);
    }

    let mut providers = ProviderSet::build(&cli)?;
    let mut runtime = AppRuntime::build(&cli).await?;
    // A resumed session keeps its own most-recent model. Explicit CLI model
    // arguments are an intentional one-off override of that session choice.
    if cli.provider.is_none() && cli.model.is_none() {
        if let Some((provider, model)) = runtime.session.as_ref().and_then(|session| {
            let context = session.build_session_context();
            context.provider.zip(context.model_id)
        }) {
            if let Err(err) = providers.restore_session_model(&provider, &model) {
                tracing::warn!(
                    provider = %provider,
                    model = %model,
                    "failed to restore session model; using the global default: {err}"
                );
            }
        }
    }
    // Drive auto-compact threshold from model/settings context_window (~85%).
    runtime.set_context_window(providers.context_window());
    // Pi/Grok agentic search: hosted inject on main request when capable.
    runtime.refresh_web_search_backend(&providers).await?;
    // Bind LLM for nested `task` → harness::run (same provider as parent).
    runtime.bind_task_provider(providers.as_arc()).await;
    runtime.sync_task_session().await;

    if cli.share {
        #[cfg(feature = "network")]
        {
            let Some(session) = &runtime.session else {
                return Err("no session to share (use interactive mode or --session)".into());
            };
            let html = export_html(session);
            let title = session
                .session_name()
                .unwrap_or_else(|| "One Session".to_string());
            let url = one_session::share_to_gist(html, title).await?;
            println!("shared: {url}");
            return Ok(ExitCode::SUCCESS);
        }
        #[cfg(not(feature = "network"))]
        {
            return Err("share requires --features network".into());
        }
    }

    if let Some(export_path) = &cli.export {
        let Some(session) = &runtime.session else {
            return Err("no session to export (use interactive mode or --session)".into());
        };
        let html = export_html(session);
        tokio::fs::write(export_path, html).await?;
        println!("exported to {}", export_path.display());
        return Ok(ExitCode::SUCCESS);
    }

    let want_json_envelope = cli
        .output_format
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    // `one exec` with a durable session: surface the id on stderr (stdout stays
    // clean for piping) so users/scripts can `one resume <id>` afterwards.
    if cli.exec_session && matches!(run_mode, RunMode::Print | RunMode::Json) && !want_json_envelope
    {
        if let Some(session) = &runtime.session {
            let sid = &session.header().id;
            let prefix: String = sid.chars().take(8).collect();
            eprintln!("[session] {sid} · resume: one resume {prefix}");
        }
    }

    // Native control plane: one user-only Unix socket per live One process.
    // It controls this exact AppRuntime (no sidecar runtime).
    // Prompt capability is decided by the real transport wiring below —
    // never by matching on the frontend label string.
    let control_frontend = match run_mode {
        RunMode::Interactive => Some("interactive"),
        RunMode::Print => Some("print"),
        RunMode::Json => Some("json"),
        _ => None,
    };
    let mut native_control = if let Some(frontend) = control_frontend {
        // Prompt capability = the real transport wiring: only the interactive
        // TUI consumes native prompts (prompt_rx is taken by run_interactive).
        // Print / json are one-shot runs — no live prompt consumer.
        let transport = match run_mode {
            RunMode::Interactive => {
                crate::runtime::control::ControlPromptTransport::InteractiveChannel
            }
            _ => crate::runtime::control::ControlPromptTransport::None,
        };
        match crate::runtime::control::start_control_server(&runtime, frontend, transport).await {
            Ok(server) => Some(server),
            Err(err) => {
                tracing::warn!(error = %err, "failed to start native control socket; continuing without it");
                None
            }
        }
    } else {
        None
    };

    let exit = match run_mode {
        RunMode::Print => {
            let prompt = initial_prompt(&cli)
                .ok_or("print/exec mode requires a prompt (`one exec <prompt>` or legacy `-p`)")?;
            if want_json_envelope {
                run_print_envelope(&mut runtime, providers.as_llm(), &prompt).await?
            } else {
                modes::run_print(&mut runtime, providers.as_llm(), &prompt, false).await?;
                ExitCode::SUCCESS
            }
        }
        RunMode::Json => {
            let prompt = initial_prompt(&cli).unwrap_or_else(|| "Say hello.".to_string());
            if want_json_envelope {
                run_print_envelope(&mut runtime, providers.as_llm(), &prompt).await?
            } else {
                modes::run_print(&mut runtime, providers.as_llm(), &prompt, true).await?;
                ExitCode::SUCCESS
            }
        }
        RunMode::Rpc => {
            modes::run_rpc(&mut runtime, providers.as_llm()).await?;
            ExitCode::SUCCESS
        }
        RunMode::Acp => {
            // Handled before AppRuntime::build.
            unreachable!("acp mode exits earlier");
        }
        RunMode::Web => {
            // Handled before AppRuntime::build.
            unreachable!("web mode exits earlier");
        }
        RunMode::Bot => {
            // Handled before AppRuntime::build.
            unreachable!("bot mode exits earlier");
        }
        RunMode::Interactive => {
            // Positional PROMPT is the normal first TUI turn; legacy `--tui -p` still works.
            let control_rx = native_control
                .as_mut()
                .and_then(|server| server.take_prompt_rx());
            modes::run_interactive(
                &mut runtime,
                &mut providers,
                initial_prompt(&cli),
                control_rx,
            )
            .await?;
            ExitCode::SUCCESS
        }
    };

    // Session-owned background bash / agent jobs die with the process.
    runtime.shutdown_owned_tasks();
    // Ensure Langfuse HTTP worker drains before process exit.
    runtime.flush_trace();

    Ok(exit)
}

/// `one web` — start ACP Web server.
async fn run_web_command(
    mut cli: Cli,
    web: crate::cli::WebCli,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    cli.mode = RunMode::Web;
    if cli.always_approve {
        cli.auto_approve = true;
    }
    modes::run_web_server(cli, &web.host, web.port, web.open).await?;
    Ok(ExitCode::SUCCESS)
}

/// `one config` — 本地配置控制台 (Config Studio)。
///
/// 刻意不构建 provider、不检查认证：配置坏掉时正是最需要打开它的时候。
async fn run_config_command(
    cli: Cli,
    config: crate::cli::ConfigCli,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if config.dump {
        let studio = config_studio::ConfigStudio::new(cli.cwd.clone());
        let catalog = studio.catalog();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "context": catalog.context,
                "modules": catalog.modules,
                "documents": catalog.documents,
                "backup_root": catalog.backup_root,
            }))?
        );
        return Ok(ExitCode::SUCCESS);
    }

    let cfg = config_studio::StudioServerConfig {
        host: config.host,
        port: config.port,
        open_browser: config.open,
        cwd: cli.cwd.clone(),
    };
    config_studio::run(cfg).await?;
    Ok(ExitCode::SUCCESS)
}

/// `one acp` — apply yolo / mode flags and enter ACP stdio server.
async fn run_acp_command(
    mut cli: Cli,
    _acp: AcpCli,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    cli.mode = RunMode::Acp;
    if cli.always_approve {
        cli.auto_approve = true;
    }
    // Do not build a default TUI/print runtime first — ACP owns session lifecycle.
    modes::run_acp(cli).await?;
    Ok(ExitCode::SUCCESS)
}

/// `--output-format json`: single RunResult line (docs/protocol.md).
async fn run_print_envelope(
    runtime: &mut AppRuntime,
    provider: &dyn one_core::agent::LlmProvider,
    prompt: &str,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let session_id = runtime.session.as_ref().map(|s| s.header().id.clone());
    let session_path = runtime.session_path().map(|p| p.display().to_string());

    match runtime.prompt(provider, prompt).await {
        Ok(text) => {
            let usage = runtime.token_usage().await;
            let rr = RunResult::success(text, t0.elapsed().as_millis() as u64)
                .with_session(session_id, session_path)
                .with_usage(UsageSnapshot {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_write_tokens: usage.cache_write_tokens,
                    estimated_cost_usd: None,
                })
                .with_agent_echo(crate::protocol::AgentRunEcho {
                    name: Some("main".into()),
                    model: Some(crate::protocol::ModelSpec {
                        provider: Some(provider.name().to_string()),
                        id: Some(provider.model().to_string()),
                        thinking: None,
                        inherit: false,
                    }),
                    ..Default::default()
                });
            println!("{}", rr.to_json_line());
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            let rr = RunResult::failure_msg(
                crate::protocol::error_code::PROVIDER_ERROR,
                e.to_string(),
                t0.elapsed().as_millis() as u64,
            )
            .with_session(session_id, session_path)
            .with_agent_echo(crate::protocol::AgentRunEcho {
                name: Some("main".into()),
                model: Some(crate::protocol::ModelSpec {
                    provider: Some(provider.name().to_string()),
                    id: Some(provider.model().to_string()),
                    thinking: None,
                    inherit: false,
                }),
                ..Default::default()
            });
            println!("{}", rr.to_json_line());
            Ok(ExitCode::from(1))
        }
    }
}

#[cfg(test)]
mod cli_semantics_tests {
    use super::*;

    fn parse(args: &[&str]) -> (Cli, clap::ArgMatches) {
        let matches = Cli::command()
            .try_get_matches_from(args)
            .expect("parse CLI");
        let cli = Cli::from_arg_matches(&matches).expect("build CLI");
        (cli, matches)
    }

    #[test]
    fn positional_prompt_keeps_interactive_mode() {
        let (cli, matches) = parse(&["one", "fix the bug"]);
        assert_eq!(cli.prompt.as_deref(), Some("fix the bug"));
        assert!(cli.command.is_none());
        assert!(matches!(
            resolve_run_mode(&cli, &matches),
            RunMode::Interactive
        ));
    }

    #[test]
    fn legacy_p_still_selects_print_mode() {
        let (cli, matches) = parse(&["one", "-p", "fix the bug"]);
        assert_eq!(cli.print.as_deref(), Some("fix the bug"));
        assert!(matches!(resolve_run_mode(&cli, &matches), RunMode::Print));
    }

    #[test]
    fn exec_is_explicit_headless_mode() {
        let (mut cli, _matches) = parse(&[
            "one",
            "exec",
            "fix the bug",
            "--max-turns",
            "7",
            "--output-format",
            "json",
        ]);
        assert!(matches!(&cli.command, Some(Commands::Exec(_))));
        let forced = normalize_cli_invocation(&mut cli);
        assert!(matches!(forced, Some(RunMode::Print)));
        assert!(cli.command.is_none());
        assert_eq!(initial_prompt(&cli).as_deref(), Some("fix the bug"));
        assert_eq!(cli.max_turns, 7);
        assert_eq!(cli.output_format.as_deref(), Some("json"));
        // exec persists its session by default.
        assert!(cli.exec_session);
        assert!(cli.exec_resume.is_none());
    }

    #[test]
    fn exec_no_session_stays_ephemeral() {
        let (mut cli, _matches) = parse(&["one", "exec", "hi", "--no-session"]);
        normalize_cli_invocation(&mut cli);
        assert!(cli.no_session);
        assert!(!cli.exec_session);
    }

    #[test]
    fn exec_resume_spec_maps_to_session_resume() {
        let (mut cli, _matches) = parse(&["one", "exec", "--resume", "abc123", "continue"]);
        normalize_cli_invocation(&mut cli);
        assert_eq!(cli.exec_resume.as_deref(), Some("abc123"));
        assert!(cli.exec_session);
    }

    #[test]
    fn exec_continue_maps_to_most_recent() {
        let (mut cli, _matches) = parse(&["one", "exec", "-c", "continue"]);
        normalize_cli_invocation(&mut cli);
        assert_eq!(cli.exec_resume.as_deref(), Some(""));
        assert!(cli.exec_session);
    }

    #[test]
    fn legacy_p_stays_ephemeral() {
        let (mut cli, _matches) = parse(&["one", "-p", "fix the bug"]);
        let forced = normalize_cli_invocation(&mut cli);
        assert!(matches!(forced, None));
        assert!(!cli.exec_session);
    }

    #[test]
    fn yolo_means_always_approve_plus_full_access() {
        let (mut cli, _matches) = parse(&["one", "--yolo"]);
        normalize_cli_invocation(&mut cli);
        assert!(cli.yolo);
        assert!(cli.always_approve);
        assert!(cli.auto_approve);
        assert!(cli.full_access);
    }

    #[test]
    fn always_approve_does_not_imply_full_access() {
        let (mut cli, _matches) = parse(&["one", "--always-approve"]);
        normalize_cli_invocation(&mut cli);
        assert!(cli.always_approve);
        assert!(!cli.full_access);
    }
}
