//! Shared external-CLI process runner for agent backends.
//!
//! Verified on this machine (2026-09-26):
//! - `codex exec --json` → JSONL events on stdout (`thread.started`,
//!   `turn.started`, `item.*`, `turn.completed`/`turn.failed`, `error`).
//! - `grok --single --output-format streaming-json` → NDJSON records on
//!   stdout (ACP-shaped `session/update` payloads).
//!
//! Reliability rules implemented here:
//! - independent process group (`process_group(0)`), cancel kills the whole
//!   tree (SIGTERM → grace → SIGKILL `-pgid`)
//! - stdout parsed line-by-line as structured JSON; malformed/unknown lines
//!   are counted and treated as liveness, never panic
//! - stderr captured separately (capped)
//! - stdout is never buffered unbounded: lines are consumed incrementally and
//!   dropped after normalization (retained diagnostics capped)
//! - process exit without a terminal event ⇒ `Failed`
//! - events arriving after a terminal event are ignored (late/duplicate)
//! - abort flag checked each line → immediate group kill

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use super::{AgentEventSink, AgentTaskEvent, BackendControl};

/// Cap on raw stdout bytes retained for diagnostics.
pub const CLI_STDOUT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Cap on stderr retained for diagnostics.
pub const CLI_STDERR_MAX_BYTES: usize = 1 * 1024 * 1024;

/// SIGTERM grace before SIGKILL of the process group.
pub const CLI_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Full command description for a CLI backend spawn.
#[derive(Debug, Clone)]
pub struct CliBackendCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Extra env (merged over inherited).
    pub env: Vec<(String, String)>,
    /// Prompt passed as the trailing positional argument (appended after args).
    pub prompt: String,
}

/// Outcome of a completed CLI process run.
pub struct CliRunOutcome {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub final_text: String,
    pub error_message: Option<String>,
    pub malformed_lines: u64,
    pub unknown_lines: u64,
    pub stderr_tail: String,
    pub timed_out: bool,
}

struct RunningChild {
    child: tokio::sync::Mutex<Option<Child>>,
    pid: Option<u32>,
}

impl RunningChild {
    fn id(&self) -> Option<u32> {
        self.child
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|c| c.id()))
            .or(self.pid)
    }

    /// Process still running (`Some(false)` once reaped). `None` = no process
    /// reference left (already consumed).
    fn process_alive(&self) -> Option<bool> {
        match self.child.try_lock() {
            Ok(guard) => Some(match guard.as_ref() {
                Some(c) => c.id().is_some(),
                None => false,
            }),
            // Locked elsewhere (actively being reaped) → still alive.
            Err(_) => Some(true),
        }
    }

    async fn kill_tree(&self) {
        let mut guard = self.child.lock().await;
        if let Some(child) = guard.as_mut() {
            if let Some(pid) = child.id() {
                one_tools::term_process_group(pid);
                if tokio::time::timeout(CLI_CANCEL_GRACE, child.wait())
                    .await
                    .is_err()
                {
                    one_tools::kill_process_group(pid);
                    let _ = child.wait().await;
                }
            } else {
                let _ = child.start_kill();
            }
        }
    }
}

/// Spawn + run a CLI backend process to completion, feeding normalized events
/// through `sink`. `normalize` maps one parsed stdout JSON value to 0..n
/// normalized events (backend-specific).
///
/// Single place external processes are launched for agent backends.
pub async fn run_cli_backend<F>(
    cmd: CliBackendCommand,
    control: BackendControl,
    sink: Arc<dyn AgentEventSink>,
    normalize: F,
) -> CliRunOutcome
where
    F: Fn(&serde_json::Value) -> Vec<AgentTaskEvent>,
{
    let mut command = Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .arg(&cmd.prompt)
        .current_dir(&cmd.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false)
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0");
    for (k, v) in &cmd.env {
        command.env(k, v);
    }
    #[cfg(unix)]
    {
        // Independent process group so cancel reaps the whole tree.
        command.process_group(0);
    }

    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to spawn `{}`: {e}", cmd.program);
            sink.on_event(AgentTaskEvent::Failed {
                message: msg.clone(),
            });
            return CliRunOutcome {
                success: false,
                exit_code: None,
                final_text: String::new(),
                error_message: Some(msg),
                malformed_lines: 0,
                unknown_lines: 0,
                stderr_tail: String::new(),
                timed_out: false,
            };
        }
    };

    let running = Arc::new(RunningChild {
        pid: child.id(),
        child: tokio::sync::Mutex::new(Some(child)),
    });
    let _ = running.pid;

    let stdout = {
        let mut guard = running.child.lock().await;
        guard.as_mut().and_then(|c| c.stdout.take())
    };
    let stderr = {
        let mut guard = running.child.lock().await;
        guard.as_mut().and_then(|c| c.stderr.take())
    };

    sink.on_event(AgentTaskEvent::Started {
        session_id: None,
        process_id: running.pid,
    });

    // stderr: independent capped capture.
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stderr_task = tokio::spawn({
        let buf = stderr_buf.clone();
        async move {
            if let Some(err) = stderr {
                if let Ok(s) = one_tools::read_pipe_capped(err, Some(CLI_STDERR_MAX_BYTES)).await {
                    let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
                    *b = s;
                }
            }
        }
    });

    let stdout_bytes_seen = AtomicU64::new(0);
    let malformed = AtomicU64::new(0);
    let unknown = AtomicU64::new(0);
    let mut final_text = String::new();
    let mut terminal_seen = false;
    let mut terminal_error: Option<String> = None;
    let mut turns: u64 = 0;

    // Inline stdout loop (keeps `normalize` borrow local, no 'static bound).
    if let Some(out) = stdout {
        let mut reader = BufReader::new(out);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    stdout_bytes_seen.fetch_add(line.len() as u64, Ordering::Relaxed);
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let value: serde_json::Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(_) => {
                            // Non-JSON diagnostics line (warnings etc.): count,
                            // never crash; still liveness evidence.
                            malformed.fetch_add(1, Ordering::Relaxed);
                            sink.on_event(AgentTaskEvent::OutputActivity {
                                bytes: trimmed.len() as u64,
                            });
                            continue;
                        }
                    };
                    let events = normalize(&value);
                    if events.is_empty() {
                        unknown.fetch_add(1, Ordering::Relaxed);
                        // Even unknown JSON is liveness evidence.
                        sink.on_event(AgentTaskEvent::OutputActivity {
                            bytes: trimmed.len() as u64,
                        });
                        continue;
                    }
                    let is_terminal = events.iter().any(|e| e.is_terminal());
                    if terminal_seen {
                        // Late/duplicate terminal-band events after a terminal:
                        // ignore safely.
                        continue;
                    }
                    for ev in events {
                        match &ev {
                            AgentTaskEvent::Completed { result_text, .. } => {
                                final_text = result_text.clone();
                            }
                            AgentTaskEvent::Failed { message } => {
                                terminal_error = Some(message.clone());
                            }
                            AgentTaskEvent::TurnCompleted { .. } => {
                                turns += 1;
                            }
                            _ => {}
                        }
                        sink.on_event(ev);
                    }
                    if is_terminal {
                        terminal_seen = true;
                    }
                }
                Err(e) => {
                    // Read error: treat as malformed input, keep going.
                    malformed.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(error = %e, "cli backend: stdout read error");
                }
            }
            // Cooperative abort: kill the tree promptly on the next line.
            if control.abort.load(Ordering::Relaxed) {
                running.kill_tree().await;
                break;
            }
        }
    }

    let _ = turns;

    // Wait for the process itself (bounded by wall timeout when set).
    let running_wait = running.clone();
    let wait_fut = async move {
        let mut guard = running_wait.child.lock().await;
        match guard.as_mut() {
            Some(child) => child.wait().await.ok().and_then(|s| s.code()),
            None => None,
        }
    };
    let exit_code = match control.wall_timeout {
        Some(limit) => match tokio::time::timeout(limit, wait_fut).await {
            Ok(code) => code,
            Err(_) => {
                running.kill_tree().await;
                let _ = stderr_task.abort();
                let msg = format!("cli backend wall timeout ({limit:?})");
                sink.on_event(AgentTaskEvent::Failed {
                    message: msg.clone(),
                });
                return CliRunOutcome {
                    success: false,
                    exit_code: None,
                    final_text,
                    error_message: Some(msg),
                    malformed_lines: malformed.load(Ordering::Relaxed),
                    unknown_lines: unknown.load(Ordering::Relaxed),
                    stderr_tail: String::new(),
                    timed_out: true,
                };
            }
        },
        None => wait_fut.await,
    };

    let _ = stderr_task.await;
    let stderr_text = stderr_buf.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let stderr_tail: String = stderr_text
        .trim()
        .chars()
        .rev()
        .take(400)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    // Process exited without a terminal event ⇒ Failed.
    if !terminal_seen {
        let base = match exit_code {
            Some(0) => "process exited without a terminal event".to_string(),
            Some(c) => format!("process exited (code {c}) without a terminal event"),
            None => "process terminated (signal) without a terminal event".to_string(),
        };
        let msg = if !stderr_tail.is_empty() {
            format!("{base}; stderr: {stderr_tail}")
        } else {
            base
        };
        sink.on_event(AgentTaskEvent::Failed {
            message: msg.clone(),
        });
        return CliRunOutcome {
            success: false,
            exit_code,
            final_text,
            error_message: Some(msg),
            malformed_lines: malformed.load(Ordering::Relaxed),
            unknown_lines: unknown.load(Ordering::Relaxed),
            stderr_tail,
            timed_out: false,
        };
    }

    let success = terminal_error.is_none();
    CliRunOutcome {
        success,
        exit_code,
        final_text,
        error_message: terminal_error,
        malformed_lines: malformed.load(Ordering::Relaxed),
        unknown_lines: unknown.load(Ordering::Relaxed),
        stderr_tail,
        timed_out: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::agent_backend::{AgentEventSink, AgentTaskEvent, BackendControl};
    use crate::runtime::jobs::JobEventLog;
    use std::sync::atomic::AtomicBool;

    fn control() -> BackendControl {
        BackendControl {
            abort: Arc::new(AtomicBool::new(false)),
            event_log: JobEventLog::new(),
            wall_timeout: Some(Duration::from_secs(30)),
        }
    }

    fn control_with(abort: Arc<AtomicBool>, wall: Duration) -> BackendControl {
        BackendControl {
            abort,
            event_log: JobEventLog::new(),
            wall_timeout: Some(wall),
        }
    }

    #[derive(Default)]
    struct Collector {
        events: Mutex<Vec<AgentTaskEvent>>,
    }

    impl AgentEventSink for Collector {
        fn on_event(&self, event: AgentTaskEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn sink() -> Arc<Collector> {
        Arc::new(Collector::default())
    }

    fn sh(script: &str) -> CliBackendCommand {
        CliBackendCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: PathBuf::from("/tmp"),
            env: vec![],
            prompt: String::new(),
        }
    }

    fn fake_normalize(v: &serde_json::Value) -> Vec<AgentTaskEvent> {
        match v.get("type").and_then(|t| t.as_str()) {
            Some("done") => vec![AgentTaskEvent::Completed {
                result_text: v.get("text").and_then(|t| t.as_str()).unwrap_or("").into(),
                turns: None,
            }],
            Some("boom") => vec![AgentTaskEvent::Failed {
                message: v
                    .get("msg")
                    .and_then(|m| m.as_str())
                    .unwrap_or("boom")
                    .into(),
            }],
            Some("tick") => vec![AgentTaskEvent::ToolProgress {
                tool_call_id: None,
                note: "tick".into(),
            }],
            _ => vec![],
        }
    }

    #[tokio::test]
    async fn normalizes_fake_jsonl_events() {
        let script = r#"printf '%s\n' '{"type":"tick"}' '{"type":"done","text":"final answer"}'"#;
        let s = sink();
        let out = run_cli_backend(sh(script), control(), s.clone(), fake_normalize).await;
        assert!(out.success, "err={:?}", out.error_message);
        assert_eq!(out.final_text, "final answer");
        assert_eq!(out.malformed_lines, 0);
        assert_eq!(out.unknown_lines, 0);
        let events = s.events.lock().unwrap().clone();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentTaskEvent::Started { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentTaskEvent::Completed { .. })));
    }

    #[tokio::test]
    async fn malformed_lines_do_not_crash() {
        let script = r#"printf '%s\n' 'not json {{{' 'plain text' '{"type":"done","text":"ok"}'"#;
        let s = sink();
        let out = run_cli_backend(sh(script), control(), s.clone(), fake_normalize).await;
        assert!(out.success, "{:?}", out.error_message);
        assert_eq!(out.malformed_lines, 2);
        assert_eq!(out.final_text, "ok");
    }

    #[tokio::test]
    async fn unknown_events_counted_not_fatal() {
        let script = r#"printf '%s\n' '{"type":"mystery"}' '{"type":"done","text":"x"}'"#;
        let s = sink();
        let out = run_cli_backend(sh(script), control(), s.clone(), fake_normalize).await;
        assert!(out.success);
        assert_eq!(out.unknown_lines, 1);
    }

    #[tokio::test]
    async fn exit_without_terminal_event_fails() {
        let s = sink();
        let out = run_cli_backend(
            sh("echo nothing structured"),
            control(),
            s.clone(),
            fake_normalize,
        )
        .await;
        assert!(!out.success);
        let msg = out.error_message.expect("error message");
        assert!(msg.contains("without a terminal event"), "{msg}");
        let events = s.events.lock().unwrap().clone();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentTaskEvent::Failed { .. })));
    }

    #[tokio::test]
    async fn nonzero_exit_with_terminal_error_maps_failed() {
        let script = r#"printf '%s\n' '{"type":"boom","msg":"auth error"}'; exit 3"#;
        let s = sink();
        let out = run_cli_backend(sh(script), control(), s.clone(), fake_normalize).await;
        assert!(!out.success);
        assert_eq!(out.error_message.as_deref(), Some("auth error"));
        assert_eq!(out.exit_code, Some(3));
    }

    #[tokio::test]
    async fn late_duplicate_terminal_events_ignored() {
        // done then failed then done again: only the first terminal counts.
        let script = r#"printf '%s\n' '{"type":"done","text":"first"}' '{"type":"boom","msg":"late"}' '{"type":"done","text":"dup"}'"#;
        let s = sink();
        let out = run_cli_backend(sh(script), control(), s.clone(), fake_normalize).await;
        assert!(out.success, "first terminal wins");
        assert_eq!(out.final_text, "first");
        assert!(out.error_message.is_none());
    }

    /// Abort mid-stream must SIGTERM the whole process group (the background
    /// `sleep 300` child included) and return promptly.
    #[tokio::test]
    async fn cancel_kills_entire_process_group() {
        struct AbortingSink {
            abort: Arc<AtomicBool>,
            ticks: AtomicU64,
        }
        impl AgentEventSink for AbortingSink {
            fn on_event(&self, event: AgentTaskEvent) {
                if matches!(event, AgentTaskEvent::ToolProgress { .. }) {
                    let n = self.ticks.fetch_add(1, Ordering::SeqCst);
                    if n >= 3 {
                        self.abort.store(true, Ordering::SeqCst);
                    }
                }
            }
        }
        let abort = Arc::new(AtomicBool::new(false));
        let sink: Arc<dyn AgentEventSink> = Arc::new(AbortingSink {
            abort: abort.clone(),
            ticks: AtomicU64::new(0),
        });
        // Parent spawns `sleep 300` into the same process group, then ticks
        // forever. Post-abort, the group (both processes) must be reaped.
        let cmd = CliBackendCommand {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "sleep 300 & while true; do echo '{\"type\":\"tick\"}'; sleep 0.05; done".into(),
            ],
            cwd: PathBuf::from("/tmp"),
            env: vec![],
            prompt: String::new(),
        };
        let start = std::time::Instant::now();
        let out = run_cli_backend(
            cmd,
            control_with(abort, Duration::from_secs(60)),
            sink,
            fake_normalize,
        )
        .await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(20),
            "cancel must kill group promptly (took {elapsed:?})"
        );
        assert!(!out.success);
        // Give the group a moment to die, then assert no `sleep 300` remains.
        // Match the exact argv (`[s]leep 300` avoids self-matching grep; count
        // only processes whose args begin with "sleep 300").
        tokio::time::sleep(Duration::from_millis(500)).await;
        let leftovers = std::process::Command::new("sh")
            .arg("-c")
            .arg("ps -eo args= | awk '$1==\"sleep\" && $2==\"300\"' | wc -l")
            .output()
            .expect("ps");
        let count: String = String::from_utf8_lossy(&leftovers.stdout)
            .trim()
            .to_string();
        assert_eq!(
            count, "0",
            "process group must be fully reaped (found {count} sleepers)"
        );
    }

    #[tokio::test]
    async fn stdout_never_buffered_unbounded() {
        // 20k unknown JSON lines then done: outcome bounded, unknown counted.
        let script = "i=0; while [ $i -lt 20000 ]; do echo '{\"type\":\"noise\",\"i\":'$i'}'; i=$((i+1)); done; echo '{\"type\":\"done\",\"text\":\"ok\"}'";
        let s = sink();
        let out = run_cli_backend(
            sh(script),
            control_with(Arc::new(AtomicBool::new(false)), Duration::from_secs(60)),
            s.clone(),
            fake_normalize,
        )
        .await;
        assert!(out.success, "{:?}", out.error_message);
        assert_eq!(out.unknown_lines, 20_000);
    }
}
