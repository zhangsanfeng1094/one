mod approval;
mod auth_cmd;
mod cli;
mod hitl;
mod mcp_cmd;
mod modes;
mod preferences;
mod provider;
mod runtime;
mod settings;

use clap::Parser;
use one_session::export_html;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Commands, RunMode};
use crate::provider::ProviderSet;
use crate::runtime::AppRuntime;

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Interactive TUI owns the terminal — never print tracing to stderr
    // (MCP background connect would otherwise corrupt the alternate screen).
    let interactive_tui = matches!(cli.mode, RunMode::Interactive)
        && cli.print.is_none()
        && cli.command.is_none()
        && !cli.list_models
        && !cli.list_providers;
    init_tracing(interactive_tui);

    if let Some(Commands::Mcp(mcp)) = cli.command {
        return mcp_cmd::run_mcp(mcp).await;
    }
    if let Some(Commands::Login(login)) = cli.command {
        auth_cmd::run_login(login).await?;
        return Ok(());
    }
    if let Some(Commands::Logout(logout)) = cli.command {
        return auth_cmd::run_logout(logout).await;
    }

    if cli.list_providers {
        let set = ProviderSet::build(&cli)?;
        println!(
            "{:<14} {:<36} {}",
            "provider", "description", "auth"
        );
        println!("{}", "-".repeat(72));
        for (id, desc, auth) in one_ai::ModelRegistry::builtin_provider_catalog() {
            println!("{id:<14} {desc:<36} {auth}");
        }
        // Extra providers from models.json not in builtins.
        let builtins: std::collections::HashSet<&str> = one_ai::ModelRegistry::builtin_provider_catalog()
            .iter()
            .map(|(id, _, _)| *id)
            .collect();
        for id in set.available_providers() {
            if !builtins.contains(id.as_str()) {
                println!("{id:<14} {:<36} models.json", "custom");
            }
        }
        return Ok(());
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
        return Ok(());
    }

    let mut providers = ProviderSet::build(&cli)?;
    let mut runtime = AppRuntime::build(&cli).await?;
    // Drive auto-compact threshold from model/settings context_window (~70%).
    runtime.set_context_window(providers.context_window());

    if cli.share {
        #[cfg(feature = "network")]
        {
            let Some(session) = &runtime.session else {
                return Err("no session to share (use interactive mode or --session)".into());
            };
            let html = export_html(session);
            let title = session.session_name().unwrap_or_else(|| "One Session".to_string());
            let url = one_session::share_to_gist(html, title).await?;
            println!("shared: {url}");
            return Ok(());
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
        return Ok(());
    }

    let mode = if cli.print.is_some() {
        RunMode::Print
    } else {
        cli.mode.clone()
    };

    match mode {
        RunMode::Print => {
            let prompt = cli.print.expect("print prompt");
            modes::run_print(&mut runtime, providers.as_llm(), &prompt, false).await?;
        }
        RunMode::Json => {
            let prompt = cli.print.unwrap_or_else(|| "Say hello.".to_string());
            modes::run_print(&mut runtime, providers.as_llm(), &prompt, true).await?;
        }
        RunMode::Rpc => modes::run_rpc(&mut runtime, providers.as_llm()).await?,
        RunMode::Interactive => {
            modes::run_interactive(&mut runtime, &mut providers, cli.print).await?;
        }
    }

    Ok(())
}