//! Background agent jobs (subagent). Completions push into the same
//! notification queue as background bash; the parent `Agent` drains them
//! before each LLM turn as User messages.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use one_core::agent::LlmProvider;
use tokio::sync::{Notify, OwnedSemaphorePermit};
use tokio::time::timeout;

use super::harness::{self, HarnessOptions, RunControl};
use crate::protocol::{error_code, ProtocolError, RunRequest, RunResult, TaskExitStatus};

static JOB_SEQ: AtomicU64 = AtomicU64::new(1);

/// Default wall-time for one background agent job (5 minutes).
const DEFAULT_JOB_MAX_WALL_MS: u64 = 300_000;

/// Override with `ONE_JOB_MAX_WALL_MS` (milliseconds). `0` = no wall limit.
pub fn job_max_wall_ms() -> Option<u64> {
    match std::env::var("ONE_JOB_MAX_WALL_MS") {
        Ok(s) => {
            let s = s.trim();
            if s.is_empty() {
                return Some(DEFAULT_JOB_MAX_WALL_MS);
            }
            match s.parse::<u64>() {
                Ok(0) => None,
                Ok(n) => Some(n),
                Err(_) => Some(DEFAULT_JOB_MAX_WALL_MS),
            }
        }
        Err(_) => Some(DEFAULT_JOB_MAX_WALL_MS),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Completed,
    Aborted,
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub id: String,
    pub kind: &'static str,
    pub agent: String,
    pub description: Option<String>,
    pub state: JobState,
    pub status: Option<TaskExitStatus>,
    pub summary: String,
    pub ok: bool,
    pub duration_ms: u64,
    pub turns: Option<u64>,
    /// Max turns for the child agent (for `turns/max` progress).
    pub max_turns: Option<u64>,
    pub error: Option<String>,
    pub notified: bool,
}

struct JobInner {
    id: String,
    agent: String,
    description: Option<String>,
    state: JobState,
    result: Option<RunResult>,
    started: Instant,
    finished: Option<Instant>,
    notified: bool,
    abort: Arc<AtomicBool>,
    turn_progress: Arc<AtomicU64>,
    max_turns: u64,
    done: Arc<Notify>,
}

/// Registry for background `task` jobs (one-cli only).
pub struct AgentJobRegistry {
    jobs: Mutex<HashMap<String, JobInner>>,
    notifications: Arc<Mutex<Vec<String>>>,
}

impl AgentJobRegistry {
    pub fn new(notifications: Arc<Mutex<Vec<String>>>) -> Arc<Self> {
        Arc::new(Self {
            jobs: Mutex::new(HashMap::new()),
            notifications,
        })
    }

    pub fn notification_queue(&self) -> Arc<Mutex<Vec<String>>> {
        self.notifications.clone()
    }

    fn next_id() -> String {
        let n = JOB_SEQ.fetch_add(1, Ordering::Relaxed);
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() % 0xFFFF)
            .unwrap_or(0);
        format!("job_{ms:x}_{n}")
    }

    /// Spawn background harness; holds `slot` until the job finishes.
    pub fn spawn(
        self: &Arc<Self>,
        req: RunRequest,
        provider: Arc<dyn LlmProvider>,
        opts: HarnessOptions,
        agent_name: String,
        description: Option<String>,
        slot: Option<OwnedSemaphorePermit>,
    ) -> String {
        let id = Self::next_id();
        let abort = Arc::new(AtomicBool::new(false));
        let turn_progress = Arc::new(AtomicU64::new(0));
        let done = Arc::new(Notify::new());
        let max_turns = req.agent.max_turns.unwrap_or(16) as u64;

        {
            let mut jobs = self.jobs.lock().expect("jobs lock");
            jobs.insert(
                id.clone(),
                JobInner {
                    id: id.clone(),
                    agent: agent_name.clone(),
                    description: description.clone(),
                    state: JobState::Running,
                    result: None,
                    started: Instant::now(),
                    finished: None,
                    notified: false,
                    abort: abort.clone(),
                    turn_progress: turn_progress.clone(),
                    max_turns,
                    done: done.clone(),
                },
            );
        }

        let registry = Arc::clone(self);
        let job_id = id.clone();
        let control = RunControl {
            abort: Some(abort.clone()),
            turn_progress: Some(turn_progress),
        };
        tokio::spawn(async move {
            let _slot = slot;
            let wall = job_max_wall_ms();
            let result = if let Some(ms) = wall {
                match timeout(
                    Duration::from_millis(ms),
                    harness::run_with_control(req, provider.as_ref(), &opts, control),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => {
                        // Signal child agent to stop if still in LLM call.
                        abort.store(true, Ordering::Relaxed);
                        let mut rr = RunResult::failure(
                            ProtocolError::new(
                                error_code::TIMEOUT,
                                format!("background job wall time exceeded ({ms}ms)"),
                            ),
                            ms,
                        )
                        .with_status(TaskExitStatus::RuntimeError);
                        rr.stop_reason = Some("wall_timeout".into());
                        rr
                    }
                }
            } else {
                harness::run_with_control(req, provider.as_ref(), &opts, control).await
            };
            registry.finalize(&job_id, result);
        });

        id
    }

    pub fn get(&self, id: &str) -> Option<JobSnapshot> {
        self.jobs
            .lock()
            .expect("jobs lock")
            .get(id)
            .map(snapshot_of)
    }

    pub fn list(&self) -> Vec<JobSnapshot> {
        let mut list: Vec<_> = self
            .jobs
            .lock()
            .expect("jobs lock")
            .values()
            .map(snapshot_of)
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// Request abort on the child agent; mark job aborted and notify once.
    pub fn kill(&self, id: &str) -> Result<JobSnapshot, String> {
        let mut jobs = self.jobs.lock().expect("jobs lock");
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| format!("unknown job_id: {id}"))?;
        if job.state.is_terminal() {
            return Ok(snapshot_of(job));
        }
        job.abort.store(true, Ordering::Relaxed);
        job.state = JobState::Aborted;
        job.finished = Some(Instant::now());
        let mut rr = RunResult::failure(
            ProtocolError::new(error_code::ABORTED, "aborted by job_kill"),
            job.started.elapsed().as_millis() as u64,
        )
        .with_status(TaskExitStatus::Aborted);
        rr.ok = false;
        job.result = Some(rr);
        if !job.notified {
            job.notified = true;
            let snap = snapshot_of(job);
            let text = format_job_completed_notification(&snap);
            job.done.notify_waiters();
            drop(jobs);
            self.push_notification(text);
            return Ok(snap);
        }
        job.done.notify_waiters();
        Ok(snapshot_of(job))
    }

    /// Abort every running job (parent Esc / session abort).
    pub fn kill_all(&self) {
        let ids: Vec<String> = self
            .jobs
            .lock()
            .expect("jobs lock")
            .iter()
            .filter(|(_, j)| !j.state.is_terminal())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let _ = self.kill(&id);
        }
    }

    fn push_notification(&self, text: String) {
        self.notifications
            .lock()
            .expect("notifications lock")
            .push(text);
    }

    fn finalize(&self, id: &str, result: RunResult) {
        let mut jobs = self.jobs.lock().expect("jobs lock");
        let Some(job) = jobs.get_mut(id) else {
            return;
        };
        // Kill already finalized — still wake waiters; keep aborted snapshot.
        if job.state.is_terminal() {
            job.done.notify_waiters();
            return;
        }
        let status = result.status.unwrap_or(if result.ok {
            TaskExitStatus::Success
        } else {
            TaskExitStatus::RuntimeError
        });
        job.state = match status {
            TaskExitStatus::Aborted => JobState::Aborted,
            TaskExitStatus::Success
            | TaskExitStatus::IncompleteInfo
            | TaskExitStatus::MaxTurnsExceeded
            | TaskExitStatus::Started => JobState::Completed,
            TaskExitStatus::RuntimeError => JobState::Failed,
        };
        job.finished = Some(Instant::now());
        job.result = Some(result);
        if !job.notified {
            job.notified = true;
            let snap = snapshot_of(job);
            let text = format_job_completed_notification(&snap);
            job.done.notify_waiters();
            drop(jobs);
            self.push_notification(text);
            return;
        }
        job.done.notify_waiters();
    }

    pub async fn wait(&self, id: &str, wait_ms: Option<u64>) -> Result<JobSnapshot, String> {
        let done = {
            let jobs = self.jobs.lock().expect("jobs lock");
            let job = jobs
                .get(id)
                .ok_or_else(|| format!("unknown job_id: {id}"))?;
            if job.state.is_terminal() {
                return Ok(snapshot_of(job));
            }
            job.done.clone()
        };
        let ms = wait_ms.unwrap_or(0);
        if ms == 0 {
            return self
                .get(id)
                .ok_or_else(|| format!("unknown job_id: {id}"));
        }
        let _ = tokio::time::timeout(Duration::from_millis(ms), done.notified()).await;
        self.get(id)
            .ok_or_else(|| format!("unknown job_id: {id}"))
    }

    /// Ids currently non-terminal (for default `wait_tasks` target set).
    pub fn running_ids(&self) -> Vec<String> {
        self.jobs
            .lock()
            .expect("jobs lock")
            .iter()
            .filter(|(_, j)| !j.state.is_terminal())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Drop queued `[job completed]` notices that mention these job ids (avoid double delivery after join).
    pub fn absorb_notifications_for(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let mut q = self.notifications.lock().expect("notifications lock");
        q.retain(|text| !ids.iter().any(|id| text.contains(id.as_str())));
    }

    /// Blocking join: wait for background jobs (thread-join style).
    ///
    /// - `mode=all` (default): until every target id is terminal  
    /// - `mode=any`: until at least one still-running target becomes terminal  
    ///
    /// While waiting, each newly completed job is recorded in order (`events`).
    /// Matching notification-queue lines are absorbed so the next LLM turn is not double-notified.
    pub async fn join(
        &self,
        ids: Option<Vec<String>>,
        mode: JoinMode,
        wait_ms: Option<u64>,
    ) -> Result<JoinReport, String> {
        let mut targets: Vec<String> = match ids {
            Some(list) if !list.is_empty() => list,
            _ => {
                // Prefer still-running; if none, all known jobs (already done → immediate return).
                let running = self.running_ids();
                if !running.is_empty() {
                    running
                } else {
                    self.list().into_iter().map(|j| j.id).collect()
                }
            }
        };
        targets.sort();
        targets.dedup();

        if targets.is_empty() {
            return Ok(JoinReport {
                mode,
                timed_out: false,
                events: vec![],
                finals: vec![],
                message: "No agent jobs to join.\n".into(),
            });
        }

        // Validate ids exist.
        for id in &targets {
            if self.get(id).is_none() {
                return Err(format!("unknown job_id: {id}"));
            }
        }

        let deadline = wait_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        let mut seen_terminal: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut events: Vec<JobSnapshot> = Vec::new();

        // Seed: already-finished targets count as immediate "events".
        for id in &targets {
            if let Some(s) = self.get(id) {
                if s.state.is_terminal() {
                    seen_terminal.insert(id.clone());
                    events.push(s);
                }
            }
        }
        self.absorb_notifications_for(
            &events.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        );

        // If nothing still running, return immediately (all already terminal).
        let pending_at_start: Vec<String> = targets
            .iter()
            .filter(|id| !seen_terminal.contains(*id))
            .cloned()
            .collect();
        if pending_at_start.is_empty() {
            let finals: Vec<_> = targets.iter().filter_map(|id| self.get(id)).collect();
            let message = format_join_report(mode, false, &events, &finals);
            return Ok(JoinReport {
                mode,
                timed_out: false,
                events,
                finals,
                message,
            });
        }

        // mode=any: wait until ≥1 previously-running target finishes.
        // mode=all: wait until every target is terminal.
        let mut timed_out = false;
        let mut newly_completed_since_wait = 0u32;

        loop {
            for id in &targets {
                if seen_terminal.contains(id) {
                    continue;
                }
                if let Some(s) = self.get(id) {
                    if s.state.is_terminal() {
                        seen_terminal.insert(id.clone());
                        self.absorb_notifications_for(std::slice::from_ref(id));
                        events.push(s);
                        newly_completed_since_wait += 1;
                    }
                }
            }

            let all_done = targets.iter().all(|id| seen_terminal.contains(id));
            match mode {
                JoinMode::All if all_done => break,
                JoinMode::Any if newly_completed_since_wait > 0 || all_done => break,
                _ => {}
            }

            let pending: Vec<_> = targets
                .iter()
                .filter(|id| !seen_terminal.contains(*id))
                .cloned()
                .collect();
            if pending.is_empty() {
                break;
            }

            if let Some(dl) = deadline {
                let now = Instant::now();
                if now >= dl {
                    timed_out = true;
                    break;
                }
                let slice = (dl - now).min(Duration::from_millis(200));
                let done = {
                    let jobs = self.jobs.lock().expect("jobs lock");
                    pending
                        .first()
                        .and_then(|id| jobs.get(id).map(|j| j.done.clone()))
                };
                if let Some(done) = done {
                    let _ = timeout(slice, done.notified()).await;
                } else {
                    tokio::time::sleep(slice).await;
                }
            } else {
                let done = {
                    let jobs = self.jobs.lock().expect("jobs lock");
                    pending
                        .first()
                        .and_then(|id| jobs.get(id).map(|j| j.done.clone()))
                };
                if let Some(done) = done {
                    // Cap silent wait so we re-scan progress periodically.
                    let _ = timeout(Duration::from_secs(2), done.notified()).await;
                } else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }

        let finals: Vec<_> = targets.iter().filter_map(|id| self.get(id)).collect();
        // Absorb any late notices for all targets.
        self.absorb_notifications_for(&targets);
        let message = format_join_report(mode, timed_out, &events, &finals);
        Ok(JoinReport {
            mode,
            timed_out,
            events,
            finals,
            message,
        })
    }
}

/// Wait-all vs wait-next-completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinMode {
    All,
    Any,
}

impl JoinMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" | "join" => Some(Self::All),
            "any" | "next" => Some(Self::Any),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
        }
    }
}

#[derive(Debug, Clone)]
pub struct JoinReport {
    pub mode: JoinMode,
    pub timed_out: bool,
    /// Completions observed in order (including already-done at start).
    pub events: Vec<JobSnapshot>,
    pub finals: Vec<JobSnapshot>,
    pub message: String,
}

fn format_join_report(
    mode: JoinMode,
    timed_out: bool,
    events: &[JobSnapshot],
    finals: &[JobSnapshot],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("[wait_tasks · mode={}]\n", mode.as_str()));
    if timed_out {
        out.push_str("timed_out: true\n");
    }
    out.push_str(&format!(
        "completed_events: {} · targets: {}\n",
        events.len(),
        finals.len()
    ));

    if !events.is_empty() {
        out.push_str("\n--- completion stream ---\n");
        for (i, e) in events.iter().enumerate() {
            let st = e.status.map(|s| s.as_str()).unwrap_or(e.state.as_str());
            out.push_str(&format!(
                "\n[{}/{}] id={} agent={} status={}\n",
                i + 1,
                events.len(),
                e.id,
                e.agent,
                st
            ));
            if let Some(t) = e.turns {
                if let Some(m) = e.max_turns {
                    out.push_str(&format!("turns: {t}/{m}\n"));
                }
            }
            if !e.summary.is_empty() {
                out.push_str(&e.summary);
                if !e.summary.ends_with('\n') {
                    out.push('\n');
                }
            } else {
                out.push_str("(no summary)\n");
            }
        }
    }

    let still: Vec<_> = finals
        .iter()
        .filter(|j| !j.state.is_terminal())
        .collect();
    if !still.is_empty() {
        out.push_str("\n--- still running ---\n");
        for j in still {
            let progress = match (j.turns, j.max_turns) {
                (Some(t), Some(m)) => format!(" turn {t}/{m}"),
                _ => String::new(),
            };
            out.push_str(&format!("- {} · {}{progress}\n", j.id, j.agent));
        }
    }

    let failed = finals
        .iter()
        .filter(|j| {
            j.state.is_terminal()
                && !j.ok
                && !matches!(j.status, Some(TaskExitStatus::Success))
        })
        .count();
    let ok_n = finals
        .iter()
        .filter(|j| j.state.is_terminal() && j.ok)
        .count();
    out.push_str(&format!(
        "\nsummary: ok={ok_n} failed_or_partial={} running={} timed_out={timed_out}\n",
        failed,
        finals.iter().filter(|j| !j.state.is_terminal()).count(),
    ));
    out
}

fn snapshot_of(job: &JobInner) -> JobSnapshot {
    let duration_ms = job
        .finished
        .unwrap_or_else(Instant::now)
        .duration_since(job.started)
        .as_millis() as u64;
    let live_turns = job.turn_progress.load(Ordering::Relaxed);
    let (status, summary, ok, turns, error) = if let Some(r) = &job.result {
        (
            r.status,
            r.result.clone(),
            r.ok && r.status.map(|s| s.is_ok()).unwrap_or(r.ok),
            r.turns.or(if live_turns > 0 {
                Some(live_turns)
            } else {
                None
            }),
            r.error.as_ref().map(|e| e.to_string()),
        )
    } else {
        (
            None,
            String::new(),
            false,
            if live_turns > 0 {
                Some(live_turns)
            } else {
                None
            },
            None,
        )
    };
    JobSnapshot {
        id: job.id.clone(),
        kind: "task",
        agent: job.agent.clone(),
        description: job.description.clone(),
        state: job.state,
        status,
        summary,
        ok,
        duration_ms,
        turns,
        max_turns: Some(job.max_turns),
        error,
        notified: job.notified,
    }
}

/// Format completion notice for the parent agent (User message after drain).
pub fn format_job_completed_notification(snap: &JobSnapshot) -> String {
    let status = snap
        .status
        .map(|s| s.as_str())
        .unwrap_or(snap.state.as_str());
    let mut out = String::new();
    out.push_str("[job completed]\n");
    out.push_str(&format!("kind: {}\n", snap.kind));
    out.push_str(&format!("id: {}\n", snap.id));
    out.push_str(&format!("agent: {}\n", snap.agent));
    if let Some(d) = &snap.description {
        out.push_str(&format!("description: {d}\n"));
    }
    out.push_str(&format!("status: {status}\n"));
    out.push_str(&format!("duration_ms: {}\n", snap.duration_ms));
    if let Some(t) = snap.turns {
        if let Some(m) = snap.max_turns {
            out.push_str(&format!("turns: {t}/{m}\n"));
        } else {
            out.push_str(&format!("turns: {t}\n"));
        }
    }
    if let Some(err) = &snap.error {
        out.push_str(&format!("error: {err}\n"));
    }
    out.push('\n');
    if snap.summary.is_empty() {
        out.push_str("(no summary)\n");
    } else {
        out.push_str(&snap.summary);
        if !snap.summary.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

pub fn format_job_list(jobs: &[JobSnapshot]) -> String {
    if jobs.is_empty() {
        return "No agent jobs.\n".into();
    }
    let mut out = String::from("Agent jobs:\n");
    for j in jobs {
        let st = j.status.map(|s| s.as_str()).unwrap_or(j.state.as_str());
        let desc = j
            .description
            .as_deref()
            .map(|d| format!(" · {d}"))
            .unwrap_or_default();
        let progress = match (j.turns, j.max_turns) {
            (Some(t), Some(m)) if j.state == JobState::Running => format!(" · turn {t}/{m}"),
            (Some(t), Some(m)) => format!(" · {t}/{m} turns"),
            _ => String::new(),
        };
        out.push_str(&format!(
            "- {} · {}{desc} · {}{progress} · {}ms\n",
            j.id, j.agent, st, j.duration_ms
        ));
    }
    out
}

pub fn format_job_snapshot(snap: &JobSnapshot) -> String {
    let status = snap
        .status
        .map(|s| s.as_str())
        .unwrap_or(snap.state.as_str());
    let mut out = String::new();
    out.push_str(&format!("job_id: {}\n", snap.id));
    out.push_str(&format!("kind: {}\n", snap.kind));
    out.push_str(&format!("agent: {}\n", snap.agent));
    out.push_str(&format!("state: {}\n", snap.state.as_str()));
    out.push_str(&format!("status: {status}\n"));
    out.push_str(&format!("duration_ms: {}\n", snap.duration_ms));
    if let Some(t) = snap.turns {
        if let Some(m) = snap.max_turns {
            out.push_str(&format!("turns: {t}/{m}\n"));
        } else {
            out.push_str(&format!("turns: {t}\n"));
        }
    } else if let Some(m) = snap.max_turns {
        out.push_str(&format!("turns: 0/{m}\n"));
    }
    if let Some(err) = &snap.error {
        out.push_str(&format!("error: {err}\n"));
    }
    if snap.state == JobState::Running {
        out.push_str("(still running)\n");
    } else if snap.summary.is_empty() {
        out.push_str("(no summary)\n");
    } else {
        out.push_str("--- summary ---\n");
        out.push_str(&snap.summary);
        if !snap.summary.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_completed_has_prefix() {
        let snap = JobSnapshot {
            id: "job_1".into(),
            kind: "task",
            agent: "explore".into(),
            description: Some("auth".into()),
            state: JobState::Completed,
            status: Some(TaskExitStatus::Success),
            summary: "found login".into(),
            ok: true,
            duration_ms: 10,
            turns: Some(2),
            max_turns: Some(16),
            error: None,
            notified: true,
        };
        let t = format_job_completed_notification(&snap);
        assert!(t.starts_with("[job completed]"), "{t}");
        assert!(t.contains("id: job_1"));
        assert!(t.contains("found login"));
        assert!(t.contains("turns: 2/16"));
    }

    #[tokio::test]
    async fn spawn_mock_pushes_notification() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue.clone());
        let provider = Arc::new(one_ai::MockProvider::new());
        let opts = HarnessOptions::from_cwd(std::env::temp_dir());
        let mut req = RunRequest::new(
            crate::protocol::AgentSpec::builtin_explore(),
            "Summarize auth",
        );
        req.session.mode = crate::protocol::SessionMode::Ephemeral;
        let id = reg.spawn(
            req,
            provider,
            opts,
            "explore".into(),
            Some("auth".into()),
            None,
        );
        for _ in 0..100 {
            if let Some(s) = reg.get(&id) {
                if s.state.is_terminal() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let snap = reg.get(&id).expect("job");
        assert!(snap.state.is_terminal(), "{:?}", snap.state);
        let notes = queue.lock().unwrap().clone();
        assert!(
            notes.iter().any(|n| n.contains("[job completed]")),
            "notes={notes:?}"
        );
        assert!(notes.iter().any(|n| n.contains(&id)), "notes={notes:?}");
    }

    #[tokio::test]
    async fn kill_sets_aborted_and_notifies() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue.clone());
        // Slow path: wall timeout huge; we kill immediately after spawn.
        // Use a prompt that still needs a harness round-trip.
        let provider = Arc::new(one_ai::MockProvider::new());
        let opts = HarnessOptions::from_cwd(std::env::temp_dir());
        let mut req = RunRequest::new(
            crate::protocol::AgentSpec::builtin_explore(),
            "long research",
        );
        req.session.mode = crate::protocol::SessionMode::Ephemeral;
        let id = reg.spawn(req, provider, opts, "explore".into(), None, None);
        let snap = reg.kill(&id).expect("kill");
        assert_eq!(snap.state, JobState::Aborted);
        let notes = queue.lock().unwrap().clone();
        assert!(
            notes.iter().any(|n| n.contains("status: aborted") || n.contains("aborted")),
            "notes={notes:?}"
        );
    }

    #[test]
    fn wall_timeout_env_parsing() {
        std::env::set_var("ONE_JOB_MAX_WALL_MS", "1");
        assert_eq!(job_max_wall_ms(), Some(1));
        std::env::remove_var("ONE_JOB_MAX_WALL_MS");
        assert_eq!(job_max_wall_ms(), Some(DEFAULT_JOB_MAX_WALL_MS));
    }

    #[test]
    fn list_empty() {
        let reg = AgentJobRegistry::new(Arc::new(Mutex::new(Vec::new())));
        assert!(reg.list().is_empty());
        assert_eq!(format_job_list(&[]), "No agent jobs.\n");
    }

    #[test]
    fn wall_ms_zero_disables() {
        std::env::set_var("ONE_JOB_MAX_WALL_MS", "0");
        assert_eq!(job_max_wall_ms(), None);
        std::env::remove_var("ONE_JOB_MAX_WALL_MS");
    }

    #[tokio::test]
    async fn join_all_waits_and_streams() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue.clone());
        let provider = Arc::new(one_ai::MockProvider::new());
        let opts = HarnessOptions::from_cwd(std::env::temp_dir());
        let mut ids = Vec::new();
        for prompt in ["research a", "research b"] {
            let mut req = RunRequest::new(
                crate::protocol::AgentSpec::builtin_explore(),
                prompt,
            );
            req.session.mode = crate::protocol::SessionMode::Ephemeral;
            let id = reg.spawn(
                req,
                provider.clone(),
                opts.clone(),
                "explore".into(),
                None,
                None,
            );
            ids.push(id);
        }
        let report = reg
            .join(Some(ids.clone()), JoinMode::All, Some(30_000))
            .await
            .expect("join");
        assert!(!report.timed_out, "{}", report.message);
        assert_eq!(report.finals.len(), 2);
        assert!(report.finals.iter().all(|j| j.state.is_terminal()));
        assert!(report.message.contains("[wait_tasks"), "{}", report.message);
        assert!(report.message.contains("completion stream"), "{}", report.message);
        // Notices absorbed so queue should not still list both (may be empty or unrelated).
        let notes = queue.lock().unwrap().clone();
        for id in &ids {
            assert!(
                !notes.iter().any(|n| n.contains(id)),
                "notification for {id} should be absorbed after join; notes={notes:?}"
            );
        }
    }

    #[tokio::test]
    async fn join_any_returns_after_one() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue);
        let provider = Arc::new(one_ai::MockProvider::new());
        let opts = HarnessOptions::from_cwd(std::env::temp_dir());
        let mut req = RunRequest::new(
            crate::protocol::AgentSpec::builtin_explore(),
            "one job",
        );
        req.session.mode = crate::protocol::SessionMode::Ephemeral;
        let id = reg.spawn(req, provider, opts, "explore".into(), None, None);
        let report = reg
            .join(Some(vec![id]), JoinMode::Any, Some(30_000))
            .await
            .expect("join any");
        assert!(!report.events.is_empty());
        assert!(report.message.contains("mode=any"));
    }
}
