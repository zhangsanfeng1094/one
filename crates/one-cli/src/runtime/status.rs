//! Unified runtime lifecycle status — the single source of truth for "what is
//! this One process doing right now".
//!
//! Ownership rules:
//! - The [`RuntimeStatusStore`] lives in [`super::AppRuntime`] and is shared
//!   (via `Arc`) with [`super::control::RuntimeControlHandle`], the native
//!   control socket, and RPC status reporting.
//! - Frontends (TUI / print / RPC / ACP) must **project** this state, never
//!   maintain their own parallel busy flags as runtime truth. UI-local caches
//!   for animation are fine as long as they are driven from runtime events.
//! - Lifecycle transitions go through [`RuntimeStatusStore`] methods (or the
//!   RAII [`TurnGuard`]); scattered `AtomicBool::store` busy flags are removed.
//!
//! Waiting semantics: `waiting_input` / `waiting_work` are runtime-observable
//! overlays. They can only be reported while a turn is in flight — `idle`
//! (no in-flight turn) is by definition *also* waiting for input, so the
//! external state stays unambiguous. Within a turn:
//! - `waiting_input`: permission gate or HITL (ask_user) has a pending prompt.
//! - `waiting_work`: ReAct parked on `wait_tasks` background work
//!   (`WaitParkStart` … `WaitParkEnd`).
//! - activity `compacting`: `CompactionStart` … `CompactionEnd`.
//!
//! `ready_for_check` is intentionally not modeled: nothing in the runtime can
//! reliably decide it today, and a guessed value would poison external control.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use one_core::events::AgentEvent;
use serde::Serialize;

static NEXT_TURN: AtomicU64 = AtomicU64::new(1);

/// Lifecycle of one runtime process (per in-flight user turn).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeLifecycleState {
    /// No in-flight turn.
    Idle,
    /// A user turn is in flight (model / tools / compaction / parking).
    Running,
    /// A turn is in flight and blocked on human input (approval / ask_user).
    WaitingInput,
    /// A turn is in flight and parked on background work (`wait_tasks`).
    WaitingWork,
    /// Abort was requested for the current turn; the turn is unwinding.
    AbortRequested,
}

impl RuntimeLifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::WaitingWork => "waiting_work",
            Self::AbortRequested => "abort_requested",
        }
    }
}

/// Terminal outcome of the last finished turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Aborted,
    Failed,
}

impl TurnOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }
}

/// Fine-grained activity while a turn runs (authority: runtime events).
///
/// `None` means "generic turn" — model sampling / tool loop without a more
/// specific observable activity. Do not invent frontend guesses here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeActivity {
    /// Context compaction in progress (`CompactionStart` … `CompactionEnd`).
    Compacting,
    /// ReAct parked on background work (`WaitParkStart` … `WaitParkEnd`).
    WaitingWork,
    /// A tool call is currently executing.
    Tool { name: String },
}

impl RuntimeActivity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compacting => "compacting",
            Self::WaitingWork => "waiting_work",
            Self::Tool { .. } => "tool",
        }
    }
}

#[derive(Debug)]
struct StatusInner {
    /// Nesting depth of turn frames (RPC wraps runtime.prompt; compaction
    /// wraps a running turn). Busy ⇔ depth > 0.
    depth: Mutex<u64>,
    /// A frame was opened by the AgentStart observer (print path has no
    /// explicit frontend guard). AgentEnd closes exactly this frame.
    observer_frame: AtomicBool,
    /// In-flight turn id; `None` = idle. **busy is derived from this.**
    turn: Mutex<Option<u64>>,
    /// Recomputed lifecycle (see [`Self::recompute`]).
    state: Mutex<RuntimeLifecycleState>,
    /// Overlay: permission gate has a pending interactive ask.
    gate_pending: AtomicBool,
    /// Overlay: HITL (ask_user) has a pending select prompt.
    hitl_pending: AtomicBool,
    /// Overlay: ReAct parked on background work.
    wait_parked: AtomicBool,
    /// Overlay: abort requested for the current turn.
    abort_requested: AtomicBool,
    /// Overlay: compaction running inside the turn.
    compacting: AtomicBool,
    /// Latest running tool name (activity projection).
    tool: Mutex<Option<String>>,
    /// Terminal outcome of the last finished turn.
    last_outcome: Mutex<Option<TurnOutcome>>,
}

impl StatusInner {
    fn new() -> Self {
        Self {
            depth: Mutex::new(0),
            observer_frame: AtomicBool::new(false),
            turn: Mutex::new(None),
            state: Mutex::new(RuntimeLifecycleState::Idle),
            gate_pending: AtomicBool::new(false),
            hitl_pending: AtomicBool::new(false),
            wait_parked: AtomicBool::new(false),
            abort_requested: AtomicBool::new(false),
            compacting: AtomicBool::new(false),
            tool: Mutex::new(None),
            last_outcome: Mutex::new(None),
        }
    }

    /// Single place that derives lifecycle from turn + overlays.
    /// All mutexes are leaf locks (never held across await / other locks).
    fn recompute(&self) {
        let busy = {
            let depth = self.depth.lock().expect("status depth lock");
            if *depth == 0 {
                let mut turn = self.turn.lock().expect("status turn lock");
                *turn = None;
                false
            } else {
                true
            }
        };
        let mut state = self.state.lock().expect("status state lock");
        *state = if !busy {
            RuntimeLifecycleState::Idle
        } else if self.abort_requested.load(Ordering::Acquire) {
            RuntimeLifecycleState::AbortRequested
        } else if self.gate_pending.load(Ordering::Acquire)
            || self.hitl_pending.load(Ordering::Acquire)
        {
            RuntimeLifecycleState::WaitingInput
        } else if self.wait_parked.load(Ordering::Acquire) {
            RuntimeLifecycleState::WaitingWork
        } else {
            RuntimeLifecycleState::Running
        };
    }
}

/// Runtime-owned, thread-safe status store. All methods are non-blocking and
/// await-free, so concurrent native-socket status reads cannot deadlock with
/// steer / followup / abort writers.
#[derive(Debug, Clone)]
pub struct RuntimeStatusStore(Arc<StatusInner>);

impl Default for RuntimeStatusStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeStatusStore {
    pub fn new() -> Self {
        Self(Arc::new(StatusInner::new()))
    }

    // ── Turn lifecycle ────────────────────────────────────────────────

    /// Begin (or nest into) an in-flight turn. The first frame assigns the
    /// turn id; nested frames (compaction / RPC wrapper) only bump depth so
    /// an inner guard drop cannot close the outer turn.
    pub fn begin_turn(&self) {
        self.begin_turn_inner(false);
    }

    fn begin_turn_inner(&self, observer: bool) {
        {
            let mut depth = self.0.depth.lock().expect("status depth lock");
            if *depth == 0 {
                let mut turn = self.0.turn.lock().expect("status turn lock");
                *turn = Some(NEXT_TURN.fetch_add(1, Ordering::Relaxed));
                self.0.observer_frame.store(observer, Ordering::Release);
            }
            *depth += 1;
        }
        self.0.recompute();
    }

    /// End one turn frame. The outermost frame records the terminal outcome,
    /// clears overlays, and returns the store to idle; nested frames only
    /// decrement. Ignored when no frame is open (out-of-order drop safety).
    pub fn end_turn(&self, outcome: TurnOutcome) {
        let closed = {
            let mut depth = self.0.depth.lock().expect("status depth lock");
            if *depth == 0 {
                false
            } else {
                *depth -= 1;
                *depth == 0
            }
        };
        if closed {
            let mut turn = self.0.turn.lock().expect("status turn lock");
            *turn = None;
            *self.0.last_outcome.lock().expect("last outcome lock") = Some(outcome);
            self.clear_overlays();
        }
        self.0.recompute();
    }

    /// RAII guard that ends the turn on drop with a lazily computed outcome
    /// (the closure can inspect the final `Result` of the turn).
    pub fn turn_guard(
        &self,
        outcome_on_drop: impl Fn() -> TurnOutcome + Send + Sync + 'static,
    ) -> TurnGuard {
        self.begin_turn();
        TurnGuard {
            store: Some(self.clone()),
            outcome: Box::new(outcome_on_drop),
        }
    }

    /// RAII guard flavor with a fixed outcome (print / RPC / ACP).
    pub fn turn_guard_fixed(&self, outcome: TurnOutcome) -> TurnGuard {
        self.begin_turn();
        TurnGuard {
            store: Some(self.clone()),
            outcome: Box::new(move || outcome),
        }
    }

    /// Overlay updates (event-driven). Each recomputes the lifecycle.
    pub fn set_gate_pending(&self, pending: bool) {
        self.0.gate_pending.store(pending, Ordering::Release);
        self.0.recompute();
    }

    pub fn set_hitl_pending(&self, pending: bool) {
        self.0.hitl_pending.store(pending, Ordering::Release);
        self.0.recompute();
    }

    pub fn set_wait_parked(&self, parked: bool) {
        self.0.wait_parked.store(parked, Ordering::Release);
        self.0.recompute();
    }

    pub fn set_compacting(&self, active: bool) {
        self.0.compacting.store(active, Ordering::Release);
        self.0.recompute();
    }

    /// Record abort requested for the current turn (control / Esc abort).
    pub fn set_abort_requested(&self) {
        self.0.abort_requested.store(true, Ordering::Release);
        self.0.recompute();
    }

    /// Current tool execution name (activity projection).
    pub fn set_tool_activity(&self, name: Option<&str>) {
        *self.0.tool.lock().expect("tool lock") = name.map(str::to_string);
    }

    /// Clear overlays when a turn boundary is crossed.
    fn clear_overlays(&self) {
        self.0.observer_frame.store(false, Ordering::Release);
        self.0.gate_pending.store(false, Ordering::Release);
        self.0.hitl_pending.store(false, Ordering::Release);
        self.0.wait_parked.store(false, Ordering::Release);
        self.0.abort_requested.store(false, Ordering::Release);
        self.0.compacting.store(false, Ordering::Release);
        *self.0.tool.lock().expect("tool lock") = None;
    }

    // ── Reads (native status / session browser / TUI projection) ──────

    /// Authoritative lifecycle state.
    pub fn state(&self) -> RuntimeLifecycleState {
        *self.0.state.lock().expect("status state lock")
    }

    /// Derived compatibility field: busy ⇔ at least one turn frame is open.
    /// Superset of "running" (waiting / abort-unwinding turns are also busy)
    /// so prompt admission on the control plane keeps its previous semantics.
    pub fn is_busy(&self) -> bool {
        *self.0.depth.lock().expect("status depth lock") > 0
    }

    /// Turn identity of the in-flight turn (`None` when idle).
    pub fn current_turn(&self) -> Option<u64> {
        *self.0.turn.lock().expect("status turn lock")
    }

    /// Terminal outcome of the last finished turn.
    pub fn last_outcome(&self) -> Option<TurnOutcome> {
        *self.0.last_outcome.lock().expect("last outcome lock")
    }

    /// Activity projection: compacting > waiting_work > tool > None(generic).
    pub fn activity(&self) -> Option<RuntimeActivity> {
        if self.0.compacting.load(Ordering::Acquire) {
            return Some(RuntimeActivity::Compacting);
        }
        if self.0.wait_parked.load(Ordering::Acquire) {
            return Some(RuntimeActivity::WaitingWork);
        }
        self.0
            .tool
            .lock()
            .expect("tool lock")
            .as_ref()
            .map(|name| RuntimeActivity::Tool { name: name.clone() })
    }

    /// Apply agent-event overlays (subscribe bridge entry point).
    ///
    /// `AgentStart` opens an **observer frame** when no frontend guard did
    /// (print path). `AgentEnd` closes exactly that observer frame — the
    /// provider run is over; frontend guards (RPC / interactive / ACP) open
    /// and close their own frames independently and post-turn persistence
    /// still runs inside those explicit frames.
    pub fn observe_agent_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::AgentStart => {
                let depth = self.0.depth.lock().expect("status depth lock");
                if *depth == 0 {
                    drop(depth);
                    self.begin_turn_inner(true);
                }
            }
            AgentEvent::AgentEnd { .. } => {
                if self.0.observer_frame.swap(false, Ordering::AcqRel) {
                    self.end_turn(TurnOutcome::Completed);
                }
            }
            AgentEvent::CompactionStart => self.set_compacting(true),
            AgentEvent::CompactionEnd { .. } => self.set_compacting(false),
            AgentEvent::WaitParkStart { .. } => self.set_wait_parked(true),
            AgentEvent::WaitParkEnd => self.set_wait_parked(false),
            AgentEvent::ToolExecutionStart { tool_call } => {
                self.set_tool_activity(Some(&tool_call.name))
            }
            AgentEvent::ToolExecutionEnd { .. } => self.set_tool_activity(None),
            _ => {}
        }
    }
}

/// RAII guard: the store began a turn on creation; drop ends it (state → idle
/// + `last_outcome`). `finish` records the outcome eagerly.
pub struct TurnGuard {
    store: Option<RuntimeStatusStore>,
    outcome: Box<dyn Fn() -> TurnOutcome + Send + Sync>,
}

impl TurnGuard {
    /// End the turn now with an explicit outcome and disarm drop.
    pub fn finish(mut self, outcome: TurnOutcome) {
        self.store.take().map(|store| store.end_turn(outcome));
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            let outcome = (self.outcome)();
            store.end_turn(outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::agent::WaitInterestMode;
    use one_core::tool::ToolCall;
    use serde_json::json;

    #[test]
    fn idle_running_idle_lifecycle() {
        let store = RuntimeStatusStore::new();
        assert_eq!(store.state(), RuntimeLifecycleState::Idle);
        assert!(!store.is_busy());
        assert_eq!(store.current_turn(), None);

        let guard = store.turn_guard_fixed(TurnOutcome::Completed);
        assert_eq!(store.state(), RuntimeLifecycleState::Running);
        assert!(store.is_busy());
        let first_turn = store.current_turn();
        assert!(first_turn.is_some());

        guard.finish(TurnOutcome::Completed);
        assert_eq!(store.state(), RuntimeLifecycleState::Idle);
        assert!(!store.is_busy());
        assert_eq!(store.last_outcome(), Some(TurnOutcome::Completed));

        let guard2 = store.turn_guard_fixed(TurnOutcome::Aborted);
        assert_ne!(store.current_turn(), first_turn, "turn ids must advance");
        drop(guard2);
        assert_eq!(store.last_outcome(), Some(TurnOutcome::Aborted));
    }

    #[test]
    fn running_abort_requested_then_idle() {
        let store = RuntimeStatusStore::new();
        let guard = store.turn_guard_fixed(TurnOutcome::Aborted);
        assert_eq!(store.state(), RuntimeLifecycleState::Running);
        store.set_abort_requested();
        assert_eq!(store.state(), RuntimeLifecycleState::AbortRequested);
        assert!(store.is_busy(), "abort unwinding stays busy");
        guard.finish(TurnOutcome::Aborted);
        assert_eq!(store.state(), RuntimeLifecycleState::Idle);
        assert!(!store.is_busy());
        assert_eq!(store.last_outcome(), Some(TurnOutcome::Aborted));
    }

    #[test]
    fn waiting_input_from_gate_and_hitl_overlays() {
        let store = RuntimeStatusStore::new();
        let guard = store.turn_guard_fixed(TurnOutcome::Completed);
        assert_eq!(store.state(), RuntimeLifecycleState::Running);

        store.set_gate_pending(true);
        assert_eq!(store.state(), RuntimeLifecycleState::WaitingInput);
        store.set_gate_pending(false);
        assert_eq!(store.state(), RuntimeLifecycleState::Running);

        store.set_hitl_pending(true);
        assert_eq!(store.state(), RuntimeLifecycleState::WaitingInput);
        store.set_hitl_pending(false);
        assert_eq!(store.state(), RuntimeLifecycleState::Running);
        drop(guard);
    }

    #[test]
    fn waiting_work_park_overlay_and_activity() {
        let store = RuntimeStatusStore::new();
        let guard = store.turn_guard_fixed(TurnOutcome::Completed);
        store.set_wait_parked(true);
        assert_eq!(store.state(), RuntimeLifecycleState::WaitingWork);
        assert_eq!(store.activity().map(|a| a.as_str()), Some("waiting_work"));
        store.set_wait_parked(false);
        assert_eq!(store.state(), RuntimeLifecycleState::Running);
        assert_eq!(store.activity(), None);
        drop(guard);

        // Overlays outside a turn can never leave idle.
        store.set_wait_parked(true);
        assert_eq!(store.state(), RuntimeLifecycleState::Idle);
        assert!(!store.is_busy());
    }

    #[test]
    fn agent_events_drive_activity() {
        let store = RuntimeStatusStore::new();
        let guard = store.turn_guard_fixed(TurnOutcome::Completed);

        store.observe_agent_event(&AgentEvent::CompactionStart);
        assert_eq!(store.activity().map(|a| a.as_str()), Some("compacting"));
        store.observe_agent_event(&AgentEvent::CompactionEnd {
            tokens_before: 10,
            tokens_after: 5,
            kept_turns: 2,
        });
        assert_eq!(store.activity(), None);

        store.observe_agent_event(&AgentEvent::WaitParkStart {
            mode: WaitInterestMode::All,
            ids: vec!["job-1".into()],
        });
        assert_eq!(store.state(), RuntimeLifecycleState::WaitingWork);
        store.observe_agent_event(&AgentEvent::WaitParkEnd);
        assert_eq!(store.state(), RuntimeLifecycleState::Running);

        store.observe_agent_event(&AgentEvent::ToolExecutionStart {
            tool_call: ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                arguments: json!({"command": "ls"}),
            },
        });
        assert_eq!(store.activity().map(|a| a.as_str()), Some("tool"));
        store.observe_agent_event(&AgentEvent::ToolExecutionEnd {
            tool_call: ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                arguments: json!({}),
            },
            output: one_core::tool::ToolOutput::text("done"),
            is_error: false,
        });
        assert_eq!(store.activity(), None);
        drop(guard);
    }

    #[test]
    fn agent_start_without_explicit_guard_marks_turn() {
        // Print path: AgentStart arrives with no TurnGuard — status must
        // still become busy for control-socket observers, and AgentEnd
        // closes exactly that observer frame.
        let store = RuntimeStatusStore::new();
        store.observe_agent_event(&AgentEvent::AgentStart);
        assert!(store.is_busy());
        assert_eq!(store.state(), RuntimeLifecycleState::Running);

        store.observe_agent_event(&AgentEvent::AgentEnd {
            new_messages: Vec::new(),
        });
        assert!(!store.is_busy());

        // With an explicit frontend guard, AgentStart/End are absorbed by it:
        // the observer never opens a frame and AgentEnd cannot close the
        // guard's frame (post-turn persistence stays "busy").
        let guard = store.turn_guard_fixed(TurnOutcome::Completed);
        store.observe_agent_event(&AgentEvent::AgentStart);
        store.observe_agent_event(&AgentEvent::AgentEnd {
            new_messages: Vec::new(),
        });
        assert!(store.is_busy());
        drop(guard);
        assert!(!store.is_busy());
    }

    #[test]
    fn nested_begin_is_idempotent_and_out_of_order_end_is_safe() {
        let store = RuntimeStatusStore::new();
        let g1 = store.turn_guard_fixed(TurnOutcome::Completed);
        let first = store.current_turn();
        let g2 = store.turn_guard_fixed(TurnOutcome::Aborted);
        assert_eq!(store.current_turn(), first, "nested begin keeps turn");
        drop(g2); // inner guard drop must NOT close the outer turn
        assert!(store.is_busy());
        drop(g1);
        assert!(!store.is_busy());

        // End without begin is ignored.
        store.end_turn(TurnOutcome::Failed);
        assert!(!store.is_busy());
        assert_eq!(store.last_outcome(), Some(TurnOutcome::Completed));
    }

    #[test]
    fn lazy_outcome_guard_reads_final_result() {
        let store = RuntimeStatusStore::new();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag2 = flag.clone();
        let guard = store.turn_guard(move || {
            if flag2.load(Ordering::SeqCst) {
                TurnOutcome::Failed
            } else {
                TurnOutcome::Completed
            }
        });
        flag.store(true, Ordering::SeqCst);
        drop(guard);
        assert_eq!(store.last_outcome(), Some(TurnOutcome::Failed));
    }

    #[test]
    fn concurrent_reads_and_overlay_writers_do_not_deadlock() {
        // Native socket reads state while steer/abort/turn boundaries write.
        let store = RuntimeStatusStore::new();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let mut handles = Vec::new();
        for i in 0..4 {
            let store = store.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for n in 0..2000 {
                    match i {
                        0 => {
                            let g = store.turn_guard_fixed(TurnOutcome::Completed);
                            std::hint::black_box(g);
                        }
                        1 => store.set_wait_parked(n % 2 == 0),
                        2 => {
                            store.set_abort_requested();
                            store.observe_agent_event(&AgentEvent::AgentEnd {
                                new_messages: Vec::new(),
                            });
                        }
                        _ => {
                            std::hint::black_box(store.state());
                            std::hint::black_box(store.is_busy());
                            std::hint::black_box(store.activity());
                            std::hint::black_box(store.last_outcome());
                        }
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().expect("no deadlock / no panic");
        }
        assert!(!store.is_busy());
    }
}
