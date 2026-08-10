//! `one agent …` / `one run …` — CLI harness entry (before TaskTool).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Subcommand};

use crate::cli::Cli;
use crate::protocol::{AgentRef, RunRequest, SessionMode};
use crate::provider::ProviderSet;
use crate::runtime::harness::{self, HarnessOptions, RunControl};
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
    Inspect { name: String },
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

    /// File isolation: none (shared cwd) or worktree (git worktree under .one/worktrees).
    #[arg(long = "isolation", value_name = "MODE", default_value = "none")]
    pub isolation: String,
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

    /// File isolation: none | worktree.
    #[arg(long = "isolation", value_name = "MODE", default_value = "none")]
    pub isolation: String,
}

pub async fn run_agent_command(
    cmd: AgentCommands,
    global: &Cli,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let cwd = global
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| global.cwd.clone());
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
            println!("tools.allow: {:?}", spec.tools.allow);
            println!("tools.deny: {:?}", spec.tools.deny);
            println!("tools.extra: {:?}", spec.tools.extra);
            println!("tools.mcp: {}", spec.tools.mcp);
            let resolved = crate::runtime::harness::preview_tool_names(&spec);
            println!("tools.resolved: {resolved:?}");
            println!("isolation: {:?}", spec.isolation);
            println!("spawn_policy.allow: {:?}", spec.spawn_policy.allow);
            if !spec.agents.is_empty() {
                let kids: Vec<_> = spec.agents.keys().cloned().collect();
                println!("agents: {kids:?}");
            }
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
                &args.isolation,
            )
            .await
        }
    }
}

pub async fn run_run_cli(
    run: RunCli,
    global: &Cli,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let cwd = global
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| global.cwd.clone());
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
        &run.isolation,
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
    isolation: &str,
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

    let mut spec = presets::resolve_agent_ref(&agent_ref, cwd)?;
    if let Some(iso) = crate::protocol::IsolationMode::parse(isolation) {
        spec.isolation = iso;
    } else if !isolation.is_empty() && isolation != "none" {
        return Err(format!("unknown --isolation `{isolation}` (use none|worktree)").into());
    }
    let mut req = RunRequest::new(spec, prompt);
    req.session.mode = SessionMode::Ephemeral;

    let providers = ProviderSet::build(global)?;
    let mut opts = HarnessOptions::from_cwd(cwd.to_path_buf());
    opts.full_access = global.full_access;
    opts.add_dirs = global.add_dir.clone();
    opts.auto_approve = global.auto_approve
        || std::env::var_os("ONE_AUTO_APPROVE").is_some_and(|v| v != "0" && v != "false");

    // Optional Langfuse on the same path as main agent / nested task (control.trace).
    let mut control = RunControl::default();
    let langfuse_handle = attach_run_trace(global, &req, &opts, &mut control);

    let result = harness::run_with_control(req, providers.as_llm(), &opts, control).await;

    // Drain OTLP before process exit (short-lived CLI).
    if let Some(sink) = langfuse_handle {
        sink.shutdown();
    }

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

fn run_trace_enabled(cli: &Cli) -> bool {
    if cli.trace {
        return true;
    }
    std::env::var_os("ONE_TRACE").is_some_and(|v| {
        let s = v.to_string_lossy();
        s == "1" || s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes")
    })
}

/// Wire Langfuse into [`RunControl`] when `--trace` / `ONE_TRACE` and keys are set.
fn attach_run_trace(
    global: &Cli,
    req: &RunRequest,
    opts: &HarnessOptions,
    control: &mut RunControl,
) -> Option<Arc<crate::langfuse::LangfuseTraceSink>> {
    if !run_trace_enabled(global) {
        return None;
    }
    let Some(cfg) = crate::langfuse::LangfuseConfig::from_env() else {
        eprintln!(
            "trace: requested but Langfuse keys missing — set LANGFUSE_PUBLIC_KEY and LANGFUSE_SECRET_KEY"
        );
        return None;
    };
    let host = cfg.project_url_hint();
    tracing::info!(%host, "langfuse tracing enabled (one run harness)");
    let sink = crate::langfuse::LangfuseTraceSink::start(cfg);
    let max_turns = req.agent.max_turns.unwrap_or(16);
    control.trace = Some(sink.clone());
    control.trace_meta = Some(one_core::TraceRunMeta {
        agent_version: Some(env!("CARGO_PKG_VERSION").into()),
        config: Some(serde_json::json!({
            "max_turns": max_turns,
            "auto_approve": opts.auto_approve,
            "cwd": opts.cwd.display().to_string(),
            "trace_full": global.trace_full,
            "backend": "langfuse",
            "harness": "one-run",
            "agent": req.agent.display_name(),
        })),
        task_id: req.agent.name.clone(),
        session_id: Some(format!("run:{}", uuid::Uuid::new_v4().simple())),
        user_id: crate::langfuse::user_id_from_env(),
        trace_full: global.trace_full,
    });
    eprintln!("trace: langfuse otel ({host}/api/public/otel/v1/traces)");
    if global.trace_full {
        eprintln!("trace: full I/O previews enabled (--trace-full)");
    }
    Some(sink)
}
