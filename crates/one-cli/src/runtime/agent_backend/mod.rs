//! Unified agent backend layer.
//!
//! One [`AgentBackend`] interface behind the existing `task` / delegate
//! semantics; concrete implementations:
//!
//! - [`OneInternalBackend`] — the in-process One harness (ReAct, existing
//!   subagent capability; no behavior change)
//! - [`CodexCliBackend`] — `codex exec --json` (verified: codex-cli 0.156.1,
//!   JSONL events `thread.started` / `turn.started` / `item.*` / `turn.*`)
//! - [`GrokCliBackend`] — `grok --single --output-format streaming-json`
//!   (verified: grok 1.0.41, NDJSON with ACP `session/update`-style records)
//! - Claude / Pi — **reserved only, NOT implemented** (see [`BackendKind`])
//!
//! The coordinator (spawn/wait/cancel/snapshot) lives in
//! [`super::super::jobs::AgentJobRegistry`] plus
//! [`super::super::coordinator::SubagentCoordinator`]; backends never touch
//! the parent model loop, WaitInterest, Notify, or result spill — those stay
//! in the existing layers.

mod codex;
mod grok;
mod health;
mod one_internal;
mod process;

pub use codex::{normalize_codex_event, CodexCliBackend};
pub use grok::{normalize_grok_event, GrokCliBackend};
pub use health::{AgentHealth, HealthEvaluation, HealthEvaluator, HealthInputs, HealthThresholds};
pub use one_internal::OneInternalBackend;
pub use process::{
    run_cli_backend, CliBackendCommand, CliRunOutcome, CLI_STDERR_MAX_BYTES, CLI_STDOUT_MAX_BYTES,
};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::harness::HarnessOptions;
use crate::protocol::{AgentSpec, RunRequest, RunResult};

/// Which backend executes a child agent task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// In-process One harness (ReAct child). Default; existing behavior.
    #[default]
    OneInternal,
    /// External `codex exec --json` process.
    CodexCli,
    /// External `grok --single --output-format streaming-json` process.
    GrokCli,
    /// **Reserved — NOT implemented.** Fails fast with a clear error.
    ClaudeCli,
    /// **Reserved — NOT implemented.** Fails fast with a clear error.
    PiCli,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneInternal => "one-internal",
            Self::CodexCli => "codex-cli",
            Self::GrokCli => "grok-cli",
            Self::ClaudeCli => "claude-cli",
            Self::PiCli => "pi-cli",
        }
    }

    /// Parse from agent-name suffix or explicit `backend:` style value.
    /// Accepted spellings: `one`, `internal`, `one-internal`, `codex`,
    /// `codex-cli`, `grok`, `grok-cli`, `claude`, `claude-cli`, `pi`, `pi-cli`.
    pub fn parse(s: &str) -> Option<Self> {
        match s
            .trim()
            .trim_start_matches("backend:")
            .to_ascii_lowercase()
            .as_str()
        {
            "one" | "internal" | "one-internal" | "one/internal" => Some(Self::OneInternal),
            "codex" | "codex-cli" | "codexcli" => Some(Self::CodexCli),
            "grok" | "grok-cli" | "grokcli" | "grok-build" | "grokbuild" => Some(Self::GrokCli),
            "claude" | "claude-cli" | "claudecli" => Some(Self::ClaudeCli),
            "pi" | "pi-cli" | "picli" => Some(Self::PiCli),
            _ => None,
        }
    }

    /// Backends that only exist as reserved names this round.
    pub fn is_reserved_stub(self) -> bool {
        matches!(self, Self::ClaudeCli | Self::PiCli)
    }
}

/// A raw event emitted by a backend before/after normalization.
///
/// Backends produce [`AgentTaskEvent`]s through their runner; the registry
/// feeds them into the job's activity log and health tracker.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentTaskEvent {
    /// Process/session started (maps internal `AgentStart`). CLI backends
    /// report their child pid here so the tracker can record it.
    Started {
        session_id: Option<String>,
        process_id: Option<u32>,
    },
    /// One model/agent turn began.
    TurnStarted { turn: u64 },
    /// A tool call began.
    ToolStarted {
        tool_call_id: String,
        tool: String,
        title: Option<String>,
    },
    /// Progress / output activity for a running tool or stream.
    /// Never carries thought/reasoning body text (see `TextActivity` note).
    ToolProgress {
        tool_call_id: Option<String>,
        note: String,
    },
    /// Output activity (stdout bytes, streaming deltas) — progress signal only.
    OutputActivity { bytes: u64 },
    /// A tool call finished (ok or error). `note` is a short result brief.
    ToolCompleted {
        tool_call_id: String,
        tool: String,
        is_error: bool,
        note: String,
    },
    /// Assistant text activity (progress only; body text is *not* retained by
    /// the coordinator — final text arrives via `RunResult`).
    TextActivity { delta_chars: usize },
    /// One model/agent turn completed.
    TurnCompleted { turn: u64 },
    /// Terminal: success with final text.
    Completed {
        result_text: String,
        turns: Option<u64>,
    },
    /// Terminal: failure.
    Failed { message: String },
}

impl AgentTaskEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Started { .. } => "started",
            Self::TurnStarted { .. } => "turn_started",
            Self::ToolStarted { .. } => "tool_started",
            Self::ToolProgress { .. } => "tool_progress",
            Self::OutputActivity { .. } => "output_activity",
            Self::ToolCompleted { .. } => "tool_completed",
            Self::TextActivity { .. } => "text_activity",
            Self::TurnCompleted { .. } => "turn_completed",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. })
    }
}

/// Live per-task tracking state (lifecycle + health counters).
///
/// Lifecycle and health are **separate**: lifecycle mirrors the job row
/// (`JobState`), health is the anti-fake-death signal computed by
/// [`HealthEvaluator`].
#[derive(Debug, Clone)]
pub struct AgentTask {
    pub id: String,
    pub backend: BackendKind,
    pub agent: String,
    /// starting | running | waiting | completed | failed | cancelled
    pub lifecycle: TaskLifecycle,
    pub health: AgentHealth,
    pub session_id: Option<String>,
    pub process_id: Option<u32>,
    pub started_at: std::time::Instant,
    pub last_event_at: Option<std::time::Instant>,
    pub last_progress_at: Option<std::time::Instant>,
    pub turn_count: u64,
    pub tool_call_count: u64,
    pub current_activity: String,
    pub current_tool: Option<String>,
    pub result_ref: Option<std::path::PathBuf>,
}

/// Lifecycle independent of `JobState` (which stays for compat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskLifecycle {
    #[default]
    Starting,
    Running,
    /// Awaiting an external dependency (never blocks the parent loop).
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

impl TaskLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// How a backend run ends (returned by [`AgentBackend::run`]).
pub struct BackendRunOutcome {
    pub result: RunResult,
    /// Best-effort text from terminal events (may be empty for internal).
    pub session_id: Option<String>,
}

/// Thin spawn-time request for any backend.
pub struct BackendSpawn {
    pub req: RunRequest,
    pub opts: HarnessOptions,
    pub agent_name: String,
    pub description: Option<String>,
    /// Existing child AgentSpec when resolvable (internal backend needs it;
    /// CLI backends derive their own CLI flags).
    pub child_spec: AgentSpec,
    /// Bound parent provider (used by OneInternal; ignored by CLI backends).
    pub provider: Option<Arc<dyn one_core::agent::LlmProvider>>,
}

/// Minimal backend trait — deliberately thin.
///
/// Implementations own *process/harness execution and event emission* only.
/// The registry owns job rows, notifications, spill, WaitInterest, etc.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    fn kind(&self) -> BackendKind;

    /// Human label for logs.
    fn label(&self) -> &'static str;

    /// Run to completion, emitting normalized events into `sink`
    /// (called from the backend's execution context; must not block the
    /// registry). Returns the terminal outcome.
    async fn run(
        &self,
        spawn: BackendSpawn,
        control: BackendControl,
        sink: Arc<dyn AgentEventSink>,
    ) -> RunResult;
}

/// Control plane handed to backends (registry-owned).
#[derive(Clone)]
pub struct BackendControl {
    pub abort: Arc<std::sync::atomic::AtomicBool>,
    pub event_log: Arc<super::jobs::JobEventLog>,
    pub wall_timeout: Option<Duration>,
}

/// Event sink: backends push normalized events; the tracker side records
/// health/activity. Keep allocation-light.
pub trait AgentEventSink: Send + Sync {
    fn on_event(&self, event: AgentTaskEvent);
}

/// Simple collecting sink (tests + registry tracking).
#[derive(Default)]
pub struct CollectorSink {
    pub events: std::sync::Mutex<Vec<AgentTaskEvent>>,
}

impl CollectorSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn snapshot(&self) -> Vec<AgentTaskEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl AgentEventSink for CollectorSink {
    fn on_event(&self, event: AgentTaskEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }
}

/// Shared sink handle (Arc<dyn AgentEventSink>).
pub type SharedAgentEventSink = Arc<dyn AgentEventSink>;

/// Resolve the default backend for an agent name / explicit backend argument.
///
/// Order: explicit `backend` string → agent name parse (`codex:foo`,
/// `grok:foo`) → [`BackendKind::OneInternal`].
pub fn resolve_backend(explicit: Option<&str>, agent_name: &str) -> Result<BackendKind, String> {
    if let Some(b) = explicit.and_then(BackendKind::parse) {
        if b.is_reserved_stub() {
            return Err(format!(
                "backend `{}` is reserved but NOT implemented in this build; \
                 available: one-internal, codex-cli, grok-cli",
                b.as_str()
            ));
        }
        return Ok(b);
    }
    if let Some((prefix, _rest)) = agent_name.split_once(':') {
        if let Some(b) = BackendKind::parse(prefix) {
            if b.is_reserved_stub() {
                return Err(format!(
                    "backend `{}` is reserved but NOT implemented in this build; \
                     available: one-internal, codex-cli, grok-cli",
                    b.as_str()
                ));
            }
            return Ok(b);
        }
    }
    if let Some(b) = BackendKind::parse(agent_name) {
        // Bare `codex` / `grok` agent names select that backend.
        return Ok(b);
    }
    Ok(BackendKind::OneInternal)
}

/// Build the concrete backend for a kind (process spawn config comes from env).
pub fn backend_for(kind: BackendKind) -> Arc<dyn AgentBackend> {
    match kind {
        BackendKind::OneInternal => Arc::new(OneInternalBackend::new()),
        BackendKind::CodexCli => Arc::new(CodexCliBackend::new()),
        BackendKind::GrokCli => Arc::new(GrokCliBackend::new()),
        // Reserved stubs are rejected at resolve time; guard anyway.
        BackendKind::ClaudeCli | BackendKind::PiCli => Arc::new(OneInternalBackend::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_kind_parse_roundtrip() {
        assert_eq!(BackendKind::parse("one"), Some(BackendKind::OneInternal));
        assert_eq!(
            BackendKind::parse("internal"),
            Some(BackendKind::OneInternal)
        );
        assert_eq!(BackendKind::parse("codex"), Some(BackendKind::CodexCli));
        assert_eq!(BackendKind::parse("grok-build"), Some(BackendKind::GrokCli));
        assert_eq!(BackendKind::parse("claude"), Some(BackendKind::ClaudeCli));
        assert_eq!(BackendKind::parse("pi"), Some(BackendKind::PiCli));
        assert_eq!(BackendKind::parse("nope"), None);
        assert_eq!(
            BackendKind::parse("backend:codex"),
            Some(BackendKind::CodexCli)
        );
    }

    #[test]
    fn reserved_stubs_rejected_at_resolve() {
        let err = resolve_backend(Some("claude"), "x").unwrap_err();
        assert!(err.contains("NOT implemented"), "{err}");
        let err = resolve_backend(None, "pi:researcher").unwrap_err();
        assert!(err.contains("NOT implemented"), "{err}");
    }

    #[test]
    fn agent_prefix_selects_backend() {
        assert_eq!(
            resolve_backend(None, "codex:implementer").unwrap(),
            BackendKind::CodexCli
        );
        assert_eq!(
            resolve_backend(None, "grok:writer").unwrap(),
            BackendKind::GrokCli
        );
        assert_eq!(
            resolve_backend(None, "explore").unwrap(),
            BackendKind::OneInternal
        );
    }

    #[test]
    fn terminal_event_flag() {
        assert!(AgentTaskEvent::Completed {
            result_text: String::new(),
            turns: None
        }
        .is_terminal());
        assert!(AgentTaskEvent::Failed {
            message: "x".into()
        }
        .is_terminal());
        assert!(!AgentTaskEvent::TurnStarted { turn: 1 }.is_terminal());
    }
}
