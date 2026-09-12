use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use one_core::tool_gate::{ToolGate, ToolGateDecision};
use serde_json::json;
use tokio::process::Command;

use crate::os_sandbox::OsSandbox;
use crate::path_policy::{PathPolicy, SandboxMode};
use crate::process_io::{
    configure_shell_stdio, consume_child, kill_child_process_group, stream_pipe_into_tee,
    CapturedOutput, EXEC_OUTPUT_MAX_BYTES, IO_DRAIN_TIMEOUT,
};
use crate::sandbox_permissions::{
    looks_like_sandbox_denial, sandbox_permissions_of, SandboxPermissions,
};
use crate::tasks::BackgroundTaskRegistry;
use crate::tool_args::{bool_arg_names, u64_arg};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Default foreground blocking budget in milliseconds before auto-backgrounding (Grok-aligned: 15s).
pub const DEFAULT_FOREGROUND_BUDGET_MS: u64 = 15_000;

/// Resolve auto-background config from environment:
/// - `auto_bg`: bool (default true; disabled by `ONE_BASH_AUTO_BACKGROUND=0|false|off|no`)
/// - `budget_ms`: u64 (default 15,000ms; overridden by `ONE_BASH_FOREGROUND_BUDGET_MS`)
pub fn resolve_auto_background_config() -> (bool, u64) {
    let auto_bg = match std::env::var("ONE_BASH_AUTO_BACKGROUND")
        .or_else(|_| std::env::var("ONE_BASH_AUTO_BACKGROUND_ON_TIMEOUT"))
    {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    };
    let budget_ms = std::env::var("ONE_BASH_FOREGROUND_BUDGET_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_FOREGROUND_BUDGET_MS);
    (auto_bg, budget_ms)
}

static CWD_PROBE_SEQ: AtomicU64 = AtomicU64::new(1);

const STATE_START: &str = "__ONE_SHELL_STATE__";
const STATE_END: &str = "__ONE_SHELL_STATE_END__";
/// Cap on the replay script stored between commands. Oversized dumps keep cwd only.
const MAX_REPLAY_BYTES: usize = 512 * 1024;

/// Grok-style dump/replay snapshot: each bash invocation is a fresh process,
/// but cwd / exported vars / shopt / aliases / functions are restored.
struct ShellPersist {
    cwd: PathBuf,
    replay: String,
}

pub struct BashTool {
    /// Last observed bash session state. File-tool PathPolicy stays on the
    /// workspace root; only subsequent bash spawns replay this snapshot.
    persist: Mutex<ShellPersist>,
    /// Kept for backwards-compat tests; high-risk asks are handled by ToolGate.
    auto_approve: bool,
    registry: Arc<BackgroundTaskRegistry>,
    sandbox_mode: SandboxMode,
    os_sandbox: OsSandbox,
    /// Permission gate for Codex-style escalate-on-failure re-approval.
    /// When `None`, failure under the sandbox is returned as-is (model must
    /// re-call with `sandbox_permissions: require_escalated`).
    tool_gate: Option<Arc<dyn ToolGate>>,
    /// Explicit override for auto-backgrounding behavior.
    auto_background: Option<bool>,
    /// Explicit override for foreground budget duration in ms.
    foreground_budget_ms: Option<u64>,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_registry(cwd, true, Arc::new(BackgroundTaskRegistry::new()))
    }

    pub fn with_auto_approve(cwd: PathBuf, auto_approve: bool) -> Self {
        Self::with_registry(cwd, auto_approve, Arc::new(BackgroundTaskRegistry::new()))
    }

    pub fn with_registry(
        cwd: PathBuf,
        auto_approve: bool,
        registry: Arc<BackgroundTaskRegistry>,
    ) -> Self {
        Self::with_policy(PathPolicy::workspace(cwd), auto_approve, registry)
    }

    pub fn with_policy(
        policy: PathPolicy,
        auto_approve: bool,
        registry: Arc<BackgroundTaskRegistry>,
    ) -> Self {
        Self::with_policy_and_gate(policy, auto_approve, registry, None)
    }

    pub fn with_policy_and_gate(
        policy: PathPolicy,
        auto_approve: bool,
        registry: Arc<BackgroundTaskRegistry>,
        tool_gate: Option<Arc<dyn ToolGate>>,
    ) -> Self {
        let os_sandbox = OsSandbox::from_policy(&policy);
        // Share default sandbox settings with background registry.
        registry.set_os_sandbox(os_sandbox.clone());
        Self {
            persist: Mutex::new(ShellPersist {
                cwd: policy.cwd().to_path_buf(),
                replay: String::new(),
            }),
            auto_approve,
            registry,
            sandbox_mode: policy.mode(),
            os_sandbox,
            tool_gate,
            auto_background: None,
            foreground_budget_ms: None,
        }
    }

    pub fn with_auto_background(mut self, enabled: bool) -> Self {
        self.auto_background = Some(enabled);
        self
    }

    pub fn with_foreground_budget_ms(mut self, budget_ms: u64) -> Self {
        self.foreground_budget_ms = Some(budget_ms);
        self
    }

    fn resolve_auto_background(&self) -> (bool, u64) {
        let auto_bg = self.auto_background.unwrap_or_else(|| {
            match std::env::var("ONE_BASH_AUTO_BACKGROUND")
                .or_else(|_| std::env::var("ONE_BASH_AUTO_BACKGROUND_ON_TIMEOUT"))
            {
                Ok(v) => !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "no"
                ),
                Err(_) => true,
            }
        });
        let budget_ms = self.foreground_budget_ms.unwrap_or_else(|| {
            std::env::var("ONE_BASH_FOREGROUND_BUDGET_MS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(DEFAULT_FOREGROUND_BUDGET_MS)
        });
        (auto_bg, budget_ms)
    }

    pub fn registry(&self) -> Arc<BackgroundTaskRegistry> {
        self.registry.clone()
    }

    pub fn live_cwd(&self) -> PathBuf {
        self.persist.lock().expect("persist lock").cwd.clone()
    }

    fn remember_state(&self, dump: &Path) {
        let Ok(raw) = std::fs::read_to_string(dump) else {
            return;
        };
        if let Some((cwd, replay)) = parse_state_dump(&raw) {
            apply_persist(&self.persist, cwd, Some(replay));
            return;
        }
        // Fallback: dump is a bare pwd line (older trap, or dump markers missing).
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        apply_persist(&self.persist, PathBuf::from(trimmed), None);
    }

    fn check_command(&self, command: &str) -> Result<()> {
        // Hard block only. High-risk confirmation is owned by PermissionGate / ToolGate
        // so interactive Ask can approve once and still execute.
        if let Some(pattern) = crate::sandbox::is_command_blocked(command) {
            return Err(tool_error(
                "bash",
                format!("blocked command pattern: {pattern}"),
            ));
        }
        let _ = self.auto_approve; // reserved for future per-tool flags
        Ok(())
    }

    /// Effective OS sandbox for this call (Codex `sandbox_permissions`).
    fn sandbox_for_call(&self, perms: SandboxPermissions) -> OsSandbox {
        let cwd = self.live_cwd();
        let mut sandbox = match perms {
            SandboxPermissions::UseDefault => self.os_sandbox.clone(),
            SandboxPermissions::RequireEscalated => OsSandbox::disabled(cwd.clone()),
        };
        sandbox.cwd = cwd;
        sandbox
    }

    fn sandbox_banner(&self, sandbox: &OsSandbox, escalated: bool) -> (bool, String) {
        let sandboxed = sandbox.enabled && OsSandbox::bwrap_available();
        // Wording: "OS bwrap off" ≠ "outside workspace". PathPolicy for file
        // tools is unchanged when bash runs without bubblewrap.
        let line = if escalated && !sandboxed {
            format!(
                "sandbox: OS bwrap off for this command (path boundary still {})",
                self.sandbox_mode.as_str()
            )
        } else if sandboxed {
            format!(
                "sandbox: bwrap · mode={} · writes limited to workspace (+ --add-dir)",
                self.sandbox_mode.as_str()
            )
        } else if sandbox.enabled {
            "sandbox: requested but bwrap missing — bash is UNSANDBOXED".to_string()
        } else {
            format!(
                "sandbox: off · mode={} (use workspace-write default or unset --full-access)",
                self.sandbox_mode.as_str()
            )
        };
        (sandboxed, line)
    }

    fn spawn_shell_child(
        &self,
        command: &str,
        sandbox: &OsSandbox,
        kill_on_drop: bool,
    ) -> Result<(tokio::process::Child, PathBuf)> {
        let (cwd, replay) = {
            let g = self.persist.lock().expect("persist lock");
            (g.cwd.clone(), g.replay.clone())
        };
        let mut sandbox = sandbox.clone();
        sandbox.cwd = cwd.clone();
        let dump = state_temp_path("one-dump");
        let replay_path = write_replay_file(&replay);
        let wrapped = wrap_shell_persist(command, replay_path.as_deref(), Some(&dump));
        let (prog, args) = sandbox.command_line(&wrapped);
        let mut cmd = Command::new(&prog);
        cmd.args(&args).current_dir(&cwd).kill_on_drop(kill_on_drop);
        // Prefer plain text from CLIs (rustfmt/cargo/grep). Ratatui cannot host
        // raw SGR; residual escapes are still stripped in present_tool_output.
        cmd.env("NO_COLOR", "1");
        cmd.env("CLICOLOR", "0");
        cmd.env_remove("CLICOLOR_FORCE");
        cmd.env_remove("FORCE_COLOR");
        // Codex-aligned: piped stdio + process group for kill-on-timeout.
        configure_shell_stdio(&mut cmd);
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                if let Some(path) = replay_path {
                    let _ = std::fs::remove_file(path);
                }
                return Err(tool_error("bash", err.to_string()));
            }
        };
        Ok((child, dump))
    }

    async fn run_command(
        &self,
        command: &str,
        sandbox: &OsSandbox,
        timeout_secs: u64,
    ) -> Result<CapturedOutput> {
        let (child, probe) = self.spawn_shell_child(command, sandbox, true)?;
        // Concurrent drain + cap + process-group kill + IO drain timeout.
        // See `crate::process_io` (mirrors Codex `consume_output`).
        let cap = consume_child(child, Some(timeout_secs), Some(EXEC_OUTPUT_MAX_BYTES))
            .await
            .map_err(|err| tool_error("bash", err.to_string()))?;
        self.remember_state(&probe);
        let _ = std::fs::remove_file(&probe);
        Ok(cap)
    }

    fn present_result(
        &self,
        command: &str,
        description: Option<String>,
        exit_code: Option<i32>,
        stdout_buf: String,
        stderr_buf: String,
        sandbox: &OsSandbox,
        escalated: bool,
        escalated_on_failure: bool,
    ) -> ToolOutput {
        let is_success = exit_code == Some(0);
        let mut body = String::new();
        if !stdout_buf.is_empty() {
            body.push_str(&stdout_buf);
        }
        if !stderr_buf.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&stderr_buf);
        }

        let (sandboxed, sandbox_line) = self.sandbox_banner(sandbox, escalated);
        // Keep the sandbox line on success and failure, but lead failures with a
        // command-centric title so a non-zero exit is not read as "sandbox crashed".
        let status_line = if is_success {
            match exit_code {
                Some(c) => format!("exit {c}"),
                None => "exit 0".into(),
            }
        } else {
            match exit_code {
                Some(c) => format!("command failed (exit {c})"),
                None => "command failed (signal)".into(),
            }
        };
        let mut output = format!(
            "{status_line}\n{sandbox_line}\ncwd: {}",
            self.live_cwd().display()
        );
        if escalated_on_failure {
            output.push_str(
                "\nnote: re-ran without OS bwrap after a sandbox-like denial (user approved); \
path boundary / workspace mode unchanged",
            );
        }
        let mut truncated = false;
        let mut spill_path: Option<String> = None;
        if !body.is_empty() {
            let cwd = self.live_cwd();
            let presented = crate::truncate::present_tool_output(
                body.trim_end(),
                "bash",
                &cwd,
                crate::truncate::PreviewStyle::Tail,
            );
            truncated = presented.truncated;
            spill_path = presented
                .spill_path
                .as_ref()
                .map(|p| p.display().to_string());
            output.push('\n');
            output.push_str(&presented.text);
        }

        let mut details = json!({
            "exitCode": exit_code,
            "command": command,
            "ok": is_success,
            "background": false,
            "timedOut": false,
            "sandboxed": sandboxed,
            "sandboxMode": self.sandbox_mode.as_str(),
            "sandboxPermissions": if escalated {
                SandboxPermissions::RequireEscalated.as_str()
            } else {
                SandboxPermissions::UseDefault.as_str()
            },
            "escalated": escalated,
            "escalatedOnFailure": escalated_on_failure,
            "truncated": truncated,
            "fullOutputPath": spill_path,
            "cwd": self.live_cwd().display().to_string(),
        });
        if let Some(d) = description {
            details
                .as_object_mut()
                .unwrap()
                .insert("description".into(), json!(d));
        }
        ToolOutput::text_with_details(output, details)
    }

    /// Codex-style timeout result: partial stdout/stderr still returned to the model.
    fn present_timeout(
        &self,
        command: &str,
        description: Option<String>,
        timeout_secs: u64,
        stdout_buf: String,
        stderr_buf: String,
        sandbox: &OsSandbox,
        escalated: bool,
    ) -> ToolOutput {
        let mut body = String::new();
        if !stdout_buf.is_empty() {
            body.push_str(&stdout_buf);
        }
        if !stderr_buf.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&stderr_buf);
        }

        let (sandboxed, sandbox_line) = self.sandbox_banner(sandbox, escalated);
        let mut output = format!(
            "command timed out after {timeout_secs}s\n{sandbox_line}\ncwd: {}",
            self.live_cwd().display()
        );
        let mut truncated = false;
        let mut spill_path: Option<String> = None;
        if !body.is_empty() {
            let cwd = self.live_cwd();
            let presented = crate::truncate::present_tool_output(
                body.trim_end(),
                "bash",
                &cwd,
                crate::truncate::PreviewStyle::Tail,
            );
            truncated = presented.truncated;
            spill_path = presented
                .spill_path
                .as_ref()
                .map(|p| p.display().to_string());
            output.push('\n');
            output.push_str(&presented.text);
        }

        let mut details = json!({
            "exitCode": null,
            "command": command,
            "ok": false,
            "background": false,
            "timedOut": true,
            "timeoutSecs": timeout_secs,
            "sandboxed": sandboxed,
            "sandboxMode": self.sandbox_mode.as_str(),
            "sandboxPermissions": if escalated {
                SandboxPermissions::RequireEscalated.as_str()
            } else {
                SandboxPermissions::UseDefault.as_str()
            },
            "escalated": escalated,
            "escalatedOnFailure": false,
            "truncated": truncated,
            "fullOutputPath": spill_path,
            "cwd": self.live_cwd().display().to_string(),
        });
        if let Some(d) = description {
            details
                .as_object_mut()
                .unwrap()
                .insert("description".into(), json!(d));
        }
        ToolOutput::text_with_details(output, details)
    }

    /// Codex `escalate_on_failure`: after a sandboxed denial-like failure, ask
    /// the permission gate (interactive) to re-run outside the sandbox.
    async fn try_escalate_on_failure(
        &self,
        call: &ToolCall,
        command: &str,
        exit_code: Option<i32>,
        body: &str,
    ) -> Option<(Option<i32>, String, String, OsSandbox)> {
        if !self.os_sandbox.enabled || !OsSandbox::bwrap_available() {
            return None;
        }
        if !looks_like_sandbox_denial(exit_code, body) {
            return None;
        }
        let gate = self.tool_gate.as_ref()?;

        let code_label = match exit_code {
            Some(c) => c.to_string(),
            None => "signal".into(),
        };
        let mut args = call.arguments.clone();
        if let Some(obj) = args.as_object_mut() {
            obj.insert(
                "sandbox_permissions".into(),
                json!(SandboxPermissions::RequireEscalated.as_str()),
            );
            obj.insert(
                "justification".into(),
                json!(format!(
                    "sandboxed run failed (exit {code_label}); re-run without OS bwrap \
(workspace path boundary unchanged)"
                )),
            );
        }
        let escalate_call = ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: args,
        };

        match gate.check(&escalate_call).await {
            ToolGateDecision::Allow | ToolGateDecision::Rewrite { .. } => {
                let sandbox = OsSandbox::disabled(self.live_cwd());
                let timeout_secs =
                    resolve_timeout_secs(&call.arguments).unwrap_or(DEFAULT_TIMEOUT_SECS);
                match self.run_command(command, &sandbox, timeout_secs).await {
                    Ok(cap) if !cap.timed_out => {
                        Some((cap.status.code(), cap.stdout, cap.stderr, sandbox))
                    }
                    _ => None,
                }
            }
            ToolGateDecision::Deny { .. } => None,
        }
    }

    /// Foreground wait up to `budget`.
    ///
    /// - Completes in time → never registers (short commands stay out of `/ps`).
    /// - User kick (Ctrl+B) → adopt into the background registry.
    /// - Budget expiry with `adopt_on_budget` → auto-background.
    /// - Budget expiry without adopt → SIGTERM/KILL and a timeout result.
    async fn run_foreground_auto_bg(
        &self,
        call: &ToolCall,
        command: &str,
        description: Option<String>,
        sandbox: OsSandbox,
        escalated: bool,
        perms: SandboxPermissions,
        effective_timeout_secs: u64,
        budget: Duration,
        adopt_on_budget: bool,
    ) -> Result<ToolOutput> {
        let (mut child, probe) = self.spawn_shell_child(command, &sandbox, false)?;
        let _ = child.stdin.take();
        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pending_id = format!(
            "fg_{}_{}",
            std::process::id(),
            CWD_PROBE_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let (log_path, file_sink) = crate::tasks::open_task_log(&pending_id, command);
        let out_target = stdout_buf.clone();
        let out_file = file_sink.clone();
        let mut stdout_handle = tokio::spawn(async move {
            if let Some(out) = stdout {
                let _ =
                    stream_pipe_into_tee(out, out_target, out_file, Some(EXEC_OUTPUT_MAX_BYTES))
                        .await;
            }
        });
        let err_target = stderr_buf.clone();
        let err_file = file_sink;
        let mut stderr_handle = tokio::spawn(async move {
            if let Some(err) = stderr {
                let _ =
                    stream_pipe_into_tee(err, err_target, err_file, Some(EXEC_OUTPUT_MAX_BYTES))
                        .await;
            }
        });

        let mut guard = FgChildGuard::new(child);
        let started = Instant::now();
        let _fg_wait = self.registry.enter_fg_wait();
        let kick = self.registry.kick_notify().notified();
        tokio::pin!(kick);

        enum FgEnd {
            Exited(std::io::Result<std::process::ExitStatus>),
            Kicked,
            Budget,
        }

        let outcome = tokio::select! {
            biased;
            status = async {
                match guard.inner_mut() {
                    Some(c) => c.wait().await,
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "foreground child missing",
                    )),
                }
            } => FgEnd::Exited(status),
            _ = &mut kick => FgEnd::Kicked,
            _ = tokio::time::sleep(budget) => FgEnd::Budget,
        };

        match outcome {
            FgEnd::Exited(Ok(status)) => {
                drain_fg_readers(&mut stdout_handle, &mut stderr_handle).await;
                self.remember_state(&probe);
                let _ = std::fs::remove_file(&probe);
                if let Some(ref path) = log_path {
                    let _ = std::fs::remove_file(path);
                }
                let stdout_s = stdout_buf.lock().expect("stdout lock").clone();
                let stderr_s = stderr_buf.lock().expect("stderr lock").clone();
                let exit_code = status.code();
                let is_success = exit_code == Some(0);
                let mut body = String::new();
                if !stdout_s.is_empty() {
                    body.push_str(&stdout_s);
                }
                if !stderr_s.is_empty() {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(&stderr_s);
                }

                if !is_success
                    && !escalated
                    && sandbox.enabled
                    && looks_like_sandbox_denial(exit_code, &body)
                {
                    if let Some((code2, out2, err2, sb2)) = self
                        .try_escalate_on_failure(call, command, exit_code, &body)
                        .await
                    {
                        return Ok(self.present_result(
                            command,
                            description,
                            code2,
                            out2,
                            err2,
                            &sb2,
                            true,
                            true,
                        ));
                    }
                    let out = self.present_result(
                        command,
                        description,
                        exit_code,
                        stdout_s,
                        stderr_s,
                        &sandbox,
                        false,
                        false,
                    );
                    let hint = "\n\n[sandbox] Command failed under the OS bubblewrap sandbox \
(this is not the workspace path boundary). To retry without bwrap, re-call bash with \
sandbox_permissions=\"require_escalated\" and a short justification \
(the user will be prompted to approve).";
                    let details = out.details.clone().unwrap_or_else(|| json!({}));
                    return Ok(ToolOutput::text_with_details(
                        format!("{}{hint}", out.as_text()),
                        details,
                    ));
                }

                Ok(self.present_result(
                    command,
                    description,
                    exit_code,
                    stdout_s,
                    stderr_s,
                    &sandbox,
                    escalated,
                    false,
                ))
            }
            FgEnd::Exited(Err(err)) => {
                let _ = std::fs::remove_file(&probe);
                Err(tool_error("bash", err.to_string()))
            }
            FgEnd::Budget if !adopt_on_budget => {
                if let Some(ref mut c) = guard.inner_mut() {
                    let _ = crate::process_io::graceful_kill_child(c).await;
                }
                drain_fg_readers(&mut stdout_handle, &mut stderr_handle).await;
                self.remember_state(&probe);
                let _ = std::fs::remove_file(&probe);
                if let Some(ref path) = log_path {
                    let _ = std::fs::remove_file(path);
                }
                let stdout_s = stdout_buf.lock().expect("stdout lock").clone();
                let stderr_s = stderr_buf.lock().expect("stderr lock").clone();
                Ok(self.present_timeout(
                    command,
                    description,
                    effective_timeout_secs,
                    stdout_s,
                    stderr_s,
                    &sandbox,
                    escalated,
                ))
            }
            FgEnd::Kicked | FgEnd::Budget => {
                let remaining =
                    Duration::from_secs(effective_timeout_secs).saturating_sub(started.elapsed());
                let timeout_secs = Some(remaining.as_secs().max(1));
                let child = guard
                    .take()
                    .ok_or_else(|| tool_error("bash", "foreground child missing"))?;
                let user_kicked = matches!(outcome, FgEnd::Kicked);
                let _ = std::fs::remove_file(&probe);
                let id = self
                    .registry
                    .adopt_running(
                        command.to_string(),
                        child,
                        stdout_buf,
                        stderr_buf,
                        stdout_handle,
                        stderr_handle,
                        timeout_secs,
                        log_path,
                    )
                    .map_err(|err| tool_error("bash", err))?;

                Ok(present_backgrounded(
                    &id,
                    command,
                    description,
                    &sandbox,
                    escalated,
                    perms,
                    budget,
                    user_kicked,
                    self.live_cwd(),
                ))
            }
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        let boundary = match self.sandbox_mode {
            SandboxMode::WorkspaceWrite => {
                if self.os_sandbox.enabled && OsSandbox::bwrap_available() {
                    " File tools are workspace-scoped. Bash runs in a Codex-style bubblewrap \
sandbox: full FS read-only, workspace + /tmp writable (home/system not writable). \
High-risk commands prompt for approval unless --yes. \
When a command needs host writes outside that boundary (or full unsandboxed access), \
set sandbox_permissions to \"require_escalated\" with justification — the user will \
be asked to approve. If a sandboxed command fails with a sandbox-like denial, one may \
prompt to re-run escalated (escalate_on_failure)."
                } else {
                    " File tools are workspace-scoped. High-risk bash commands need approval \
unless --yes / ONE_AUTO_APPROVE=1."
                }
            }
            SandboxMode::FullAccess => {
                " Full filesystem access (--full-access); bash is not OS-sandboxed. \
sandbox_permissions=require_escalated is a no-op."
            }
        };
        ToolDefinition {
            name: "bash".to_string(),
            description: format!(
                "Execute a shell command in the project working directory (Claude Code Bash-compatible).{boundary} \
Bash is a persistent session across foreground calls: working directory, exported variables, aliases, functions, and shopt survive (fresh process + dump/replay, not a live PTY). \
Each result includes a `cwd:` line — trust it instead of re-running `cd`/`pwd` to the same directory. \
Background commands inherit the snapshot but do not update it. File tools stay on the workspace root. \
At most 10 background bash/monitor tasks run at once (ONE_BASH_MAX_BACKGROUND). \
Prefer dedicated tools (read/edit/grep/find/ls) over shell for file work. \
`ls` already includes line counts (and size for binaries) — do not `bash ls`/`wc`/`stat` just to size files. \
Do not use bash to read or write a path that read/edit just denied — that bypasses \
the workspace boundary. Format Rust with `cargo fmt -p <crate>` or \
`cargo fmt -- --check -p <crate>`; never invoke bare `rustfmt` on a single file \
(it assumes edition 2015 and fails on async fn). \
Git: prefer compact commands (`git status --short`, `git diff --stat`, \
`git diff --cached --stat`, `git log -5 --oneline`). Avoid dumping full file diffs \
into the conversation when a stat/name-only view is enough. To re-stage selectively, \
use `git restore --staged <path>` + `git add <paths>` — do **not** use bare `git reset` \
(clears the whole index; needs confirmation and often destroys intentional staging). \
Do not stage local junk (`.rustup/`, local secrets). \
Do not write inline python or node scripts with complex nested quotes inside bash — write a temporary script file first via `write`, or use heredocs (`python3 - << 'EOF'`). \
Always set `description` to a short human-readable summary of what the command does. \
For long-running work (tests, builds, dev servers) set run_in_background=true \
(aliases: background, is_background): returns a task_id immediately so you can continue other tools. \
When the task finishes, a [Background task completed] notice is injected into the conversation. \
Use bash_output or get_command_or_subagent_output to poll/wait for output, bash_kill / \
kill_command_or_subagent to stop a task. \
Background tasks are session-owned: quitting one or /new/resume kills them (Esc abort does not). \
Commands running longer than the foreground budget (~15s) automatically transition to the background and return a task_id without being killed. \
Omit run_in_background (or false) for short commands whose result you need before acting. \
Stdout/stderr returned to the model are capped (~2000 lines / 50KB by default; \
over limit → full spill under ~/.one/agent/tool-outputs/ + preview + path for read/grep)."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run" },
                    "description": {
                        "type": "string",
                        "description": "Short clear description of what this command does (Claude Code; shown in UI / logs)"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Max seconds before the command is killed (foreground default 120; background optional hard limit). Preferred over `timeout`."
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Optional timeout in milliseconds (Grok / Claude). Foreground default 120000. Ignored when timeout_secs is set. 0 = default / unbounded for background."
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "If true, start the command in the background and return task_id immediately (default false). Aliases: background, is_background."
                    },
                    "sandbox_permissions": {
                        "type": "string",
                        "enum": ["use_default", "require_escalated"],
                        "description": "Per-command sandbox override (Codex-aligned). Defaults to use_default. Use require_escalated to request unsandboxed execution; the user must approve (unless --yes / always-approve). Provide justification when using require_escalated."
                    },
                    "justification": {
                        "type": "string",
                        "description": "User-facing reason for sandbox_permissions=require_escalated (shown in the approval prompt). Omit otherwise."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let command = call
            .arguments
            .get("command")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid_args("bash", "missing `command`"))?;

        self.check_command(command)?;

        let description = call
            .arguments
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let run_in_background = bool_arg_names(
            &call.arguments,
            &["run_in_background", "background", "is_background"],
        )
        .unwrap_or(false);

        let timeout_secs = resolve_timeout_secs(&call.arguments).filter(|s| *s > 0);
        let perms = sandbox_permissions_of(call);
        let want_escalate = matches!(perms, SandboxPermissions::RequireEscalated);
        // Escalation only changes OS bwrap; PathPolicy for file tools is separate.
        let sandbox = self.sandbox_for_call(perms);
        let escalated = want_escalate && self.os_sandbox.enabled;

        if run_in_background {
            let cwd = self.live_cwd();
            let replay = self.persist.lock().expect("persist lock").replay.clone();
            let replay_path = write_replay_file(&replay);
            let script = wrap_shell_persist(command, replay_path.as_deref(), None);
            let id = self
                .registry
                .spawn_with_sandbox_opts(
                    command.to_string(),
                    cwd.clone(),
                    timeout_secs,
                    sandbox.clone(),
                    false,
                    Some(script),
                )
                .await
                .map_err(|err| {
                    if let Some(path) = replay_path {
                        let _ = std::fs::remove_file(path);
                    }
                    tool_error("bash", err)
                })?;

            let sb_note = if escalated {
                "sandbox: OS bwrap off for this background task (path boundary unchanged)"
            } else if sandbox.enabled && OsSandbox::bwrap_available() {
                "sandbox: bwrap (workspace-write)"
            } else {
                "sandbox: off"
            };
            let text = format!(
                "<task-id>{id}</task-id>\n\
                 <task-type>bash</task-type>\n\
                 <status>running</status>\n\
                 <summary>Command \"{command}\" started in the background.</summary>\n\
                 Background task started\n\
                 task_id: {id}\n\
                 command: {command}\n\
                 cwd: {}\n\
                 {sb_note}\n\
                 TUI: /ps · Enter log · x kill.\n\
                 Use bash_output or get_command_or_subagent_output with task_id=\"{id}\" when you need the output.\n\
                 A [Background task completed] notice will appear when it finishes.",
                cwd.display()
            );
            let mut details = json!({
                "background": true,
                "task_id": id,
                "command": command,
                "ok": true,
                "escalated": escalated,
                "sandboxPermissions": perms.as_str(),
                "cwd": cwd.display().to_string(),
            });
            if let Some(d) = &description {
                details
                    .as_object_mut()
                    .unwrap()
                    .insert("description".into(), json!(d));
            }
            return Ok(ToolOutput::text_with_details(text, details));
        }

        // —— Foreground (blocking / auto-backgroundable / Ctrl+B) ——
        let effective_timeout_secs = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let (auto_bg, budget_ms) = self.resolve_auto_background();
        let budget = if auto_bg {
            if budget_ms == 0 {
                Duration::from_secs(effective_timeout_secs)
            } else {
                Duration::from_millis(budget_ms).min(Duration::from_secs(effective_timeout_secs))
            }
        } else {
            Duration::from_secs(effective_timeout_secs)
        };

        return self
            .run_foreground_auto_bg(
                call,
                command,
                description,
                sandbox,
                escalated,
                perms,
                effective_timeout_secs,
                budget,
                auto_bg,
            )
            .await;
    }
}

/// `timeout_secs` preferred (seconds). `timeout` / `timeout_ms` are milliseconds
/// (Grok / Claude). `0` is left as `Some(0)` for the caller to treat as default.
fn resolve_timeout_secs(args: &serde_json::Value) -> Option<u64> {
    if let Some(s) = u64_arg(args, "timeout_secs") {
        return Some(s);
    }
    if let Some(ms) = u64_arg(args, "timeout_ms").or_else(|| u64_arg(args, "timeout")) {
        return Some(ms_to_secs(ms));
    }
    None
}

fn ms_to_secs(ms: u64) -> u64 {
    if ms == 0 {
        0
    } else {
        ms.saturating_add(999) / 1000
    }
}

fn state_temp_path(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        CWD_PROBE_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn write_replay_file(replay: &str) -> Option<PathBuf> {
    if replay.trim().is_empty() {
        return None;
    }
    let path = state_temp_path("one-snap");
    if std::fs::write(&path, replay).is_err() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Some(path)
}

fn apply_persist(slot: &Mutex<ShellPersist>, cwd: PathBuf, replay: Option<String>) {
    if !cwd.is_dir() {
        return;
    }
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let mut g = slot.lock().expect("persist lock");
    g.cwd = cwd.clone();
    let Some(replay) = replay else {
        return;
    };
    if replay.len() <= MAX_REPLAY_BYTES {
        g.replay = replay;
        return;
    }
    // Keep cwd persistence even when the full dump is too large.
    g.replay = format!(
        "builtin cd {} || true\n",
        sh_single_quote(&cwd.display().to_string())
    );
}

fn parse_state_dump(raw: &str) -> Option<(PathBuf, String)> {
    let mut lines = raw.lines();
    let cwd_line = lines.next()?.trim();
    if cwd_line.is_empty() {
        return None;
    }
    let cwd = PathBuf::from(cwd_line);
    let rest = lines.collect::<Vec<_>>().join("\n");
    let start = rest.find(STATE_START)?;
    let after = &rest[start + STATE_START.len()..];
    // `declare -f` of the dump helper embeds this marker in its body; the real
    // terminator is the last occurrence (emitted after the function dump).
    let end = after.rfind(STATE_END)?;
    let replay = after[..end].trim().to_string();
    Some((cwd, replay))
}

/// Dump cwd + export/shopt/alias/functions to a file (stdout-safe).
const DUMP_STATE_FN: &str = r#"
__one_dump_state() {
  local _out="$1"
  {
    builtin pwd
    builtin printf '%s\n' '__ONE_SHELL_STATE__'
    builtin printf 'builtin cd %q || true\n' "$PWD"
    if command -v grep >/dev/null 2>&1; then
      builtin export -p 2>/dev/null | command grep -viE '_proxy=|SSH_AUTH_SOCK=|DBUS_SESSION_BUS_ADDRESS=|XDG_RUNTIME_DIR=|WAYLAND_DISPLAY=|GPG_TTY=|SUDO_ASKPASS=|ELECTRON_RUN_AS_NODE=' || true
      builtin shopt -po 2>/dev/null | command grep -vE '^set [-+]o (nounset|errexit|pipefail)$' || true
    else
      builtin export -p 2>/dev/null || true
    fi
    builtin shopt -p 2>/dev/null || true
    builtin alias -p 2>/dev/null || true
    if command -v awk >/dev/null 2>&1; then
      while IFS= read -r _fn; do
        case "$_fn" in
          __one_*) continue ;;
        esac
        builtin declare -f "$_fn" 2>/dev/null || true
      done < <(builtin declare -F 2>/dev/null | command awk '{print $NF}')
    else
      builtin declare -f 2>/dev/null || true
    fi
    builtin printf '%s\n' '__ONE_SHELL_STATE_END__'
  } >"$_out" 2>/dev/null || true
}
"#;

/// Wrap `command` with optional snapshot replay and post-command dump.
///
/// Each invocation is still `bash -lc`; the snapshot overlays session-level
/// cwd/env/aliases/functions. Background calls pass `dump_path = None`.
fn wrap_shell_persist(
    command: &str,
    replay_path: Option<&Path>,
    dump_path: Option<&Path>,
) -> String {
    let mut script = String::from("builtin shopt -s expand_aliases 2>/dev/null || true\n");
    script.push_str("builtin set +u 2>/dev/null || true\n");
    if let Some(replay) = replay_path {
        let q = sh_single_quote(&replay.display().to_string());
        script.push_str(&format!(
            "if [ -r {q} ]; then \
builtin source {q} 2>/dev/null || true; \
command rm -f {q}; \
builtin shopt -s expand_aliases 2>/dev/null || true; \
fi\n"
        ));
    }
    if let Some(dump) = dump_path {
        let q = sh_single_quote(&dump.display().to_string());
        script.push_str(DUMP_STATE_FN);
        script.push_str(&format!(
            "__one_dump_done=0\n\
             __one_dump_once() {{ if [ \"${{__one_dump_done:-0}}\" != 1 ]; then __one_dump_done=1; __one_dump_state {q}; fi; }}\n\
             trap '__one_dump_once' EXIT\n"
        ));
    }
    script.push_str(command);
    if dump_path.is_some() {
        script.push_str(
            "\n__one_cmd_ec=$?\n\
             __one_dump_once\n\
             builtin exit \"$__one_cmd_ec\"\n",
        );
    }
    script
}

fn present_backgrounded(
    id: &str,
    command: &str,
    description: Option<String>,
    sandbox: &OsSandbox,
    escalated: bool,
    perms: SandboxPermissions,
    budget: Duration,
    user_kicked: bool,
    cwd: PathBuf,
) -> ToolOutput {
    let sb_note = if escalated {
        "sandbox: OS bwrap off for this background task (path boundary unchanged)"
    } else if sandbox.enabled && OsSandbox::bwrap_available() {
        "sandbox: bwrap (workspace-write)"
    } else {
        "sandbox: off"
    };
    let budget_secs = (budget.as_millis() as f64) / 1000.0;
    let (summary, started_line, auto, user) = if user_kicked {
        (
            format!(
                "Command \"{command}\" was sent to the background (Ctrl+B). Process is still running."
            ),
            "Background task started (user backgrounded)".to_string(),
            false,
            true,
        )
    } else {
        (
            format!(
                "Command \"{command}\" exceeded the default timeout and was automatically moved to background ({budget_secs:.1}s). Process is still running."
            ),
            format!("Background task started (auto-backgrounded after {budget_secs:.1}s)"),
            true,
            false,
        )
    };
    let text = format!(
        "<task-id>{id}</task-id>\n\
         <task-type>bash</task-type>\n\
         <status>running</status>\n\
         <summary>{summary}</summary>\n\
         {started_line}\n\
         task_id: {id}\n\
         command: {command}\n\
         cwd: {}\n\
         {sb_note}\n\
         TUI: /ps · Enter log · x kill · Ctrl+B backgrounds a running foreground command.\n\
         Use bash_output or get_command_or_subagent_output with task_id=\"{id}\" when you need the output.\n\
         A [Background task completed] notice will appear when it finishes.",
        cwd.display()
    );
    let mut details = json!({
        "background": true,
        "autoBackgrounded": auto,
        "userBackgrounded": user,
        "task_id": id,
        "command": command,
        "ok": true,
        "budgetMs": budget.as_millis() as u64,
        "escalated": escalated,
        "sandboxPermissions": perms.as_str(),
        "cwd": cwd.display().to_string(),
    });
    if let Some(d) = description {
        details
            .as_object_mut()
            .unwrap()
            .insert("description".into(), json!(d));
    }
    ToolOutput::text_with_details(text, details)
}

/// Kills the child on drop unless [`Self::take`]n into the background registry.
struct FgChildGuard {
    child: Option<tokio::process::Child>,
}

impl FgChildGuard {
    fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn inner_mut(&mut self) -> Option<&mut tokio::process::Child> {
        self.child.as_mut()
    }

    fn take(&mut self) -> Option<tokio::process::Child> {
        self.child.take()
    }
}

impl Drop for FgChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_child_process_group(&mut child);
        }
    }
}

async fn drain_fg_readers(
    stdout: &mut tokio::task::JoinHandle<()>,
    stderr: &mut tokio::task::JoinHandle<()>,
) {
    if tokio::time::timeout(IO_DRAIN_TIMEOUT, &mut *stdout)
        .await
        .is_err()
    {
        stdout.abort();
    }
    if tokio::time::timeout(IO_DRAIN_TIMEOUT, &mut *stderr)
        .await
        .is_err()
    {
        stderr.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;
    use serde_json::json;

    #[test]
    fn schema_exposes_timeout_secs_and_ms() {
        let dir = std::env::temp_dir();
        let tool = BashTool::with_policy(
            PathPolicy::workspace(dir),
            true,
            Arc::new(BackgroundTaskRegistry::new()),
        );
        let def = tool.definition();
        let props = def.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("timeout_secs"));
        assert!(
            props.contains_key("timeout"),
            "advertise Grok/Claude millisecond `timeout`: {props:?}"
        );
        let desc = &def.description;
        assert!(desc.contains("cargo fmt -p"), "{desc}");
        assert!(desc.contains("background, is_background"), "{desc}");
        assert!(desc.contains("cwd:"), "{desc}");
        assert!(
            desc.contains("exported variables"),
            "schema must advertise env persist: {desc}"
        );
    }

    #[test]
    fn timeout_aliases_prefer_secs_then_ms() {
        assert_eq!(
            resolve_timeout_secs(&json!({ "timeout_secs": 5, "timeout": 99_000 })),
            Some(5)
        );
        assert_eq!(
            resolve_timeout_secs(&json!({ "timeout": 30_000 })),
            Some(30)
        );
        assert_eq!(
            resolve_timeout_secs(&json!({ "timeout_ms": 1500 })),
            Some(2)
        );
        assert_eq!(resolve_timeout_secs(&json!({ "timeout": 0 })), Some(0));
        assert_eq!(resolve_timeout_secs(&json!({})), None);
    }

    #[tokio::test]
    async fn bash_output_reports_sandbox_status() {
        if !OsSandbox::bwrap_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "one-bash-sb-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = BashTool::with_policy(
            PathPolicy::workspace(dir.clone()),
            true,
            Arc::new(BackgroundTaskRegistry::new()),
        );
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "echo hello-sandbox" }),
            })
            .await
            .expect("bash ok");
        let text = out.as_text();
        assert!(
            text.starts_with("exit 0\n"),
            "success should lead with exit code, got:\n{text}"
        );
        assert!(
            text.contains("sandbox: bwrap"),
            "expected visible sandbox banner, got:\n{text}"
        );
        assert!(text.contains("hello-sandbox"), "{text}");
        let sandboxed = out
            .details
            .as_ref()
            .and_then(|d| d.get("sandboxed"))
            .and_then(|v| v.as_bool());
        assert_eq!(sandboxed, Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bash_failure_leads_with_command_failed_not_sandbox() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-fail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = BashTool::with_policy(
            PathPolicy::workspace(dir.clone()),
            true,
            Arc::new(BackgroundTaskRegistry::new()),
        );
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "python3 -c 'raise SystemExit(7)'" }),
            })
            .await
            .expect("bash should return ToolOutput even on non-zero exit");
        let text = out.as_text();
        assert!(
            text.starts_with("command failed (exit 7)\n"),
            "failure title must be command-centric, got:\n{text}"
        );
        assert!(
            text.contains("sandbox:"),
            "sandbox banner should still appear on failure:\n{text}"
        );
        assert!(
            !text.starts_with("exit 7"),
            "must not look like a bare exit header that reads as sandbox noise:\n{text}"
        );
        let ok = out
            .details
            .as_ref()
            .and_then(|d| d.get("ok"))
            .and_then(|v| v.as_bool());
        assert_eq!(ok, Some(false));
        let code = out
            .details
            .as_ref()
            .and_then(|d| d.get("exitCode"))
            .and_then(|v| v.as_i64());
        assert_eq!(code, Some(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_bash_result_keeps_tail() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-tail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = BashTool::with_policy(
            PathPolicy::workspace(dir.clone()),
            true,
            Arc::new(BackgroundTaskRegistry::new()),
        );
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .status()
            .unwrap();
        let stdout = format!("HEAD_MARKER\n{}\n", "x".repeat(60 * 1024));
        let out = tool.present_result(
            "simulated command",
            None,
            status.code(),
            stdout,
            "TAIL_MARKER".into(),
            &OsSandbox::disabled(dir.clone()),
            false,
            false,
        );

        let text = out.as_text();
        assert!(
            text.contains("TAIL_MARKER"),
            "truncated command output must preserve the tail: {text}"
        );
        assert!(
            !text.contains("HEAD_MARKER"),
            "truncated command output should not use a head preview: {text}"
        );
        let details = out.details.as_ref().unwrap();
        assert_eq!(
            details.get("truncated").and_then(|v| v.as_bool()),
            Some(true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_cannot_create_host_file_outside_workspace() {
        if !OsSandbox::bwrap_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "one-bash-ws-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let temp_roots = [
            Some(PathBuf::from("/tmp")),
            Some(PathBuf::from("/var/tmp")),
            std::env::var_os("TMPDIR").map(PathBuf::from),
        ]
        .into_iter()
        .flatten()
        .map(|path| path.canonicalize().unwrap_or(path))
        .filter(|path| path.is_absolute())
        .collect::<Vec<_>>();
        let canonical_dir = dir.canonicalize().unwrap();
        let outside_base = [
            std::env::var_os("HOME").map(PathBuf::from),
            std::env::current_dir().ok(),
        ]
        .into_iter()
        .flatten()
        .filter(|path| path.is_absolute() && path.is_dir())
        .filter_map(|path| path.canonicalize().ok())
        .find(|path| {
            !path.starts_with(&canonical_dir)
                && !temp_roots.iter().any(|root| path.starts_with(root))
        });
        let Some(outside_base) = outside_base else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let outside = outside_base.join(format!(
            ".one-bash-leak-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&outside);
        let outside_link = dir.join("outside-link");
        std::os::unix::fs::symlink(&outside, &outside_link).unwrap();

        let tool = BashTool::with_policy(
            PathPolicy::workspace(dir.clone()),
            true,
            Arc::new(BackgroundTaskRegistry::new()),
        );
        let _ = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "echo leaked > outside-link"
                }),
            })
            .await;

        let leaked = outside.exists();
        let _ = std::fs::remove_file(&outside_link);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !leaked,
            "bash OS sandbox must not create host file {}",
            outside.display()
        );
    }

    #[tokio::test]
    async fn require_escalated_can_write_outside_workspace() {
        if !OsSandbox::bwrap_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "one-bash-esc-ws-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let outside = std::env::temp_dir().join(format!(
            "one-bash-esc-out-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&outside);

        // auto_approve=true so gate would allow; here no gate — BashTool trusts
        // that PermissionGate already approved require_escalated before execute.
        let tool = BashTool::with_policy(
            PathPolicy::workspace(dir.clone()),
            true,
            Arc::new(BackgroundTaskRegistry::new()),
        );
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": format!("echo escalated > {}", outside.display()),
                    "sandbox_permissions": "require_escalated",
                    "justification": "test write outside workspace"
                }),
            })
            .await
            .expect("bash ok");
        let text = out.as_text();
        assert!(
            text.contains("OS bwrap off") || text.contains("sandbox: off"),
            "expected escalated (OS bwrap off) banner, got:\n{text}"
        );
        assert!(
            outside.exists(),
            "require_escalated must allow host write {}",
            outside.display()
        );
        let escalated = out
            .details
            .as_ref()
            .and_then(|d| d.get("escalated"))
            .and_then(|v| v.as_bool());
        assert_eq!(escalated, Some(true));
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: wait-then-read deadlocks once stdout exceeds the OS pipe
    /// buffer (~64 KiB). Concurrent drain must let a multi-hundred-KiB writer
    /// exit promptly.
    #[tokio::test]
    async fn large_stdout_does_not_deadlock() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-pipe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = BashTool::with_policy(
            PathPolicy::workspace(dir.clone()),
            true,
            Arc::new(BackgroundTaskRegistry::new()),
        );
        // ~512 KiB of 'x' — well above typical pipe capacity.
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "python3 -c 'print(\"x\" * (512 * 1024), end=\"\")'",
                    "timeout_secs": 30,
                }),
            })
            .await
            .expect("large stdout must complete without pipe deadlock");
        let text = out.as_text();
        assert!(
            text.starts_with("exit 0\n"),
            "expected success, got:\n{text}"
        );
        assert!(
            text.contains('x'),
            "stdout should include captured payload:\n{text}"
        );
        let timed_out = out
            .details
            .as_ref()
            .and_then(|d| d.get("timedOut"))
            .and_then(|v| v.as_bool());
        assert_eq!(timed_out, Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeout_returns_tool_output_not_hard_error() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-to-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = BashTool::with_policy(
            PathPolicy::workspace(dir.clone()),
            true,
            Arc::new(BackgroundTaskRegistry::new()),
        )
        .with_auto_background(false);

        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "sleep 60",
                    "timeout_secs": 1,
                }),
            })
            .await
            .expect("timeout should be ToolOutput, not Err");
        let text = out.as_text();
        assert!(
            text.starts_with("command timed out after 1s\n"),
            "got:\n{text}"
        );
        let timed_out = out
            .details
            .as_ref()
            .and_then(|d| d.get("timedOut"))
            .and_then(|v| v.as_bool());
        assert_eq!(timed_out, Some(true));
        let ok = out
            .details
            .as_ref()
            .and_then(|d| d.get("ok"))
            .and_then(|v| v.as_bool());
        assert_eq!(ok, Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn auto_background_on_timeout_transitions_to_background() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-autobg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let registry = Arc::new(BackgroundTaskRegistry::new());
        let tool =
            BashTool::with_policy(PathPolicy::workspace(dir.clone()), true, registry.clone())
                .with_auto_background(true)
                .with_foreground_budget_ms(500);

        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "sleep 2; echo done_bg",
                    "timeout_secs": 10,
                }),
            })
            .await
            .expect("auto-bg should succeed with ToolOutput");

        let text = out.as_text();
        assert!(
            text.contains("Background task started (auto-backgrounded after"),
            "got:\n{text}"
        );
        let details = out.details.as_ref().expect("details must exist");
        assert_eq!(
            details.get("autoBackgrounded").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            details.get("background").and_then(|v| v.as_bool()),
            Some(true)
        );
        let task_id = details
            .get("task_id")
            .and_then(|v| v.as_str())
            .expect("task_id must be present");

        // Wait for background task to complete
        let snap = registry
            .wait(task_id, Some(5))
            .await
            .expect("task must finish");
        assert_eq!(snap.state, crate::tasks::TaskState::Completed);
        assert!(snap.stdout.contains("done_bg"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grok_background_alias_starts_task() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-bg-alias-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let tool =
            BashTool::with_policy(PathPolicy::workspace(dir.clone()), true, registry.clone());
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "sleep 2; echo from_bg",
                    "background": true,
                }),
            })
            .await
            .expect("background alias");
        let details = out.details.as_ref().expect("details");
        assert_eq!(
            details.get("background").and_then(|v| v.as_bool()),
            Some(true)
        );
        let task_id = details
            .get("task_id")
            .and_then(|v| v.as_str())
            .expect("task_id");
        assert!(task_id.starts_with("bg_"), "{task_id}");
        let snap = registry.wait(task_id, Some(5)).await.expect("finish");
        assert_eq!(snap.state, crate::tasks::TaskState::Completed);
        assert!(snap.stdout.contains("from_bg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn short_foreground_command_does_not_register() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-short-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let tool =
            BashTool::with_policy(PathPolicy::workspace(dir.clone()), true, registry.clone())
                .with_auto_background(true)
                .with_foreground_budget_ms(15_000);
        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "echo short_ok" }),
            })
            .await
            .expect("short fg");
        assert!(out.as_text().contains("short_ok"), "{}", out.as_text());
        assert_eq!(
            out.details
                .as_ref()
                .and_then(|d| d.get("background"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(
            registry.list().is_empty(),
            "short commands must not enter the background registry: {:?}",
            registry
                .list()
                .iter()
                .map(|t| t.id.clone())
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn foreground_cd_persists_for_next_command() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let tool = BashTool::with_policy(
            PathPolicy::workspace(dir.clone()),
            true,
            Arc::new(BackgroundTaskRegistry::new()),
        )
        .with_auto_background(false);

        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "cd sub && pwd" }),
            })
            .await
            .expect("cd");
        assert!(
            out.as_text().contains("sub"),
            "first pwd should be in sub:\n{}",
            out.as_text()
        );

        let out2 = tool
            .execute(&ToolCall {
                id: "2".into(),
                name: "bash".into(),
                arguments: json!({ "command": "pwd" }),
            })
            .await
            .expect("pwd");
        let text = out2.as_text();
        assert!(
            text.contains("sub"),
            "second command must start in the persisted cwd:\n{text}"
        );
        assert!(
            tool.live_cwd().ends_with("sub"),
            "live_cwd={:?}",
            tool.live_cwd()
        );
        assert!(
            text.contains("cwd:") && text.contains("sub"),
            "model-visible result must include cwd:\n{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn foreground_export_alias_and_function_persist() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-snap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = BashTool::with_policy(
            PathPolicy::workspace(dir.clone()),
            true,
            Arc::new(BackgroundTaskRegistry::new()),
        )
        .with_auto_background(false);

        tool.execute(&ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({
                "command": "export ONE_SHELL_PERSIST_PROBE=persist_ok\n\
            alias one_persist_al='printf alias_ok'\n\
            one_persist_fn() { printf fn_ok; }"
            }),
        })
        .await
        .expect("define persist bits");

        let out = tool
            .execute(&ToolCall {
                id: "2".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "printf '%s %s %s\\n' \"$ONE_SHELL_PERSIST_PROBE\" \"$(one_persist_al)\" \"$(one_persist_fn)\""
                }),
            })
            .await
            .expect("replay");
        let text = out.as_text();
        assert!(
            text.contains("persist_ok"),
            "exported var must persist:\n{text}"
        );
        assert!(text.contains("alias_ok"), "alias must persist:\n{text}");
        assert!(text.contains("fn_ok"), "function must persist:\n{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn background_command_does_not_update_session_cwd() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-bgcwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let tool =
            BashTool::with_policy(PathPolicy::workspace(dir.clone()), true, registry.clone())
                .with_auto_background(false);

        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "cd sub && pwd",
                    "background": true,
                }),
            })
            .await
            .expect("bg cd");
        let task_id = out
            .details
            .as_ref()
            .and_then(|d| d.get("task_id"))
            .and_then(|v| v.as_str())
            .expect("task_id")
            .to_string();
        let _ = registry.wait(&task_id, Some(5)).await;

        assert!(
            !tool.live_cwd().ends_with("sub"),
            "background cd must not move session cwd: {:?}",
            tool.live_cwd()
        );
        let out2 = tool
            .execute(&ToolCall {
                id: "2".into(),
                name: "bash".into(),
                arguments: json!({ "command": "pwd" }),
            })
            .await
            .expect("pwd");
        let text = out2.as_text();
        let expected = dir.canonicalize().unwrap_or(dir.clone());
        assert_eq!(
            tool.live_cwd(),
            expected,
            "fg after bg cd must stay at workspace root:\n{text}"
        );
        assert!(
            text.contains(&expected.display().to_string()),
            "pwd should be workspace root:\n{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_state_dump_reads_cwd_and_replay() {
        let raw = "/tmp/work\n__ONE_SHELL_STATE__\nbuiltin cd /tmp/work || true\ndeclare -x FOO=bar\n__ONE_SHELL_STATE_END__\n";
        let (cwd, replay) = parse_state_dump(raw).expect("parse");
        assert_eq!(cwd, PathBuf::from("/tmp/work"));
        assert!(replay.contains("declare -x FOO=bar"), "{replay}");
        assert!(parse_state_dump("/tmp/work\njust pwd\n").is_none());

        // Dump helper body embeds the end marker; parse must take the last one.
        let nested = "/tmp/work\n__ONE_SHELL_STATE__\n\
builtin printf '%s\\n' '__ONE_SHELL_STATE_END__'\n\
one_persist_fn () { printf fn_ok; }\n\
__ONE_SHELL_STATE_END__\n";
        let (_, replay) = parse_state_dump(nested).expect("nested marker");
        assert!(
            replay.contains("one_persist_fn"),
            "must not truncate at embedded marker:\n{replay}"
        );
    }

    #[tokio::test]
    async fn ctrl_b_kick_adopts_foreground_child() {
        let dir = std::env::temp_dir().join(format!(
            "one-bash-kick-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let tool =
            BashTool::with_policy(PathPolicy::workspace(dir.clone()), true, registry.clone())
                .with_auto_background(true)
                .with_foreground_budget_ms(30_000);

        let kick = registry.clone();
        tokio::spawn(async move {
            for _ in 0..40 {
                if kick.has_foreground_waiter() {
                    kick.request_background_now();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            kick.request_background_now();
        });

        let out = tool
            .execute(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "sleep 20",
                    "timeout_secs": 30,
                }),
            })
            .await
            .expect("kick should background");
        let details = out.details.as_ref().expect("details");
        assert_eq!(
            details.get("userBackgrounded").and_then(|v| v.as_bool()),
            Some(true),
            "text:\n{}",
            out.as_text()
        );
        let task_id = details
            .get("task_id")
            .and_then(|v| v.as_str())
            .expect("task_id");
        let snap = registry.kill(task_id).await.expect("kill kicked task");
        assert_eq!(snap.state, crate::tasks::TaskState::Killed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
