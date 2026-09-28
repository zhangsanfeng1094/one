//! One in-process backend — the existing One harness (ReAct child).
//!
//! This is a **pure adapter**: it calls [`super::super::harness::run_with_control`]
//! with the exact same arguments the registry used before, and translates the
//! harness's [`one_core::events::AgentEvent`] stream into normalized
//! [`AgentTaskEvent`]s for the tracker. No behavior change versus the legacy
//! path — the registry previously invoked the harness directly; now it goes
//! through this backend with identical inputs and identical finalization.

use std::sync::Arc;

use async_trait::async_trait;
use one_core::events::AgentEvent;

use super::super::harness;
use super::{
    AgentBackend, AgentEventSink, AgentTaskEvent, BackendControl, BackendKind, BackendSpawn,
};
use crate::protocol::RunResult;

pub struct OneInternalBackend;

impl OneInternalBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for OneInternalBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Translate a harness `AgentEvent` into 0..n normalized events.
/// Reasoning deltas (`ThinkingDelta`) update activity only — never stored.
pub fn normalize_internal_event(event: &AgentEvent) -> Vec<AgentTaskEvent> {
    match event {
        AgentEvent::AgentStart => vec![AgentTaskEvent::Started {
            session_id: None,
            process_id: None,
        }],
        AgentEvent::TurnStart { turn } => vec![AgentTaskEvent::TurnStarted {
            turn: *turn as u64 + 1,
        }],
        AgentEvent::TurnEnd { turn, .. } => vec![AgentTaskEvent::TurnCompleted {
            turn: *turn as u64 + 1,
        }],
        AgentEvent::TextDelta { delta } => vec![AgentTaskEvent::TextActivity {
            delta_chars: delta.chars().count(),
        }],
        AgentEvent::ThinkingDelta { .. } => vec![AgentTaskEvent::ToolProgress {
            tool_call_id: None,
            note: "thinking".into(),
        }],
        AgentEvent::ToolExecutionStart { tool_call } => vec![AgentTaskEvent::ToolStarted {
            tool_call_id: tool_call.id.clone(),
            tool: tool_call.name.clone(),
            title: tool_brief(tool_call),
        }],
        AgentEvent::ToolExecutionEnd {
            tool_call,
            is_error,
            output,
        } => vec![AgentTaskEvent::ToolCompleted {
            tool_call_id: tool_call.id.clone(),
            tool: tool_call.name.clone(),
            is_error: *is_error,
            note: brief_output(output.as_text()),
        }],
        AgentEvent::RetryScheduled { reason, .. } => vec![AgentTaskEvent::ToolProgress {
            tool_call_id: None,
            note: format!("retry: {reason}"),
        }],
        AgentEvent::RetryStarted { .. } => vec![AgentTaskEvent::ToolProgress {
            tool_call_id: None,
            note: "retry started".into(),
        }],
        AgentEvent::ServerTool { tool, status, .. } => match status {
            one_core::ServerToolStatus::Started => vec![AgentTaskEvent::ToolStarted {
                tool_call_id: format!("server_{tool:?}"),
                tool: tool.as_str().to_string(),
                title: None,
            }],
            one_core::ServerToolStatus::Completed | one_core::ServerToolStatus::Failed => {
                vec![AgentTaskEvent::ToolCompleted {
                    tool_call_id: format!("server_{tool:?}"),
                    tool: tool.as_str().to_string(),
                    is_error: matches!(status, one_core::ServerToolStatus::Failed),
                    note: String::new(),
                }]
            }
        },
        AgentEvent::UsageUpdate { .. } => vec![],
        AgentEvent::WaitParkStart { mode, ids } => vec![AgentTaskEvent::ToolProgress {
            tool_call_id: None,
            note: format!("waiting · {} task(s) ({})", ids.len(), mode.as_str()),
        }],
        AgentEvent::WaitParkEnd => vec![AgentTaskEvent::ToolProgress {
            tool_call_id: None,
            note: "wait ended".into(),
        }],
        AgentEvent::SteerApplied { text } => vec![AgentTaskEvent::ToolProgress {
            tool_call_id: None,
            note: format!("steer applied: {}", brief_output(text.clone())),
        }],
        AgentEvent::CompactionStart | AgentEvent::CompactionEnd { .. } => {
            vec![AgentTaskEvent::ToolProgress {
                tool_call_id: None,
                note: "compacting".into(),
            }]
        }
        // AgentEnd is not terminal here: the harness RunResult is the terminal
        // authority (the registry finalizes from it).
        AgentEvent::AgentEnd { .. } => vec![],
    }
}

fn brief_output(text: String) -> String {
    let t = text.replace('\n', " ");
    let t = t.trim();
    if t.chars().count() <= 48 {
        t.to_string()
    } else {
        let cut: String = t.chars().take(47).collect();
        format!("{cut}…")
    }
}

fn tool_brief(call: &one_core::tool::ToolCall) -> Option<String> {
    let raw = match call.name.as_str() {
        "read" | "write" | "edit" => call
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "bash" => call
            .arguments
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "grep" => call
            .arguments
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        _ => "",
    };
    if raw.is_empty() {
        None
    } else {
        let brief: String = raw.replace('\n', " ").chars().take(48).collect();
        Some(brief)
    }
}

#[async_trait]
impl AgentBackend for OneInternalBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::OneInternal
    }

    fn label(&self) -> &'static str {
        "one-internal"
    }

    async fn run(
        &self,
        spawn: BackendSpawn,
        control: BackendControl,
        sink: Arc<dyn AgentEventSink>,
    ) -> RunResult {
        let Some(provider) = spawn.provider.clone() else {
            return crate::protocol::RunResult::failure(
                crate::protocol::ProtocolError::new(
                    crate::protocol::error_code::INTERNAL,
                    "one-internal backend requires a bound LLM provider",
                ),
                0,
            )
            .with_status(crate::protocol::TaskExitStatus::RuntimeError);
        };
        // Identical to the legacy registry path: harness::run_with_control
        // with abort + the registry's live event log.
        let result = harness::run_with_control(
            spawn.req,
            provider.as_ref(),
            &spawn.opts,
            harness::RunControl {
                abort: Some(control.abort.clone()),
                turn_progress: None,
                event_log: Some(control.event_log.clone()),
                trace: None,
                trace_meta: None,
            },
        )
        .await;
        // Terminal event from the result itself (AgentEnd is not terminal —
        // the RunResult is the authority).
        if result.ok {
            sink.on_event(AgentTaskEvent::Completed {
                result_text: result.result.clone(),
                turns: result.turns,
            });
        } else {
            sink.on_event(AgentTaskEvent::Failed {
                message: result
                    .error
                    .as_ref()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "harness run failed".into()),
            });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_events_normalize() {
        use one_core::tool::ToolCall;
        let evs = normalize_internal_event(&AgentEvent::AgentStart);
        assert_eq!(
            evs,
            vec![AgentTaskEvent::Started {
                session_id: None,
                process_id: None
            }]
        );

        let evs = normalize_internal_event(&AgentEvent::TurnStart { turn: 0 });
        assert_eq!(evs, vec![AgentTaskEvent::TurnStarted { turn: 1 }]);

        let evs = normalize_internal_event(&AgentEvent::ToolExecutionStart {
            tool_call: ToolCall {
                id: "c1".into(),
                name: "grep".into(),
                arguments: serde_json::json!({"pattern": "auth"}),
            },
        });
        assert!(
            matches!(&evs[0], AgentTaskEvent::ToolStarted { tool, title, .. } if tool == "grep" && title.as_deref() == Some("auth"))
        );

        let evs = normalize_internal_event(&AgentEvent::ThinkingDelta {
            delta: "HIDDEN CHAIN".into(),
        });
        assert_eq!(
            evs,
            vec![AgentTaskEvent::ToolProgress {
                tool_call_id: None,
                note: "thinking".into()
            }]
        );
        let dump = format!("{evs:?}");
        assert!(!dump.contains("HIDDEN"), "thought leaked: {dump}");
    }

    #[tokio::test]
    async fn internal_backend_runs_harness_without_regression() {
        use crate::runtime::agent_backend::CollectorSink;
        use crate::runtime::jobs::JobEventLog;
        use std::sync::atomic::AtomicBool;

        let backend = OneInternalBackend::new();
        let spec = crate::protocol::AgentSpec::builtin_explore();
        let mut req = crate::protocol::RunRequest::new(spec.clone(), "one-line summary");
        req.session.mode = crate::protocol::SessionMode::Ephemeral;
        let spawn = BackendSpawn {
            req,
            opts: crate::runtime::harness::HarnessOptions::from_cwd(std::env::temp_dir()),
            agent_name: "explore".into(),
            description: Some("probe".into()),
            child_spec: spec,
            provider: Some(Arc::new(one_ai::MockProvider::new())),
        };
        let sink = CollectorSink::new();
        let result = backend
            .run(
                spawn,
                BackendControl {
                    abort: Arc::new(AtomicBool::new(false)),
                    event_log: JobEventLog::new(),
                    wall_timeout: None,
                },
                sink.clone(),
            )
            .await;
        assert!(result.ok, "{:?}", result.error);
        let events = sink.snapshot();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentTaskEvent::Completed { .. })));
    }
}
