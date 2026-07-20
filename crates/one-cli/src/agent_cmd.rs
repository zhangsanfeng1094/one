//! `one agent …` / `one run …` — CLI harness entry (before TaskTool).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand};

use crate::cli::Cli;
use crate::protocol::{AgentRef, RunRequest, SessionMode};
use crate::provider::ProviderSet;
use crate::runtime::harness::{self, HarnessOptions};
use crate::runtime::presets;

#[derive(Debug, Clone, Subcommand)]
pub enum AgentCommands {
    /// Run a preset or named agent (same harness as subagent).
    Run(AgentRunArgs),
    /// Print full harness JSON for a preset (for --spec editing).
    Dump {
        /// Preset id (e.g. explore).
        name: String,
    },
    /// Summarize tools + prompt for a preset.
    Inspect {
        name: String,
    },
}

#[derive(Debug, Clone, Args)]
pub struct AgentRunArgs {
    /// Preset or disk agent name (e.g. explore). Ignored if --spec is set.
    #[arg(value_name = "PRESET", default_value = "explore")]
    pub preset: String,

    /// User prompt (required).
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    pub prompt: Option<String>,

    /// Full AgentSpec JSON file (overrides preset name).
    #[arg(long = "spec", value_name = "PATH")]
    pub spec: Option<PathBuf>,

    /// Output: text or json (RunResult envelope). Default json.
    #[arg(long = "output-format", default_value = "json")]
    pub output_format: String,
}

/// Top-level `one run` (alias surface for --preset / --spec).
#[derive(Debug, Clone, Args)]
pub struct RunCli {
    /// User prompt.
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    pub prompt: Option<String>,

    /// Preset name (e.g. explore).
    #[arg(long = "preset", value_name = "NAME")]
    pub preset: Option<String>,

    /// Full AgentSpec JSON path.
    #[arg(long = "spec", value_name = "PATH")]
    pub spec: Option<PathBuf>,

    #[arg(long = "output-format", default_value = "json")]
    pub output_format: String,
}

pub async fn run_agent_command(
    cmd: AgentCommands,
    global: &Cli,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let cwd = global.cwd.canonicalize().unwrap_or_else(|_| global.cwd.clone());
    match cmd {
        AgentCommands::Dump { name } => {
            let json = presets::dump_preset(&name, &cwd)?;
            println!("{json}");
            Ok(ExitCode::SUCCESS)
        }
        AgentCommands::Inspect { name } => {
            let spec = presets::load_preset(&name, &cwd)?;
            println!("name: {}", spec.display_name());
            if let Some(d) = &spec.description {
                println!("description: {d}");
            }
            println!("max_turns: {:?}", spec.max_turns);
            println!("permission_mode: {:?}", spec.permission_mode);
            println!("tools.profile: {:?}", spec.tools.profile);
            println!("tools.mcp: {}", spec.tools.mcp);
            println!("spawn_policy.allow: {:?}", spec.spawn_policy.allow);
            if let Some(sys) = &spec.system_prompt {
                let preview: String = sys.chars().take(200).collect();
                println!("system_prompt (preview): {preview}…");
            } else {
                println!("system_prompt: (default template)");
            }
            Ok(ExitCode::SUCCESS)
        }
        AgentCommands::Run(args) => {
            let prompt = args
                .prompt
                .or_else(|| global.print.clone())
                .ok_or("--print / -p PROMPT is required for agent run")?;
            execute_run(
                global,
                &cwd,
                args.spec.as_deref(),
                Some(args.preset.as_str()),
                &prompt,
                &args.output_format,
            )
            .await
        }
    }
}

pub async fn run_run_cli(
    run: RunCli,
    global: &Cli,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let cwd = global.cwd.canonicalize().unwrap_or_else(|_| global.cwd.clone());
    let prompt = run
        .prompt
        .or_else(|| global.print.clone())
        .ok_or("--print / -p PROMPT is required for run")?;
    if run.spec.is_none() && run.preset.is_none() {
        return Err("one run requires --preset NAME or --spec PATH".into());
    }
    execute_run(
        global,
        &cwd,
        run.spec.as_deref(),
        run.preset.as_deref(),
        &prompt,
        &run.output_format,
    )
    .await
}

async fn execute_run(
    global: &Cli,
    cwd: &std::path::Path,
    spec_path: Option<&std::path::Path>,
    preset: Option<&str>,
    prompt: &str,
    output_format: &str,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let agent_ref = if let Some(path) = spec_path {
        let spec = if path.as_os_str() == "-" {
            let mut buf = String::new();
            use std::io::Read;
            std::io::stdin().read_to_string(&mut buf)?;
            presets::load_spec_json(&buf)?
        } else {
            presets::load_spec_file(path)?
        };
        AgentRef::Spec(spec)
    } else {
        AgentRef::Preset(preset.unwrap_or("explore").to_string())
    };

    let spec = presets::resolve_agent_ref(&agent_ref, cwd)?;
    let mut req = RunRequest::new(spec, prompt);
    req.session.mode = SessionMode::Ephemeral;

    let providers = ProviderSet::build(global)?;
    let mut opts = HarnessOptions::from_cwd(cwd.to_path_buf());
    opts.full_access = global.full_access;
    opts.add_dirs = global.add_dir.clone();
    opts.auto_approve = global.auto_approve
        || std::env::var_os("ONE_AUTO_APPROVE").is_some_and(|v| v != "0" && v != "false");

    let result = harness::run(req, providers.as_llm(), &opts).await;

    match output_format {
        "json" => {
            println!("{}", result.to_json_line());
        }
        "text" | _ => {
            if result.ok {
                if !result.result.is_empty() {
                    println!("{}", result.result);
                }
            } else if let Some(err) = &result.error {
                eprintln!("{}: {}", err.code, err.message);
                if !result.result.is_empty() {
                    println!("{}", result.result);
                }
            } else {
                eprintln!("run failed");
            }
        }
    }

    Ok(if result.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
