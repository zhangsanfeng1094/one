mod agent_cmd;
mod approval;
mod auth_cmd;
mod bench_cmd;
mod cli;
mod hitl;
mod langfuse;
mod mcp_cmd;
mod modes;
mod preferences;
mod protocol;
mod provider;
mod runtime;
mod settings;

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches};
use one_session::export_html;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Commands, RunMode};
use crate::protocol::{RunResult, UsageSnapshot};
use crate::provider::ProviderSet;
use crate::runtime::AppRuntime;
use std::process::ExitCode;
use std::time::Instant;

/// Resolve run mode.
///
/// | invocation | mode |
/// |------------|------|
/// | `one` | Interactive |
/// | `one -p "…"` | Print (compat) |
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

fn init_tracing(interactive_tui: bool) {
    let filter = EnvFilter::from_default_env();
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
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let run_mode = resolve_run_mode(&cli, &matches);

    // Interactive TUI owns the terminal — never print tracing to stderr
    // (MCP background connect would otherwise corrupt the alternate screen).
    let interactive_tui = matches!(run_mode, RunMode::Interactive)
        && cli.command.is_none()
        && !cli.list_models
        && !cli.list_providers;
    init_tracing(interactive_tui);

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

    let mut providers = ProviderSet::build(&cli)?;
    let mut runtime = AppRuntime::build(&cli).await?;
    // Drive auto-compact threshold from model/settings context_window (~70%).
    runtime.set_context_window(providers.context_window());
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

    let exit = match run_mode {
        RunMode::Print => {
            let prompt = cli
                .print
                .clone()
                .ok_or("--mode print requires -p / --print <prompt>")?;
            if want_json_envelope {
                run_print_envelope(&mut runtime, providers.as_llm(), &prompt).await?
            } else {
                modes::run_print(&mut runtime, providers.as_llm(), &prompt, false).await?;
                ExitCode::SUCCESS
            }
        }
        RunMode::Json => {
            let prompt = cli
                .print
                .clone()
                .unwrap_or_else(|| "Say hello.".to_string());
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
        RunMode::Interactive => {
            // `-p` / `--tui -p` seeds the first user turn inside the TUI.
            modes::run_interactive(&mut runtime, &mut providers, cli.print.clone()).await?;
            ExitCode::SUCCESS
        }
    };

    // Ensure Langfuse HTTP worker drains before process exit.
    runtime.flush_trace();

    Ok(exit)
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
