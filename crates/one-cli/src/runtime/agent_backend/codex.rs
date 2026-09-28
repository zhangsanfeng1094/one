//! Codex CLI backend — `codex exec --json`.
//!
//! Args verified against `codex-cli 0.156.1` (`codex exec --help` on this
//! machine, 2026-09-26):
//! - `codex exec --json` prints events to stdout as JSONL
//! - `-C/--cd <DIR>` working root, `-s/--sandbox read-only|workspace-write|danger-full-access`
//! - `-c key=value` config overrides, `--skip-git-repo-check`
//! - `-o/--output-last-message <FILE>` last agent message file
//! - `--color never` to keep stdout clean
//!
//! Observed event shapes (from a live run):
//! `{"type":"thread.started","thread_id":"…"}` · `{"type":"turn.started"}`
//! · `{"type":"item.started","item":{…}}` · `{"type":"item.completed","item":{…}}`
//! · `{"type":"item.updated",…}` · `{"type":"turn.completed",…}`
//! · `{"type":"turn.failed","error":{"message":"…"}}` · `{"type":"error","message":"…"}`.
//!
//! Items seen inside `item.completed` include `reasoning` (encrypted /
//! reasoning text — **never stored**, only activity), `agent_message` (text),
//! `command_execution` (tool), `file_change` (tool), `mcp_tool_call`, `todo_list`,
//! `web_search`.
//!
//! Reasoning policy: reasoning deltas/items update `last_event_at` /
//! `current_activity` only — the body is never retained or injected.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::process::{run_cli_backend, CliBackendCommand, CliRunOutcome};
use super::{
    AgentBackend, AgentEventSink, AgentTaskEvent, BackendControl, BackendKind, BackendSpawn,
};
use crate::protocol::{error_code, ProtocolError, RunResult, TaskExitStatus};

pub struct CodexCliBackend {
    /// Override the codex binary (tests inject `/bin/sh` scripts).
    pub program_override: Option<String>,
    /// Extra args injected before the positional prompt.
    pub extra_args: Vec<String>,
    /// Env additions.
    pub env: Vec<(String, String)>,
}

impl CodexCliBackend {
    pub fn new() -> Self {
        Self {
            // `ONE_CODEX_BIN` lets tests (or users) point at a fake/alternate
            // binary; falls back to `codex` on PATH.
            program_override: std::env::var("ONE_CODEX_BIN")
                .ok()
                .filter(|s| !s.is_empty()),
            extra_args: vec![],
            env: vec![],
        }
    }

    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program_override: Some(program.into()),
            extra_args: vec![],
            env: vec![],
        }
    }
}

impl Default for CodexCliBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Map one parsed `codex exec --json` line to normalized events.
/// Unknown/malformed shapes return `[]` (runner counts them as unknown).
pub fn normalize_codex_event(v: &Value) -> Vec<AgentTaskEvent> {
    let Some(t) = v.get("type").and_then(|t| t.as_str()) else {
        return vec![];
    };
    match t {
        "thread.started" => vec![AgentTaskEvent::Started {
            session_id: v
                .get("thread_id")
                .and_then(|i| i.as_str())
                .map(str::to_string),
            process_id: None,
        }],
        "turn.started" => vec![AgentTaskEvent::TurnStarted { turn: 0 }],
        "turn.completed" => {
            // Codex emits usage here; final text arrives via agent_message item.
            vec![AgentTaskEvent::TurnCompleted { turn: 0 }]
        }
        "turn.failed" => {
            let msg = v
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("codex turn failed");
            vec![AgentTaskEvent::Failed {
                message: msg.to_string(),
            }]
        }
        "error" => {
            // Transient reconnect errors are not terminal on their own; codex
            // retries and eventually emits turn.failed/completed. Only treat
            // as terminal when no turn is in flight is undecidable per-line —
            // map to Failed only when message looks fatal? Keep it simple and
            // safe: non-terminal activity note (turn.failed is the terminal).
            vec![AgentTaskEvent::ToolProgress {
                tool_call_id: None,
                note: truncate(
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("codex error event"),
                    80,
                ),
            }]
        }
        "item.started" | "item.completed" | "item.updated" => {
            normalize_codex_item(t, v.get("item").unwrap_or(&Value::Null))
        }
        _ => vec![],
    }
}

fn normalize_codex_item(lifecycle: &str, item: &Value) -> Vec<AgentTaskEvent> {
    let kind = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let id = item
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("item")
        .to_string();
    match kind {
        // Reasoning: NEVER store the body. Activity only.
        "reasoning" => match lifecycle {
            "item.started" => vec![AgentTaskEvent::ToolProgress {
                tool_call_id: None,
                note: "reasoning".into(),
            }],
            _ => vec![AgentTaskEvent::OutputActivity { bytes: 0 }],
        },
        "agent_message" => match lifecycle {
            "item.completed" => {
                let text = item
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                vec![
                    AgentTaskEvent::TextActivity {
                        delta_chars: text.chars().count(),
                    },
                    AgentTaskEvent::Completed {
                        result_text: text,
                        turns: None,
                    },
                ]
            }
            _ => vec![AgentTaskEvent::TextActivity { delta_chars: 0 }],
        },
        // Tool-ish items.
        "command_execution"
        | "file_change"
        | "mcp_tool_call"
        | "mcp_tool_call_update"
        | "web_search"
        | "todo_list"
        | "gpu_status" => {
            let tool = pretty_tool(kind);
            let title = item
                .get("command")
                .or_else(|| item.get("tool"))
                .or_else(|| item.get("path"))
                .and_then(|c| c.as_str())
                .map(|s| truncate(s, 48));
            match lifecycle {
                "item.started" => vec![AgentTaskEvent::ToolStarted {
                    tool_call_id: id,
                    tool,
                    title,
                }],
                "item.updated" => vec![AgentTaskEvent::ToolProgress {
                    tool_call_id: Some(id),
                    note: title.unwrap_or_default(),
                }],
                _ => {
                    // item.completed
                    let is_error = item
                        .get("status")
                        .and_then(|s| s.as_str())
                        .map(|s| s == "failed")
                        .unwrap_or(false)
                        || item.get("exit_code").and_then(|c| c.as_i64()) == Some(127);
                    let note = item
                        .get("last_agent_message")
                        .or_else(|| item.get("output"))
                        .and_then(|o| o.as_str())
                        .map(|s| truncate(s, 64))
                        .unwrap_or_default();
                    vec![AgentTaskEvent::ToolCompleted {
                        tool_call_id: id,
                        tool,
                        is_error,
                        note,
                    }]
                }
            }
        }
        _ => vec![],
    }
}

fn pretty_tool(kind: &str) -> String {
    match kind {
        "command_execution" => "shell".into(),
        "file_change" => "file_change".into(),
        "mcp_tool_call" | "mcp_tool_call_update" => "mcp".into(),
        "web_search" => "web_search".into(),
        "todo_list" => "todo_list".into(),
        other => other.to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn codex_sandbox_map(spawn: &BackendSpawn) -> &'static str {
    // One permission → codex sandbox. Default conservative: read-only.
    match spawn
        .child_spec
        .sandbox
        .as_deref()
        .or(spawn.req.agent.sandbox.as_deref())
        .unwrap_or("")
    {
        "full-access" => "danger-full-access",
        "workspace-write" => "workspace-write",
        _ => "read-only",
    }
}

#[async_trait]
impl AgentBackend for CodexCliBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::CodexCli
    }

    fn label(&self) -> &'static str {
        "codex-cli"
    }

    async fn run(
        &self,
        spawn: BackendSpawn,
        control: BackendControl,
        sink: Arc<dyn AgentEventSink>,
    ) -> RunResult {
        let program = self
            .program_override
            .clone()
            .unwrap_or_else(|| "codex".into());
        let cwd = spawn
            .child_spec
            .cwd
            .clone()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| spawn.opts.cwd.clone());
        let sandbox = codex_sandbox_map(&spawn);
        let mut args: Vec<String> = vec![
            "exec".into(),
            "--json".into(),
            "--color".into(),
            "never".into(),
            "--skip-git-repo-check".into(),
            "-s".into(),
            sandbox.into(),
            "-C".into(),
            cwd.display().to_string(),
        ];
        if let Some(model) = spawn.req.agent.model.id.clone() {
            args.push("-m".into());
            args.push(model);
        }
        if let Some(mt) = spawn.req.agent.max_turns {
            args.push("-c".into());
            args.push(format!("turn_limit={mt}"));
        }
        args.extend(self.extra_args.iter().cloned());

        let cmd = CliBackendCommand {
            program,
            args,
            cwd: cwd.clone(),
            env: self.env.clone(),
            prompt: spawn.req.prompt.text.clone(),
        };
        let started = std::time::Instant::now();
        let outcome: CliRunOutcome =
            run_cli_backend(cmd, control, sink, normalize_codex_event).await;
        cli_outcome_to_run_result(outcome, "codex", started)
    }
}

/// Shared: convert a CLI outcome into the protocol `RunResult`.
pub(crate) fn cli_outcome_to_run_result(
    outcome: CliRunOutcome,
    backend: &str,
    started: std::time::Instant,
) -> RunResult {
    let duration = started.elapsed().as_millis() as u64;
    if outcome.success {
        let mut rr = RunResult::success(outcome.final_text.clone(), duration);
        rr.status = Some(TaskExitStatus::Success);
        rr
    } else {
        let msg = outcome
            .error_message
            .clone()
            .unwrap_or_else(|| format!("{backend} backend failed"));
        let mut rr = RunResult::failure(
            ProtocolError::new(error_code::PROVIDER_ERROR, msg),
            duration,
        )
        .with_status(if outcome.timed_out {
            TaskExitStatus::TimedOut
        } else {
            TaskExitStatus::RuntimeError
        });
        if outcome.timed_out {
            rr.stop_reason = Some("wall_timeout".into());
        }
        rr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn thread_started_maps_started_with_session() {
        let ev = normalize_codex_event(&v(
            r#"{"type":"thread.started","thread_id":"01a0dcd0-327d"}"#,
        ));
        assert_eq!(
            ev,
            vec![AgentTaskEvent::Started {
                session_id: Some("01a0dcd0-327d".into()),
                process_id: None
            }]
        );
    }

    #[test]
    fn turn_lifecycle_maps() {
        assert_eq!(
            normalize_codex_event(&v(r#"{"type":"turn.started"}"#)),
            vec![AgentTaskEvent::TurnStarted { turn: 0 }]
        );
        assert_eq!(
            normalize_codex_event(&v(r#"{"type":"turn.completed","usage":{}}"#)),
            vec![AgentTaskEvent::TurnCompleted { turn: 0 }]
        );
    }

    #[test]
    fn turn_failed_maps_failed() {
        let ev = normalize_codex_event(&v(
            r#"{"type":"turn.failed","error":{"message":"401 Unauthorized"}}"#,
        ));
        assert_eq!(
            ev,
            vec![AgentTaskEvent::Failed {
                message: "401 Unauthorized".into()
            }]
        );
    }

    #[test]
    fn agent_message_completed_maps_completed() {
        let ev = normalize_codex_event(&v(
            r#"{"type":"item.completed","item":{"id":"item_9","type":"agent_message","text":"HELLO_ONE_TEST"}}"#,
        ));
        assert_eq!(
            ev,
            vec![
                AgentTaskEvent::TextActivity {
                    delta_chars: "HELLO_ONE_TEST".chars().count()
                },
                AgentTaskEvent::Completed {
                    result_text: "HELLO_ONE_TEST".into(),
                    turns: None
                }
            ]
        );
    }

    #[test]
    fn reasoning_body_never_stored() {
        // Reasoning items must produce activity only — no body text anywhere.
        let ev = normalize_codex_event(&v(
            r#"{"type":"item.started","item":{"id":"item_0","type":"reasoning","summary":[]}}"#,
        ));
        assert_eq!(
            ev,
            vec![AgentTaskEvent::ToolProgress {
                tool_call_id: None,
                note: "reasoning".into()
            }]
        );
        for e in normalize_codex_event(&v(
            r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"SECRET THOUGHTS"}}"#,
        )) {
            let dump = format!("{e:?}");
            assert!(!dump.contains("SECRET"), "reasoning leaked: {dump}");
        }
    }

    #[test]
    fn command_execution_tool_lifecycle() {
        let started = normalize_codex_event(&v(
            r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"cargo test -p one-core"}}"#,
        ));
        assert_eq!(
            started,
            vec![AgentTaskEvent::ToolStarted {
                tool_call_id: "i1".into(),
                tool: "shell".into(),
                title: Some("cargo test -p one-core".into())
            }]
        );
        let done = normalize_codex_event(&v(
            r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"cargo test","exit_code":0,"output":"ok. 12 passed"}}"#,
        ));
        assert!(
            matches!(done[0], AgentTaskEvent::ToolCompleted { ref tool, is_error, .. } if tool == "shell" && !is_error)
        );
    }

    #[test]
    fn unknown_type_maps_empty() {
        assert!(normalize_codex_event(&v(r#"{"type":"brand_new_event"}"#)).is_empty());
        assert!(normalize_codex_event(&v(r#"{"no_type":true}"#)).is_empty());
    }

    #[test]
    fn error_event_is_activity_not_terminal() {
        // codex emits `{"type":"error","message":"Reconnecting... 2/5 ..."}`
        // while retrying — must NOT be terminal (turn.failed is).
        let ev = normalize_codex_event(&v(
            r#"{"type":"error","message":"Reconnecting... 2/5 (unexpected status 401)"}"#,
        ));
        assert!(!ev.iter().any(|e| e.is_terminal()));
    }

    #[tokio::test]
    async fn fake_codex_jsonl_end_to_end() {
        use crate::runtime::agent_backend::CollectorSink;
        use crate::runtime::jobs::JobEventLog;
        use std::sync::atomic::AtomicBool;

        // Fake codex: shell script emitting the verified JSONL protocol.
        let script = r#"cat <<'EOF'
{"type":"thread.started","thread_id":"th_fake"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_0","type":"reasoning"}}
{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"internal"}}
{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"ls"}}
{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","exit_code":0}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"RESEARCH_DONE summary"}}
{"type":"turn.completed","usage":{}}
EOF"#;
        let dir = std::env::temp_dir().join(format!("one-codex-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("codex");
        std::fs::write(&fake, format!("#!/bin/sh\n{script}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CodexCliBackend::with_program(fake.display().to_string());
        let jobsink = CollectorSink::new();
        let spawn = fake_spawn(&dir);
        let control = BackendControl {
            abort: Arc::new(AtomicBool::new(false)),
            event_log: JobEventLog::new(),
            wall_timeout: Some(std::time::Duration::from_secs(15)),
        };
        let result = backend.run(spawn, control, jobsink.clone()).await;
        assert!(result.ok, "{:?}", result.error);
        assert_eq!(result.result, "RESEARCH_DONE summary");
        let events = jobsink.snapshot();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentTaskEvent::Started { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentTaskEvent::ToolStarted { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentTaskEvent::Completed { .. })));
        // Reasoning body must never appear in any event payload.
        for e in &events {
            let dump = format!("{e:?}");
            assert!(!dump.contains("internal"), "reasoning leaked: {dump}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fake_spawn(dir: &std::path::Path) -> BackendSpawn {
        let spec = crate::protocol::AgentSpec::builtin_explore();
        let mut req = crate::protocol::RunRequest::new(spec.clone(), "research the repo");
        req.agent.cwd = Some(dir.display().to_string());
        BackendSpawn {
            req,
            opts: crate::runtime::harness::HarnessOptions::from_cwd(dir.to_path_buf()),
            agent_name: "codex:explore".into(),
            description: Some("fake codex probe".into()),
            child_spec: spec,
            provider: None,
        }
    }
}
