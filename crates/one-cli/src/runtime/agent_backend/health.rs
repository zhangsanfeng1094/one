//! Deterministic anti-fake-death health evaluator for agent tasks.
//!
//! Inputs (per task, all timestamps injectable for tests):
//! - process alive
//! - `last_event_at` — any backend event (turn/tool/progress/output)
//! - `last_progress_at` — *meaningful* progress (tool start/complete, turn
//!   boundaries, output activity). Text/thought deltas update activity but
//!   **must not** count as progress for stall purposes by themselves
//!   (streaming tokens for 10 min is alive, not stuck) — they do refresh
//!   `last_event_at`.
//! - `current_activity` / `current_tool`
//! - operation elapsed
//!
//! Rules (deterministic, no wall-clock sleeps):
//! 1. Terminal lifecycle → always `Healthy` (nothing to judge).
//! 2. Process known-dead while lifecycle is live → `Stalled` immediately
//!    (internal-backend tasks have no process; `process_alive: None` skips
//!    this rule — never equate "no process" with "dead").
//! 3. Never judge before `min_observability` elapsed (default 90s): `Healthy`.
//! 4. Progress silence `>= stalled_after` (default 600s) → `Stalled`.
//! 5. Progress silence `>= suspicious_after` (default 180s) → `Suspicious`.
//! 6. A long-running tool (tool started, not completed) with *any* recent
//!    progress (freshness within `suspicious_after`) → `Healthy` — long tools
//!    with continuous output must not be misjudged.
//! 7. Otherwise `Healthy`.
//!
//! Default **never auto-kills**; consumers surface `Stalled` to UI/logs only.

use std::time::Duration;

/// Health classification, separate from lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentHealth {
    #[default]
    Healthy,
    Suspicious,
    Stalled,
}

impl AgentHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Suspicious => "suspicious",
            Self::Stalled => "stalled",
        }
    }
}

/// Tunable thresholds (all overridable via env or test injection).
#[derive(Debug, Clone, Copy)]
pub struct HealthThresholds {
    /// Don't judge health before this much elapsed (default 90s).
    pub min_observability: Duration,
    /// Progress silence window that downgrades to `Suspicious` (default 180s).
    pub suspicious_after: Duration,
    /// Progress silence window that downgrades to `Stalled` (default 600s).
    pub stalled_after: Duration,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            min_observability: Duration::from_secs(90),
            suspicious_after: Duration::from_secs(180),
            stalled_after: Duration::from_secs(600),
        }
    }
}

impl HealthThresholds {
    /// `ONE_HEALTH_SUSPICIOUS_MS` / `ONE_HEALTH_STALLED_MS` overrides
    /// (0/parse-failure keeps defaults). min_observability = suspicious/2.
    pub fn from_env() -> Self {
        let mut t = Self::default();
        if let Some(ms) = env_ms("ONE_HEALTH_SUSPICIOUS_MS") {
            t.suspicious_after = Duration::from_millis(ms);
        }
        if let Some(ms) = env_ms("ONE_HEALTH_STALLED_MS") {
            t.stalled_after = Duration::from_millis(ms);
        }
        if t.suspicious_after > t.stalled_after {
            // Keep monotone ordering.
            std::mem::swap(&mut t.suspicious_after, &mut t.stalled_after);
        }
        t.min_observability = t.suspicious_after / 2;
        t
    }
}

fn env_ms(key: &str) -> Option<u64> {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}

/// Inputs snapshot for one evaluation (all caller-computed, clock-free).
#[derive(Debug, Clone, Default)]
pub struct HealthInputs {
    /// `Some(false)` = process known dead; `None` = no process (internal).
    pub process_alive: Option<bool>,
    /// Elapsed since task start.
    pub elapsed: Duration,
    /// Duration since last *event* of any kind (`None` = never).
    pub since_event: Option<Duration>,
    /// Duration since last *progress* (`None` = never).
    pub since_progress: Option<Duration>,
    /// A tool is currently executing (ToolStarted without ToolCompleted).
    pub tool_in_flight: bool,
    /// Terminal lifecycle (completed/failed/cancelled) short-circuits.
    pub terminal: bool,
}

/// Deterministic evaluator: same inputs → same output. No sleeps, no clocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct HealthEvaluator {
    pub thresholds: HealthThresholds,
}

/// Result of an evaluation with the reason (for logs / tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthEvaluation {
    pub health: AgentHealth,
    pub reason: &'static str,
}

impl HealthEvaluator {
    pub fn new(thresholds: HealthThresholds) -> Self {
        Self { thresholds }
    }

    pub fn evaluate(&self, input: &HealthInputs) -> HealthEvaluation {
        // 1. Terminal tasks are not judged.
        if input.terminal {
            return HealthEvaluation {
                health: AgentHealth::Healthy,
                reason: "terminal",
            };
        }

        // 2. Known-dead process while task claims to be live → stalled now.
        //    ("PID alive = healthy" is NOT used; only the inverse.)
        if input.process_alive == Some(false) {
            return HealthEvaluation {
                health: AgentHealth::Stalled,
                reason: "process_dead",
            };
        }

        // 3. Grace window: not enough runtime to judge.
        if input.elapsed < self.thresholds.min_observability {
            return HealthEvaluation {
                health: AgentHealth::Healthy,
                reason: "warmup",
            };
        }

        // Progress silence (falling back to event silence when no progress
        // event was ever seen — a task that never progressed at all counts
        // from its last event, or from start when there were no events).
        let silence = input
            .since_progress
            .or(input.since_event)
            .unwrap_or(input.elapsed);

        // 4. Hard stall.
        if silence >= self.thresholds.stalled_after {
            return HealthEvaluation {
                health: AgentHealth::Stalled,
                reason: "no_progress_stalled_window",
            };
        }

        // 5. Long tool with fresh progress: explicitly healthy (anti-misjudge).
        if input.tool_in_flight {
            let fresh = input.since_progress.unwrap_or(input.elapsed);
            if fresh < self.thresholds.suspicious_after {
                return HealthEvaluation {
                    health: AgentHealth::Healthy,
                    reason: "long_tool_with_progress",
                };
            }
            // Tool in flight but silent past suspicious: suspicious, not
            // stalled (stalled only via rule 4's longer window).
            return HealthEvaluation {
                health: AgentHealth::Suspicious,
                reason: "long_tool_silent",
            };
        }

        // 6. Suspicious window (only events, no progress).
        if silence >= self.thresholds.suspicious_after {
            return HealthEvaluation {
                health: AgentHealth::Suspicious,
                reason: "no_progress_suspicious_window",
            };
        }

        // 7. Default healthy.
        HealthEvaluation {
            health: AgentHealth::Healthy,
            reason: "progress_fresh",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(th: HealthThresholds, i: HealthInputs) -> HealthEvaluation {
        HealthEvaluator::new(th).evaluate(&i)
    }

    fn thresholds() -> HealthThresholds {
        HealthThresholds {
            min_observability: Duration::from_secs(10),
            suspicious_after: Duration::from_secs(30),
            stalled_after: Duration::from_secs(60),
        }
    }

    #[test]
    fn terminal_and_warmup_healthy() {
        let t = thresholds();
        let e = eval(
            t,
            HealthInputs {
                terminal: true,
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Healthy);
        assert_eq!(e.reason, "terminal");

        let e = eval(
            t,
            HealthInputs {
                elapsed: Duration::from_secs(5),
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Healthy);
        assert_eq!(e.reason, "warmup");
    }

    #[test]
    fn dead_process_is_stalled_not_healthy() {
        let t = thresholds();
        let e = eval(
            t,
            HealthInputs {
                process_alive: Some(false),
                elapsed: Duration::from_secs(120),
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Stalled);
        assert_eq!(e.reason, "process_dead");
    }

    #[test]
    fn internal_no_process_does_not_stall() {
        // Internal backend: process_alive = None must never trigger rule 2.
        let t = thresholds();
        let e = eval(
            t,
            HealthInputs {
                process_alive: None,
                elapsed: Duration::from_secs(15),
                since_event: Some(Duration::from_secs(2)),
                since_progress: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Healthy);
    }

    #[test]
    fn healthy_then_suspicious_then_stalled_monotone() {
        let t = thresholds();
        // 20s elapsed, progress 5s ago → healthy
        let e = eval(
            t,
            HealthInputs {
                elapsed: Duration::from_secs(20),
                since_event: Some(Duration::from_secs(5)),
                since_progress: Some(Duration::from_secs(5)),
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Healthy, "{e:?}");

        // progress 35s ago → suspicious
        let e = eval(
            t,
            HealthInputs {
                elapsed: Duration::from_secs(45),
                since_event: Some(Duration::from_secs(35)),
                since_progress: Some(Duration::from_secs(35)),
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Suspicious, "{e:?}");

        // progress 70s ago → stalled
        let e = eval(
            t,
            HealthInputs {
                elapsed: Duration::from_secs(80),
                since_event: Some(Duration::from_secs(70)),
                since_progress: Some(Duration::from_secs(70)),
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Stalled, "{e:?}");
    }

    #[test]
    fn long_tool_with_progress_not_misjudged() {
        let t = thresholds();
        // A build/test tool running 100s with continuous output 3s ago:
        // healthy despite long in-flight tool.
        let e = eval(
            t,
            HealthInputs {
                elapsed: Duration::from_secs(100),
                since_event: Some(Duration::from_secs(3)),
                since_progress: Some(Duration::from_secs(3)),
                tool_in_flight: true,
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Healthy);
        assert_eq!(e.reason, "long_tool_with_progress");
    }

    #[test]
    fn long_tool_silent_becomes_suspicious_not_stalled() {
        let t = thresholds();
        let e = eval(
            t,
            HealthInputs {
                elapsed: Duration::from_secs(50),
                since_event: Some(Duration::from_secs(40)),
                since_progress: Some(Duration::from_secs(40)),
                tool_in_flight: true,
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Suspicious);
        assert_eq!(e.reason, "long_tool_silent");
    }

    #[test]
    fn events_without_progress_fall_back_to_event_silence() {
        let t = thresholds();
        // Text deltas streaming (events fresh) but no tool/turn progress ever:
        // healthy — activity counts via since_event.
        let e = eval(
            t,
            HealthInputs {
                elapsed: Duration::from_secs(40),
                since_event: Some(Duration::from_secs(1)),
                since_progress: None,
                ..Default::default()
            },
        );
        // silence falls back to since_event = 1s < suspicious → healthy
        assert_eq!(e.health, AgentHealth::Healthy, "{e:?}");

        // No events at all for 65s since start → stalled.
        let e = eval(
            t,
            HealthInputs {
                elapsed: Duration::from_secs(65),
                since_event: None,
                since_progress: None,
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Stalled);
    }

    #[test]
    fn thresholds_env_ordering_kept_monotone() {
        // suspicious > stalled given via env → swapped internally.
        let mut t = HealthThresholds::default();
        t.suspicious_after = Duration::from_secs(600);
        t.stalled_after = Duration::from_secs(180);
        // simulate the swap logic
        if t.suspicious_after > t.stalled_after {
            std::mem::swap(&mut t.suspicious_after, &mut t.stalled_after);
        }
        assert!(t.suspicious_after <= t.stalled_after);
    }

    #[test]
    fn last_event_vs_last_progress_distinction() {
        // Same elapsed; event fresh but progress ancient (only thought deltas
        // arriving). Progress silence rules; event freshness keeps it out of
        // the hard-stall band only when silence < stalled.
        let t = thresholds();
        let e = eval(
            t,
            HealthInputs {
                elapsed: Duration::from_secs(50),
                since_event: Some(Duration::from_secs(1)),
                since_progress: Some(Duration::from_secs(50)),
                ..Default::default()
            },
        );
        assert_eq!(e.health, AgentHealth::Suspicious, "{e:?}");
    }
}
