//! Session-first browser and explicit entry points.

use std::path::Path;
use std::process::ExitCode;

use serde_json::{json, Value};

use crate::cli::{Cli, RunMode, SessionAction, SessionCli};
use crate::runtime::control::{
    control_call, discover_live_endpoints, discover_live_endpoints_strict, LiveControlEndpoint,
};
use one_session::{SessionInfo, SessionManager};

pub enum SessionDisposition {
    Complete(ExitCode),
    Enter(one_session::SessionLock),
}

fn matches_query(info: &SessionInfo, query: &str) -> bool {
    let q = query.to_lowercase();
    [
        info.id.as_str(),
        info.cwd.as_str(),
        info.path.to_str().unwrap_or(""),
        info.name.as_deref().unwrap_or(""),
        info.preview.as_deref().unwrap_or(""),
    ]
    .iter()
    .any(|value| value.to_lowercase().contains(&q))
}

fn resolve(sessions: Vec<SessionInfo>, spec: &str) -> Result<SessionInfo, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("empty session spec".into());
    }
    let lower = spec.to_lowercase();
    let canonical_path = Path::new(spec).canonicalize().ok();
    // Exact full identity is decisive even when its text occurs in other labels.
    let tiers: [Box<dyn Fn(&SessionInfo) -> bool>; 5] = [
        Box::new(|s| {
            s.id == spec
                || s.path == Path::new(spec)
                || canonical_path.as_ref().is_some_and(|path| &s.path == path)
        }),
        Box::new(|s| s.id.starts_with(spec)),
        Box::new(|s| {
            s.name
                .as_ref()
                .is_some_and(|n| n.eq_ignore_ascii_case(spec))
        }),
        Box::new(|s| s.path.file_name().and_then(|n| n.to_str()) == Some(spec)),
        Box::new(|s| {
            s.name
                .as_ref()
                .is_some_and(|n| n.to_lowercase().contains(&lower))
                || s.preview
                    .as_ref()
                    .is_some_and(|p| p.to_lowercase().contains(&lower))
                || s.path.to_string_lossy().to_lowercase().contains(&lower)
        }),
    ];
    for tier in tiers {
        let found: Vec<_> = sessions.iter().filter(|s| tier(s)).collect();
        match found.len() {
            0 => continue,
            1 => return Ok(found[0].clone()),
            _ => {
                let choices = found
                    .iter()
                    .map(|s| {
                        format!(
                            "  {}  {}  {}  {}",
                            s.id,
                            s.modified.format("%Y-%m-%d %H:%M"),
                            s.display_label(),
                            s.cwd
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(format!(
                    "ambiguous session `{spec}`; candidates:\n{choices}"
                ));
            }
        }
    }
    Err(format!("no session matching `{spec}`"))
}

fn live_for<'a>(
    session: &SessionInfo,
    live: &'a [LiveControlEndpoint],
) -> Option<&'a LiveControlEndpoint> {
    live.iter().find(|endpoint| {
        let status = &endpoint.status;
        status["session_id"].as_str() == Some(session.id.as_str())
            && status["session_path"].as_str() == session.path.to_str()
            && status["cwd"].as_str() == Some(session.cwd.as_str())
            && status["pid"].as_u64().is_some_and(|pid| pid > 0)
            && status["process_start"]
                .as_str()
                .is_some_and(|start| !start.is_empty())
    })
}

/// Lifecycle projection for one session row.
///
/// Live states come from the runtime's unified status (`state` field):
/// - `live_busy`       — turn in flight (running / abort unwinding)
/// - `live_waiting_input` — turn blocked on approval / ask_user
/// - `live_waiting_work`  — turn parked on background work
/// - `live_idle`       — live process, no in-flight turn
/// - `dormant`         — no live runtime owns this session
pub(crate) fn state(live: Option<&LiveControlEndpoint>) -> &'static str {
    let Some(runtime) = live else {
        return "dormant";
    };
    match runtime.status["state"].as_str() {
        Some("waiting_input") => "live_waiting_input",
        Some("waiting_work") => "live_waiting_work",
        // Newer servers report `state`; busy=true without it maps to live_busy.
        _ if runtime.status["busy"] == true => "live_busy",
        _ => "live_idle",
    }
}

fn record(info: &SessionInfo, live: Option<&LiveControlEndpoint>) -> Value {
    json!({
        "id": info.id,
        "path": info.path,
        "cwd": info.cwd,
        "name": info.name,
        "preview": info.preview,
        "modified": info.modified,
        "model": info.model,
        "usage_total": info.usage_total,
        "label": info.display_label(),
        "state": state(live),
        "live": live.map(|runtime| json!({
            "pid": runtime.status["pid"],
            "process_start": runtime.status["process_start"],
            "frontend": runtime.status["frontend"],
            "run_mode": runtime.status["run_mode"],
            "state": runtime.status["state"],
            "busy": runtime.status["busy"],
            "activity": runtime.status["activity"],
            "capabilities": runtime.status["capabilities"],
            "control_endpoint": runtime.endpoint,
        })),
    })
}

fn live_error(runtime: &LiveControlEndpoint) -> String {
    format!(
        "session is live (pid {}, frontend {}); hot attach is unavailable",
        runtime.status["pid"],
        runtime.status["frontend"].as_str().unwrap_or("unknown")
    )
}

pub async fn apply(cli: &mut Cli, session_cli: SessionCli) -> Result<SessionDisposition, String> {
    let cwd = cli.cwd.canonicalize().unwrap_or_else(|_| cli.cwd.clone());
    let all = match &session_cli.action {
        SessionAction::List { all, .. }
        | SessionAction::Show { all, .. }
        | SessionAction::Tui { all, .. }
        | SessionAction::Exec { all, .. } => *all,
    };
    let sessions = if all {
        SessionManager::list_all().await
    } else {
        SessionManager::list(&cwd).await
    }
    .map_err(|err| err.to_string())?;

    match session_cli.action {
        SessionAction::List {
            query, limit, json, ..
        } => {
            let live = discover_live_endpoints().await.unwrap_or_default();
            let rows: Vec<_> = sessions
                .iter()
                .filter(|s| query.as_deref().is_none_or(|q| matches_query(s, q)))
                .take(limit)
                .map(|s| record(s, live_for(s, &live)))
                .collect();
            if json {
                println!("{}", Value::Array(rows));
            } else {
                println!(
                    "{:<16} {:<10} {:<12} {:<12} {:<32} CWD",
                    "MODIFIED", "STATE", "FRONTEND", "ID", "LABEL"
                );
                for row in rows {
                    println!(
                        "{:<16} {:<10} {:<12} {:<12} {:<32} {}",
                        row["modified"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(16)
                            .collect::<String>()
                            .replace('T', " "),
                        row["state"].as_str().unwrap_or(""),
                        row["live"]["frontend"].as_str().unwrap_or("-"),
                        row["id"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(12)
                            .collect::<String>(),
                        row["label"].as_str().unwrap_or(""),
                        row["cwd"].as_str().unwrap_or("")
                    );
                }
            }
            Ok(SessionDisposition::Complete(ExitCode::SUCCESS))
        }
        action => {
            let spec = match &action {
                SessionAction::Show { spec, .. }
                | SessionAction::Tui { spec, .. }
                | SessionAction::Exec { spec, .. } => spec,
                SessionAction::List { .. } => unreachable!(),
            };
            let info = resolve(sessions, spec)?;
            // Entering a session requires a successful discovery pass; otherwise
            // absence of a runtime cannot be established safely.
            let discovery = if matches!(&action, SessionAction::Show { .. }) {
                discover_live_endpoints().await
            } else {
                discover_live_endpoints_strict().await
            };
            let live = match discovery {
                Ok(live) => live,
                Err(err) if matches!(action, SessionAction::Show { .. }) => {
                    eprintln!("one session: Native Control discovery unavailable: {err}");
                    Vec::new()
                }
                Err(err) => return Err(format!("cannot verify session is dormant: {err}")),
            };
            let runtime = live_for(&info, &live);
            match action {
                SessionAction::Show { json, .. } => {
                    let row = record(&info, runtime);
                    if json {
                        println!("{row}");
                    } else if let Some(object) = row.as_object() {
                        for (key, value) in object {
                            let rendered = value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string());
                            println!("{key}: {rendered}");
                        }
                    }
                    Ok(SessionDisposition::Complete(ExitCode::SUCCESS))
                }
                SessionAction::Tui { .. } => {
                    if let Some(runtime) = runtime {
                        return Err(live_error(runtime));
                    }
                    let lock = one_session::SessionLock::acquire(&info.path, &info.id)
                        .map_err(|err| format!("cannot lock session: {err}"))?;
                    cli.session = Some(info.path);
                    cli.cwd = Path::new(&info.cwd).to_path_buf();
                    cli.mode = RunMode::Interactive;
                    cli.no_session = false;
                    Ok(SessionDisposition::Enter(lock))
                }
                SessionAction::Exec { prompt, .. } => {
                    if let Some(runtime) = runtime {
                        if runtime.status["busy"] == true {
                            return Err(format!(
                                "{}; use one control steer or one control followup",
                                live_error(runtime)
                            ));
                        }
                        if runtime.status["capabilities"]["prompt"] != true {
                            return Err(format!(
                                "{}; this runtime does not support prompt",
                                live_error(runtime)
                            ));
                        }
                        let (_, result) = control_call(
                            &runtime.endpoint,
                            "prompt",
                            json!({
                                "text": prompt.join(" "),
                                "expected_session_id": info.id,
                                "expected_session_path": info.path,
                            }),
                        )
                        .await
                        .map_err(|err| format!("live prompt failed: {err}"))?;
                        println!("{result}");
                        return Ok(SessionDisposition::Complete(ExitCode::SUCCESS));
                    }
                    let lock = one_session::SessionLock::acquire(&info.path, &info.id)
                        .map_err(|err| format!("cannot lock session: {err}"))?;
                    cli.session = Some(info.path);
                    cli.cwd = Path::new(&info.cwd).to_path_buf();
                    cli.print = Some(prompt.join(" "));
                    cli.mode = RunMode::Print;
                    cli.exec_session = true;
                    cli.no_session = false;
                    Ok(SessionDisposition::Enter(lock))
                }
                SessionAction::List { .. } => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn endpoint(status: serde_json::Value) -> LiveControlEndpoint {
        LiveControlEndpoint {
            endpoint: PathBuf::from("/tmp/one-test.sock"),
            status,
        }
    }

    #[test]
    fn state_maps_unified_runtime_status_to_session_rows() {
        // dormant: no live runtime owns the session.
        assert_eq!(state(None), "dormant");

        // Live runtime, no in-flight turn.
        assert_eq!(
            state(Some(&endpoint(json!({"state": "idle", "busy": false})))),
            "live_idle"
        );

        // Turn in flight — running, abort-requested, or legacy busy-only.
        assert_eq!(
            state(Some(&endpoint(json!({"state": "running", "busy": true})))),
            "live_busy"
        );
        assert_eq!(
            state(Some(&endpoint(
                json!({"state": "abort_requested", "busy": true})
            ))),
            "live_busy"
        );
        // Older runtime without `state` still maps by the derived busy flag.
        assert_eq!(state(Some(&endpoint(json!({"busy": true})))), "live_busy");

        // Waiting states keep their finer granularity instead of collapsing
        // into live_idle / live_busy.
        assert_eq!(
            state(Some(&endpoint(
                json!({"state": "waiting_input", "busy": true})
            ))),
            "live_waiting_input"
        );
        assert_eq!(
            state(Some(&endpoint(
                json!({"state": "waiting_work", "busy": true})
            ))),
            "live_waiting_work"
        );
    }
}
