//! `job_output` / `wait_tasks` / `job_kill` — poll, wait, or stop background work.
//!
//! **Unified IDs**: `wait_tasks` and `job_kill` accept agent job ids (`job_*`),
//! bash background task ids (`bg_*`), and monitor ids (`mon_*`). Specialized
//! `bash_output` / `bash_kill`
//! remain available for shell-only use.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use one_tools::{format_task_list, format_task_output, BackgroundTaskRegistry, TaskState};
use serde_json::json;

use super::jobs::{format_job_list, format_job_snapshot, AgentJobRegistry, JobState, JoinMode};

pub struct JobOutputTool {
    jobs: Arc<AgentJobRegistry>,
    bash: Arc<BackgroundTaskRegistry>,
}

impl JobOutputTool {
    pub fn new(jobs: Arc<AgentJobRegistry>) -> Self {
        Self {
            jobs,
            bash: Arc::new(BackgroundTaskRegistry::new()),
        }
    }

    pub fn with_bash(jobs: Arc<AgentJobRegistry>, bash: Arc<BackgroundTaskRegistry>) -> Self {
        Self { jobs, bash }
    }
}

#[async_trait]
impl Tool for JobOutputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "job_output".into(),
            description: "\
Get status and summary of a background agent job (`job_*`), bash task (`bg_*`), or monitor (`mon_*`). \
Omit id to list agent jobs and bash tasks. Optional wait_ms waits for completion \
(0 = snapshot only)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Agent job id or bash task id. Alias: task_id."
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Alias for job_id (bash bg_* or agent job_*)"
                    },
                    "wait_ms": {
                        "type": "integer",
                        "description": "Max ms to wait for completion (default 0 = immediate snapshot)"
                    }
                }
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let job_id = call
            .arguments
            .get("job_id")
            .or_else(|| call.arguments.get("task_id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let wait_ms = call
            .arguments
            .get("wait_ms")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                call.arguments
                    .get("wait_ms")
                    .and_then(|v| v.as_i64())
                    .map(|n| n.max(0) as u64)
            });

        if job_id.is_none() {
            let jobs = self.jobs.list();
            let bash = self.bash.list();
            let mut text = format_job_list(&jobs);
            if !bash.is_empty() {
                text.push_str("\n--- bash background ---\n");
                text.push_str(&format_task_list(&bash));
            }
            return Ok(ToolOutput::text_with_details(
                text,
                json!({
                    "ok": true,
                    "count": jobs.len() + bash.len(),
                    "jobs": jobs.iter().map(|j| json!({
                        "id": j.id,
                        "kind": "agent",
                        "agent": j.agent,
                        "state": j.state.as_str(),
                        "status": j.status.map(|s| s.as_str()),
                    })).collect::<Vec<_>>(),
                    "bash": bash.iter().map(|t| json!({
                        "id": t.id,
                        "kind": "bash",
                        "state": t.state.as_str(),
                        "exitCode": t.exit_code,
                    })).collect::<Vec<_>>(),
                }),
            ));
        }

        let id = job_id.unwrap();
        if looks_like_bash_id(&id) || self.bash.get(&id).is_some() {
            let secs = wait_ms.map(|ms| (ms.saturating_add(999)) / 1000);
            let snap = self
                .bash
                .wait(&id, secs)
                .await
                .map_err(|e| tool_error("job_output", e))?;
            let running = snap.state == TaskState::Running;
            let text = format_task_output(&snap, 50_000);
            return Ok(ToolOutput::text_with_details(
                text,
                json!({
                    "ok": snap.state != TaskState::Failed || running,
                    "running": running,
                    "kind": "bash",
                    "job_id": snap.id,
                    "task_id": snap.id,
                    "state": snap.state.as_str(),
                    "exitCode": snap.exit_code,
                }),
            ));
        }

        let snap = self
            .jobs
            .wait(&id, wait_ms)
            .await
            .map_err(|e| tool_error("job_output", e))?;

        let running = snap.state == JobState::Running;
        let text = format_job_snapshot(&snap);
        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "ok": snap.ok || running,
                "running": running,
                "kind": "agent",
                "job_id": snap.id,
                "agent": snap.agent,
                "state": snap.state.as_str(),
                "status": snap.status.map(|s| s.as_str()),
                "duration_ms": snap.duration_ms,
                "turns": snap.turns,
                "max_turns": snap.max_turns,
            }),
        ))
    }
}

/// Block until background agent jobs and/or bash tasks finish.
pub struct WaitTasksTool {
    jobs: Arc<AgentJobRegistry>,
    bash: Arc<BackgroundTaskRegistry>,
}

impl WaitTasksTool {
    pub fn new(jobs: Arc<AgentJobRegistry>) -> Self {
        Self {
            jobs,
            bash: Arc::new(BackgroundTaskRegistry::new()),
        }
    }

    pub fn with_bash(jobs: Arc<AgentJobRegistry>, bash: Arc<BackgroundTaskRegistry>) -> Self {
        Self { jobs, bash }
    }
}

#[async_trait]
impl Tool for WaitTasksTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wait_tasks".into(),
            description: "\
Wait for background work to finish — agent jobs from task(background=true) (`job_*`) \
and/or bash tasks (`bg_*`) and monitors (`mon_*`). \
Use ONLY after you have spawned all needed background work and have nothing else useful to do. \
mode=all (default) waits for every target; mode=any returns when the next running task \
completes. Omit ids to wait on all currently running agent jobs and bash tasks. \
Optional wait_ms caps how long this call may block."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Ids to wait on (job_* / bg_* / mon_*). Omit = all currently running."
                    },
                    "task_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Alias for job_ids"
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["all", "any"],
                        "description": "all = wait for every target (default). any = wait for the next completion only."
                    },
                    "wait_ms": {
                        "type": "integer",
                        "description": "Max ms to block (optional). On timeout returns partial results + still-running list."
                    }
                }
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let mode = call
            .arguments
            .get("mode")
            .and_then(|v| v.as_str())
            .and_then(JoinMode::parse)
            .unwrap_or(JoinMode::All);

        let wait_ms = call
            .arguments
            .get("wait_ms")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                call.arguments
                    .get("wait_ms")
                    .and_then(|v| v.as_i64())
                    .map(|n| n.max(0) as u64)
            });

        let explicit = parse_id_list(call, &["job_ids", "task_ids"]);
        let report = unified_join(
            self.jobs.clone(),
            self.bash.clone(),
            explicit,
            mode,
            wait_ms,
        )
        .await
        .map_err(|e| tool_error("wait_tasks", e))?;

        Ok(ToolOutput::text_with_details(
            report.message,
            json!({
                "ok": report.ok,
                "mode": mode.as_str(),
                "timed_out": report.timed_out,
                "completed_events": report.events.len(),
                "events": report.events,
                "finals": report.finals,
            }),
        ))
    }
}

pub struct JobKillTool {
    jobs: Arc<AgentJobRegistry>,
    bash: Arc<BackgroundTaskRegistry>,
}

impl JobKillTool {
    pub fn new(jobs: Arc<AgentJobRegistry>) -> Self {
        Self {
            jobs,
            bash: Arc::new(BackgroundTaskRegistry::new()),
        }
    }

    pub fn with_bash(jobs: Arc<AgentJobRegistry>, bash: Arc<BackgroundTaskRegistry>) -> Self {
        Self { jobs, bash }
    }
}

#[async_trait]
impl Tool for JobKillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "job_kill".into(),
            description: "\
Stop a background agent job (`job_*`), bash task (`bg_*`), or monitor (`mon_*`). \
Accepts `job_id` or `task_id`. No-op if already finished."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Agent job id or bash task id"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Alias for job_id"
                    }
                }
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let id = call
            .arguments
            .get("job_id")
            .or_else(|| call.arguments.get("task_id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("job_kill", "missing job_id or task_id"))?
            .to_string();

        if looks_like_bash_id(&id) || self.bash.get(&id).is_some() {
            let snap = self
                .bash
                .kill(&id)
                .await
                .map_err(|e| tool_error("job_kill", e))?;
            let text = format!(
                "Killed background bash task\n{}",
                format_task_output(&snap, 8_000)
            );
            return Ok(ToolOutput::text_with_details(
                text,
                json!({
                    "ok": true,
                    "kind": "bash",
                    "job_id": snap.id,
                    "task_id": snap.id,
                    "state": snap.state.as_str(),
                    "exitCode": snap.exit_code,
                }),
            ));
        }

        let snap = self.jobs.kill(&id).map_err(|e| tool_error("job_kill", e))?;

        let text = format!(
            "job_id: {}\nstate: {}\nstatus: {}\n",
            snap.id,
            snap.state.as_str(),
            snap.status.map(|s| s.as_str()).unwrap_or("aborted")
        );
        Ok(ToolOutput::text_with_details(
            text,
            json!({
                "ok": true,
                "kind": "agent",
                "job_id": snap.id,
                "state": snap.state.as_str(),
                "status": snap.status.map(|s| s.as_str()),
            }),
        ))
    }
}

fn looks_like_bash_id(id: &str) -> bool {
    id.starts_with("bg_") || id.starts_with("mon_")
}

fn parse_id_list(call: &ToolCall, keys: &[&str]) -> Option<Vec<String>> {
    for key in keys {
        if let Some(v) = call.arguments.get(*key) {
            if let Some(arr) = v.as_array() {
                let ids: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                if !ids.is_empty() {
                    return Some(ids);
                }
            } else if let Some(s) = v.as_str() {
                let ids: Vec<String> = s
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .map(str::trim)
                    .filter(|x| !x.is_empty())
                    .map(|x| x.to_string())
                    .collect();
                if !ids.is_empty() {
                    return Some(ids);
                }
            }
        }
    }
    None
}

#[derive(Debug)]
struct UnifiedJoinReport {
    ok: bool,
    timed_out: bool,
    message: String,
    events: Vec<serde_json::Value>,
    finals: Vec<serde_json::Value>,
}

async fn unified_join(
    jobs: Arc<AgentJobRegistry>,
    bash: Arc<BackgroundTaskRegistry>,
    ids: Option<Vec<String>>,
    mode: JoinMode,
    wait_ms: Option<u64>,
) -> std::result::Result<UnifiedJoinReport, String> {
    let mut targets: Vec<String> = match ids {
        Some(list) if !list.is_empty() => list,
        _ => {
            let mut t = jobs.running_ids();
            for snap in bash.list() {
                if !snap.state.is_terminal() {
                    t.push(snap.id);
                }
            }
            if t.is_empty() {
                // Nothing running — still list known terminals if any.
                t.extend(jobs.list().into_iter().map(|j| j.id));
                t.extend(bash.list().into_iter().map(|s| s.id));
            }
            t
        }
    };
    targets.sort();
    targets.dedup();

    if targets.is_empty() {
        return Ok(UnifiedJoinReport {
            ok: true,
            timed_out: false,
            message: "No background tasks or agent jobs to wait on.\n".into(),
            events: vec![],
            finals: vec![],
        });
    }

    // Validate each id exists in one of the registries.
    for id in &targets {
        let known = jobs.get(id).is_some() || bash.get(id).is_some();
        if !known {
            return Err(format!("unknown id: {id} (not an agent job or bash task)"));
        }
    }

    let deadline = wait_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut seen_terminal: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut event_ids: Vec<String> = Vec::new();

    fn collect_terminal(
        id: &str,
        jobs: &AgentJobRegistry,
        bash: &BackgroundTaskRegistry,
        seen: &mut std::collections::HashSet<String>,
        events: &mut Vec<serde_json::Value>,
        event_ids: &mut Vec<String>,
    ) -> bool {
        if seen.contains(id) {
            return false;
        }
        if let Some(j) = jobs.get(id) {
            if j.state.is_terminal() {
                seen.insert(id.to_string());
                event_ids.push(id.to_string());
                events.push(json!({
                    "id": j.id,
                    "kind": "agent",
                    "agent": j.agent,
                    "state": j.state.as_str(),
                    "status": j.status.map(|s| s.as_str()),
                    "ok": j.ok,
                }));
                return true;
            }
        } else if let Some(t) = bash.get(id) {
            if t.state.is_terminal() {
                seen.insert(id.to_string());
                event_ids.push(id.to_string());
                events.push(json!({
                    "id": t.id,
                    "kind": "bash",
                    "state": t.state.as_str(),
                    "exitCode": t.exit_code,
                    "ok": t.state != TaskState::Failed,
                }));
                return true;
            }
        }
        false
    }

    for id in &targets {
        let _ = collect_terminal(
            id,
            &jobs,
            &bash,
            &mut seen_terminal,
            &mut events,
            &mut event_ids,
        );
    }
    jobs.absorb_notifications_for(&event_ids);
    if let Ok(mut guard) = bash.notification_queue().lock() {
        guard.retain(|text| !event_ids.iter().any(|id| text.contains(id.as_str())));
    }

    if targets.iter().all(|id| seen_terminal.contains(id)) {
        let finals = snapshot_finals(&jobs, &bash, &targets);
        let message = format_unified_report(mode, false, &events, &finals);
        return Ok(UnifiedJoinReport {
            ok: true,
            timed_out: false,
            message,
            events,
            finals,
        });
    }

    let mut timed_out = false;

    loop {
        let mut newly = 0u32;
        for id in &targets {
            if collect_terminal(
                id,
                &jobs,
                &bash,
                &mut seen_terminal,
                &mut events,
                &mut event_ids,
            ) {
                newly += 1;
            }
        }
        if newly > 0 {
            jobs.absorb_notifications_for(&event_ids);
            if let Ok(mut guard) = bash.notification_queue().lock() {
                guard.retain(|text| !event_ids.iter().any(|id| text.contains(id.as_str())));
            }
        }

        let still_pending = targets.iter().any(|id| !seen_terminal.contains(id));
        let done = match mode {
            JoinMode::All => !still_pending,
            JoinMode::Any => newly > 0 || !still_pending,
        };
        if done {
            break;
        }

        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                timed_out = true;
                break;
            }
            let remaining = dl.saturating_duration_since(Instant::now());
            let slice = remaining.min(Duration::from_millis(200));
            tokio::time::sleep(slice).await;
        } else {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    let finals = snapshot_finals(&jobs, &bash, &targets);
    let all_terminal = finals.iter().all(|f| {
        f.get("running")
            .and_then(|v| v.as_bool())
            .map(|r| !r)
            .unwrap_or(true)
    });
    let ok = if timed_out {
        false
    } else {
        match mode {
            JoinMode::All => all_terminal,
            JoinMode::Any => !events.is_empty(),
        }
    };
    let message = format_unified_report(mode, timed_out, &events, &finals);
    Ok(UnifiedJoinReport {
        ok,
        timed_out,
        message,
        events,
        finals,
    })
}

fn snapshot_finals(
    jobs: &AgentJobRegistry,
    bash: &BackgroundTaskRegistry,
    targets: &[String],
) -> Vec<serde_json::Value> {
    targets
        .iter()
        .filter_map(|id| {
            if let Some(j) = jobs.get(id) {
                Some(json!({
                    "id": j.id,
                    "kind": "agent",
                    "agent": j.agent,
                    "state": j.state.as_str(),
                    "status": j.status.map(|s| s.as_str()),
                    "ok": j.ok,
                    "running": j.state == JobState::Running,
                }))
            } else {
                bash.get(id).map(|t| {
                    json!({
                        "id": t.id,
                        "kind": "bash",
                        "state": t.state.as_str(),
                        "exitCode": t.exit_code,
                        "ok": t.state != TaskState::Failed,
                        "running": t.state == TaskState::Running,
                    })
                })
            }
        })
        .collect()
}

fn format_unified_report(
    mode: JoinMode,
    timed_out: bool,
    events: &[serde_json::Value],
    finals: &[serde_json::Value],
) -> String {
    let mut out = format!("[wait_tasks · mode={}]\n", mode.as_str());
    if timed_out {
        out.push_str("timed_out: true\n");
    }
    out.push_str(&format!("completed_events: {}\n", events.len()));
    for e in events {
        let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let state = e.get("state").and_then(|v| v.as_str()).unwrap_or("?");
        out.push_str(&format!("- {kind} {id}: {state}\n"));
    }
    out.push_str("finals:\n");
    for f in finals {
        let id = f.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = f.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let state = f.get("state").and_then(|v| v.as_str()).unwrap_or("?");
        let running = f
            .get("running")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        out.push_str(&format!(
            "- {kind} {id}: {state}{}\n",
            if running { " (running)" } else { "" }
        ));
    }
    out
}

