//! `job_output` / `wait_tasks` / `job_kill` — inspect, wait (event-driven), or stop
//! background work.
//!
//! **Unified IDs**: `wait_tasks` and `job_kill` accept agent job ids (`job_*`),
//! bash background task ids (`bg_*`), and monitor ids (`mon_*`). Specialized
//! `bash_output` / `bash_kill`
//! remain available for shell-only use.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use one_core::{RegisteredWaiter, TaskWaitWaiter, WaitInterest, WaitInterestMode};
use one_tools::{format_task_list, format_task_output, BackgroundTaskRegistry, TaskState};
use serde_json::json;

use super::jobs::{format_job_list, format_job_snapshot, AgentJobRegistry, JobState, JoinMode};

pub struct JobOutputTool {
    jobs: Arc<AgentJobRegistry>,
    bash: Arc<BackgroundTaskRegistry>,
    name: String,
}

impl JobOutputTool {
    pub fn new(jobs: Arc<AgentJobRegistry>) -> Self {
        Self {
            jobs,
            bash: Arc::new(BackgroundTaskRegistry::new()),
            name: "job_output".into(),
        }
    }

    pub fn with_bash(jobs: Arc<AgentJobRegistry>, bash: Arc<BackgroundTaskRegistry>) -> Self {
        Self {
            jobs,
            bash,
            name: "job_output".into(),
        }
    }

    pub fn named(
        jobs: Arc<AgentJobRegistry>,
        bash: Arc<BackgroundTaskRegistry>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            jobs,
            bash,
            name: name.into(),
        }
    }
}

#[async_trait]
impl Tool for JobOutputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: "\
Get status and summary of a background agent job (`job_*`), bash task (`bg_*`), or monitor (`mon_*`). \
Omit job_id to list agent jobs and bash tasks. \
Omit wait_ms (or pass 0) for an immediate snapshot. \
A positive wait_ms waits up to that many milliseconds, then returns the current \
status — still-running is a successful snapshot, not an error. You will be notified \
when the job completes; do not tight-poll."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Agent job id (`job_*`), bash task (`bg_*`), or monitor (`mon_*`)."
                    },
                    "wait_ms": {
                        "type": "integer",
                        "description": "0/omit = snapshot now. Positive = wait up to this many milliseconds."
                    }
                }
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let ids = parse_id_list(call, &["job_ids", "task_ids"]).unwrap_or_default();
        let job_id = call
            .arguments
            .get("job_id")
            .or_else(|| call.arguments.get("task_id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| ids.first().cloned());

        let wait_ms = parse_wait_ms(&call.arguments, WaitMsMode::SnapshotOrBlock);

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
        let mut text = format_job_snapshot(&snap);
        if running && wait_ms.unwrap_or(0) > 0 {
            text.push_str(
                "\nWaited; still running. You will be notified when it completes. \
                 Do not poll job_output again.\n",
            );
        }
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
                "log_path": snap.log_path.as_ref().map(|p| p.display().to_string()),
                "result_ref": snap.result_ref.as_ref().map(|p| p.display().to_string()),
                "result_bytes": snap.result_bytes,
                "result_chars": snap.result_chars,
                "result_truncated": snap.result_truncated,
            }),
        ))
    }
}

/// Combines subagent jobs and background bash tasks into a unified [`TaskWaitWaiter`].
pub struct CombinedTaskWaiter {
    jobs: Arc<AgentJobRegistry>,
    bash: Arc<BackgroundTaskRegistry>,
}

impl CombinedTaskWaiter {
    pub fn new(jobs: Arc<AgentJobRegistry>, bash: Arc<BackgroundTaskRegistry>) -> Self {
        Self { jobs, bash }
    }
}

impl TaskWaitWaiter for CombinedTaskWaiter {
    fn is_terminal(&self, id: &str) -> bool {
        if looks_like_bash_id(id) || self.bash.get(id).is_some() {
            self.bash.is_terminal(id)
        } else {
            self.jobs.is_terminal(id)
        }
    }

    fn subscribe_terminal<'a>(
        &'a self,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        if looks_like_bash_id(id) || self.bash.get(id).is_some() {
            if let Some(notify) = self.bash.done_notifier(id) {
                return Box::pin(RegisteredWaiter::new(notify));
            }
        } else {
            if let Some(notify) = self.jobs.done_notifier(id) {
                return Box::pin(RegisteredWaiter::new(notify));
            }
        }
        Box::pin(async {})
    }
}

/// Block until background agent jobs and/or bash tasks finish.
pub struct WaitTasksTool {
    jobs: Arc<AgentJobRegistry>,
    bash: Arc<BackgroundTaskRegistry>,
    name: String,
    wait_interest: Arc<Mutex<Option<WaitInterest>>>,
}

impl WaitTasksTool {
    pub fn new(jobs: Arc<AgentJobRegistry>) -> Self {
        Self {
            jobs,
            bash: Arc::new(BackgroundTaskRegistry::new()),
            name: "wait_tasks".into(),
            wait_interest: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_bash(jobs: Arc<AgentJobRegistry>, bash: Arc<BackgroundTaskRegistry>) -> Self {
        Self {
            jobs,
            bash,
            name: "wait_tasks".into(),
            wait_interest: Arc::new(Mutex::new(None)),
        }
    }

    pub fn named(
        jobs: Arc<AgentJobRegistry>,
        bash: Arc<BackgroundTaskRegistry>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            jobs,
            bash,
            name: name.into(),
            wait_interest: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_interest(
        jobs: Arc<AgentJobRegistry>,
        bash: Arc<BackgroundTaskRegistry>,
        name: impl Into<String>,
        wait_interest: Arc<Mutex<Option<WaitInterest>>>,
    ) -> Self {
        Self {
            jobs,
            bash,
            name: name.into(),
            wait_interest,
        }
    }

    pub fn wait_interest_handle(&self) -> Arc<Mutex<Option<WaitInterest>>> {
        self.wait_interest.clone()
    }
}

#[async_trait]
impl Tool for WaitTasksTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: "\
Wait for background work to finish — agent jobs from task(background=true) (`job_*`) \
and/or bash tasks (`bg_*`) and monitors (`mon_*`). \
Use ONLY after you have spawned all needed background work and have nothing else useful to do. \
Call wait_tasks ONCE: it checks the current state, and if targets are still running it \
parks the agent until they finish (event-driven; steer/abort still interrupts). \
Prefer job_output with a positive wait_ms for a single id. \
mode=all (default) waits for every target; mode=any returns when the next running task \
completes (aliases: wait_all / wait_any). Omit job_ids to wait on all currently running \
agent jobs and bash tasks. wait_ms is accepted for compatibility but ignored — \
the wait is event-driven with no timeout."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Ids to wait on (job_* / bg_* / mon_*). Omit = all currently running."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["all", "any", "wait_all", "wait_any"],
                        "description": "all / wait_all = wait for every target (default). any / wait_any = next completion only."
                    },
                    "wait_ms": {
                        "type": "integer",
                        "description": "Accepted for compatibility; ignored. The wait is event-driven with no timeout."
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

        // wait_ms is accepted for compatibility but ignored: the wait is
        // event-driven with no timeout (ReAct parks on the WaitInterest).
        let explicit = parse_id_list(call, &["job_ids", "task_ids"]);
        let report = unified_join(
            self.jobs.clone(),
            self.bash.clone(),
            explicit,
            mode,
            &self.wait_interest,
        )
        .await
        .map_err(|e| tool_error("wait_tasks", e))?;

        let still_running = report
            .finals
            .iter()
            .any(|f| f.get("running").and_then(|v| v.as_bool()).unwrap_or(false));

        // unified_join has already installed (or cleared) the WaitInterest for
        // still-running targets; the ReAct loop parks on it after this batch.

        Ok(ToolOutput::text_with_details(
            report.message,
            json!({
                "ok": report.ok,
                "running": still_running,
                "mode": mode.as_str(),
                "parked": still_running,
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
    name: String,
}

impl JobKillTool {
    pub fn new(jobs: Arc<AgentJobRegistry>) -> Self {
        Self {
            jobs,
            bash: Arc::new(BackgroundTaskRegistry::new()),
            name: "job_kill".into(),
        }
    }

    pub fn with_bash(jobs: Arc<AgentJobRegistry>, bash: Arc<BackgroundTaskRegistry>) -> Self {
        Self {
            jobs,
            bash,
            name: "job_kill".into(),
        }
    }

    pub fn named(
        jobs: Arc<AgentJobRegistry>,
        bash: Arc<BackgroundTaskRegistry>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            jobs,
            bash,
            name: name.into(),
        }
    }
}

#[async_trait]
impl Tool for JobKillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: "\
Stop a background agent job (`job_*`), bash task (`bg_*`), or monitor (`mon_*`). \
Pass `job_id`. No-op if already finished."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Agent job id (`job_*`), bash task (`bg_*`), or monitor (`mon_*`)."
                    }
                },
                "required": ["job_id"]
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

#[derive(Clone, Copy)]
enum WaitMsMode {
    /// `job_output`: 0 / omit = snapshot; any positive value waits.
    SnapshotOrBlock,
}

fn parse_wait_ms(args: &serde_json::Value, mode: WaitMsMode) -> Option<u64> {
    let raw = args
        .get("wait_ms")
        .or_else(|| args.get("timeout_ms"))
        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64)));
    match (mode, raw) {
        (WaitMsMode::SnapshotOrBlock, None) => None,
        (WaitMsMode::SnapshotOrBlock, Some(0)) => Some(0),
        (_, Some(ms)) => Some(ms),
    }
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
    /// True when still-running targets caused a WaitInterest registration;
    /// the ReAct loop parks after this tool batch and wakes on completion.
    parked: bool,
    message: String,
    events: Vec<serde_json::Value>,
    finals: Vec<serde_json::Value>,
}

/// Event-wait declaration: check current state once, then either return
/// immediately (target condition already satisfied) or install a
/// [`WaitInterest`] so the ReAct loop parks until completion notifications
/// fire. No polling, no default timeout.
async fn unified_join(
    jobs: Arc<AgentJobRegistry>,
    bash: Arc<BackgroundTaskRegistry>,
    ids: Option<Vec<String>>,
    mode: JoinMode,
    wait_interest: &Arc<Mutex<Option<WaitInterest>>>,
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
            parked: false,
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

    // Single state pass: targets already terminal become report events and
    // their queued completion notifications are absorbed (report is the
    // delivery channel for these — no double-notify on the next turn).
    let mut seen_terminal: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut event_ids: Vec<String> = Vec::new();

    for id in &targets {
        if let Some(j) = jobs.get(id) {
            if j.state.is_terminal() {
                seen_terminal.insert(id.to_string());
                event_ids.push(id.to_string());
                events.push(json!({
                    "id": j.id,
                    "kind": "agent",
                    "agent": j.agent,
                    "state": j.state.as_str(),
                    "status": j.status.map(|s| s.as_str()),
                    "ok": j.ok,
                }));
            }
        } else if let Some(t) = bash.get(id) {
            if t.state.is_terminal() {
                seen_terminal.insert(id.to_string());
                event_ids.push(id.to_string());
                events.push(json!({
                    "id": t.id,
                    "kind": "bash",
                    "state": t.state.as_str(),
                    "exitCode": t.exit_code,
                    "ok": t.state != TaskState::Failed,
                }));
            }
        }
    }
    if !event_ids.is_empty() {
        jobs.absorb_notifications_for(&event_ids);
        if let Ok(mut guard) = bash.notification_queue().lock() {
            guard.retain(|text| !event_ids.iter().any(|id| text.contains(id.as_str())));
        }
    }

    let still_running: Vec<String> = targets
        .iter()
        .filter(|id| !seen_terminal.contains(*id))
        .cloned()
        .collect();

    // Satisfied right now? Nothing still running → return immediately
    // (mode=any still waits for the next *running* completion, per its
    // documented semantics; already-terminal targets are events, not
    // satisfaction).
    let satisfied = still_running.is_empty();

    let finals = snapshot_finals(&jobs, &bash, &targets);

    if satisfied {
        // Fresh declaration superseded any stale interest.
        if let Ok(mut g) = wait_interest.lock() {
            *g = None;
        }
        let message = format_unified_report(mode, false, &events, &finals);
        return Ok(UnifiedJoinReport {
            ok: true,
            parked: false,
            message,
            events,
            finals,
        });
    }

    // Declare the wait: park the ReAct loop on these still-running ids.
    // Completion notifications (done_notifier) wake it; steer/followup/abort
    // interrupt the park as usual.
    let interest_mode = match mode {
        JoinMode::All => WaitInterestMode::All,
        JoinMode::Any => WaitInterestMode::Any,
    };
    let waiter = Arc::new(CombinedTaskWaiter::new(jobs.clone(), bash.clone()));
    let interest = WaitInterest::new(still_running, interest_mode, waiter);
    if let Ok(mut g) = wait_interest.lock() {
        *g = Some(interest);
    }

    let message = format_unified_report(mode, true, &events, &finals);
    Ok(UnifiedJoinReport {
        ok: true,
        parked: true,
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
                    "running": !j.state.is_terminal(),
                }))
            } else {
                bash.get(id).map(|t| {
                    json!({
                        "id": t.id,
                        "kind": "bash",
                        "state": t.state.as_str(),
                        "exitCode": t.exit_code,
                        "ok": t.state != TaskState::Failed,
                        "running": !t.state.is_terminal(),
                    })
                })
            }
        })
        .collect()
}

fn format_unified_report(
    mode: JoinMode,
    parked: bool,
    events: &[serde_json::Value],
    finals: &[serde_json::Value],
) -> String {
    let mut out = format!("[wait_tasks · mode={}]\n", mode.as_str());
    if parked {
        out.push_str("waiting: true (parked on completion notifications)\n");
    }
    out.push_str(&format!("completed_events: {}\n", events.len()));
    for e in events {
        let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let state = e.get("state").and_then(|v| v.as_str()).unwrap_or("?");
        out.push_str(&format!("- {kind} {id}: {state}\n"));
    }
    out.push_str("finals:\n");
    let mut any_running = false;
    for f in finals {
        let id = f.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = f.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let state = f.get("state").and_then(|v| v.as_str()).unwrap_or("?");
        let running = f.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
        any_running |= running;
        out.push_str(&format!(
            "- {kind} {id}: {state}{}\n",
            if running { " (running)" } else { "" }
        ));
    }
    if parked && any_running {
        out.push_str(
            "\nParked until the running targets complete — a completion notice will be \
             injected automatically. Do not call wait_tasks again.\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::{CompletionRequest, CompletionResponse, LlmProvider};
    use serde_json::json;

    #[test]
    fn wait_ms_snapshot_allows_zero_and_omit() {
        assert_eq!(parse_wait_ms(&json!({}), WaitMsMode::SnapshotOrBlock), None);
        assert_eq!(
            parse_wait_ms(&json!({"wait_ms": 0}), WaitMsMode::SnapshotOrBlock),
            Some(0)
        );
        assert_eq!(
            parse_wait_ms(&json!({"wait_ms": 30_000}), WaitMsMode::SnapshotOrBlock),
            Some(30_000)
        );
        assert_eq!(
            parse_wait_ms(&json!({"timeout_ms": 5_000}), WaitMsMode::SnapshotOrBlock),
            Some(5_000)
        );
    }

    #[test]
    fn wait_tasks_report_parks_with_notify_hint() {
        let msg = format_unified_report(
            JoinMode::All,
            true,
            &[],
            &[json!({
                "id": "job_1",
                "kind": "agent",
                "state": "running",
                "running": true,
            })],
        );
        assert!(msg.contains("waiting: true"), "{msg}");
        assert!(msg.contains("Parked until"), "{msg}");
        assert!(msg.contains("Do not call wait_tasks again"), "{msg}");
    }

    #[test]
    fn join_mode_accepts_grok_wait_all_alias() {
        assert_eq!(JoinMode::parse("wait_all"), Some(JoinMode::All));
        assert_eq!(JoinMode::parse("wait_any"), Some(JoinMode::Any));
        assert_eq!(JoinMode::parse("all"), Some(JoinMode::All));
    }

    #[test]
    fn job_output_schema_hides_task_id_alias() {
        let jobs = AgentJobRegistry::new(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        let tool = JobOutputTool::new(jobs);
        let def = tool.definition();
        let props = def.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("job_id"));
        assert!(
            !props.contains_key("task_id"),
            "do not advertise task_id alias: {props:?}"
        );
    }

    #[tokio::test]
    async fn wait_tasks_registers_wait_interest_immediately_when_running() {
        let jobs = AgentJobRegistry::new(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        let bash = Arc::new(BackgroundTaskRegistry::new());
        let wait_interest = Arc::new(Mutex::new(None));
        let tool = WaitTasksTool::with_interest(
            jobs.clone(),
            bash.clone(),
            "wait_tasks",
            wait_interest.clone(),
        );

        // Spawn a background bash task that sleeps
        let task_id = bash
            .spawn("sleep 5".into(), std::path::PathBuf::from("."), None)
            .await
            .expect("spawn");

        // wait_tasks must return immediately (no blocking wait) and register
        // the interest — wait_ms is ignored now.
        let started = std::time::Instant::now();
        let out = tool
            .execute(&ToolCall {
                id: "call_1".into(),
                name: "wait_tasks".into(),
                arguments: json!({
                    "task_ids": [task_id.clone()],
                }),
            })
            .await
            .expect("execute");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "wait_tasks must not block; took {:?}",
            started.elapsed()
        );

        let out_text = out
            .content
            .iter()
            .filter_map(|c| match c {
                one_core::message::TextOrImage::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out_text.contains("waiting: true"), "{out_text}");

        let interest = wait_interest.lock().unwrap().clone();
        assert!(
            interest.is_some(),
            "WaitInterest must be registered while still running"
        );
        let interest = interest.unwrap();
        assert_eq!(interest.ids, vec![task_id.clone()]);
        assert_eq!(interest.mode, WaitInterestMode::All);
        assert!(!interest.is_satisfied());

        // Now kill the task — the interest must become satisfiable.
        let _ = bash.kill(&task_id).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(interest.is_satisfied());
    }

    #[tokio::test]
    async fn wait_tasks_clears_wait_interest_on_completion() {
        let jobs = AgentJobRegistry::new(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        let bash = Arc::new(BackgroundTaskRegistry::new());
        let wait_interest = Arc::new(Mutex::new(None));
        let tool = WaitTasksTool::with_interest(
            jobs.clone(),
            bash.clone(),
            "wait_tasks",
            wait_interest.clone(),
        );

        // Spawn a quick task and let it finish.
        let task_id = bash
            .spawn("echo done".into(), std::path::PathBuf::from("."), None)
            .await
            .expect("spawn");
        let _ = bash
            .wait(&task_id, Some(5))
            .await
            .expect("task must finish");

        // Already-satisfied target: wait_tasks returns immediately and must
        // NOT register an interest (it also clears any stale one).
        let out = tool
            .execute(&ToolCall {
                id: "call_2".into(),
                name: "wait_tasks".into(),
                arguments: json!({
                    "task_ids": [task_id.clone()],
                }),
            })
            .await
            .expect("execute");

        let out_text = out
            .content
            .iter()
            .filter_map(|c| match c {
                one_core::message::TextOrImage::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!out_text.contains("waiting: true"), "{out_text}");
        assert!(out_text.contains("completed"), "{out_text}");
        let interest = wait_interest.lock().unwrap().clone();
        assert!(
            interest.is_none(),
            "WaitInterest must be None on completion"
        );
    }

    #[tokio::test]
    async fn agent_with_wait_tasks_parks_and_resumes_on_bash_completion() {
        use one_core::message::{AgentMessage, ContentBlock, StopReason};
        use one_core::{
            Agent, AgentConfig, CompletionRequest, CompletionResponse, LlmProvider, TokenUsage,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let notif_queue = agent.notification_queue_handle();
        let jobs = AgentJobRegistry::new(notif_queue.clone());
        let bash = Arc::new(BackgroundTaskRegistry::with_notification_queue(
            notif_queue.clone(),
        ));
        let wait_interest = agent.wait_interest_handle();

        // Spawn a background bash task that takes ~120ms
        let task_id = bash
            .spawn(
                "sleep 0.12 && echo 'finished_work'".into(),
                std::path::PathBuf::from("."),
                None,
            )
            .await
            .expect("spawn");

        let wait_tool = Arc::new(WaitTasksTool::with_interest(
            jobs.clone(),
            bash.clone(),
            "wait_tasks",
            wait_interest.clone(),
        ));
        agent.set_tools(vec![wait_tool]);

        struct E2eProvider {
            turns: AtomicUsize,
            task_id: String,
        }

        #[async_trait]
        impl LlmProvider for E2eProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> one_core::error::Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                match turn {
                    0 => Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::ToolCall {
                            id: "tc_wait".into(),
                            name: "wait_tasks".into(),
                            arguments: json!({
                                "task_ids": [self.task_id],
                            }),
                        }],
                        stop_reason: StopReason::ToolUse,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    }),
                    _ => {
                        let has_completion = request.messages.iter().any(|m| match m {
                            AgentMessage::User(u) => {
                                let txt = u.content.as_plain_text();
                                txt.contains(&self.task_id) && txt.contains("completed")
                            }
                            _ => false,
                        });
                        assert!(
                            has_completion,
                            "woken turn must receive background task completion notification"
                        );
                        Ok(CompletionResponse {
                            provider: self.name().into(),
                            model: self.model().into(),
                            content: vec![ContentBlock::text("Task completed and verified!")],
                            stop_reason: StopReason::Stop,
                            usage: TokenUsage::default(),
                            citations: Vec::new(),
                        })
                    }
                }
            }
        }

        let provider = E2eProvider {
            turns: AtomicUsize::new(0),
            task_id,
        };
        let final_answer = agent
            .prompt(&provider, "run and wait")
            .await
            .expect("prompt");
        assert_eq!(final_answer, "Task completed and verified!");
        assert_eq!(provider.turns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn job_output_on_200kb_subagent_result_is_capped() {
        use crate::protocol::RunResult;
        use crate::runtime::harness::HarnessOptions;
        use crate::runtime::jobs::SpawnOptions;

        // Isolate the spill dir: the default (~/.one/agent/jobs) may be
        // read-only in sandboxed test environments. ENV_LOCK serializes with
        // other suites that mutate the same process-global env vars.
        let _guard = super::super::jobs::test_env::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let spill_dir = std::env::temp_dir().join(format!(
            "one-job-output-spill-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&spill_dir).expect("create spill dir");
        std::env::set_var("ONE_JOB_RESULT_DIR", &spill_dir);

        let queue = Arc::new(Mutex::new(Vec::new()));
        let jobs = AgentJobRegistry::new(queue);
        let bash = Arc::new(BackgroundTaskRegistry::new());
        let (id, _ctrl, _abort) = jobs.register_job(
            "explore",
            Some("heavy task"),
            &HarnessOptions::from_cwd(std::env::temp_dir()),
            16,
            &SpawnOptions {
                notify_completion: false,
                apply_wall_timeout: false,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
                backend: None,
            },
            false,
        );

        let prefix = "OUTPUT_PREFIX_200K_";
        let tail = "_OUTPUT_TAIL_200K_MARKER";
        let heavy = format!("{prefix}{}{tail}", "M".repeat(200_000));
        jobs.finalize(&id, RunResult::success(heavy.clone(), 120));

        let tool = JobOutputTool::with_bash(jobs.clone(), bash);
        let output = tool
            .execute(&ToolCall {
                id: "c_job_out".into(),
                name: "job_output".into(),
                arguments: json!({
                    "job_id": id,
                    "wait_ms": 0,
                }),
            })
            .await
            .expect("job_output execute");

        let text = output
            .content
            .first()
            .map(|c| c.as_display_text())
            .unwrap_or_default();
        assert!(
            text.len() < 16 * 1024,
            "job_output text must be bounded (got {} bytes)",
            text.len()
        );
        assert!(text.contains(prefix));
        assert!(
            !text.contains(tail),
            "job_output must NOT dump full 200KB tail"
        );
        assert!(text.contains("result_ref:"));
        assert!(text.contains("result_truncated: true"));

        let details = output.details.expect("details json");
        assert_eq!(details["kind"], "agent");
        assert_eq!(details["result_chars"], heavy.chars().count());
        assert_eq!(details["result_bytes"], heavy.len());
        assert_eq!(details["result_truncated"], true);
        let result_ref_str = details["result_ref"].as_str().expect("result_ref string");
        let path = std::path::PathBuf::from(result_ref_str);
        assert!(path.exists());
        let disk_text = std::fs::read_to_string(&path).expect("read result_ref");
        assert_eq!(disk_text, heavy);
        let _ = std::fs::remove_file(path);
        std::env::remove_var("ONE_JOB_RESULT_DIR");
        let _ = std::fs::remove_dir_all(&spill_dir);
    }

    #[tokio::test]
    async fn agent_with_subagent_200kb_result_parks_and_resumes_without_full_result_in_request() {
        use crate::protocol::RunResult;
        use crate::runtime::harness::HarnessOptions;
        use crate::runtime::jobs::SpawnOptions;
        use one_core::message::{AgentMessage, ContentBlock, StopReason};
        use one_core::{
            Agent, AgentConfig, CompletionRequest, CompletionResponse, LlmProvider, TokenUsage,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Isolate the spill dir (read-only HOME in sandboxed test runs would
        // make the spill fail and drop result_ref from the notification).
        let _guard = super::super::jobs::test_env::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let spill_dir = std::env::temp_dir().join(format!(
            "one-job-parks-spill-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&spill_dir).expect("create spill dir");
        std::env::set_var("ONE_JOB_RESULT_DIR", &spill_dir);

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let notif_queue = agent.notification_queue_handle();
        let jobs = AgentJobRegistry::new(notif_queue.clone());
        let bash = Arc::new(BackgroundTaskRegistry::with_notification_queue(
            notif_queue.clone(),
        ));
        let wait_interest = agent.wait_interest_handle();

        let (job_id, _ctrl, _abort) = jobs.register_job(
            "explore",
            Some("async 200kb worker"),
            &HarnessOptions::from_cwd(std::env::temp_dir()),
            16,
            &SpawnOptions {
                notify_completion: true,
                apply_wall_timeout: false,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
                backend: None,
            },
            false,
        );

        let wait_tool = Arc::new(WaitTasksTool::with_interest(
            jobs.clone(),
            bash.clone(),
            "wait_tasks",
            wait_interest.clone(),
        ));
        agent.set_tools(vec![wait_tool]);

        let prefix = "REACT_200K_START_";
        let tail = "_REACT_200K_TAIL_MARKER_NEVER_IN_PROMPT";
        let big_body = format!("{prefix}{}{tail}", "K".repeat(200_000));
        let big_body_clone = big_body.clone();

        // Simulate async completion after 50ms
        let jobs_clone = jobs.clone();
        let jid = job_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            jobs_clone.finalize(&jid, RunResult::success(big_body_clone, 88));
        });

        struct Subagent200kProvider {
            turns: AtomicUsize,
            job_id: String,
            tail_marker: String,
        }

        #[async_trait]
        impl LlmProvider for Subagent200kProvider {
            fn name(&self) -> &str {
                "test-subagent-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> one_core::error::Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                match turn {
                    0 => Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::ToolCall {
                            id: "tc_wait".into(),
                            name: "wait_tasks".into(),
                            arguments: json!({
                                "job_id": self.job_id,
                            }),
                        }],
                        stop_reason: StopReason::ToolUse,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    }),
                    _ => {
                        // Resumed turn: inspect all messages in request
                        let mut found_notification = false;
                        for m in &request.messages {
                            let text = match m {
                                AgentMessage::User(u) => u.content.as_plain_text(),
                                AgentMessage::Assistant(a) => a
                                    .content
                                    .iter()
                                    .filter_map(|c| match c {
                                        ContentBlock::Text { text } => Some(text.as_str()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" "),
                                _ => String::new(),
                            };
                            if text.contains(&self.job_id) && text.contains("[job completed]") {
                                found_notification = true;
                                assert!(
                                    text.contains("result_ref:"),
                                    "completion notification must include result_ref"
                                );
                                assert!(
                                    text.contains("result_truncated: true"),
                                    "completion notification must indicate result_truncated"
                                );
                            }
                            assert!(
                                !text.contains(&self.tail_marker),
                                "CRITICAL: LLM request must NOT contain the full 200KB body or tail marker!"
                            );
                        }
                        assert!(
                            found_notification,
                            "woken turn must receive subagent job completion notification"
                        );

                        Ok(CompletionResponse {
                            provider: self.name().into(),
                            model: self.model().into(),
                            content: vec![ContentBlock::text("Subagent finished and verified!")],
                            stop_reason: StopReason::Stop,
                            usage: TokenUsage::default(),
                            citations: Vec::new(),
                        })
                    }
                }
            }
        }

        let provider = Subagent200kProvider {
            turns: AtomicUsize::new(0),
            job_id: job_id.clone(),
            tail_marker: tail.to_string(),
        };

        let final_answer = agent
            .prompt(&provider, "wait for subagent")
            .await
            .expect("agent prompt");

        assert_eq!(final_answer, "Subagent finished and verified!");
        assert_eq!(provider.turns.load(Ordering::SeqCst), 2);

        // Verify result_ref file exists and has full 200KB content
        let snap = jobs.get(&job_id).expect("snapshot");
        let path = snap.result_ref.expect("result_ref");
        assert!(path.exists());
        let disk_text = std::fs::read_to_string(&path).expect("read spill file");
        assert_eq!(disk_text, big_body);
        let _ = std::fs::remove_file(path);
        std::env::remove_var("ONE_JOB_RESULT_DIR");
        let _ = std::fs::remove_dir_all(&spill_dir);
    }

    // ---- Acceptance: event-driven wait_tasks (no 30s default / 200ms poll) ----

    /// Shared fake provider:
    /// - turn 0: issues `first_calls[0]` (e.g. bash with auto-bg)
    /// - turn 1: issues `first_calls[1]` (wait_tasks) if present, else asserts
    /// - final turn: asserts the conversation contains `expected_snippet`
    struct AcceptanceProvider {
        turns: std::sync::atomic::AtomicUsize,
        first_calls: Vec<one_core::message::ContentBlock>,
        expected_snippet: String,
        final_text: &'static str,
    }

    #[async_trait]
    impl LlmProvider for AcceptanceProvider {
        fn name(&self) -> &str {
            "acceptance"
        }
        fn model(&self) -> &str {
            "test"
        }
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> one_core::error::Result<CompletionResponse> {
            use one_core::message::{AgentMessage, ContentBlock};
            use std::sync::atomic::Ordering;
            let turn = self.turns.fetch_add(1, Ordering::SeqCst);
            if let Some(call) = self.first_calls.get(turn) {
                return Ok(CompletionResponse {
                    provider: self.name().into(),
                    model: self.model().into(),
                    content: vec![call.clone()],
                    stop_reason: one_core::message::StopReason::ToolUse,
                    usage: Default::default(),
                    citations: Vec::new(),
                });
            }
            let conversation = request
                .messages
                .iter()
                .map(|m| match m {
                    AgentMessage::User(u) => u.content.as_plain_text(),
                    AgentMessage::Assistant(a) => a
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    AgentMessage::ToolResult(t) => t
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            one_core::message::TextOrImage::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                conversation.contains(&self.expected_snippet),
                "woken turn must see completion ({:?}); got:\n{conversation}",
                self.expected_snippet
            );
            Ok(CompletionResponse {
                provider: self.name().into(),
                model: self.model().into(),
                content: vec![ContentBlock::text(self.final_text)],
                stop_reason: one_core::message::StopReason::Stop,
                usage: Default::default(),
                citations: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn acceptance_bash_auto_bg_then_wait_tasks_parks_until_done() {
        use one_core::message::{ContentBlock, StopReason};
        use one_core::{Agent, AgentConfig, LlmProvider};
        use one_tools::BashTool;
        use std::sync::atomic::AtomicUsize;

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let notif_queue = agent.notification_queue_handle();
        let jobs = AgentJobRegistry::new(notif_queue.clone());
        let bash = Arc::new(BackgroundTaskRegistry::with_notification_queue(
            notif_queue.clone(),
        ));
        let wait_interest = agent.wait_interest_handle();

        // Auto-bg after 300ms (shortened 15s budget) — sleep 3 outlives it.
        let bash_tool: Arc<dyn one_core::Tool> = Arc::new(
            BashTool::with_policy(
                one_tools::PathPolicy::workspace(std::path::PathBuf::from(".")),
                true,
                bash.clone(),
            )
            .with_auto_background(true)
            .with_foreground_budget_ms(300),
        );

        let wait_tool = Arc::new(WaitTasksTool::with_interest(
            jobs.clone(),
            bash.clone(),
            "wait_tasks",
            wait_interest.clone(),
        ));
        agent.set_tools(vec![bash_tool, wait_tool]);

        let provider = AcceptanceProvider {
            turns: AtomicUsize::new(0),
            first_calls: vec![
                ContentBlock::ToolCall {
                    id: "tc_bash".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "sleep 3; echo done_long", "timeout_secs": 30}),
                },
                // Omitted ids = all currently running (the auto-bg'd task).
                ContentBlock::ToolCall {
                    id: "tc_wait".into(),
                    name: "wait_tasks".into(),
                    arguments: json!({}),
                },
            ],
            expected_snippet: "[Background task completed]".into(),
            final_text: "bash done",
        };

        let started = std::time::Instant::now();
        let answer = agent.prompt(&provider, "run long sleep then wait").await;
        let answer = answer.expect("agent prompt");
        assert_eq!(answer, "bash done");
        // Auto-bg (300ms) + park + completion (3s). Assert the park was
        // event-driven: total >= sleep duration (did not return early).
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(2_800),
            "agent must park until bash completion; returned after {:?}",
            started.elapsed()
        );
        assert_eq!(provider.turns.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(
            wait_interest.lock().unwrap().is_none(),
            "interest must be cleared after satisfaction"
        );
    }

    #[tokio::test]
    async fn acceptance_bash_hard_timeout_wakes_park() {
        use one_core::message::{ContentBlock, StopReason};
        use one_core::{Agent, AgentConfig, LlmProvider};
        use one_tools::BashTool;
        use std::sync::atomic::AtomicUsize;

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let notif_queue = agent.notification_queue_handle();
        let jobs = AgentJobRegistry::new(notif_queue.clone());
        let bash = Arc::new(BackgroundTaskRegistry::with_notification_queue(
            notif_queue.clone(),
        ));
        let wait_interest = agent.wait_interest_handle();

        let bash_tool: Arc<dyn one_core::Tool> = Arc::new(
            BashTool::with_policy(
                one_tools::PathPolicy::workspace(std::path::PathBuf::from(".")),
                true,
                bash.clone(),
            )
            .with_auto_background(true)
            .with_foreground_budget_ms(300),
        );

        let wait_tool = Arc::new(WaitTasksTool::with_interest(
            jobs.clone(),
            bash.clone(),
            "wait_tasks",
            wait_interest.clone(),
        ));
        agent.set_tools(vec![bash_tool, wait_tool]);

        // Hard timeout 1s: auto-bg at 300ms, then TimedOut at 1s → kill → wake.
        let provider = AcceptanceProvider {
            turns: AtomicUsize::new(0),
            first_calls: vec![
                ContentBlock::ToolCall {
                    id: "tc_bash".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "sleep 60", "timeout_secs": 1}),
                },
                ContentBlock::ToolCall {
                    id: "tc_wait".into(),
                    name: "wait_tasks".into(),
                    arguments: json!({}),
                },
            ],
            expected_snippet: "timed_out".into(),
            final_text: "bash timed out",
        };

        let started = std::time::Instant::now();
        let answer = agent.prompt(&provider, "run then wait").await;
        assert_eq!(answer.expect("agent prompt"), "bash timed out");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "park must wake on timeout notification"
        );
        assert!(
            wait_interest.lock().unwrap().is_none(),
            "interest must be cleared after satisfaction"
        );
    }

    #[tokio::test]
    async fn acceptance_agent_jobs_all_and_any() {
        use crate::protocol::RunResult;
        use crate::runtime::harness::HarnessOptions;
        use crate::runtime::jobs::SpawnOptions;

        // --- mode=all: two jobs finalized at different times; park until both.
        let queue = Arc::new(Mutex::new(Vec::new()));
        let jobs = AgentJobRegistry::new(queue);
        let bash = Arc::new(BackgroundTaskRegistry::new());
        let wait_interest: Arc<Mutex<Option<WaitInterest>>> = Arc::new(Mutex::new(None));
        let tool = WaitTasksTool::with_interest(
            jobs.clone(),
            bash.clone(),
            "wait_tasks",
            wait_interest.clone(),
        );

        let (id_fast, _ctrl, _abort) = jobs.register_job(
            "explore",
            Some("fast"),
            &HarnessOptions::from_cwd(std::env::temp_dir()),
            16,
            &SpawnOptions {
                notify_completion: false,
                apply_wall_timeout: false,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
                backend: None,
            },
            false,
        );
        let (id_slow, _ctrl, _abort) = jobs.register_job(
            "explore",
            Some("slow"),
            &HarnessOptions::from_cwd(std::env::temp_dir()),
            16,
            &SpawnOptions {
                notify_completion: false,
                apply_wall_timeout: false,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
                backend: None,
            },
            false,
        );

        let jobs_slow = jobs.clone();
        let slow_id = id_slow.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            jobs_slow.finalize(&slow_id, RunResult::success("slow done", 2));
        });

        // wait_tasks while both running → must park (not return terminal).
        let out = tool
            .execute(&ToolCall {
                id: "c_all".into(),
                name: "wait_tasks".into(),
                arguments: json!({"task_ids": [id_fast.clone(), id_slow.clone()], "mode": "all"}),
            })
            .await
            .expect("execute all");
        let details = out.details.expect("details");
        assert_eq!(details["parked"], true, "{details}");
        let interest = wait_interest.lock().unwrap().clone().expect("interest");
        assert_eq!(interest.mode, WaitInterestMode::All);
        assert_eq!(interest.ids.len(), 2);

        // Finalize the fast one; still not satisfied (mode=all).
        jobs.finalize(&id_fast, RunResult::success("fast done", 1));
        assert!(!interest.is_satisfied(), "all-mode must need both");

        // Wait for slow finalize then satisfied + a second call returns immediately.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(interest.is_satisfied(), "all-mode satisfied after both");

        let started = std::time::Instant::now();
        let out2 = tool
            .execute(&ToolCall {
                id: "c_all2".into(),
                name: "wait_tasks".into(),
                arguments: json!({"task_ids": [id_fast.clone(), id_slow.clone()], "mode": "all"}),
            })
            .await
            .expect("execute all again");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let details2 = out2.details.expect("details");
        assert_eq!(details2["parked"], false, "{details2}");
        assert_eq!(details2["completed_events"], 2, "{details2}");
        assert!(wait_interest.lock().unwrap().is_none());

        // --- mode=any: interest satisfied by the first completion.
        let (id_a, _c, _a) = jobs.register_job(
            "explore",
            Some("any a"),
            &HarnessOptions::from_cwd(std::env::temp_dir()),
            16,
            &SpawnOptions {
                notify_completion: false,
                apply_wall_timeout: false,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
                backend: None,
            },
            false,
        );
        let (id_b, _c, _b) = jobs.register_job(
            "explore",
            Some("any b"),
            &HarnessOptions::from_cwd(std::env::temp_dir()),
            16,
            &SpawnOptions {
                notify_completion: false,
                apply_wall_timeout: false,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
                backend: None,
            },
            false,
        );
        let out_any = tool
            .execute(&ToolCall {
                id: "c_any".into(),
                name: "wait_tasks".into(),
                arguments: json!({"task_ids": [id_a.clone(), id_b.clone()], "mode": "any"}),
            })
            .await
            .expect("execute any");
        let details_any = out_any.details.expect("details");
        assert_eq!(details_any["parked"], true, "{details_any}");
        let interest_any = wait_interest.lock().unwrap().clone().expect("any interest");
        assert_eq!(interest_any.mode, WaitInterestMode::Any);

        jobs.finalize(&id_a, RunResult::success("a done", 1));
        assert!(interest_any.is_satisfied(), "any-mode satisfied by one");
        assert!(!interest_any.waiter.is_terminal(&id_b));

        // job_output stays snapshot-only: no wait_ms → immediate, running state.
        let out_snap = JobOutputTool::with_bash(jobs.clone(), bash.clone())
            .execute(&ToolCall {
                id: "c_snap".into(),
                name: "job_output".into(),
                arguments: json!({"job_id": id_b.clone()}),
            })
            .await
            .expect("snapshot");
        let details_snap = out_snap.details.expect("snap details");
        assert_eq!(details_snap["running"], true, "{details_snap}");
        let text_snap = out_snap
            .content
            .iter()
            .filter_map(|c| match c {
                one_core::message::TextOrImage::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text_snap.contains("Waited"), "snapshot must not block");
    }
}
