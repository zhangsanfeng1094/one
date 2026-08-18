//! Subagent jobs (`task` tool). Completions for **background** jobs push into
//! the same notification queue as background bash; the parent `Agent` drains
//! them before each LLM turn as User messages.
//!
//! **UI layer (separate from bash `/ps`):** each job keeps a live event log
//! (turns / tools / activity) for TUI `/tasks` · `SubagentDetail` — not the
//! process list.
//!
//! **Durable log:** each job also appends the same event stream to
//! `~/.one/agent/jobs/<job_id>.jsonl` (override with `ONE_JOB_LOG_DIR`; disable
//! with `ONE_JOB_LOG=0`) so post-mortem after kill/crash still shows where the
//! child stuck (turns / tools / activity).

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use one_core::agent::LlmProvider;
use one_core::events::AgentEvent;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;

use super::harness::{self, HarnessOptions, RunControl};
use crate::protocol::{error_code, ProtocolError, RunRequest, RunResult, TaskExitStatus};

static JOB_SEQ: AtomicU64 = AtomicU64::new(1);

/// Default wall-time for one background agent job (5 minutes).
const DEFAULT_JOB_MAX_WALL_MS: u64 = 300_000;

/// Cap live event lines retained per job (ring buffer).
const EVENT_LOG_CAP: usize = 200;

/// Directory for durable job JSONL logs.
///
/// Override with `ONE_JOB_LOG_DIR`. Default: `~/.one/agent/jobs`.
pub fn job_log_dir() -> PathBuf {
    if let Ok(p) = std::env::var("ONE_JOB_LOG_DIR") {
        let t = p.trim();
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }
    one_session::agent_dir().join("jobs")
}

/// Whether durable job logs are enabled (default: on).
pub fn job_log_enabled() -> bool {
    match std::env::var("ONE_JOB_LOG") {
        Ok(s) => !matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// Path for a job's durable JSONL log (`…/jobs/<safe_id>.jsonl`).
pub fn job_log_path(job_id: &str) -> PathBuf {
    let safe: String = job_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    let name = if safe.is_empty() {
        "job_unknown".into()
    } else {
        safe
    };
    job_log_dir().join(format!("{name}.jsonl"))
}

fn now_rfc3339() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // Prefer chrono when available for readable UTC; fall back to epoch ms.
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| format!("epoch_ms:{ms}"))
}

struct DurableLog {
    path: PathBuf,
    file: File,
}

/// Live activity + event ring for one job (shared with harness subscribe).
#[derive(Debug, Default)]
pub struct JobEventLog {
    lines: Mutex<VecDeque<String>>,
    activity: Mutex<String>,
    durable: Mutex<Option<DurableLog>>,
}

impl std::fmt::Debug for DurableLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableLog")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl JobEventLog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Open `path` for append and write a `meta` header line (best-effort).
    pub fn open_durable(&self, path: impl Into<PathBuf>, meta: Value) {
        if !job_log_enabled() {
            return;
        }
        let path = path.into();
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "job log: failed to create directory"
                );
                return;
            }
        }
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                let rec = json!({
                    "t": "meta",
                    "ts": now_rfc3339(),
                    "meta": meta,
                });
                if let Err(e) = writeln!(file, "{rec}") {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "job log: failed to write meta"
                    );
                    return;
                }
                let _ = file.flush();
                if let Ok(mut slot) = self.durable.lock() {
                    *slot = Some(DurableLog { path, file });
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "job log: failed to open"
                );
            }
        }
    }

    /// Path of the durable log if opened.
    pub fn log_path(&self) -> Option<PathBuf> {
        self.durable
            .lock()
            .ok()
            .and_then(|d| d.as_ref().map(|x| x.path.clone()))
    }

    /// Append a terminal `end` record (state + optional fields).
    pub fn write_end(&self, state: &str, extra: Value) {
        let mut rec = json!({
            "t": "end",
            "ts": now_rfc3339(),
            "state": state,
        });
        if let Some(obj) = rec.as_object_mut() {
            if let Some(extra_obj) = extra.as_object() {
                for (k, v) in extra_obj {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        self.append_json_record(&rec);
        // Ensure durable file is flushed for post-mortem after kill.
        if let Ok(mut slot) = self.durable.lock() {
            if let Some(d) = slot.as_mut() {
                let _ = d.file.flush();
            }
        }
    }

    fn append_json_record(&self, rec: &Value) {
        if let Ok(mut slot) = self.durable.lock() {
            if let Some(d) = slot.as_mut() {
                if let Err(e) = writeln!(d.file, "{rec}") {
                    tracing::debug!(
                        path = %d.path.display(),
                        error = %e,
                        "job log: append failed"
                    );
                } else {
                    let _ = d.file.flush();
                }
            }
        }
    }

    pub fn set_activity(&self, text: impl Into<String>) {
        let text = text.into();
        if let Ok(mut a) = self.activity.lock() {
            *a = text;
        }
    }

    pub fn activity(&self) -> String {
        self.activity.lock().map(|a| a.clone()).unwrap_or_default()
    }

    pub fn push_line(&self, line: impl Into<String>) {
        let line = line.into();
        if line.is_empty() {
            return;
        }
        if let Ok(mut lines) = self.lines.lock() {
            if lines.len() >= EVENT_LOG_CAP {
                lines.pop_front();
            }
            lines.push_back(line.clone());
        }
        // Durable JSONL — one record per UI log line (survives kill/crash).
        self.append_json_record(&json!({
            "t": "line",
            "ts": now_rfc3339(),
            "text": line,
        }));
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Feed a child [`AgentEvent`] into activity + ring buffer.
    pub fn on_agent_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::AgentStart => {
                self.set_activity("starting");
                self.push_line("▸ started");
            }
            AgentEvent::TurnStart { turn } => {
                // Agent loop is 0-based; job progress UIs use 1-based.
                let label = format!("turn {}", turn + 1);
                self.set_activity(label.clone());
                self.push_line(format!("▸ {label}"));
            }
            AgentEvent::TextDelta { .. } => {
                // Don't spam the log with tokens; keep activity soft.
                let act = self.activity();
                if act.is_empty() || act.starts_with("turn ") || act == "starting" {
                    self.set_activity("writing");
                }
            }
            AgentEvent::ThinkingDelta { .. } => {
                self.set_activity("thinking");
            }
            AgentEvent::RetryScheduled {
                retry,
                max_retries,
                delay,
                reason,
            } => {
                let line = format!(
                    "▸ retry {retry}/{max_retries} in {}s · {reason}",
                    delay.as_secs()
                );
                self.set_activity(line.clone());
                self.push_line(line);
            }
            AgentEvent::RetryStarted { retry, max_retries } => {
                let line = format!("→ retry {retry}/{max_retries} started");
                self.set_activity(line.clone());
                self.push_line(line);
            }
            AgentEvent::ToolExecutionStart { tool_call } => {
                let detail = tool_call_brief(tool_call);
                let line = if detail.is_empty() {
                    format!("→ {}", tool_call.name)
                } else {
                    format!("→ {} · {}", tool_call.name, detail)
                };
                self.set_activity(line.clone());
                self.push_line(line);
            }
            AgentEvent::ToolExecutionEnd {
                tool_call,
                is_error,
                output,
            } => {
                let mark = if *is_error { "✗" } else { "✓" };
                let brief = truncate_chars(&output.as_text().replace('\n', " "), 48);
                let line = if brief.is_empty() {
                    format!("{mark} {}", tool_call.name)
                } else {
                    format!("{mark} {} · {brief}", tool_call.name)
                };
                self.push_line(line);
                // Activity falls back to waiting for next model step.
                self.set_activity(format!("{} done", tool_call.name));
            }
            AgentEvent::ServerTool {
                provider,
                tool,
                status,
            } => {
                let st = match status {
                    one_core::ServerToolStatus::Started => "start",
                    one_core::ServerToolStatus::Completed => "done",
                    one_core::ServerToolStatus::Failed => "fail",
                };
                let line = format!("server · {provider} · {} · {st}", tool.as_str());
                self.set_activity(line.clone());
                self.push_line(line);
            }
            AgentEvent::TurnEnd { turn, .. } => {
                self.push_line(format!("◂ turn {} end", turn + 1));
            }
            AgentEvent::AgentEnd { .. } => {
                self.set_activity("finishing");
                self.push_line("▸ finishing");
            }
        }
    }
}

fn tool_call_brief(call: &one_core::tool::ToolCall) -> String {
    let args = &call.arguments;
    let raw = match call.name.as_str() {
        "read" | "write" | "edit" => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "bash" | "shell" => args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "grep" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "find" => args
            .get("glob")
            .or_else(|| args.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "ls" => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string(),
        _ => String::new(),
    };
    truncate_chars(&raw.replace('\n', " "), 36)
}

fn truncate_chars(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        format!(
            "{}…",
            t.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

/// Options for [`AgentJobRegistry::spawn_with`].
#[derive(Clone)]
pub struct SpawnOptions {
    /// Push `[job completed]` into the parent notification queue (background only).
    pub notify_completion: bool,
    /// Apply `ONE_JOB_MAX_WALL_MS` wall-time budget.
    pub apply_wall_timeout: bool,
    /// Optional trace sink for the child harness (Langfuse nested under parent tool).
    pub trace: Option<one_core::SharedTrace>,
    /// Trace labels for the child run.
    pub trace_meta: Option<one_core::TraceRunMeta>,
    /// If `slot` is `None`, acquire this semaphore inside the spawned task
    /// (admission timeout → auto-background while still queued).
    pub acquire_slot: Option<Arc<Semaphore>>,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        // Background jobs: notify + wall timeout on by default.
        Self {
            notify_completion: true,
            apply_wall_timeout: true,
            trace: None,
            trace_meta: None,
            acquire_slot: None,
        }
    }
}

impl std::fmt::Debug for SpawnOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnOptions")
            .field("notify_completion", &self.notify_completion)
            .field("apply_wall_timeout", &self.apply_wall_timeout)
            .field("trace", &self.trace.is_some())
            .field("trace_meta", &self.trace_meta.is_some())
            .field("acquire_slot", &self.acquire_slot.is_some())
            .finish()
    }
}

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

/// Why a live job was terminalized from outside the harness.
///
/// Shown in durable logs + parent tool text so "aborted by job_kill" is not the
/// only message for Esc / wall timeout / session teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    /// Explicit `job_kill` tool or `/tasks` kill.
    JobKill,
    /// Parent Esc / soft abort (`AppRuntime::abort`).
    ParentAbort,
    /// Session teardown (`/new`, `/resume`, process exit).
    SessionTeardown,
    /// Independent wall-clock watchdog (`ONE_JOB_MAX_WALL_MS`).
    WallTimeout,
}

impl KillReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JobKill => "job_kill",
            Self::ParentAbort => "parent_abort",
            Self::SessionTeardown => "session_teardown",
            Self::WallTimeout => "wall_timeout",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Registered, waiting for a coordinator slot (Grok queued spawn).
    Queued,
    Running,
    Completed,
    Aborted,
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }

    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// Enough of a finished child to implement Grok-style `resume_from`.
#[derive(Debug, Clone, Default)]
pub struct ResumeSource {
    pub job_id: String,
    pub agent: String,
    pub parent_session_id: Option<String>,
    pub prompt: String,
    pub summary: String,
    pub cwd: Option<String>,
    pub worktree_path: Option<String>,
    pub transcript: Vec<Value>,
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
    /// Short live activity (e.g. `→ grep · auth`).
    pub activity: String,
    /// Condensed event log lines (newest last).
    pub event_lines: Vec<String>,
    /// When false, completion did not (and will not) notify the parent agent.
    pub notify_completion: bool,
    /// Durable JSONL log path (`~/.one/agent/jobs/<id>.jsonl`), if enabled.
    pub log_path: Option<PathBuf>,
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
    event_log: Arc<JobEventLog>,
    notify_completion: bool,
    resume: ResumeSource,
}

/// Registry for background `task` jobs (one-cli only).
pub struct AgentJobRegistry {
    jobs: Mutex<HashMap<String, JobInner>>,
    notifications: Arc<Mutex<Vec<String>>>,
    /// Coordinator finish hook (job id). Fired after `finalize` / `kill`.
    finish_sink: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

impl AgentJobRegistry {
    pub fn new(notifications: Arc<Mutex<Vec<String>>>) -> Arc<Self> {
        Arc::new(Self {
            jobs: Mutex::new(HashMap::new()),
            notifications,
            finish_sink: Mutex::new(None),
        })
    }

    /// Wire the independent coordinator so it can dequeue when a child ends.
    pub fn set_finish_sink(&self, tx: mpsc::UnboundedSender<String>) {
        *self.finish_sink.lock().expect("finish_sink") = Some(tx);
    }

    fn notify_finish(&self, id: &str) {
        if let Ok(g) = self.finish_sink.lock() {
            if let Some(tx) = g.as_ref() {
                let _ = tx.send(id.to_string());
            }
        }
    }

    /// Append a live-log line (coordinator handoff, etc.).
    pub fn push_event(&self, id: &str, line: impl Into<String>) {
        if let Some(job) = self.jobs.lock().expect("jobs lock").get(id) {
            job.event_log.push_line(line.into());
        }
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

    /// Spawn harness as a background job (notifies parent on completion).
    pub fn spawn(
        self: &Arc<Self>,
        req: RunRequest,
        provider: Arc<dyn LlmProvider>,
        opts: HarnessOptions,
        agent_name: String,
        description: Option<String>,
        slot: Option<OwnedSemaphorePermit>,
    ) -> String {
        self.spawn_with(
            req,
            provider,
            opts,
            agent_name,
            description,
            slot,
            SpawnOptions::default(),
        )
    }

    /// Register a live job row (TUI `/tasks` + durable log) and build [`RunControl`].
    ///
    /// Does **not** start the harness — caller either awaits it on the current
    /// task ([`Self::run_foreground`]) or detaches it ([`Self::spawn_with`] /
    /// [`Self::launch_registered`]). `queued` starts the row as
    /// [`JobState::Queued`] until the coordinator admits it.
    pub fn register_job(
        &self,
        agent_name: &str,
        description: Option<&str>,
        opts: &HarnessOptions,
        max_turns: u64,
        spawn_opts: &SpawnOptions,
        queued: bool,
    ) -> (String, RunControl, Arc<AtomicBool>) {
        let id = Self::next_id();
        let abort = Arc::new(AtomicBool::new(false));
        let turn_progress = Arc::new(AtomicU64::new(0));
        let done = Arc::new(Notify::new());
        let event_log = JobEventLog::new();
        event_log.open_durable(
            job_log_path(&id),
            json!({
                "job_id": id,
                "agent": agent_name,
                "description": description,
                "max_turns": max_turns,
                "notify_completion": spawn_opts.notify_completion,
                "apply_wall_timeout": spawn_opts.apply_wall_timeout,
                "cwd": opts.cwd.display().to_string(),
            }),
        );
        event_log.set_activity(if queued { "queued" } else { "starting" });
        event_log.push_line(format!(
            "▸ job {} · {}{}{}",
            id,
            agent_name,
            description.map(|d| format!(" · {d}")).unwrap_or_default(),
            if queued { " · queued" } else { "" }
        ));

        {
            let mut jobs = self.jobs.lock().expect("jobs lock");
            jobs.insert(
                id.clone(),
                JobInner {
                    id: id.clone(),
                    agent: agent_name.to_string(),
                    description: description.map(|s| s.to_string()),
                    state: if queued {
                        JobState::Queued
                    } else {
                        JobState::Running
                    },
                    result: None,
                    started: Instant::now(),
                    finished: None,
                    notified: false,
                    abort: abort.clone(),
                    turn_progress: turn_progress.clone(),
                    max_turns,
                    done: done.clone(),
                    event_log: event_log.clone(),
                    notify_completion: spawn_opts.notify_completion,
                    resume: ResumeSource {
                        job_id: id.clone(),
                        agent: agent_name.to_string(),
                        ..ResumeSource::default()
                    },
                },
            );
        }

        let control = RunControl {
            abort: Some(abort.clone()),
            turn_progress: Some(turn_progress),
            event_log: Some(event_log),
            trace: spawn_opts.trace.clone(),
            trace_meta: spawn_opts.trace_meta.clone(),
        };
        (id, control, abort)
    }

    /// Independent wall-clock watchdog.
    ///
    /// `tokio::time::timeout` around the harness **cannot** cancel a task stuck
    /// in blocking code (e.g. Langfuse `force_flush` on a Tokio worker after
    /// `AgentEnd`). A separate sleep task calls [`Self::kill_with_reason`] so
    /// the job row becomes terminal, waiters wake, and the parent is not parked
    /// on "Waiting for model…" holding a dead child forever.
    fn arm_wall_watchdog(self: &Arc<Self>, id: &str, apply_wall: bool) {
        if !apply_wall {
            return;
        }
        let Some(ms) = job_max_wall_ms() else {
            return;
        };
        let reg = Arc::clone(self);
        let jid = id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            if let Some(snap) = reg.get(&jid) {
                if !snap.state.is_terminal() {
                    let _ = reg.kill_with_reason(&jid, KillReason::WallTimeout);
                }
            }
        });
    }

    async fn run_harness_with_wall(
        req: RunRequest,
        provider: Arc<dyn LlmProvider>,
        opts: HarnessOptions,
        control: RunControl,
        abort: Arc<AtomicBool>,
        apply_wall: bool,
    ) -> RunResult {
        // Soft async timeout (cancels cooperative awaits). Hard terminalization
        // of a non-cooperative hang is handled by [`Self::arm_wall_watchdog`].
        let wall = if apply_wall { job_max_wall_ms() } else { None };
        if let Some(ms) = wall {
            match timeout(
                Duration::from_millis(ms),
                harness::run_with_control(req, provider.as_ref(), &opts, control),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => {
                    abort.store(true, Ordering::Relaxed);
                    let mut rr = RunResult::failure(
                        ProtocolError::new(
                            error_code::TIMEOUT,
                            format!("job wall time exceeded ({ms}ms)"),
                        ),
                        ms,
                    )
                    .with_status(TaskExitStatus::TimedOut);
                    rr.stop_reason = Some("wall_timeout".into());
                    rr
                }
            }
        } else {
            harness::run_with_control(req, provider.as_ref(), &opts, control).await
        }
    }

    /// **Foreground / sync `task` path** — completion **is** the return value of
    /// the awaited harness, not a `Notify` side-channel.
    ///
    /// Still registers a live job so TUI `/tasks` and durable JSONL work, and so
    /// Esc / `job_kill` can set the shared abort flag. The parent never
    /// `wait_until_done`s: when this future resolves, the child has finished.
    ///
    /// `on_registered(job_id)` runs after the live row exists and **before** the
    /// harness await — use it to `bind_tool_job` for TUI click-to-open.
    ///
    /// This is the fix for "UI stuck on ▸ finishing forever": that state meant
    /// the child had already emitted `AgentEnd`, but the parent was waiting on
    /// a lost `notify_waiters` from a detached spawn.
    pub async fn run_foreground(
        self: &Arc<Self>,
        req: RunRequest,
        provider: Arc<dyn LlmProvider>,
        opts: HarnessOptions,
        agent_name: String,
        description: Option<String>,
        slot: Option<OwnedSemaphorePermit>,
        spawn_opts: SpawnOptions,
        on_registered: impl FnOnce(&str),
    ) -> (String, RunResult) {
        let max_turns = req.agent.max_turns.unwrap_or(16) as u64;
        let (id, control, abort) = self.register_job(
            &agent_name,
            description.as_deref(),
            &opts,
            max_turns,
            &spawn_opts,
            false,
        );
        on_registered(&id);
        let apply_wall = spawn_opts.apply_wall_timeout;
        self.arm_wall_watchdog(&id, apply_wall);
        // Race harness against an external terminalization (job_kill / wall
        // timeout finalize on another path). If kill() already sealed the job
        // while the harness is stuck ignoring abort, return the stored result
        // instead of parking the parent `task` tool forever on "Thinking…".
        let harness =
            Self::run_harness_with_wall(req, provider, opts, control, abort.clone(), apply_wall);
        let early_terminal = {
            let reg = Arc::clone(self);
            let jid = id.clone();
            async move {
                let _ = reg.wait_until_done(&jid).await;
                reg.take_result_clone(&jid)
            }
        };
        let result = tokio::select! {
            r = harness => r,
            early = early_terminal => {
                // Prefer the snapshot kill/timeout already stored; if missing,
                // synthesize an aborted result so the parent unblocks.
                if let Some(r) = early {
                    r
                } else {
                    RunResult::failure(
                        ProtocolError::new(error_code::ABORTED, "job terminated"),
                        0,
                    )
                    .with_status(TaskExitStatus::Aborted)
                }
            }
        };
        drop(slot);
        // If kill() already terminalized the job, keep that snapshot's result.
        self.finalize(&id, result);
        let result = self
            .take_result_clone(&id)
            .expect("finalize always stores a result");
        (id, result)
    }

    /// Spawn harness as a **background** job (`task(background=true)`).
    ///
    /// Completions push `[job completed]` when `notify_completion` is set.
    /// Waiters should prefer polling / `wait` with timeout; the Notify is a
    /// wake hint only (see `wait_until_done` subscribe-before-recheck).
    pub fn spawn_with(
        self: &Arc<Self>,
        req: RunRequest,
        provider: Arc<dyn LlmProvider>,
        opts: HarnessOptions,
        agent_name: String,
        description: Option<String>,
        slot: Option<OwnedSemaphorePermit>,
        spawn_opts: SpawnOptions,
    ) -> String {
        let max_turns = req.agent.max_turns.unwrap_or(16) as u64;
        let (id, control, abort) = self.register_job(
            &agent_name,
            description.as_deref(),
            &opts,
            max_turns,
            &spawn_opts,
            false,
        );
        self.launch_registered(id.clone(), req, provider, opts, control, abort, spawn_opts);
        let _ = slot;
        id
    }

    /// Flip a queued row to running and start the harness (coordinator admit).
    pub fn launch_registered(
        self: &Arc<Self>,
        id: String,
        req: RunRequest,
        provider: Arc<dyn LlmProvider>,
        opts: HarnessOptions,
        control: RunControl,
        abort: Arc<AtomicBool>,
        spawn_opts: SpawnOptions,
    ) {
        {
            let mut jobs = self.jobs.lock().expect("jobs lock");
            if let Some(job) = jobs.get_mut(&id) {
                if job.state == JobState::Queued {
                    job.state = JobState::Running;
                    job.event_log.set_activity("starting");
                    job.event_log.push_line("▸ starting");
                }
            }
        }
        let apply_wall = spawn_opts.apply_wall_timeout;
        let late_slot = spawn_opts.acquire_slot.clone();
        self.arm_wall_watchdog(&id, apply_wall);
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let _slot = if let Some(sem) = late_slot {
                sem.acquire_owned().await.ok()
            } else {
                None
            };
            let result =
                Self::run_harness_with_wall(req, provider, opts, control, abort, apply_wall).await;
            registry.finalize(&id, result);
        });
    }

    /// Flip whether a still-running job should push `[job completed]` (auto-bg).
    pub fn set_notify_completion(&self, id: &str, notify: bool) -> bool {
        let mut jobs = self.jobs.lock().expect("jobs lock");
        if let Some(job) = jobs.get_mut(id) {
            job.notify_completion = notify;
            return true;
        }
        false
    }

    /// Attach spawn-time identity used later by `resume_from`.
    pub fn attach_resume_meta(
        &self,
        id: &str,
        parent_session_id: Option<String>,
        prompt: String,
        cwd: Option<String>,
    ) {
        let mut jobs = self.jobs.lock().expect("jobs lock");
        if let Some(job) = jobs.get_mut(id) {
            job.resume.parent_session_id = parent_session_id;
            job.resume.prompt = prompt;
            job.resume.cwd = cwd;
        }
    }

    /// Snapshot a completed job for `resume_from`.
    pub fn resume_source(&self, id: &str) -> Option<ResumeSource> {
        let jobs = self.jobs.lock().expect("jobs lock");
        let job = jobs.get(id)?;
        if !job.state.is_terminal() {
            return None;
        }
        let mut src = job.resume.clone();
        src.job_id = job.id.clone();
        src.agent = job.agent.clone();
        if let Some(r) = &job.result {
            if src.summary.is_empty() {
                src.summary = r.result.clone();
            }
            if src.transcript.is_empty() {
                src.transcript = r.transcript.clone();
            }
            if src.worktree_path.is_none() {
                src.worktree_path = r.worktree.as_ref().map(|w| w.path.clone());
            }
        }
        Some(src)
    }

    /// Full [`RunResult`] for a finished job (if available).
    pub fn take_result_clone(&self, id: &str) -> Option<RunResult> {
        self.jobs
            .lock()
            .expect("jobs lock")
            .get(id)
            .and_then(|j| j.result.clone())
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
    ///
    /// Reason is recorded in the durable log and parent-facing error so the UI
    /// is not always the opaque `"aborted by job_kill"` (Esc / wall / teardown
    /// used to look identical).
    pub fn kill(&self, id: &str) -> Result<JobSnapshot, String> {
        self.kill_with_reason(id, KillReason::JobKill)
    }

    /// Like [`Self::kill`] with an explicit cause.
    pub fn kill_with_reason(&self, id: &str, reason: KillReason) -> Result<JobSnapshot, String> {
        let mut jobs = self.jobs.lock().expect("jobs lock");
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| format!("unknown job_id: {id}"))?;
        if job.state.is_terminal() {
            return Ok(snapshot_of(job));
        }
        job.abort.store(true, Ordering::Relaxed);
        let duration_ms = job.started.elapsed().as_millis() as u64;
        let turns = job.turn_progress.load(Ordering::Relaxed);
        let (state, status, code, msg, activity) = match reason {
            KillReason::WallTimeout => {
                let ms = job_max_wall_ms().unwrap_or(DEFAULT_JOB_MAX_WALL_MS);
                (
                    JobState::Failed,
                    TaskExitStatus::TimedOut,
                    error_code::TIMEOUT,
                    format!("job wall time exceeded ({ms}ms)"),
                    "timeout",
                )
            }
            other => (
                JobState::Aborted,
                TaskExitStatus::Aborted,
                error_code::ABORTED,
                format!("aborted by {}", other.as_str()),
                "aborted",
            ),
        };
        job.state = state;
        job.finished = Some(Instant::now());
        job.event_log.set_activity(activity);
        job.event_log.push_line(format!("▸ {activity} · {}", reason.as_str()));
        job.event_log.write_end(
            activity,
            json!({
                "duration_ms": duration_ms,
                "turns": turns,
                "reason": reason.as_str(),
            }),
        );
        let mut rr = RunResult::failure(ProtocolError::new(code, msg), duration_ms).with_status(status);
        if matches!(reason, KillReason::WallTimeout) {
            rr.stop_reason = Some("wall_timeout".into());
        } else {
            rr.stop_reason = Some(reason.as_str().into());
        }
        rr.ok = false;
        job.result = Some(rr);
        let should_notify = job.notify_completion && !job.notified;
        if should_notify {
            job.notified = true;
            let snap = snapshot_of(job);
            let text = format_job_completed_notification(&snap);
            job.done.notify_waiters();
            drop(jobs);
            self.push_notification(text);
            self.notify_finish(id);
            return Ok(snap);
        }
        job.notified = true;
        job.done.notify_waiters();
        drop(jobs);
        self.notify_finish(id);
        Ok(self.get(id).expect("just killed"))
    }

    /// Abort every running job (parent Esc / session abort).
    pub fn kill_all(&self) {
        self.kill_all_with_reason(KillReason::ParentAbort);
    }

    /// Abort every running job with an explicit cause (Esc vs session teardown).
    pub fn kill_all_with_reason(&self, reason: KillReason) {
        let ids: Vec<String> = self
            .jobs
            .lock()
            .expect("jobs lock")
            .iter()
            .filter(|(_, j)| !j.state.is_terminal())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let _ = self.kill_with_reason(&id, reason);
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
            TaskExitStatus::RuntimeError | TaskExitStatus::TimedOut => JobState::Failed,
        };
        job.finished = Some(Instant::now());
        let duration_ms = job
            .finished
            .unwrap()
            .duration_since(job.started)
            .as_millis() as u64;
        let turns = result
            .turns
            .unwrap_or_else(|| job.turn_progress.load(Ordering::Relaxed));
        let stop_reason = result.stop_reason.clone();
        let err_s = result.error.as_ref().map(|e| e.to_string());
        job.result = Some(result);
        let st = job.state.as_str();
        job.event_log.set_activity(st);
        job.event_log.push_line(format!("▸ {st}"));
        job.event_log.write_end(
            st,
            json!({
                "duration_ms": duration_ms,
                "turns": turns,
                "status": status.as_str(),
                "stop_reason": stop_reason,
                "error": err_s,
            }),
        );
        let should_notify = job.notify_completion && !job.notified;
        if should_notify {
            job.notified = true;
            let snap = snapshot_of(job);
            let text = format_job_completed_notification(&snap);
            job.done.notify_waiters();
            drop(jobs);
            self.push_notification(text);
            self.notify_finish(id);
            return;
        }
        // Sync jobs: mark notified so kill/finalize do not double-fire later.
        if !job.notify_completion {
            job.notified = true;
        }
        job.done.notify_waiters();
        drop(jobs);
        self.notify_finish(id);
    }

    pub async fn wait(&self, id: &str, wait_ms: Option<u64>) -> Result<JobSnapshot, String> {
        let ms = wait_ms.unwrap_or(0);
        if ms == 0 {
            return self.get(id).ok_or_else(|| format!("unknown job_id: {id}"));
        }
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            // Subscribe *before* re-checking terminal: `notify_waiters` does not
            // store a permit, so a completion between check and subscribe is lost
            // and the waiter would hang until timeout (or forever in wait_until_done).
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
            let notified = done.notified();
            {
                let jobs = self.jobs.lock().expect("jobs lock");
                let job = jobs
                    .get(id)
                    .ok_or_else(|| format!("unknown job_id: {id}"))?;
                if job.state.is_terminal() {
                    return Ok(snapshot_of(job));
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return self.get(id).ok_or_else(|| format!("unknown job_id: {id}"));
            }
            match timeout(left, notified).await {
                Ok(()) => continue,
                Err(_) => {
                    return self.get(id).ok_or_else(|| format!("unknown job_id: {id}"));
                }
            }
        }
    }

    /// Block until the job is terminal (used by sync `task`).
    ///
    /// Uses subscribe-before-recheck so a job that finishes between the state
    /// read and `notified().await` still wakes the waiter (`Notify::notify_waiters`
    /// drops the signal when nobody is subscribed yet).
    pub async fn wait_until_done(&self, id: &str) -> Result<JobSnapshot, String> {
        loop {
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
            let notified = done.notified();
            {
                let jobs = self.jobs.lock().expect("jobs lock");
                let job = jobs
                    .get(id)
                    .ok_or_else(|| format!("unknown job_id: {id}"))?;
                if job.state.is_terminal() {
                    return Ok(snapshot_of(job));
                }
            }
            notified.await;
        }
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
        let mut seen_terminal: std::collections::HashSet<String> = std::collections::HashSet::new();
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
        self.absorb_notifications_for(&events.iter().map(|e| e.id.clone()).collect::<Vec<_>>());

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
                // Poll-with-timeout: even if a notify_waiters is missed, the
                // 200ms slice rechecks terminal state (join is not hang-critical
                // the way sync `wait_until_done` is). Still subscribe before sleep.
                let done = {
                    let jobs = self.jobs.lock().expect("jobs lock");
                    pending
                        .first()
                        .and_then(|id| jobs.get(id).map(|j| j.done.clone()))
                };
                if let Some(done) = done {
                    let notified = done.notified();
                    // Re-check under lock so we do not wait a full slice if already done.
                    let already = {
                        let jobs = self.jobs.lock().expect("jobs lock");
                        pending
                            .first()
                            .and_then(|id| jobs.get(id).map(|j| j.state.is_terminal()))
                    };
                    if already != Some(true) {
                        let _ = timeout(slice, notified).await;
                    }
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

    let still: Vec<_> = finals.iter().filter(|j| !j.state.is_terminal()).collect();
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
            j.state.is_terminal() && !j.ok && !matches!(j.status, Some(TaskExitStatus::Success))
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
    let activity = if job.state.is_live() {
        job.event_log.activity()
    } else {
        String::new()
    };
    let event_lines = job.event_log.lines();
    let log_path = job.event_log.log_path();
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
        activity,
        event_lines,
        notify_completion: job.notify_completion,
        log_path,
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
    if let Some(p) = &snap.log_path {
        out.push_str(&format!("log_path: {}\n", p.display()));
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
    one_core::system_reminder(out)
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
    if let Some(p) = &snap.log_path {
        out.push_str(&format!("log_path: {}\n", p.display()));
    }
    if !snap.activity.is_empty() && snap.state == JobState::Running {
        out.push_str(&format!("activity: {}\n", snap.activity));
    }
    if !snap.event_lines.is_empty() {
        out.push_str("--- live log ---\n");
        for line in &snap.event_lines {
            out.push_str(line);
            out.push('\n');
        }
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
            activity: String::new(),
            event_lines: vec![],
            notify_completion: true,
            log_path: None,
        };
        let t = format_job_completed_notification(&snap);
        assert!(t.contains("[job completed]"), "{t}");
        assert!(t.contains("<system-reminder>"), "{t}");
        assert!(t.contains("id: job_1"));
        assert!(t.contains("found login"));
        assert!(t.contains("turns: 2/16"));
    }

    #[test]
    fn event_log_records_tools_and_activity() {
        let log = JobEventLog::new();
        log.on_agent_event(&AgentEvent::AgentStart);
        log.on_agent_event(&AgentEvent::TurnStart { turn: 0 });
        log.on_agent_event(&AgentEvent::ToolExecutionStart {
            tool_call: one_core::tool::ToolCall {
                id: "c1".into(),
                name: "grep".into(),
                arguments: serde_json::json!({"pattern": "auth"}),
            },
        });
        assert!(log.activity().contains("grep"), "{}", log.activity());
        let lines = log.lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("grep") && l.contains("auth")),
            "{lines:?}"
        );
    }

    #[test]
    fn durable_log_appends_jsonl() {
        let dir = std::env::temp_dir().join(format!(
            "one-job-log-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::env::set_var("ONE_JOB_LOG_DIR", &dir);
        std::env::remove_var("ONE_JOB_LOG"); // ensure enabled
        let log = JobEventLog::new();
        let path = job_log_path("job_test_durable_1");
        log.open_durable(
            &path,
            json!({"job_id": "job_test_durable_1", "agent": "explore"}),
        );
        log.push_line("▸ started");
        log.on_agent_event(&AgentEvent::ToolExecutionStart {
            tool_call: one_core::tool::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
            },
        });
        log.write_end("aborted", json!({"duration_ms": 42, "reason": "job_kill"}));
        assert_eq!(log.log_path().as_deref(), Some(path.as_path()));
        let body = std::fs::read_to_string(&path).expect("read log");
        assert!(body.contains("\"t\":\"meta\""), "{body}");
        assert!(body.contains("▸ started"), "{body}");
        assert!(body.contains("read"), "{body}");
        assert!(body.contains("\"t\":\"end\""), "{body}");
        assert!(body.contains("aborted"), "{body}");
        // cleanup env so other tests keep default dir
        std::env::remove_var("ONE_JOB_LOG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Foreground completion is the harness return value — no Notify wait.
    #[tokio::test]
    async fn run_foreground_returns_when_child_ends() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue.clone());
        let provider = Arc::new(one_ai::MockProvider::new());
        let opts = HarnessOptions::from_cwd(std::env::temp_dir());
        let mut req = RunRequest::new(
            crate::protocol::AgentSpec::builtin_explore(),
            "foreground probe",
        );
        req.session.mode = crate::protocol::SessionMode::Ephemeral;
        let registered = Arc::new(AtomicBool::new(false));
        let flag = registered.clone();
        let fut = reg.run_foreground(
            req,
            provider,
            opts,
            "explore".into(),
            Some("fg".into()),
            None,
            SpawnOptions {
                notify_completion: false,
                apply_wall_timeout: true,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
            },
            |_| flag.store(true, Ordering::Relaxed),
        );
        let (id, result) = tokio::time::timeout(Duration::from_secs(5), fut)
            .await
            .expect("run_foreground must not hang after child AgentEnd");
        assert!(registered.load(Ordering::Relaxed), "on_registered fired");
        assert!(id.starts_with("job_"));
        let snap = reg.get(&id).expect("job row");
        assert!(snap.state.is_terminal(), "{:?}", snap.state);
        // No background notify for foreground.
        assert!(queue.lock().unwrap().is_empty());
        // Result must be present (success or structured failure — mock is success).
        assert!(result.duration_ms > 0 || result.ok || result.error.is_some());
    }

    /// Regression: completion must not be lost when it races with wait_until_done.
    /// Previously `notify_waiters` + check-then-await dropped the wake and hung forever
    /// (UI stuck on ▸ finishing after the child had already ended).
    #[tokio::test]
    async fn wait_until_done_survives_completion_race() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue);
        let provider = Arc::new(one_ai::MockProvider::new());
        let opts = HarnessOptions::from_cwd(std::env::temp_dir());
        for i in 0..40 {
            let mut req = RunRequest::new(
                crate::protocol::AgentSpec::builtin_explore(),
                format!("race probe {i}"),
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
            // No yield: maximize chance completion lands in the race window.
            let snap = tokio::time::timeout(Duration::from_secs(3), reg.wait_until_done(&id))
                .await
                .unwrap_or_else(|_| panic!("wait_until_done hung on job {id} (lost notify?)"))
                .expect("job exists");
            assert!(snap.state.is_terminal(), "job {id} state={:?}", snap.state);
        }
    }

    #[tokio::test]
    async fn spawn_writes_durable_log_path() {
        let dir = std::env::temp_dir().join(format!(
            "one-job-log-spawn-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::env::set_var("ONE_JOB_LOG_DIR", &dir);
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue);
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
        let path = snap.log_path.expect("log_path set");
        assert!(path.exists(), "{}", path.display());
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains(&id), "{body}");
        assert!(
            body.contains("\"t\":\"end\"") || body.contains("▸"),
            "{body}"
        );
        std::env::remove_var("ONE_JOB_LOG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
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
            notes
                .iter()
                .any(|n| n.contains("status: aborted") || n.contains("aborted")),
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

    /// Independent watchdog must terminalize even when the harness future is
    /// stuck in non-cooperative work (the soft `timeout()` cannot cancel that).
    #[tokio::test]
    async fn wall_watchdog_kills_stuck_job_row() {
        std::env::set_var("ONE_JOB_MAX_WALL_MS", "50");
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue);
        let (id, _control, _abort) = reg.register_job(
            "explore",
            Some("stuck"),
            &HarnessOptions::from_cwd(std::env::temp_dir()),
            16,
            &SpawnOptions {
                notify_completion: false,
                apply_wall_timeout: true,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
            },
            false,
        );
        // Arm only the watchdog — never start a harness (simulates hang after AgentEnd).
        reg.arm_wall_watchdog(&id, true);
        let snap = tokio::time::timeout(Duration::from_secs(3), reg.wait_until_done(&id))
            .await
            .expect("watchdog should terminalize within 3s")
            .expect("job exists");
        assert!(
            matches!(snap.state, JobState::Failed | JobState::Aborted),
            "state={:?}",
            snap.state
        );
        assert_eq!(snap.status, Some(TaskExitStatus::TimedOut));
        std::env::remove_var("ONE_JOB_MAX_WALL_MS");
    }

    #[test]
    fn kill_reason_labels() {
        assert_eq!(KillReason::ParentAbort.as_str(), "parent_abort");
        assert_eq!(KillReason::WallTimeout.as_str(), "wall_timeout");
        assert_eq!(KillReason::SessionTeardown.as_str(), "session_teardown");
    }

    #[tokio::test]
    async fn join_all_waits_and_streams() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue.clone());
        let provider = Arc::new(one_ai::MockProvider::new());
        let opts = HarnessOptions::from_cwd(std::env::temp_dir());
        let mut ids = Vec::new();
        for prompt in ["research a", "research b"] {
            let mut req = RunRequest::new(crate::protocol::AgentSpec::builtin_explore(), prompt);
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
        assert!(
            report.message.contains("completion stream"),
            "{}",
            report.message
        );
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
        let mut req = RunRequest::new(crate::protocol::AgentSpec::builtin_explore(), "one job");
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
