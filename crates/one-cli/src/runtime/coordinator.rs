//! Independent subagent admission coordinator (Grok `SubagentCoordinator`).
//!
//! `task` does **not** acquire a semaphore on the caller. This actor owns
//! capacity, an explicit FIFO queue, and the foreground admission deadline:
//!
//! - `Start` — a slot is free; the child launches immediately
//! - `Enqueue` — at `max_concurrent`; request parks until a child finishes
//! - `Reject` — at capacity and `LimitBehavior::Fail`
//!
//! `foreground_budget` applies **only** to a queued `background=false` caller.
//! If the child has not started when the deadline fires, the caller is handed
//! off (`backgrounded=true`) and the spawn stays in the queue. A child that
//! already started is never auto-backgrounded for being slow.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use one_core::agent::LlmProvider;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::harness::{HarnessOptions, RunControl};
use super::jobs::{AgentJobRegistry, JobState, KillReason, SpawnOptions};
use crate::protocol::{error_code, ProtocolError, RunRequest, RunResult, TaskExitStatus};

/// What to do when `max_concurrent` is already live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitBehavior {
    /// Park the spawn until a slot frees (Grok default).
    Queue,
    /// Fail the spawn immediately.
    Fail,
}

/// Admission + queue knobs (Grok `CoordinatorConfig` / `SubagentLimits`).
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    pub max_concurrent: usize,
    pub behavior: LimitBehavior,
    /// Queued-for-slot wait only. `Duration::ZERO` = wait forever.
    pub foreground_budget: Duration,
}

impl CoordinatorConfig {
    pub fn from_env(max_concurrent: usize) -> Self {
        Self {
            max_concurrent: max_concurrent.max(1),
            behavior: limit_behavior_from_env(),
            foreground_budget: Duration::from_millis(foreground_budget_ms()),
        }
    }
}

/// Admission-only wait budget in ms. `0` / `off` = wait forever for a slot.
/// Default 1000 (Grok `foreground_budget` — queued-for-slot, not child runtime).
pub fn foreground_budget_ms() -> u64 {
    match std::env::var("ONE_TASK_FOREGROUND_BUDGET_MS") {
        Ok(s) => {
            let t = s.trim().to_ascii_lowercase();
            if matches!(t.as_str(), "off" | "false" | "none") {
                return 0;
            }
            t.parse().unwrap_or(1_000)
        }
        Err(_) => 1_000,
    }
}

fn limit_behavior_from_env() -> LimitBehavior {
    match std::env::var("ONE_TASK_LIMIT_BEHAVIOR") {
        Ok(s) if matches!(s.trim().to_ascii_lowercase().as_str(), "fail" | "reject") => {
            LimitBehavior::Fail
        }
        _ => LimitBehavior::Queue,
    }
}

/// Payload the coordinator needs to launch a child later (may sit in the queue).
pub struct SpawnSpec {
    pub req: RunRequest,
    pub provider: Arc<dyn LlmProvider>,
    pub opts: HarnessOptions,
    pub agent_name: String,
    pub description: Option<String>,
    pub background: bool,
    pub spawn_opts: SpawnOptions,
    pub session_id: Option<String>,
    pub prompt: String,
    pub cwd: Option<String>,
}

struct PackedSpawn {
    spec: SpawnSpec,
    control: RunControl,
    abort: Arc<AtomicBool>,
}

/// Reply to `task` after admission (and after the child ends, if foreground).
#[derive(Debug)]
pub struct SpawnReply {
    pub job_id: String,
    pub backgrounded: bool,
    pub queued: bool,
    pub rejected: bool,
    pub result: Option<RunResult>,
}

impl SpawnReply {
    fn started(job_id: String, queued: bool, backgrounded: bool) -> Self {
        Self {
            job_id,
            backgrounded,
            queued,
            rejected: false,
            result: None,
        }
    }

    fn finished(job_id: String, result: RunResult) -> Self {
        Self {
            job_id,
            backgrounded: false,
            queued: false,
            rejected: false,
            result: Some(result),
        }
    }

    fn rejected(message: impl Into<String>) -> Self {
        let msg = message.into();
        let mut rr = RunResult::failure(ProtocolError::new(error_code::SPAWN_NOT_ALLOWED, msg), 0)
            .with_status(TaskExitStatus::RuntimeError);
        rr.stop_reason = Some("admission_rejected".into());
        Self {
            job_id: String::new(),
            backgrounded: false,
            queued: false,
            rejected: true,
            result: Some(rr),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CoordinatorCounts {
    pub active: usize,
    pub queued: usize,
}

enum Event {
    Spawn {
        spec: SpawnSpec,
        reply: oneshot::Sender<SpawnReply>,
    },
    Resize {
        max_concurrent: usize,
    },
    Counts {
        reply: oneshot::Sender<CoordinatorCounts>,
    },
    Shutdown,
}

enum QueuedCaller {
    Awaiting {
        reply: oneshot::Sender<SpawnReply>,
        deadline: Option<Instant>,
    },
    Backgrounded,
}

struct QueuedSpawn {
    packed: PackedSpawn,
    job_id: String,
    caller: QueuedCaller,
}

/// Independent tokio actor: one parent session, one coordinator.
pub struct SubagentCoordinator {
    jobs: Arc<AgentJobRegistry>,
    config: Arc<Mutex<CoordinatorConfig>>,
    tx: Mutex<Option<mpsc::UnboundedSender<Event>>>,
}

impl SubagentCoordinator {
    pub fn new(jobs: Arc<AgentJobRegistry>, config: CoordinatorConfig) -> Arc<Self> {
        Arc::new(Self {
            jobs,
            config: Arc::new(Mutex::new(config)),
            tx: Mutex::new(None),
        })
    }

    fn sender(&self) -> Result<mpsc::UnboundedSender<Event>, String> {
        {
            let guard = self.tx.lock().expect("coordinator tx");
            if let Some(tx) = guard.as_ref() {
                return Ok(tx.clone());
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let (finish_tx, finish_rx) = mpsc::unbounded_channel();
        self.jobs.set_finish_sink(finish_tx);
        let config = self.config.lock().expect("coordinator config").clone();
        let actor = Actor {
            jobs: self.jobs.clone(),
            config,
            active: HashSet::new(),
            queued: VecDeque::new(),
            awaiting_finish: HashMap::new(),
        };
        tokio::spawn(actor.run(rx, finish_rx));
        *self.tx.lock().expect("coordinator tx") = Some(tx.clone());
        Ok(tx)
    }

    pub async fn spawn(&self, spec: SpawnSpec) -> SpawnReply {
        let (reply_tx, reply_rx) = oneshot::channel();
        let tx = match self.sender() {
            Ok(tx) => tx,
            Err(e) => return SpawnReply::rejected(e),
        };
        if tx
            .send(Event::Spawn {
                spec,
                reply: reply_tx,
            })
            .is_err()
        {
            return SpawnReply::rejected("subagent coordinator is shut down");
        }
        reply_rx
            .await
            .unwrap_or_else(|_| SpawnReply::rejected("subagent coordinator dropped the spawn"))
    }

    pub fn resize(&self, max_concurrent: usize) {
        let max_concurrent = max_concurrent.max(1);
        self.config
            .lock()
            .expect("coordinator config")
            .max_concurrent = max_concurrent;
        if let Ok(tx) = self.sender() {
            let _ = tx.send(Event::Resize { max_concurrent });
        }
    }

    pub async fn counts(&self) -> CoordinatorCounts {
        let (reply_tx, reply_rx) = oneshot::channel();
        if let Ok(tx) = self.sender() {
            if tx.send(Event::Counts { reply: reply_tx }).is_ok() {
                if let Ok(c) = reply_rx.await {
                    return c;
                }
            }
        }
        CoordinatorCounts::default()
    }

    pub fn shutdown(&self) {
        if let Ok(guard) = self.tx.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(Event::Shutdown);
            }
        }
    }
}

struct Actor {
    jobs: Arc<AgentJobRegistry>,
    config: CoordinatorConfig,
    active: HashSet<String>,
    queued: VecDeque<QueuedSpawn>,
    awaiting_finish: HashMap<String, oneshot::Sender<SpawnReply>>,
}

impl Actor {
    async fn run(
        mut self,
        mut rx: mpsc::UnboundedReceiver<Event>,
        mut finish_rx: mpsc::UnboundedReceiver<String>,
    ) {
        let mut ticker = tokio::time::interval(Duration::from_millis(50));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        None | Some(Event::Shutdown) => break,
                        Some(Event::Spawn { spec, reply }) => self.handle_spawn(spec, reply),
                        Some(Event::Resize { max_concurrent }) => {
                            self.config.max_concurrent = max_concurrent.max(1);
                            self.start_queued_within_capacity();
                        }
                        Some(Event::Counts { reply }) => {
                            let _ = reply.send(CoordinatorCounts {
                                active: self.active.len(),
                                queued: self.queued.len(),
                            });
                        }
                    }
                }
                fin = finish_rx.recv() => {
                    match fin {
                        Some(job_id) => self.handle_finished(&job_id),
                        None => {}
                    }
                }
                _ = ticker.tick() => self.hand_off_expired(),
            }
        }
    }

    fn handle_spawn(&mut self, spec: SpawnSpec, reply: oneshot::Sender<SpawnReply>) {
        let max_turns = spec.req.agent.max_turns.unwrap_or(16) as u64;
        let background = spec.background;
        let (job_id, control, abort) = self.jobs.register_job(
            &spec.agent_name,
            spec.description.as_deref(),
            &spec.opts,
            max_turns,
            &spec.spawn_opts,
            true,
        );
        self.jobs.attach_resume_meta(
            &job_id,
            spec.session_id.clone(),
            spec.prompt.clone(),
            spec.cwd.clone(),
        );
        let packed = PackedSpawn {
            spec,
            control,
            abort,
        };

        if self.active.len() < self.config.max_concurrent {
            self.start_child(job_id, packed, Some(reply));
            return;
        }

        match self.config.behavior {
            LimitBehavior::Fail => {
                let _ = self.jobs.kill_with_reason(&job_id, KillReason::JobKill);
                let _ = reply.send(SpawnReply::rejected(format!(
                    "admission rejected: {} live subagents (max_concurrent={})",
                    self.active.len(),
                    self.config.max_concurrent
                )));
            }
            LimitBehavior::Queue => {
                let deadline = if !background && !self.config.foreground_budget.is_zero() {
                    Some(Instant::now() + self.config.foreground_budget)
                } else {
                    None
                };
                let caller = if background {
                    let _ = reply.send(SpawnReply::started(job_id.clone(), true, false));
                    QueuedCaller::Backgrounded
                } else {
                    QueuedCaller::Awaiting { reply, deadline }
                };
                self.queued.push_back(QueuedSpawn {
                    packed,
                    job_id,
                    caller,
                });
            }
        }
    }

    fn start_child(
        &mut self,
        job_id: String,
        packed: PackedSpawn,
        reply: Option<oneshot::Sender<SpawnReply>>,
    ) {
        let background = packed.spec.background;
        self.jobs.launch_registered(
            job_id.clone(),
            packed.spec.req,
            packed.spec.provider,
            packed.spec.opts,
            packed.control,
            packed.abort,
            packed.spec.spawn_opts,
        );
        self.active.insert(job_id.clone());
        match (background, reply) {
            (true, Some(reply)) => {
                let _ = reply.send(SpawnReply::started(job_id, false, false));
            }
            (false, Some(reply)) => {
                self.awaiting_finish.insert(job_id, reply);
            }
            (_, None) => {}
        }
    }

    fn handle_finished(&mut self, job_id: &str) {
        self.active.remove(job_id);
        if let Some(idx) = self.queued.iter().position(|q| q.job_id == job_id) {
            if let Some(q) = self.queued.remove(idx) {
                if let QueuedCaller::Awaiting { reply, .. } = q.caller {
                    send_finish(&self.jobs, job_id, reply);
                }
            }
        }
        if let Some(reply) = self.awaiting_finish.remove(job_id) {
            send_finish(&self.jobs, job_id, reply);
        }
        self.start_queued_within_capacity();
    }

    fn hand_off_expired(&mut self) {
        let now = Instant::now();
        for q in &mut self.queued {
            let expired = matches!(
                &q.caller,
                QueuedCaller::Awaiting {
                    deadline: Some(d),
                    ..
                } if *d <= now
            );
            if !expired {
                continue;
            }
            if let QueuedCaller::Awaiting { reply, .. } =
                std::mem::replace(&mut q.caller, QueuedCaller::Backgrounded)
            {
                self.jobs.set_notify_completion(&q.job_id, true);
                self.jobs.push_event(
                    &q.job_id,
                    "▸ queued · handed off to background (admission wait)",
                );
                let _ = reply.send(SpawnReply::started(q.job_id.clone(), true, true));
            }
        }
    }

    fn start_queued_within_capacity(&mut self) {
        while self.active.len() < self.config.max_concurrent {
            let Some(q) = self.queued.pop_front() else {
                break;
            };
            let live = self
                .jobs
                .get(&q.job_id)
                .is_some_and(|s| matches!(s.state, JobState::Queued));
            if !live {
                if let QueuedCaller::Awaiting { reply, .. } = q.caller {
                    send_finish(&self.jobs, &q.job_id, reply);
                }
                continue;
            }
            match q.caller {
                QueuedCaller::Awaiting { reply, .. } => {
                    self.start_child(q.job_id, q.packed, Some(reply));
                }
                QueuedCaller::Backgrounded => {
                    self.start_child(q.job_id, q.packed, None);
                }
            }
        }
    }
}

fn send_finish(jobs: &AgentJobRegistry, job_id: &str, reply: oneshot::Sender<SpawnReply>) {
    let result = jobs.take_result_clone(job_id);
    let _ = reply.send(match result {
        Some(r) => SpawnReply::finished(job_id.to_string(), r),
        None => SpawnReply::rejected("job ended without a result"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AgentSpec;
    use async_trait::async_trait;
    use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider, TokenUsage};
    use one_core::error::Result as CoreResult;
    use one_core::message::{ContentBlock, StopReason};
    use tokio::sync::Notify;

    struct HoldProvider {
        hold: Arc<Notify>,
        released: Arc<Notify>,
    }

    #[async_trait]
    impl LlmProvider for HoldProvider {
        fn name(&self) -> &str {
            "hold"
        }
        fn model(&self) -> &str {
            "hold-v1"
        }
        async fn complete(&self, _request: CompletionRequest) -> CoreResult<CompletionResponse> {
            self.released.notify_one();
            self.hold.notified().await;
            Ok(CompletionResponse {
                provider: "hold".into(),
                model: "hold-v1".into(),
                content: vec![ContentBlock::Text {
                    text: "held-done".into(),
                }],
                stop_reason: StopReason::Stop,
                usage: TokenUsage::default(),
                citations: Vec::new(),
            })
        }
    }

    fn spec(prompt: &str, background: bool, provider: Arc<dyn LlmProvider>) -> SpawnSpec {
        let mut req = RunRequest::new(AgentSpec::builtin_explore(), prompt);
        req.agent.max_turns = Some(2);
        SpawnSpec {
            req,
            provider,
            opts: HarnessOptions::from_cwd(std::env::temp_dir()),
            agent_name: "explore".into(),
            description: Some(prompt.into()),
            background,
            spawn_opts: SpawnOptions {
                notify_completion: background,
                apply_wall_timeout: false,
                trace: None,
                trace_meta: None,
                acquire_slot: None,
            },
            session_id: None,
            prompt: prompt.into(),
            cwd: None,
        }
    }

    fn coord(
        max: usize,
        budget_ms: u64,
        behavior: LimitBehavior,
    ) -> (Arc<SubagentCoordinator>, Arc<AgentJobRegistry>) {
        let jobs = AgentJobRegistry::new(Arc::new(std::sync::Mutex::new(Vec::new())));
        let c = SubagentCoordinator::new(
            jobs.clone(),
            CoordinatorConfig {
                max_concurrent: max,
                behavior,
                foreground_budget: Duration::from_millis(budget_ms),
            },
        );
        (c, jobs)
    }

    #[tokio::test]
    async fn background_enqueues_instead_of_blocking() {
        let (c, jobs) = coord(1, 1_000, LimitBehavior::Queue);
        let hold = Arc::new(Notify::new());
        let released = Arc::new(Notify::new());
        let p1: Arc<dyn LlmProvider> = Arc::new(HoldProvider {
            hold: hold.clone(),
            released: released.clone(),
        });
        let p2 = Arc::new(one_ai::MockProvider::new());

        let first = c.spawn(spec("first", true, p1)).await;
        assert!(!first.queued, "first should start: {}", first.job_id);
        released.notified().await;

        let second = c.spawn(spec("second", true, p2)).await;
        assert_eq!(
            second.queued, true,
            "second must park, not block the caller"
        );
        assert_eq!(second.backgrounded, false);
        assert_eq!(jobs.get(&second.job_id).unwrap().state, JobState::Queued);

        let counts = c.counts().await;
        assert_eq!(counts.active, 1);
        assert_eq!(counts.queued, 1);

        hold.notify_one();
        let _ = jobs.wait_until_done(&first.job_id).await;
        let _ = jobs.wait_until_done(&second.job_id).await;
        assert!(jobs.get(&second.job_id).unwrap().state.is_terminal());
    }

    #[tokio::test]
    async fn foreground_deadline_hands_off_without_starting_child() {
        let (c, jobs) = coord(1, 40, LimitBehavior::Queue);
        let hold = Arc::new(Notify::new());
        let released = Arc::new(Notify::new());
        let p1: Arc<dyn LlmProvider> = Arc::new(HoldProvider {
            hold: hold.clone(),
            released: released.clone(),
        });
        let p2 = Arc::new(one_ai::MockProvider::new());

        let first = c.spawn(spec("holder", true, p1)).await;
        released.notified().await;

        let second = c.spawn(spec("waiter", false, p2)).await;
        assert!(
            second.backgrounded && second.queued && second.result.is_none(),
            "queued foreground must auto-bg: job={} bg={} queued={} result={:?}",
            second.job_id,
            second.backgrounded,
            second.queued,
            second.result.as_ref().map(|r| r.status)
        );
        assert_eq!(jobs.get(&second.job_id).unwrap().state, JobState::Queued);
        assert_eq!(jobs.get(&first.job_id).unwrap().state, JobState::Running);

        hold.notify_one();
        let _ = jobs.wait_until_done(&first.job_id).await;
        let _ = jobs.wait_until_done(&second.job_id).await;
        assert!(jobs.get(&second.job_id).unwrap().state.is_terminal());
    }

    #[tokio::test]
    async fn foreground_with_free_slot_waits_for_result() {
        let (c, _) = coord(2, 40, LimitBehavior::Queue);
        let reply = c
            .spawn(spec("sync", false, Arc::new(one_ai::MockProvider::new())))
            .await;
        assert!(!reply.backgrounded);
        assert!(!reply.queued);
        let result = reply.result.expect("foreground result");
        assert!(
            result.ok || result.status == Some(TaskExitStatus::IncompleteInfo),
            "{:?}",
            result.status
        );
    }

    #[tokio::test]
    async fn fail_behavior_rejects_when_full() {
        let (c, jobs) = coord(1, 1_000, LimitBehavior::Fail);
        let hold = Arc::new(Notify::new());
        let released = Arc::new(Notify::new());
        let p1: Arc<dyn LlmProvider> = Arc::new(HoldProvider {
            hold: hold.clone(),
            released: released.clone(),
        });
        let first = c.spawn(spec("holder", true, p1)).await;
        released.notified().await;

        let second = c
            .spawn(spec("nope", true, Arc::new(one_ai::MockProvider::new())))
            .await;
        assert!(
            second.rejected,
            "expected reject, got job={}",
            second.job_id
        );
        hold.notify_one();
        let _ = jobs.wait_until_done(&first.job_id).await;
    }
}
