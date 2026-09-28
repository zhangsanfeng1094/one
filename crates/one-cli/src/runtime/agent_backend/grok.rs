//! Grok CLI backend — `grok --single --output-format streaming-json`.
//!
//! Args verified against `grok 1.0.41` (`grok --help` on this machine,
//! 2026-09-26):
//! - `-p, --single <PROMPT>` single-turn headless prompt
//! - `--output-format streaming-json` → NDJSON, "one ACP session update per
//!   line, the agent's native format"
//! - `--cwd <CWD>`, `-m/--model`, `--max-turns <N>`,
//!   `--permission-mode default|acceptEdits|auto|dontAsk|bypassPermissions|plan`
//! - `--disable-web-search`, `--agent <NAME>`
//!
//! The NDJSON lines are ACP-shaped records. Verified shapes from the ACP
//! schema (`sessionUpdate` tag, snake_case):
//! - `{"sessionUpdate":"user_message_chunk","content":{"…"}}`
//! - `{"sessionUpdate":"agent_message_chunk","content":{"text":"…"}}`
//! - `{"sessionUpdate":"agent_thought_chunk","content":{"text":"…"}}`
//! - `{"sessionUpdate":"tool_call","toolCallId":"…","title":"…","kind":"…"}`
//! - `{"sessionUpdate":"tool_call_update","toolCallId":"…","status":
//!   "in_progress"|"completed"|"failed"}`
//! plus session-level records (`session_configured`, `session_ready`, etc.)
//! and the terminal `{"type":"result",…}`-style completion the CLI emits.
//!
//! Thought policy: `agent_thought_chunk` updates activity/last_event_at only;
//! the body text is **never** stored or injected.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::codex::cli_outcome_to_run_result;
use super::process::{run_cli_backend, CliBackendCommand, CliRunOutcome};
use super::{
    AgentBackend, AgentEventSink, AgentTaskEvent, BackendControl, BackendKind, BackendSpawn,
};
use crate::protocol::RunResult;

pub struct GrokCliBackend {
    /// Override the grok binary (tests inject fake scripts).
    pub program_override: Option<String>,
    /// Extra args injected before the positional prompt.
    pub extra_args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl GrokCliBackend {
    pub fn new() -> Self {
        Self {
            // `ONE_GROK_BIN` lets tests (or users) point at a fake/alternate
            // binary; falls back to `grok` on PATH.
            program_override: std::env::var("ONE_GROK_BIN").ok().filter(|s| !s.is_empty()),
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

impl Default for GrokCliBackend {
    fn default() -> Self {
        Self::new()
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

fn text_of(content: &Value) -> Option<String> {
    // ACP ContentChunk: {"content":{"type":"text","text":"…"}} or bare string.
    if let Some(t) = content.get("text").and_then(|t| t.as_str()) {
        return Some(t.to_string());
    }
    if let Some(t) = content.as_str() {
        return Some(t.to_string());
    }
    if let Some(blocks) = content.get("blocks").and_then(|b| b.as_array()) {
        let mut out = String::new();
        for b in blocks {
            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

/// Map one parsed grok streaming-json line to normalized events.
/// Unknown/malformed shapes return `[]` (runner counts them as unknown).
pub fn normalize_grok_event(v: &Value) -> Vec<AgentTaskEvent> {
    // ACP session updates carry `sessionUpdate`; some grok records may use a
    // plain `type` discriminator instead — support both.
    if let Some(su) = v.get("sessionUpdate").and_then(|s| s.as_str()) {
        return normalize_acp_update(su, v);
    }
    match v.get("type").and_then(|t| t.as_str()) {
        Some("session_configured") | Some("session_ready") | Some("session_started") => {
            vec![AgentTaskEvent::Started {
                session_id: v
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .or_else(|| v.get("session_id").and_then(|s| s.as_str()))
                    .map(str::to_string),
                process_id: None,
            }]
        }
        Some("turn_started") | Some("turn_started_update") => {
            vec![AgentTaskEvent::TurnStarted { turn: 0 }]
        }
        Some("turn_completed") | Some("turn_completed_update") => {
            vec![AgentTaskEvent::TurnCompleted { turn: 0 }]
        }
        Some("turn_failed") | Some("turn_aborted") => {
            let msg = v
                .pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("grok turn failed");
            vec![AgentTaskEvent::Failed {
                message: msg.to_string(),
            }]
        }
        Some("result") => {
            // Final result record emitted by grok single-shot mode.
            let text = v
                .get("result")
                .and_then(|r| r.as_str())
                .map(str::to_string)
                .or_else(|| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
                .unwrap_or_default();
            let success = v.get("isError").and_then(|e| e.as_bool()).unwrap_or(false) == false
                && v.get("subtype")
                    .and_then(|s| s.as_str())
                    .map(|s| s != "error_max_turns" && s != "error_during_execution")
                    .unwrap_or(true);
            if success {
                vec![AgentTaskEvent::Completed {
                    result_text: text,
                    turns: None,
                }]
            } else {
                vec![AgentTaskEvent::Failed {
                    message: if text.is_empty() {
                        "grok run failed".into()
                    } else {
                        text
                    },
                }]
            }
        }
        Some("error") => {
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("grok error");
            // Structural transport errors are terminal in single-shot mode
            // (the CLI exits after printing it — verified with "Not signed in").
            vec![AgentTaskEvent::Failed {
                message: msg.to_string(),
            }]
        }
        Some("stream_event") => {
            // Optional wrapper when --include-partial-messages is on.
            match v.get("event").map(|e| e.to_owned()) {
                Some(inner) => normalize_grok_event(&inner),
                None => vec![],
            }
        }
        _ => vec![],
    }
}

fn normalize_acp_update(su: &str, v: &Value) -> Vec<AgentTaskEvent> {
    match su {
        "user_message_chunk" => vec![AgentTaskEvent::OutputActivity { bytes: 0 }],
        "agent_message_chunk" => {
            let text = text_of(v.get("content").unwrap_or(&Value::Null)).unwrap_or_default();
            if text.is_empty() {
                vec![AgentTaskEvent::TextActivity { delta_chars: 0 }]
            } else {
                vec![AgentTaskEvent::TextActivity {
                    delta_chars: text.chars().count(),
                }]
            }
        }
        // Thought/reasoning: activity only, body never stored.
        "agent_thought_chunk" => vec![AgentTaskEvent::ToolProgress {
            tool_call_id: None,
            note: "thinking".into(),
        }],
        "tool_call" => {
            let id = v
                .get("toolCallId")
                .and_then(|i| i.as_str())
                .unwrap_or("tool")
                .to_string();
            vec![AgentTaskEvent::ToolStarted {
                tool_call_id: id,
                tool: v
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("tool")
                    .to_string(),
                title: v
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(|t| truncate(t, 48)),
            }]
        }
        "tool_call_update" => {
            let id = v
                .get("toolCallId")
                .and_then(|i| i.as_str())
                .unwrap_or("tool")
                .to_string();
            match v.get("status").and_then(|s| s.as_str()) {
                Some("in_progress") | Some("pending") => vec![AgentTaskEvent::ToolProgress {
                    tool_call_id: Some(id),
                    note: v
                        .get("title")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                }],
                Some("completed") => vec![AgentTaskEvent::ToolCompleted {
                    tool_call_id: id,
                    tool: "tool".into(),
                    is_error: false,
                    note: String::new(),
                }],
                Some("failed") => vec![AgentTaskEvent::ToolCompleted {
                    tool_call_id: id,
                    tool: "tool".into(),
                    is_error: true,
                    note: v
                        .get("title")
                        .and_then(|t| t.as_str())
                        .unwrap_or("failed")
                        .to_string(),
                }],
                _ => vec![AgentTaskEvent::ToolProgress {
                    tool_call_id: Some(id),
                    note: String::new(),
                }],
            }
        }
        "plan"
        | "available_commands_update"
        | "current_mode_update"
        | "config_option_update"
        | "usage_update"
        | "session_info_update" => {
            vec![AgentTaskEvent::OutputActivity { bytes: 0 }]
        }
        _ => vec![],
    }
}

fn grok_permission_mode(spawn: &BackendSpawn) -> &'static str {
    match spawn
        .child_spec
        .permission_mode
        .as_deref()
        .or(spawn.req.agent.permission_mode.as_deref())
        .unwrap_or("")
    {
        "accept_edits" | "acceptEdits" => "acceptEdits",
        "dont_ask" | "dontAsk" => "dontAsk",
        "bypass" | "bypassPermissions" => "bypassPermissions",
        "plan" => "plan",
        _ => "default",
    }
}

#[async_trait]
impl AgentBackend for GrokCliBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::GrokCli
    }

    fn label(&self) -> &'static str {
        "grok-cli"
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
            .unwrap_or_else(|| "grok".into());
        let cwd = spawn
            .child_spec
            .cwd
            .clone()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| spawn.opts.cwd.clone());
        let mut args: Vec<String> = vec![
            "--single".into(),
            "--output-format".into(),
            "streaming-json".into(),
            "--permission-mode".into(),
            grok_permission_mode(&spawn).into(),
            "--cwd".into(),
            cwd.display().to_string(),
        ];
        if let Some(model) = spawn.req.agent.model.id.clone() {
            args.push("-m".into());
            args.push(model);
        }
        if let Some(mt) = spawn.req.agent.max_turns {
            args.push("--max-turns".into());
            args.push(mt.to_string());
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
            run_cli_backend(cmd, control, sink, normalize_grok_event).await;
        cli_outcome_to_run_result(outcome, "grok", started)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn session_records_map_started() {
        let ev = normalize_grok_event(&v(r#"{"type":"session_configured","sessionId":"s_123"}"#));
        assert_eq!(
            ev,
            vec![AgentTaskEvent::Started {
                session_id: Some("s_123".into()),
                process_id: None
            }]
        );
    }

    #[test]
    fn acp_agent_message_chunk_maps_text_activity() {
        let ev = normalize_grok_event(&v(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"partial answer"}}"#,
        ));
        assert_eq!(
            ev,
            vec![AgentTaskEvent::TextActivity {
                delta_chars: "partial answer".chars().count()
            }]
        );
    }

    #[test]
    fn acp_thought_chunk_body_never_stored() {
        let ev = normalize_grok_event(&v(
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"HIDDEN REASONING"}}"#,
        ));
        assert_eq!(
            ev,
            vec![AgentTaskEvent::ToolProgress {
                tool_call_id: None,
                note: "thinking".into()
            }]
        );
        let dump = format!("{ev:?}");
        assert!(!dump.contains("HIDDEN"), "thought leaked: {dump}");
    }

    #[test]
    fn acp_tool_call_lifecycle() {
        let start = normalize_grok_event(&v(
            r#"{"sessionUpdate":"tool_call","toolCallId":"tc_1","title":"read src/main.rs","kind":"read"}"#,
        ));
        assert_eq!(
            start,
            vec![AgentTaskEvent::ToolStarted {
                tool_call_id: "tc_1".into(),
                tool: "read".into(),
                title: Some("read src/main.rs".into())
            }]
        );
        let prog = normalize_grok_event(&v(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"tc_1","status":"in_progress","title":"reading"}"#,
        ));
        assert_eq!(
            prog,
            vec![AgentTaskEvent::ToolProgress {
                tool_call_id: Some("tc_1".into()),
                note: "reading".into()
            }]
        );
        let done = normalize_grok_event(&v(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"tc_1","status":"completed"}"#,
        ));
        assert!(
            matches!(done[0], AgentTaskEvent::ToolCompleted { ref tool_call_id, is_error, .. } if tool_call_id == "tc_1" && !is_error)
        );
        let fail = normalize_grok_event(&v(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"tc_2","status":"failed","title":"grep denied"}"#,
        ));
        assert!(matches!(
            fail[0],
            AgentTaskEvent::ToolCompleted { is_error: true, .. }
        ));
    }

    #[test]
    fn result_record_maps_completed() {
        let ev = normalize_grok_event(&v(
            r#"{"type":"result","subtype":"success","result":"GROK_RESEARCH_DONE","isError":false}"#,
        ));
        assert_eq!(
            ev,
            vec![AgentTaskEvent::Completed {
                result_text: "GROK_RESEARCH_DONE".into(),
                turns: None
            }]
        );
    }

    #[test]
    fn result_record_error_subtype_maps_failed() {
        let ev = normalize_grok_event(&v(
            r#"{"type":"result","subtype":"error_max_turns","result":"hit cap","isError":true}"#,
        ));
        assert_eq!(
            ev,
            vec![AgentTaskEvent::Failed {
                message: "hit cap".into()
            }]
        );
    }

    #[test]
    fn error_record_maps_failed() {
        let ev = normalize_grok_event(&v(
            r#"{"type":"error","message":"Not signed in. To authenticate without a browser, run: grok login --device-code"}"#,
        ));
        assert_eq!(
            ev,
            vec![AgentTaskEvent::Failed {
                message: "Not signed in. To authenticate without a browser, run: grok login --device-code".into()
            }]
        );
    }

    #[test]
    fn unknown_records_map_empty() {
        assert!(normalize_grok_event(&v(r#"{"sessionUpdate":"future_thing"}"#)).is_empty());
        assert!(normalize_grok_event(&v(r#"{"type":"future"}"#)).is_empty());
        assert!(normalize_grok_event(&v(r#"{"nothing":1}"#)).is_empty());
    }

    #[tokio::test]
    async fn fake_grok_streaming_json_end_to_end() {
        use crate::runtime::agent_backend::CollectorSink;
        use crate::runtime::jobs::JobEventLog;
        use std::sync::atomic::AtomicBool;

        // Fake grok single-shot: emit ACP NDJSON then a result record.
        let script = r#"cat <<'EOF'
{"type":"session_configured","sessionId":"s_fake"}
{"type":"turn_started"}
{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"SECRET THOUGHT"}}
{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working on it"}}
{"sessionUpdate":"tool_call","toolCallId":"tc_1","title":"grep auth","kind":"grep"}
{"sessionUpdate":"tool_call_update","toolCallId":"tc_1","status":"in_progress"}
{"sessionUpdate":"tool_call_update","toolCallId":"tc_1","status":"completed"}
{"type":"turn_completed"}
{"type":"result","subtype":"success","result":"GROK_FAKE_SUMMARY","isError":false}
EOF"#;
        let dir = std::env::temp_dir().join(format!("one-grok-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("grok");
        std::fs::write(&fake, format!("#!/bin/sh\n{script}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = GrokCliBackend::with_program(fake.display().to_string());
        let jobsink = CollectorSink::new();
        let spec = crate::protocol::AgentSpec::builtin_explore();
        let mut req = crate::protocol::RunRequest::new(spec.clone(), "research the repo");
        req.agent.cwd = Some(dir.display().to_string());
        let spawn = BackendSpawn {
            req,
            opts: crate::runtime::harness::HarnessOptions::from_cwd(dir.to_path_buf()),
            agent_name: "grok:explore".into(),
            description: Some("fake grok probe".into()),
            child_spec: spec,
            provider: None,
        };
        let control = BackendControl {
            abort: Arc::new(AtomicBool::new(false)),
            event_log: JobEventLog::new(),
            wall_timeout: Some(std::time::Duration::from_secs(15)),
        };
        let result = backend.run(spawn, control, jobsink.clone()).await;
        assert!(result.ok, "{:?}", result.error);
        assert_eq!(result.result, "GROK_FAKE_SUMMARY");
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
        for e in &events {
            let dump = format!("{e:?}");
            assert!(!dump.contains("SECRET"), "thought leaked: {dump}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
