//! Manual CLI for the per-process Native Control protocol.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand};
use serde_json::{json, Value};

use crate::runtime::control::{control_call, discover_live_endpoints, LiveControlEndpoint};

#[derive(Debug, Clone, Args)]
pub struct ControlCli {
    /// Print JSON on stdout (errors go to stderr).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    action: ControlAction,
}

#[derive(Debug, Clone, Args)]
struct Target {
    /// Select a live runtime by process ID.
    #[arg(long, conflicts_with = "endpoint")]
    pid: Option<u32>,
    /// Select a socket in this user's Native Control directory.
    #[arg(long, conflicts_with = "pid")]
    endpoint: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
struct TextCommand {
    #[command(flatten)]
    target: Target,
    /// Text to send to the runtime.
    #[arg(value_name = "TEXT", required = true, num_args = 1..)]
    text: Vec<String>,
}

#[derive(Debug, Clone, Subcommand)]
enum ControlAction {
    /// List live runtimes.
    List,
    /// Show runtime state.
    Status(Target),
    /// Start a new turn on an idle interactive runtime.
    Prompt(TextCommand),
    /// Steer the current turn.
    Steer(TextCommand),
    /// Queue a follow-up for the current turn.
    #[command(name = "followup", alias = "follow-up")]
    Followup(TextCommand),
    /// Interrupt the current turn.
    Abort(Target),
    /// Check connectivity and identity.
    Ping(Target),
    /// Show the active session identity and path.
    Session(Target),
}

async fn select_target(target: &Target) -> Result<LiveControlEndpoint, String> {
    if let Some(path) = &target.endpoint {
        let (status, _) = control_call(path, "handshake", Value::Null)
            .await
            .map_err(|err| format!("endpoint {}: {err}", path.display()))?;
        return Ok(LiveControlEndpoint {
            endpoint: path.clone(),
            status,
        });
    }
    let mut live = discover_live_endpoints()
        .await
        .map_err(|err| err.to_string())?;
    if let Some(pid) = target.pid {
        live.retain(|item| item.status["pid"].as_u64() == Some(pid as u64));
        return match live.len() {
            0 => Err(format!("no live runtime with pid {pid}")),
            1 => Ok(live.remove(0)),
            _ => Err(format!(
                "multiple live runtimes have pid {pid}; use --endpoint"
            )),
        };
    }
    match live.len() {
        0 => Err("no live runtime".to_owned()),
        1 => Ok(live.remove(0)),
        _ => {
            let pids = live
                .iter()
                .filter_map(|v| v.status["pid"].as_u64().map(|p| p.to_string()))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "ambiguous runtime; select --pid or --endpoint (live pids: {pids})"
            ))
        }
    }
}

fn print_value(value: &Value, json_output: bool) {
    if json_output {
        println!("{value}");
    } else if let Some(object) = value.as_object() {
        for (key, value) in object {
            let display = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            println!("{key}: {display}");
        }
    } else {
        println!("{value}");
    }
}

async fn run_action(action: ControlAction, json_output: bool) -> Result<(), String> {
    if matches!(action, ControlAction::List) {
        let live = discover_live_endpoints()
            .await
            .map_err(|err| err.to_string())?;
        if json_output {
            println!("{}", json!(live));
        } else {
            println!(
                "{:<8} {:<14} {:<5} {:<20} {:<32} {:<14} ENDPOINT",
                "PID", "STATE", "BUSY", "FRONTEND/RUN MODE", "CWD", "SESSION"
            );
            for item in live {
                let status = &item.status;
                let session = status["session_id"].as_str().unwrap_or("-");
                let session: String = session.chars().take(14).collect();
                println!(
                    "{:<8} {:<14} {:<5} {:<20} {:<32} {:<14} {}",
                    status["pid"],
                    status["state"].as_str().unwrap_or("?"),
                    status["busy"],
                    format!(
                        "{}/{}",
                        status["frontend"].as_str().unwrap_or("?"),
                        status["run_mode"].as_str().unwrap_or("?")
                    ),
                    status["cwd"].as_str().unwrap_or("?"),
                    session,
                    item.endpoint.display()
                );
            }
        }
        return Ok(());
    }

    let (target, method, params) = match action {
        ControlAction::Status(target) => (target, "status", Value::Null),
        ControlAction::Prompt(cmd) => (cmd.target, "prompt", json!({"text": cmd.text.join(" ")})),
        ControlAction::Steer(cmd) => (cmd.target, "steer", json!({"text": cmd.text.join(" ")})),
        ControlAction::Followup(cmd) => {
            (cmd.target, "follow_up", json!({"text": cmd.text.join(" ")}))
        }
        ControlAction::Abort(target) => (target, "abort", Value::Null),
        ControlAction::Ping(target) => (target, "ping", Value::Null),
        ControlAction::Session(target) => (target, "session", Value::Null),
        ControlAction::List => unreachable!(),
    };
    let selected = select_target(&target).await?;
    let (_, result) = control_call(&selected.endpoint, method, params)
        .await
        .map_err(|err| format!("{}: {err}", selected.endpoint.display()))?;
    print_value(&result, json_output);
    Ok(())
}

pub async fn run_control(cli: ControlCli) -> ExitCode {
    match run_action(cli.action, cli.json).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("one control: {message}");
            ExitCode::FAILURE
        }
    }
}
