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

use super::agent_backend::{
    backend_for, resolve_backend, AgentBackend, AgentEventSink, AgentHealth, BackendControl,
    BackendKind, BackendSpawn, HealthEvaluation, HealthEvaluator, HealthInputs, TaskLifecycle,
};
use super::harness::{self, HarnessOptions, RunControl};
use crate::protocol::{error_code, ProtocolError, RunRequest, RunResult, TaskExitStatus};

static JOB_SEQ: AtomicU64 = AtomicU64::new(1);

/// Default wall-time for one background agent job (5 minutes).
const DEFAULT_JOB_MAX_WALL_MS: u64 = 300_000;

/// Cap live event lines retained per job (ring buffer).
const EVENT_LOG_CAP: usize = 200;

/// Character threshold above which a subagent result is spilled to disk and previewed.
pub const JOB_RESULT_SPILL_THRESHOLD_CHARS: usize = 4_000;

/// Max preview characters included in a `[job completed]` notification.
pub const JOB_NOTIFICATION_PREVIEW_CHARS: usize = 4_000;

/// Max preview characters included in `job_output(job_*)`.
pub const JOB_OUTPUT_PREVIEW_CHARS: usize = 8_000;

/// Default hard cap on the durable `.result.txt` file size (16 MiB).
pub const DEFAULT_JOB_RESULT_FILE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Effective hard cap on `.result.txt` bytes (`ONE_JOB_RESULT_MAX_BYTES`).
pub fn job_result_file_max_bytes() -> usize {
    std::env::var("ONE_JOB_RESULT_MAX_BYTES")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_JOB_RESULT_FILE_MAX_BYTES)
}

/// Sanitize a `job_id` so it is safe to use as a file stem.
pub fn safe_job_file_stem(job_id: &str) -> String {
    let safe: String = job_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if safe.is_empty() {
        "job_unknown".into()
    } else {
        safe
    }
}

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

/// Directory for durable subagent result artifacts.
///
/// Override with `ONE_JOB_RESULT_DIR`, falling back to `job_log_dir()`.
pub fn job_result_dir() -> PathBuf {
    if let Ok(p) = std::env::var("ONE_JOB_RESULT_DIR") {
        let t = p.trim();
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }
    job_log_dir()
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
    let name = safe_job_file_stem(job_id);
    job_log_dir().join(format!("{name}.jsonl"))
}

/// Path for a job's durable final result artifact (`…/jobs/<safe_id>.result.txt`).
pub fn job_result_path(job_id: &str) -> PathBuf {
    let name = safe_job_file_stem(job_id);
    job_result_dir().join(format!("{name}.result.txt"))
}

fn slice_utf8_prefix_bytes<'a>(s: &'a str, max_bytes: usize) -> (&'a str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// Persist full subagent result text when it exceeds [`JOB_RESULT_SPILL_THRESHOLD_CHARS`].
/// Never panics or fails `finalize`; returns `(result_ref, file_capped, spill_error)`.
fn persist_job_result_artifact(
    job_id: &str,
    result_text: &str,
) -> (Option<PathBuf>, bool, Option<String>) {
    if result_text.chars().count() <= JOB_RESULT_SPILL_THRESHOLD_CHARS {
        return (None, false, None);
    }
    let path = job_result_path(job_id);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            let msg = format!("failed to create result dir {}: {e}", parent.display());
            tracing::warn!(job_id = %job_id, error = %msg, "subagent result spill failed");
            return (None, false, Some(msg));
        }
    }
    let max_bytes = job_result_file_max_bytes();
    let (to_write, capped) = if result_text.len() > max_bytes {
        let note = format!(
            "\n…[result file capped at {max_bytes} bytes ({} bytes total)]",
            result_text.len()
        );
        if max_bytes > note.len() {
            let prefix_budget = max_bytes - note.len();
            let (prefix, _) = slice_utf8_prefix_bytes(result_text, prefix_budget);
            let mut s = prefix.to_string();
            s.push_str(&note);
            (s, true)
        } else {
            let (prefix, _) = slice_utf8_prefix_bytes(result_text, max_bytes);
            (prefix.to_string(), true)
        }
    } else {
        (result_text.to_string(), false)
    };

    match File::create(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(to_write.as_bytes()) {
                let msg = format!("failed to write result file {}: {e}", path.display());
                tracing::warn!(job_id = %job_id, error = %msg, "subagent result spill failed");
                return (None, capped, Some(msg));
            }
            let _ = f.flush();
            (Some(path), capped, None)
        }
        Err(e) => {
            let msg = format!("failed to create result file {}: {e}", path.display());
            tracing::warn!(job_id = %job_id, error = %msg, "subagent result spill failed");
            (None, false, Some(msg))
        }
    }
}

/// Build a bounded preview of `text` up to `max_chars` characters.
pub fn preview_job_result(text: &str, max_chars: usize) -> (String, bool) {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return (text.to_string(), false);
    }
    let head: String = text.chars().take(max_chars).collect();
    (head, true)
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
            AgentEvent::SteerApplied { text } => {
                let brief: String = text.chars().take(48).collect();
                self.push_line(format!("↳ steer applied: {brief}"));
            }
            AgentEvent::UsageUpdate { .. } => {}
            AgentEvent::WaitParkStart { mode, ids } => {
                let line = format!("⏸ waiting · {} task(s) ({})", ids.len(), mode.as_str());
                self.set_activity(line.clone());
                self.push_line(format!("▸ {line}"));
            }
            AgentEvent::WaitParkEnd => {
                self.set_activity("resuming");
                self.push_line("▸ wait ended");
            }
            AgentEvent::CompactionStart => {
                self.set_activity("compacting");
                self.push_line("▸ compacting context");
            }
            AgentEvent::CompactionEnd {
                tokens_before,
                tokens_after,
                kept_turns,
            } => {
                let line = format!(
                    "▸ compacted: {tokens_before} → {tokens_after} tokens · kept {kept_turns} turns"
                );
                self.push_line(line);
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
    /// Explicit backend override (`codex` / `grok` / `one`). Default: agent
    /// name or OneInternal.
    pub backend: Option<String>,
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
            backend: None,
        }
    }
}

impl SpawnOptions {
    /// Resolve the backend for this spawn (explicit override wins).
    pub fn resolve_backend_kind(&self, agent_name: &str) -> Result<BackendKind, String> {
        resolve_backend(self.backend.as_deref(), agent_name)
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

fn wall_max_override() -> &'static std::sync::Mutex<Option<Option<u64>>> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<Option<Option<u64>>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(None))
}

/// Override max wall budget in tests without mutating `std::env`.
pub fn set_job_max_wall_ms_override(val: Option<Option<u64>>) -> Option<Option<u64>> {
    let mut g = wall_max_override()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *g, val)
}

/// Parse wall-clock timeout string.
pub fn parse_job_max_wall_ms(s: &str) -> Option<u64> {
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

/// Override with `ONE_JOB_MAX_WALL_MS` (milliseconds). `0` = no wall limit.
pub fn job_max_wall_ms() -> Option<u64> {
    if let Ok(g) = wall_max_override().lock() {
        if let Some(ref override_val) = *g {
            return *override_val;
        }
    }
    match std::env::var("ONE_JOB_MAX_WALL_MS") {
        Ok(s) => parse_job_max_wall_ms(&s),
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
    /// Backend executing this job (`one-internal` / `codex-cli` / `grok-cli`).
    pub backend: &'static str,
    /// Anti-fake-death health (`healthy` / `suspicious` / `stalled`).
    pub health: &'static str,
    pub status: Option<TaskExitStatus>,
    pub summary: String,
    pub preview: String,
    pub result_bytes: usize,
    pub result_chars: usize,
    pub result_truncated: bool,
    pub result_ref: Option<PathBuf>,
    pub spill_error: Option<String>,
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

/// Live per-job normalized-event tracker: updates the AgentTask counters and
/// mirrors compact activity lines into the job's event log. Implements
/// [`AgentEventSink`] so any backend (internal or CLI) feeds it uniformly.
///
/// Thought/reasoning policy: reasoning-shaped events only refresh
/// `last_event_at` / activity — the body never reaches this tracker because
/// normalizers never emit it.
pub struct JobTracker {
    task: std::sync::Mutex<super::agent_backend::AgentTask>,
    log: Arc<JobEventLog>,
}

impl JobTracker {
    fn new(id: &str, backend: BackendKind, agent: &str, log: Arc<JobEventLog>) -> Arc<Self> {
        Arc::new(Self {
            task: std::sync::Mutex::new(super::agent_backend::AgentTask {
                id: id.to_string(),
                backend,
                agent: agent.to_string(),
                lifecycle: super::agent_backend::TaskLifecycle::Starting,
                health: AgentHealth::default(),
                session_id: None,
                process_id: None,
                started_at: std::time::Instant::now(),
                last_event_at: None,
                last_progress_at: None,
                turn_count: 0,
                tool_call_count: 0,
                current_activity: String::new(),
                current_tool: None,
                result_ref: None,
            }),
            log,
        })
    }

    pub fn snapshot_task(&self) -> super::agent_backend::AgentTask {
        self.task.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// External process id (CLI backends).
    fn set_process(&self, pid: Option<u32>) {
        let mut t = self.task.lock().unwrap_or_else(|e| e.into_inner());
        t.process_id = pid;
    }

    /// Registry-side lifecycle fix-ups (kill/finalize).
    pub(crate) fn set_lifecycle(&self, l: super::agent_backend::TaskLifecycle) {
        let mut t = self.task.lock().unwrap_or_else(|e| e.into_inner());
        t.lifecycle = l;
    }

    /// Compute health from the current tracker state (deterministic inputs).
    pub fn evaluate_health(&self, process_alive: Option<bool>) -> HealthEvaluation {
        let evaluator = HealthEvaluator::new(Default::default());
        self.evaluate_health_with(&evaluator, process_alive)
    }

    /// Like [`Self::evaluate_health`] but with the job's configured evaluator
    /// (env-tuned thresholds via [`HealthThresholds::from_env`]).
    pub fn evaluate_health_with(
        &self,
        evaluator: &HealthEvaluator,
        process_alive: Option<bool>,
    ) -> HealthEvaluation {
        let t = self.task.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        evaluator.evaluate(&HealthInputs {
            process_alive,
            elapsed: now.duration_since(t.started_at),
            since_event: t.last_event_at.map(|at| now.duration_since(at)),
            since_progress: t.last_progress_at.map(|at| now.duration_since(at)),
            tool_in_flight: t.current_tool.is_some(),
            terminal: t.lifecycle.is_terminal(),
        })
    }
}

impl AgentEventSink for JobTracker {
    fn on_event(&self, event: super::agent_backend::AgentTaskEvent) {
        let now = std::time::Instant::now();
        let mut t = self.task.lock().unwrap_or_else(|e| e.into_inner());
        t.last_event_at = Some(now);
        match &event {
            super::agent_backend::AgentTaskEvent::Started {
                session_id,
                process_id,
            } => {
                t.lifecycle = TaskLifecycle::Running;
                t.current_activity = "running".into();
                if let Some(sid) = session_id.clone() {
                    t.session_id = Some(sid);
                }
                if let Some(pid) = *process_id {
                    t.process_id = Some(pid);
                }
                self.log.set_activity("running");
                self.log.push_line("▸ backend started");
            }
            super::agent_backend::AgentTaskEvent::TurnStarted { turn } => {
                t.lifecycle = TaskLifecycle::Running;
                t.turn_count = (*turn).max(t.turn_count + 1);
                t.last_progress_at = Some(now);
                t.current_activity = format!("turn {turn}");
                t.current_tool = None;
                self.log.set_activity(format!("turn {turn}"));
                self.log.push_line(format!("▸ turn {turn}"));
            }
            super::agent_backend::AgentTaskEvent::ToolStarted {
                tool_call_id: _,
                tool,
                title,
            } => {
                t.tool_call_count += 1;
                t.last_progress_at = Some(now);
                t.current_tool = Some(tool.clone());
                let line = match title {
                    Some(ti) if !ti.is_empty() => format!("→ {tool} · {ti}"),
                    _ => format!("→ {tool}"),
                };
                t.current_activity = line.clone();
                self.log.set_activity(line.clone());
                self.log.push_line(line);
            }
            super::agent_backend::AgentTaskEvent::ToolProgress { tool_call_id, note } => {
                // Progress signal — refreshes the anti-stall clock.
                t.last_progress_at = Some(now);
                if let Some(id) = tool_call_id {
                    if t.current_tool.is_none() {
                        t.current_tool = Some(id.clone());
                    }
                }
                if !note.is_empty() {
                    let line = format!("… {note}");
                    t.current_activity.clone_from(&line);
                    self.log.set_activity(line);
                }
            }
            super::agent_backend::AgentTaskEvent::OutputActivity { .. } => {
                // Liveness evidence only: last_event_at already refreshed.
            }
            super::agent_backend::AgentTaskEvent::ToolCompleted {
                tool,
                is_error,
                note,
                ..
            } => {
                t.last_progress_at = Some(now);
                t.current_tool = None;
                let mark = if *is_error { "✗" } else { "✓" };
                let line = if note.is_empty() {
                    format!("{mark} {tool}")
                } else {
                    format!("{mark} {tool} · {note}")
                };
                t.current_activity = format!("{tool} done");
                self.log.push_line(line);
            }
            super::agent_backend::AgentTaskEvent::TextActivity { .. } => {
                t.current_activity = "writing".into();
            }
            super::agent_backend::AgentTaskEvent::TurnCompleted { turn } => {
                t.turn_count = (*turn).max(t.turn_count);
                t.last_progress_at = Some(now);
                t.current_tool = None;
                self.log.push_line(format!("◂ turn {turn} end"));
            }
            super::agent_backend::AgentTaskEvent::Completed { .. } => {
                t.lifecycle = TaskLifecycle::Completed;
                t.current_tool = None;
            }
            super::agent_backend::AgentTaskEvent::Failed { .. } => {
                t.lifecycle = TaskLifecycle::Failed;
                t.current_tool = None;
            }
        }
    }
}

impl JobInner {
    /// Snapshot-time health (deterministic inputs from the tracker).
    fn health_now(&self) -> AgentHealth {
        // Internal jobs have no external process; CLI process liveness is not
        // polled per-snapshot (evaluator never treats None as dead).
        self.tracker
            .evaluate_health_with(&self.health_evaluator, None)
            .health
    }
}

struct JobInner {
    id: String,
    agent: String,
    description: Option<String>,
    state: JobState,
    /// Which backend executes (or executed) this job.
    backend: BackendKind,
    /// Live task tracker (normalized events → lifecycle/health counters).
    tracker: Arc<JobTracker>,
    /// Deterministic health evaluator (env-tunable thresholds).
    health_evaluator: HealthEvaluator,
    result: Option<RunResult>,
    result_bytes: usize,
    result_chars: usize,
    result_truncated: bool,
    result_ref: Option<PathBuf>,
    preview: String,
    spill_error: Option<String>,
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

/// Terminal subagent stop event drained by the runtime to fire
/// `SubagentStop` (extensions + hooks.json). Sent once per job.
#[derive(Debug, Clone)]
pub struct SubagentStopEvent {
    pub id: String,
    pub agent: String,
    pub ok: bool,
    pub summary: String,
}

/// Registry for background `task` jobs (one-cli only).
pub struct AgentJobRegistry {
    jobs: Mutex<HashMap<String, JobInner>>,
    notifications: Arc<Mutex<Vec<String>>>,
    /// Coordinator finish hook (job id). Fired after `finalize` / `kill`.
    finish_sink: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// SubagentStop hook drain (terminal snapshot per job).
    stop_sink: Mutex<Option<mpsc::UnboundedSender<SubagentStopEvent>>>,
}

impl AgentJobRegistry {
    pub fn new(notifications: Arc<Mutex<Vec<String>>>) -> Arc<Self> {
        Arc::new(Self {
            jobs: Mutex::new(HashMap::new()),
            notifications,
            finish_sink: Mutex::new(None),
            stop_sink: Mutex::new(None),
        })
    }

    /// Wire the SubagentStop drain (terminal job snapshots).
    pub fn set_stop_sink(&self, tx: mpsc::UnboundedSender<SubagentStopEvent>) {
        *self.stop_sink.lock().expect("stop_sink") = Some(tx);
    }

    /// Queue a SubagentStop event if a sink is wired.
    fn notify_subagent_stop(&self, snap: &JobSnapshot) {
        if let Ok(g) = self.stop_sink.lock() {
            if let Some(tx) = g.as_ref() {
                let _ = tx.send(SubagentStopEvent {
                    id: snap.id.clone(),
                    agent: snap.agent.clone(),
                    ok: snap.ok,
                    summary: snap.summary.clone(),
                });
            }
        }
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
            let backend = match spawn_opts.resolve_backend_kind(agent_name) {
                Ok(b) => b,
                Err(e) => {
                    // Reserved/unknown backend: register the row, then finalize
                    // it as an immediate failure (keeps wait/notify semantics).
                    tracing::warn!(agent = agent_name, error = %e, "job backend rejected");
                    let tracker = JobTracker::new(
                        &id,
                        BackendKind::OneInternal,
                        agent_name,
                        event_log.clone(),
                    );
                    {
                        let mut jobs = self.jobs.lock().expect("jobs lock");
                        jobs.insert(
                            id.clone(),
                            JobInner {
                                id: id.clone(),
                                agent: agent_name.to_string(),
                                description: description.map(|s| s.to_string()),
                                state: JobState::Running,
                                backend: BackendKind::OneInternal,
                                tracker,
                                health_evaluator: HealthEvaluator::new(Default::default()),
                                result: None,
                                result_bytes: 0,
                                result_chars: 0,
                                result_truncated: false,
                                result_ref: None,
                                preview: String::new(),
                                spill_error: None,
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
                    let rr =
                        RunResult::failure(ProtocolError::new(error_code::INVALID_REQUEST, e), 0)
                            .with_status(TaskExitStatus::RuntimeError);
                    self.finalize(&id, rr);
                    let control = RunControl {
                        abort: Some(abort.clone()),
                        turn_progress: Some(turn_progress),
                        event_log: Some(event_log),
                        trace: None,
                        trace_meta: None,
                    };
                    return (id, control, abort);
                }
            };
            let tracker = JobTracker::new(&id, backend, agent_name, event_log.clone());
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
                    backend,
                    tracker,
                    health_evaluator: HealthEvaluator::new(
                        super::agent_backend::HealthThresholds::from_env(),
                    ),
                    result: None,
                    result_bytes: 0,
                    result_chars: 0,
                    result_truncated: false,
                    result_ref: None,
                    preview: String::new(),
                    spill_error: None,
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
        // Snapshot backend + agent + tracker before detaching.
        let (backend, agent_name, tracker, event_log) = {
            let mut jobs = self.jobs.lock().expect("jobs lock");
            let Some(job) = jobs.get_mut(&id) else {
                return;
            };
            if job.state == JobState::Queued {
                job.state = JobState::Running;
                job.event_log.set_activity("starting");
                job.event_log.push_line("▸ starting");
            }
            (
                job.backend,
                job.agent.clone(),
                job.tracker.clone(),
                job.event_log.clone(),
            )
        };
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
            let result = match backend {
                BackendKind::OneInternal => {
                    // Legacy path, unchanged: harness + soft wall timeout.
                    Self::run_harness_with_wall(req, provider, opts, control, abort, apply_wall)
                        .await
                }
                BackendKind::CodexCli | BackendKind::GrokCli => {
                    // External CLI backend: normalized events feed the tracker;
                    // wall timeout handled by the runner; abort kills the tree.
                    let backend_impl: Arc<dyn AgentBackend> = backend_for(backend);
                    let child_spec = req.agent.clone();
                    let spawn = BackendSpawn {
                        req,
                        opts,
                        agent_name,
                        description: None,
                        child_spec,
                        provider: Some(provider),
                    };
                    let control = BackendControl {
                        abort,
                        event_log: event_log.clone(),
                        wall_timeout: if apply_wall {
                            job_max_wall_ms().map(Duration::from_millis)
                        } else {
                            None
                        },
                    };
                    let sink: Arc<dyn AgentEventSink> = tracker;
                    backend_impl.run(spawn, control, sink).await
                }
                // Reserved stubs never reach launch (rejected at register).
                BackendKind::ClaudeCli | BackendKind::PiCli => RunResult::failure(
                    ProtocolError::new(
                        error_code::INVALID_REQUEST,
                        format!(
                            "backend `{}` is reserved but NOT implemented",
                            backend.as_str()
                        ),
                    ),
                    0,
                )
                .with_status(TaskExitStatus::RuntimeError),
            };
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
        {
            // Lifecycle fix-up: cancellation, not backend failure.
            job.tracker
                .set_lifecycle(super::agent_backend::TaskLifecycle::Cancelled);
        }
        job.event_log
            .push_line(format!("▸ {activity} · {}", reason.as_str()));
        job.event_log.write_end(
            activity,
            json!({
                "duration_ms": duration_ms,
                "turns": turns,
                "reason": reason.as_str(),
            }),
        );
        let mut rr =
            RunResult::failure(ProtocolError::new(code, msg), duration_ms).with_status(status);
        if matches!(reason, KillReason::WallTimeout) {
            rr.stop_reason = Some("wall_timeout".into());
        } else {
            rr.stop_reason = Some(reason.as_str().into());
        }
        rr.ok = false;
        job.result_bytes = 0;
        job.result_chars = 0;
        job.result_truncated = false;
        job.result_ref = None;
        job.preview = String::new();
        job.spill_error = None;
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
            self.notify_subagent_stop(&snap);
            return Ok(snap);
        }
        job.notified = true;
        job.done.notify_waiters();
        let snap = snapshot_of(job);
        drop(jobs);
        self.notify_finish(id);
        self.notify_subagent_stop(&snap);
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

    pub(crate) fn finalize(&self, id: &str, result: RunResult) {
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

        // Process subagent final result: spill when exceeding threshold, build bounded preview.
        let raw_result = &result.result;
        let result_bytes = raw_result.len();
        let result_chars = raw_result.chars().count();
        let (result_ref, file_capped, spill_error) =
            if result_chars > JOB_RESULT_SPILL_THRESHOLD_CHARS {
                persist_job_result_artifact(id, raw_result)
            } else {
                (None, false, None)
            };
        let (preview, preview_truncated) = preview_job_result(raw_result, JOB_OUTPUT_PREVIEW_CHARS);
        let result_truncated = preview_truncated || file_capped;

        job.result_bytes = result_bytes;
        job.result_chars = result_chars;
        job.result_truncated = result_truncated;
        job.result_ref = result_ref;
        // Mirror terminal state + spill ref into the unified task tracker.
        {
            let l = match job.state {
                JobState::Completed => TaskLifecycle::Completed,
                _ => TaskLifecycle::Failed,
            };
            job.tracker.set_lifecycle(l);
            if let Ok(mut t) = job.tracker.task.lock() {
                t.result_ref = job.result_ref.clone();
            }
        }
        job.preview = preview.clone();
        job.spill_error = spill_error;

        if job.resume.summary.is_empty() {
            job.resume.summary = preview;
        }

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
                "result_bytes": result_bytes,
                "result_chars": result_chars,
                "result_truncated": result_truncated,
                "result_ref": job.result_ref.as_ref().map(|p| p.display().to_string()),
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
            self.notify_subagent_stop(&snap);
            return;
        }
        // Sync jobs: mark notified so kill/finalize do not double-fire later.
        if !job.notify_completion {
            job.notified = true;
        }
        job.done.notify_waiters();
        let snap = snapshot_of(job);
        drop(jobs);
        self.notify_finish(id);
        self.notify_subagent_stop(&snap);
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

    /// Check whether a job is terminal (or unknown/removed).
    pub fn is_terminal(&self, id: &str) -> bool {
        let jobs = self.jobs.lock().expect("jobs lock");
        jobs.get(id).map_or(true, |j| j.state.is_terminal())
    }

    /// Completion notifier for a job if present.
    pub fn done_notifier(&self, id: &str) -> Option<Arc<Notify>> {
        let jobs = self.jobs.lock().expect("jobs lock");
        jobs.get(id).map(|j| j.done.clone())
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
            "all" | "join" | "wait_all" | "waitall" | "wait-all" => Some(Self::All),
            "any" | "next" | "wait_any" | "waitany" | "wait-any" => Some(Self::Any),
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
                let (prev, _) = preview_job_result(&e.summary, JOB_OUTPUT_PREVIEW_CHARS);
                out.push_str(&prev);
                if !prev.ends_with('\n') {
                    out.push('\n');
                }
                if e.result_truncated {
                    if let Some(ref path) = e.result_ref {
                        out.push_str(&format!(
                            "[result truncated · full output: {}]\n",
                            path.display()
                        ));
                    }
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
    let (status, ok, turns, error) = if let Some(r) = &job.result {
        (
            r.status,
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
        backend: job.backend.as_str(),
        health: job.health_now().as_str(),
        status,
        summary: job.preview.clone(),
        preview: job.preview.clone(),
        result_bytes: job.result_bytes,
        result_chars: job.result_chars,
        result_truncated: job.result_truncated,
        result_ref: job.result_ref.clone(),
        spill_error: job.spill_error.clone(),
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
    if let Some(ref path) = snap.result_ref {
        out.push_str(&format!("result_ref: {}\n", path.display()));
    }
    out.push_str(&format!("result_bytes: {}\n", snap.result_bytes));
    out.push_str(&format!("result_chars: {}\n", snap.result_chars));
    if snap.result_truncated {
        out.push_str("result_truncated: true\n");
    }
    if let Some(ref err) = snap.spill_error {
        out.push_str(&format!("spill_error: {err}\n"));
    }
    if let Some(p) = &snap.log_path {
        out.push_str(&format!("log_path: {}\n", p.display()));
    }
    out.push('\n');

    let base_text = if !snap.preview.is_empty() {
        &snap.preview
    } else if !snap.summary.is_empty() {
        &snap.summary
    } else {
        ""
    };

    if base_text.is_empty() {
        out.push_str("(no summary)\n");
    } else {
        let (bounded_preview, preview_capped) =
            preview_job_result(base_text, JOB_NOTIFICATION_PREVIEW_CHARS);
        out.push_str(&bounded_preview);
        if !bounded_preview.ends_with('\n') {
            out.push('\n');
        }
        if snap.result_truncated || preview_capped {
            if let Some(ref path) = snap.result_ref {
                out.push_str(&format!(
                    "\n[Result preview capped ({}/{} chars, {} bytes). Full output saved to {}. Use read/grep to inspect on demand; do not load entire file if unnecessary.]\n",
                    bounded_preview.chars().count(),
                    snap.result_chars,
                    snap.result_bytes,
                    path.display()
                ));
            } else {
                out.push_str(&format!(
                    "\n[Result preview capped ({}/{} chars, {} bytes).]\n",
                    bounded_preview.chars().count(),
                    snap.result_chars,
                    snap.result_bytes,
                ));
            }
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
    if let Some(ref path) = snap.result_ref {
        out.push_str(&format!("result_ref: {}\n", path.display()));
    }
    out.push_str(&format!("result_bytes: {}\n", snap.result_bytes));
    out.push_str(&format!("result_chars: {}\n", snap.result_chars));
    if snap.result_truncated {
        out.push_str("result_truncated: true\n");
    }
    if let Some(ref err) = snap.spill_error {
        out.push_str(&format!("spill_error: {err}\n"));
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
    } else {
        let base_text = if !snap.preview.is_empty() {
            &snap.preview
        } else if !snap.summary.is_empty() {
            &snap.summary
        } else {
            ""
        };
        if base_text.is_empty() {
            out.push_str("(no summary)\n");
        } else {
            let (bounded_preview, preview_capped) =
                preview_job_result(base_text, JOB_OUTPUT_PREVIEW_CHARS);
            out.push_str("--- summary ---\n");
            out.push_str(&bounded_preview);
            if !bounded_preview.ends_with('\n') {
                out.push('\n');
            }
            if snap.result_truncated || preview_capped {
                if let Some(ref path) = snap.result_ref {
                    out.push_str(&format!(
                        "\n[Output preview capped ({}/{} chars, {} bytes). Full result in {}. Use read/grep to inspect on demand.]\n",
                        bounded_preview.chars().count(),
                        snap.result_chars,
                        snap.result_bytes,
                        path.display()
                    ));
                } else {
                    out.push_str(&format!(
                        "\n[Output preview capped ({}/{} chars, {} bytes).]\n",
                        bounded_preview.chars().count(),
                        snap.result_chars,
                        snap.result_bytes,
                    ));
                }
            }
        }
    }
    out
}

/// Shared lock for tests that mutate process-global env vars
/// (`ONE_JOB_RESULT_DIR`, `ONE_JOB_RESULT_MAX_BYTES`, …) so suites in
/// different modules (jobs, job_tools) cannot interleave.
#[cfg(test)]
pub(crate) mod test_env {
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
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
            backend: "one-internal",
            health: "healthy",
            status: Some(TaskExitStatus::Success),
            summary: "found login".into(),
            preview: "found login".into(),
            result_bytes: 11,
            result_chars: 11,
            result_truncated: false,
            result_ref: None,
            spill_error: None,
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
                backend: None,
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
        assert_eq!(parse_job_max_wall_ms("1"), Some(1));
        assert_eq!(parse_job_max_wall_ms("0"), None);
        assert_eq!(parse_job_max_wall_ms(""), Some(DEFAULT_JOB_MAX_WALL_MS));
        assert_eq!(
            parse_job_max_wall_ms("invalid"),
            Some(DEFAULT_JOB_MAX_WALL_MS)
        );
    }

    #[test]
    fn list_empty() {
        let reg = AgentJobRegistry::new(Arc::new(Mutex::new(Vec::new())));
        assert!(reg.list().is_empty());
        assert_eq!(format_job_list(&[]), "No agent jobs.\n");
    }

    #[test]
    fn wall_ms_zero_disables() {
        assert_eq!(parse_job_max_wall_ms("0"), None);
    }

    use super::test_env::ENV_LOCK;

    /// Independent watchdog must terminalize even when the harness future is
    /// stuck in non-cooperative work (the soft `timeout()` cannot cancel that).
    #[tokio::test]
    async fn wall_watchdog_kills_stuck_job_row() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = set_job_max_wall_ms_override(Some(Some(50)));
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
                backend: None,
            },
            false,
        );
        // Arm only the watchdog — never start a harness (simulates hang after AgentEnd).
        reg.arm_wall_watchdog(&id, true);
        let snap = tokio::time::timeout(Duration::from_secs(5), reg.wait_until_done(&id))
            .await
            .expect("watchdog should terminalize within 5s")
            .expect("job exists");
        assert!(
            matches!(snap.state, JobState::Failed | JobState::Aborted),
            "state={:?}",
            snap.state
        );
        assert_eq!(snap.status, Some(TaskExitStatus::TimedOut));
        let _ = set_job_max_wall_ms_override(prev);
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

    #[tokio::test]
    async fn test_subagent_spill_200kb_result() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue.clone());
        let (id, _ctrl, _abort) = reg.register_job(
            "explore",
            Some("deep scan"),
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

        let prefix = "START_200KB_OUTPUT_";
        let tail = "_TAIL_200KB_MARKER";
        let middle = "X".repeat(200_000);
        let large_result = format!("{prefix}{middle}{tail}");
        let expected_chars = large_result.chars().count();
        let expected_bytes = large_result.len();

        let rr = RunResult::success(large_result.clone(), 150);
        reg.finalize(&id, rr);

        let snap = reg.get(&id).expect("job snapshot exists");
        assert_eq!(snap.state, JobState::Completed);
        assert_eq!(snap.status, Some(TaskExitStatus::Success));
        assert_eq!(snap.result_chars, expected_chars);
        assert_eq!(snap.result_bytes, expected_bytes);
        assert!(snap.result_truncated);
        assert!(snap.spill_error.is_none());

        let result_file = snap.result_ref.clone().expect("result_ref must be set");
        assert!(result_file.exists(), "spill file must exist on disk");
        let disk_content = std::fs::read_to_string(&result_file).expect("read spill file");
        assert_eq!(
            disk_content, large_result,
            "spill file must contain full 200KB text"
        );

        // Verify completion notification size & contents
        let notes = queue.lock().unwrap().clone();
        assert_eq!(
            notes.len(),
            1,
            "must have exactly one completion notification"
        );
        let notif = &notes[0];
        assert!(
            notif.len() < 16 * 1024,
            "notification must be < 16KB (got {} bytes)",
            notif.len()
        );
        assert!(notif.contains("[job completed]"));
        assert!(notif.contains(&format!("id: {id}")));
        assert!(notif.contains(&format!("result_bytes: {expected_bytes}")));
        assert!(notif.contains(&format!("result_chars: {expected_chars}")));
        assert!(notif.contains("result_truncated: true"));
        assert!(notif.contains(&result_file.display().to_string()));
        assert!(
            notif.contains(prefix),
            "notification preview contains prefix"
        );
        assert!(
            !notif.contains(tail),
            "notification preview must NOT contain 200KB tail"
        );
        assert!(
            notif.contains("Full output saved to"),
            "notification includes read/grep guidance"
        );

        // Verify format_job_snapshot is also bounded
        let snap_text = format_job_snapshot(&snap);
        assert!(snap_text.len() < 16 * 1024, "job snapshot must be bounded");
        assert!(snap_text.contains(&result_file.display().to_string()));
        assert!(snap_text.contains(prefix));
        assert!(!snap_text.contains(tail));

        let _ = std::fs::remove_file(&result_file);
    }

    #[tokio::test]
    async fn test_subagent_small_result_no_spill() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue.clone());
        let (id, _ctrl, _abort) = reg.register_job(
            "explore",
            Some("quick scan"),
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

        let small_result = "SUBAGENT_DONE: all 5 files inspected cleanly.";
        let rr = RunResult::success(small_result, 42);
        reg.finalize(&id, rr);

        let snap = reg.get(&id).expect("snapshot");
        assert_eq!(snap.result_chars, small_result.chars().count());
        assert_eq!(snap.result_bytes, small_result.len());
        assert!(!snap.result_truncated);
        assert!(snap.result_ref.is_none());

        let notes = queue.lock().unwrap().clone();
        assert_eq!(notes.len(), 1);
        let notif = &notes[0];
        assert!(notif.contains(small_result));
        assert!(!notif.contains("result_truncated: true"));
        assert!(!notif.contains("Full output saved to"));
    }

    #[test]
    fn test_subagent_result_file_hard_cap() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ONE_JOB_RESULT_MAX_BYTES", "2048");
        let text = "ABCDE".repeat(2000); // 10,000 bytes
        let (result_ref, file_capped, spill_error) =
            persist_job_result_artifact("test_hard_cap_job", &text);
        std::env::remove_var("ONE_JOB_RESULT_MAX_BYTES");

        assert!(file_capped, "must report file capped at 2048 bytes");
        assert!(spill_error.is_none());
        let path = result_ref.expect("file written");
        let metadata = std::fs::metadata(&path).expect("file metadata");
        assert_eq!(
            metadata.len(),
            2048,
            "disk file must be capped exactly at 2048 bytes"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn test_subagent_spill_write_failure_resilience() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Point result dir to a non-existent path that cannot be created (a file path as parent)
        let dummy_file = std::env::temp_dir().join(format!("dummy_file_{}", std::process::id()));
        std::fs::write(&dummy_file, "blocking file").expect("write dummy");
        let uncreatable_dir = dummy_file.join("sub_dir_impossible");
        std::env::set_var("ONE_JOB_RESULT_DIR", &uncreatable_dir);

        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue.clone());
        let (id, _ctrl, _abort) = reg.register_job(
            "explore",
            None,
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

        let large_result = "Y".repeat(10_000);
        let rr = RunResult::success(large_result, 50);
        // Finalize must NOT panic or fail when spill write fails
        reg.finalize(&id, rr);

        std::env::remove_var("ONE_JOB_RESULT_DIR");
        let _ = std::fs::remove_file(&dummy_file);

        let snap = reg.get(&id).expect("snapshot exists");
        assert!(snap.spill_error.is_some(), "spill_error should be recorded");
        assert!(
            snap.result_ref.is_none(),
            "result_ref is None when disk write fails"
        );
        assert!(snap.result_truncated, "result_truncated is true");

        let notes = queue.lock().unwrap().clone();
        assert_eq!(notes.len(), 1);
        let notif = &notes[0];
        assert!(notif.contains("spill_error:"));
        assert!(notif.contains("[Result preview capped"));
    }

    #[tokio::test]
    async fn test_subagent_failed_status_preserves_error() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue.clone());
        let (id, _ctrl, _abort) = reg.register_job(
            "explore",
            None,
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

        let rr = RunResult::failure(
            ProtocolError::new(error_code::INTERNAL, "subagent failed with panic"),
            100,
        );
        reg.finalize(&id, rr);

        let snap = reg.get(&id).expect("snapshot");
        assert_eq!(snap.state, JobState::Failed);
        assert!(snap
            .error
            .as_deref()
            .unwrap_or("")
            .contains("subagent failed with panic"));

        let notes = queue.lock().unwrap().clone();
        let notif = &notes[0];
        assert!(notif.contains("subagent failed with panic"));
        assert!(notif.contains("status: failed") || notif.contains("status: runtime_error"));
    }

    #[tokio::test]
    async fn test_resume_source_preserves_semantics_after_spill() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue);
        let (id, _ctrl, _abort) = reg.register_job(
            "explore",
            None,
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

        let large_result = format!("START_RESUME_{}_END_RESUME", "Z".repeat(100_000));
        let rr = RunResult::success(large_result, 60);
        reg.finalize(&id, rr);

        let src = reg
            .resume_source(&id)
            .expect("resume_source must be available");
        assert_eq!(src.job_id, id);
        assert_eq!(src.agent, "explore");
        // resume summary should be bounded to avoid injecting 100KB into child seed messages
        assert!(src.summary.len() <= JOB_OUTPUT_PREVIEW_CHARS + 100);
        assert!(src.summary.contains("START_RESUME_"));
        assert!(!src.summary.contains("_END_RESUME"));

        if let Some(snap) = reg.get(&id) {
            if let Some(path) = snap.result_ref {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    // ---- Registry-level CLI-backend integration tests ----
    //
    // These drive the *full* registry path: register_job (backend resolution)
    // → launch_registered (backend dispatch) → tracker (normalized events)
    // → finalize (result + lifecycle). The codex/grok binaries are faked via
    // `ONE_CODEX_BIN` / `ONE_GROK_BIN` env overrides, so no real CLI or auth
    // is needed.
    use crate::protocol::AgentSpec;
    use crate::runtime::agent_backend::AgentTaskEvent;

    /// Write a fake `codex`/`grok` script printing the given NDJSON stream and
    /// return its path.
    fn write_fake_cli(dir: &std::path::Path, name: &str, stream: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\ncat <<'EOF'\n{stream}\nEOF\n"))
            .expect("write fake cli");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake cli");
        path
    }

    fn fake_cli_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-cli-backend-e2e-{}-{}",
            tag,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir fake cli dir");
        dir
    }

    fn cli_spawn_opts(dir: &std::path::Path) -> HarnessOptions {
        HarnessOptions::from_cwd(dir.to_path_buf())
    }

    async fn run_cli_backend_job(
        reg: &Arc<AgentJobRegistry>,
        dir: &std::path::Path,
        agent_name: &str,
        opts: &SpawnOptions,
    ) -> JobSnapshot {
        let provider: Arc<dyn LlmProvider> = Arc::new(one_ai::MockProvider::new());
        let mut req = RunRequest::new(AgentSpec::builtin_explore(), "probe the repo");
        req.agent.cwd = Some(dir.display().to_string());
        let id = reg.spawn_with(
            req,
            provider,
            cli_spawn_opts(dir),
            agent_name.to_string(),
            Some("fake cli probe".to_string()),
            None,
            opts.clone(),
        );
        let snap = reg.wait_until_done(&id).await.expect("job must terminate");
        reg.take_result_clone(&id);
        snap
    }

    #[tokio::test]
    async fn registry_codex_backend_job_end_to_end() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = fake_cli_dir("codex");
        let stream = concat!(
            r#"{"type":"thread.started","thread_id":"th_fake_1"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"item.started","item":{"id":"i1","type":"reasoning","text":"SECRET REASONING"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"CODEX_E2E_DONE"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{}}"#,
            "\n",
        );
        let fake = write_fake_cli(&dir, "codex", stream);
        std::env::set_var("ONE_CODEX_BIN", fake.display().to_string());

        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue);
        let snap = run_cli_backend_job(
            &reg,
            &dir,
            "codex:explore",
            &SpawnOptions {
                notify_completion: false,
                apply_wall_timeout: false,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
                backend: None,
            },
        )
        .await;

        // Backend resolution: agent-name prefix selected codex-cli.
        assert_eq!(snap.backend, "codex-cli");
        assert_eq!(snap.state, JobState::Completed, "{snap:?}");
        assert!(snap.ok, "{snap:?}");
        assert_eq!(snap.summary, "CODEX_E2E_DONE");
        // Normalized events fed the tracker: session + one turn recorded.
        let task = {
            let jobs = reg.jobs.lock().expect("jobs lock");
            jobs.get(&snap.id).unwrap().tracker.snapshot_task()
        };
        assert_eq!(task.session_id.as_deref(), Some("th_fake_1"));
        assert_eq!(task.turn_count, 1, "{task:?}");
        assert_eq!(task.tool_call_count, 0);
        assert!(matches!(task.lifecycle, TaskLifecycle::Completed));
        // Reasoning body never reaches the tracker or the event log.
        for line in &snap.event_lines {
            assert!(!line.contains("SECRET"), "reasoning leaked: {line}");
        }

        std::env::remove_var("ONE_CODEX_BIN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn registry_grok_backend_job_end_to_end() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = fake_cli_dir("grok");
        let stream = concat!(
            r#"{"type":"session_configured","sessionId":"s_fake_2"}"#,
            "\n",
            r#"{"type":"turn_started"}"#,
            "\n",
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"HIDDEN"}}"#,
            "\n",
            r#"{"sessionUpdate":"tool_call","toolCallId":"tc_9","title":"grep auth","kind":"grep"}"#,
            "\n",
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"tc_9","status":"completed"}"#,
            "\n",
            r#"{"type":"turn_completed"}"#,
            "\n",
            r#"{"type":"result","subtype":"success","result":"GROK_E2E_DONE","isError":false}"#,
            "\n",
        );
        let fake = write_fake_cli(&dir, "grok", stream);
        std::env::set_var("ONE_GROK_BIN", fake.display().to_string());

        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue);
        let snap = run_cli_backend_job(
            &reg,
            &dir,
            "grok:explore",
            &SpawnOptions {
                notify_completion: false,
                apply_wall_timeout: false,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
                backend: None,
            },
        )
        .await;

        assert_eq!(snap.backend, "grok-cli");
        assert_eq!(snap.state, JobState::Completed, "{snap:?}");
        assert!(snap.ok, "{snap:?}");
        assert_eq!(snap.summary, "GROK_E2E_DONE");
        let task = {
            let jobs = reg.jobs.lock().expect("jobs lock");
            jobs.get(&snap.id).unwrap().tracker.snapshot_task()
        };
        assert_eq!(task.session_id.as_deref(), Some("s_fake_2"));
        assert_eq!(task.turn_count, 1);
        assert_eq!(task.tool_call_count, 1);
        for line in &snap.event_lines {
            assert!(!line.contains("HIDDEN"), "thought leaked: {line}");
        }

        std::env::remove_var("ONE_GROK_BIN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn registry_cli_backend_failure_is_terminal_failed() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = fake_cli_dir("codex-fail");
        // turn.failed terminal: job must land Failed, not hang.
        let stream = concat!(
            r#"{"type":"thread.started","thread_id":"th_fail"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"401 Unauthorized"}}"#,
            "\n",
        );
        let fake = write_fake_cli(&dir, "codex", stream);
        std::env::set_var("ONE_CODEX_BIN", fake.display().to_string());

        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue);
        let snap = run_cli_backend_job(
            &reg,
            &dir,
            "codex:explore",
            &SpawnOptions {
                notify_completion: false,
                apply_wall_timeout: false,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
                backend: Some("codex".into()),
            },
        )
        .await;

        assert_eq!(snap.backend, "codex-cli");
        assert_eq!(snap.state, JobState::Failed, "{snap:?}");
        assert!(!snap.ok);
        assert!(
            snap.error
                .as_deref()
                .unwrap_or("")
                .contains("401 Unauthorized"),
            "{snap:?}"
        );
        let task = {
            let jobs = reg.jobs.lock().expect("jobs lock");
            jobs.get(&snap.id).unwrap().tracker.snapshot_task()
        };
        assert!(matches!(task.lifecycle, TaskLifecycle::Failed));

        std::env::remove_var("ONE_CODEX_BIN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn registry_reserved_backend_registers_then_fails() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let queue = Arc::new(Mutex::new(Vec::new()));
        let reg = AgentJobRegistry::new(queue);
        let (id, _ctrl, _abort) = reg.register_job(
            "claude:explore",
            None,
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
        // Row exists and is already terminal-failed; wait semantics intact.
        let snap = reg
            .wait_until_done(&id)
            .await
            .expect("reserved backend must finalize, not hang");
        assert_eq!(snap.state, JobState::Failed);
        assert!(
            snap.error
                .as_deref()
                .unwrap_or("")
                .contains("NOT implemented"),
            "{snap:?}"
        );
    }

    #[tokio::test]
    async fn tracker_distinguishes_event_vs_progress_clocks() {
        // OutputActivity refreshes last_event_at only; ToolStarted/
        // ToolProgress/ToolCompleted refresh last_progress_at too. Drive the
        // tracker directly (deterministic, no process needed).
        let log = JobEventLog::new();
        let tracker = JobTracker::new("job_t1", BackendKind::OneInternal, "explore", log);
        tracker.on_event(AgentTaskEvent::Started {
            session_id: None,
            process_id: None,
        });
        tracker.on_event(AgentTaskEvent::OutputActivity { bytes: 120 });
        {
            let t = tracker.snapshot_task();
            assert!(t.last_event_at.is_some());
            assert!(
                t.last_progress_at.is_none(),
                "OutputActivity must not touch the progress clock: {t:?}"
            );
        }
        tracker.on_event(AgentTaskEvent::ToolStarted {
            tool_call_id: "tc".into(),
            tool: "grep".into(),
            title: Some("auth".into()),
        });
        {
            let t = tracker.snapshot_task();
            assert!(t.last_progress_at.is_some());
            assert_eq!(t.current_tool.as_deref(), Some("grep"));
        }
        tracker.on_event(AgentTaskEvent::ToolProgress {
            tool_call_id: Some("tc".into()),
            note: "streaming".into(),
        });
        tracker.on_event(AgentTaskEvent::ToolCompleted {
            tool_call_id: "tc".into(),
            tool: "grep".into(),
            is_error: false,
            note: "3 hits".into(),
        });
        {
            let t = tracker.snapshot_task();
            assert_eq!(t.tool_call_count, 1);
            assert!(t.current_tool.is_none(), "completed clears in-flight");
            assert!(matches!(t.lifecycle, TaskLifecycle::Running));
        }
    }

    #[test]
    fn snapshot_exposes_health_via_tracker() {
        // Terminal jobs snapshot healthy; a tracker mid-run with no events at
        // all still evaluates (warmup → healthy).
        let log = JobEventLog::new();
        let tracker = JobTracker::new("job_h1", BackendKind::CodexCli, "codex:explore", log);
        let h = tracker.evaluate_health(None);
        assert_eq!(h.health, AgentHealth::Healthy, "{h:?}");
        assert_eq!(h.reason, "warmup", "{h:?}");
        // Terminal overrides everything.
        tracker.on_event(AgentTaskEvent::Completed {
            result_text: "x".into(),
            turns: None,
        });
        let h = tracker.evaluate_health(None);
        assert_eq!(h.health, AgentHealth::Healthy);
        assert_eq!(h.reason, "terminal");
    }
}
