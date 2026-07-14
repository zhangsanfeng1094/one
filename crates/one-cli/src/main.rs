mod cli;
mod modes;
mod preferences;
mod provider;
mod runtime;

use clap::Parser;
use one_session::export_html;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, RunMode};
use crate::provider::ProviderSet;
use crate::runtime::AppRuntime;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .without_time()
        .init();

    let cli = Cli::parse();

    if cli.list_models {
        let set = ProviderSet::build(&cli)?;
        for model in set.registry.list() {
            println!("{}:{} — {}", model.provider, model.id, model.name);
        }
        return Ok(());
    }

    let mut providers = ProviderSet::build(&cli)?;
    let mut runtime = AppRuntime::build(&cli).await?;

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