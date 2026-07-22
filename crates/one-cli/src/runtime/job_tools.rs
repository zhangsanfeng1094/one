//! `job_output` / `wait_tasks` / `job_kill` — poll, wait, or stop background agent jobs.

use std::sync::Arc;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;

use super::jobs::{format_job_list, format_job_snapshot, AgentJobRegistry, JobState, JoinMode};

pub struct JobOutputTool {
    jobs: Arc<AgentJobRegistry>,
}

impl JobOutputTool {
    pub fn new(jobs: Arc<AgentJobRegistry>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobOutputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "job_output".into(),
            description: "\
Get status and summary of a background agent job started with task(background=true). \
Omit job_id to list all agent jobs. Optional wait_ms waits for completion (0 = snapshot only)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Job id from task(background=true). Omit to list all jobs."
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
            let list = self.jobs.list();
            let text = format_job_list(&list);
            return Ok(ToolOutput::text_with_details(
                text,
                json!({
                    "ok": true,
                    "count": list.len(),
                    "jobs": list.iter().map(|j| json!({
                        "id": j.id,
                        "agent": j.agent,
                        "state": j.state.as_str(),
                        "status": j.status.map(|s| s.as_str()),
                    })).collect::<Vec<_>>(),
                }),
            ));
        }

        let id = job_id.unwrap();
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

/// Block until background `task` jobs finish — use when the foreground is idle.
pub struct WaitTasksTool {
    jobs: Arc<AgentJobRegistry>,
}

impl WaitTasksTool {
    pub fn new(jobs: Arc<AgentJobRegistry>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for WaitTasksTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wait_tasks".into(),
            description: "\
Wait for background tasks started with task(background=true) to finish. Use ONLY after \
you have spawned all needed background tasks and have nothing else useful to do. \
mode=all (default) waits for every target; mode=any returns when the next running task \
completes (call again to collect the rest). Completions are listed in order in the \
result. Omit job_ids to wait on all currently running tasks. Optional wait_ms caps how \
long this call may block."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Task job ids to wait on. Omit = all currently running."
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

        let job_ids = call.arguments.get("job_ids").and_then(|v| {
            if let Some(arr) = v.as_array() {
                let ids: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                if ids.is_empty() {
                    None
                } else {
                    Some(ids)
                }
            } else if let Some(s) = v.as_str() {
                let ids: Vec<String> = s
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .map(str::trim)
                    .filter(|x| !x.is_empty())
                    .map(|x| x.to_string())
                    .collect();
                if ids.is_empty() {
                    None
                } else {
                    Some(ids)
                }
            } else {
                None
            }
        });

        let report = self
            .jobs
            .join(job_ids, mode, wait_ms)
            .await
            .map_err(|e| tool_error("wait_tasks", e))?;

        let all_terminal = report.finals.iter().all(|j| j.state.is_terminal());
        let wait_ok = if report.timed_out {
            false
        } else {
            match mode {
                JoinMode::All => all_terminal,
                JoinMode::Any => !report.events.is_empty(),
            }
        };

        Ok(ToolOutput::text_with_details(
            report.message,
            json!({
                "ok": wait_ok,
                "mode": mode.as_str(),
                "timed_out": report.timed_out,
                "completed_events": report.events.len(),
                "events": report.events.iter().map(|e| json!({
                    "id": e.id,
                    "agent": e.agent,
                    "state": e.state.as_str(),
                    "status": e.status.map(|s| s.as_str()),
                    "ok": e.ok,
                    "turns": e.turns,
                    "max_turns": e.max_turns,
                })).collect::<Vec<_>>(),
                "finals": report.finals.iter().map(|j| json!({
                    "id": j.id,
                    "agent": j.agent,
                    "state": j.state.as_str(),
                    "status": j.status.map(|s| s.as_str()),
                    "ok": j.ok,
                    "running": j.state == JobState::Running,
                    "turns": j.turns,
                    "max_turns": j.max_turns,
                })).collect::<Vec<_>>(),
            }),
        ))
    }
}

pub struct JobKillTool {
    jobs: Arc<AgentJobRegistry>,
}

impl JobKillTool {
    pub fn new(jobs: Arc<AgentJobRegistry>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobKillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "job_kill".into(),
            description: "Stop a background agent job started with task(background=true).".into(),
            parameters: json!({
                "type": "object",
                "required": ["job_id"],
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Job id from task(background=true)"
                    }
                }
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let id = call
            .arguments
            .get("job_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("job_kill", "missing job_id"))?
            .to_string();

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
                "job_id": snap.id,
                "state": snap.state.as_str(),
                "status": snap.status.map(|s| s.as_str()),
            }),
        ))
    }
}
