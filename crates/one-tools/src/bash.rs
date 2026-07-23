use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use one_core::tool_gate::{ToolGate, ToolGateDecision};
use serde_json::json;
use tokio::process::Command;

use crate::os_sandbox::OsSandbox;
use crate::path_policy::{PathPolicy, SandboxMode};
use crate::process_io::{
    configure_shell_stdio, consume_child, CapturedOutput, EXEC_OUTPUT_MAX_BYTES,
};
use crate::sandbox_permissions::{
    looks_like_sandbox_denial, sandbox_permissions_of, SandboxPermissions,
};
use crate::tasks::BackgroundTaskRegistry;

const DEFAULT_TIMEOUT_SECS: u64 = 120;

pub struct BashTool {
    cwd: PathBuf,
    /// Kept for backwards-compat tests; high-risk asks are handled by ToolGate.
    auto_approve: bool,
    registry: Arc<BackgroundTaskRegistry>,
    sandbox_mode: SandboxMode,
    os_sandbox: OsSandbox,
    /// Permission gate for Codex-style escalate-on-failure re-approval.
    /// When `None`, failure under the sandbox is returned as-is (model must
    /// re-call with `sandbox_permissions: require_escalated`).
    tool_gate: Option<Arc<dyn ToolGate>>,
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
            cwd: policy.cwd().to_path_buf(),
            auto_approve,
            registry,
            sandbox_mode: policy.mode(),
            os_sandbox,
            tool_gate,
        }
    }

    pub fn registry(&self) -> Arc<BackgroundTaskRegistry> {
        self.registry.clone()
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
        match perms {
            SandboxPermissions::UseDefault => self.os_sandbox.clone(),
            SandboxPermissions::RequireEscalated => OsSandbox::disabled(self.cwd.clone()),
        }
    }

    fn sandbox_banner(&self, sandbox: &OsSandbox, escalated: bool) -> (bool, String) {
        let sandboxed = sandbox.enabled && OsSandbox::bwrap_available();
        let line = if escalated && !sandboxed {
            format!(
                "sandbox: escalated (outside bwrap) · mode was {}",
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

    async fn run_command(
        &self,
        command: &str,
        sandbox: &OsSandbox,
        timeout_secs: u64,
    ) -> Result<CapturedOutput> {
        let (prog, args) = sandbox.command_line(command);
        let mut cmd = Command::new(&prog);
        cmd.args(&args)
            .current_dir(&self.cwd)
            .kill_on_drop(true);
        // Codex-aligned: piped stdio + process group for kill-on-timeout.
        configure_shell_stdio(&mut cmd);
        let child = cmd
            .spawn()
            .map_err(|err| tool_error("bash", err.to_string()))?;

        // Concurrent drain + cap + process-group kill + IO drain timeout.
        // See `crate::process_io` (mirrors Codex `consume_output`).
        consume_child(child, Some(timeout_secs), Some(EXEC_OUTPUT_MAX_BYTES))
            .await
            .map_err(|err| tool_error("bash", err.to_string()))
    }

    fn present_result(
        &self,
        command: &str,
        description: Option<String>,
        status: ExitStatus,
        stdout_buf: String,
        stderr_buf: String,
        sandbox: &OsSandbox,
        escalated: bool,
        escalated_on_failure: bool,
    ) -> ToolOutput {
        let exit_code = status.code();
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
        let status_line = if status.success() {
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
        let mut output = format!("{status_line}\n{sandbox_line}");
        if escalated_on_failure {
            output.push_str("\nnote: re-ran outside sandbox after sandboxed attempt failed (user approved escalate)");
        }
        let mut truncated = false;
        let mut spill_path: Option<String> = None;
        if !body.is_empty() {
            let presented = crate::truncate::present_tool_output(
                body.trim_end(),
                "bash",
                &self.cwd,
                crate::truncate::PreviewStyle::Head,
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
            "ok": status.success(),
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
            "command timed out after {timeout_secs}s\n{sandbox_line}"
        );
        let mut truncated = false;
        let mut spill_path: Option<String> = None;
        if !body.is_empty() {
            let presented = crate::truncate::present_tool_output(
                body.trim_end(),
                "bash",
                &self.cwd,
                crate::truncate::PreviewStyle::Head,
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
    ) -> Option<(ExitStatus, String, String, OsSandbox)> {
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
                    "sandboxed run failed (exit {code_label}); re-run outside sandbox"
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
                let sandbox = OsSandbox::disabled(self.cwd.clone());
                let timeout_secs =
                    resolve_timeout_secs(&call.arguments).unwrap_or(DEFAULT_TIMEOUT_SECS);
                match self.run_command(command, &sandbox, timeout_secs).await {
                    Ok(cap) if !cap.timed_out => {
                        Some((cap.status, cap.stdout, cap.stderr, sandbox))
                    }
                    _ => None,
                }
            }
            ToolGateDecision::Deny { .. } => None,
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
Prefer dedicated tools (read/edit/grep/find/ls) over shell for file work. \
Always set `description` to a short human-readable summary of what the command does. \
For long-running work (tests, builds, dev servers) set run_in_background=true: \
returns a task_id immediately so you can continue other tools. \
When the task finishes, a [Background task completed] notice is injected into the conversation. \
Use bash_output to poll/wait for output, bash_kill to stop a task. \
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
                        "description": "Max seconds before the command is killed (foreground default 120; background optional hard limit)"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Alias for timeout_secs (seconds). If value is >= 1000, treated as milliseconds like Claude Code."
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "If true, start the command in the background and return task_id immediately (default false)"
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

        let run_in_background = call
            .arguments
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let timeout_secs = resolve_timeout_secs(&call.arguments);
        let perms = sandbox_permissions_of(call);
        let want_escalate = matches!(perms, SandboxPermissions::RequireEscalated);
        // Escalation only changes OS bwrap; PathPolicy for file tools is separate.
        let sandbox = self.sandbox_for_call(perms);
        let escalated = want_escalate && self.os_sandbox.enabled;

        if run_in_background {
            let id = self
                .registry
                .spawn_with_sandbox(
                    command.to_string(),
                    self.cwd.clone(),
                    timeout_secs,
                    sandbox.clone(),
                )
                .await
                .map_err(|err| tool_error("bash", err))?;

            let sb_note = if escalated {
                "sandbox: escalated (outside bwrap) for this background task"
            } else if sandbox.enabled && OsSandbox::bwrap_available() {
                "sandbox: bwrap (workspace-write)"
            } else {
                "sandbox: off"
            };
            let text = format!(
                "Background task started\n\
                 task_id: {id}\n\
                 command: {command}\n\
                 {sb_note}\n\
                 TUI: /ps · Enter log · x kill.\n\
                 Agent: bash_output to poll; bash_kill to stop.\n\
                 A [Background task completed] notice will appear when it finishes."
            );
            let mut details = json!({
                "background": true,
                "task_id": id,
                "command": command,
                "ok": true,
                "escalated": escalated,
                "sandboxPermissions": perms.as_str(),
            });
            if let Some(d) = &description {
                details
                    .as_object_mut()
                    .unwrap()
                    .insert("description".into(), json!(d));
            }
            return Ok(ToolOutput::text_with_details(text, details));
        }

        // —— Foreground (blocking) ——
        let timeout_secs = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);

        let cap = self.run_command(command, &sandbox, timeout_secs).await?;

        // Codex returns partial output on timeout; surface it instead of a bare error.
        if cap.timed_out {
            return Ok(self.present_timeout(
                command,
                description,
                timeout_secs,
                cap.stdout,
                cap.stderr,
                &sandbox,
                escalated,
            ));
        }

        let status = cap.status;
        let stdout_buf = cap.stdout;
        let stderr_buf = cap.stderr;
        let exit_code = status.code();
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

        // Codex escalate_on_failure: sandboxed denial → prompt → re-run outside.
        if !status.success()
            && !escalated
            && sandbox.enabled
            && looks_like_sandbox_denial(exit_code, &body)
        {
            if let Some((st2, out2, err2, sb2)) = self
                .try_escalate_on_failure(call, command, exit_code, &body)
                .await
            {
                return Ok(self.present_result(
                    command,
                    description,
                    st2,
                    out2,
                    err2,
                    &sb2,
                    true,
                    true,
                ));
            }
            // User denied or no gate: return original failure with a hint for the model.
            let out = self.present_result(
                command,
                description,
                status,
                stdout_buf,
                stderr_buf,
                &sandbox,
                false,
                false,
            );
            let hint = "\n\n[sandbox] Command failed under the OS sandbox. \
To retry outside the sandbox, re-call bash with \
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
            status,
            stdout_buf,
            stderr_buf,
            &sandbox,
            escalated,
            false,
        ))
    }
}

/// `timeout_secs` preferred; `timeout` accepted (Claude). Values ≥ 1000 on `timeout` are ms.
fn resolve_timeout_secs(args: &serde_json::Value) -> Option<u64> {
    if let Some(s) = args.get("timeout_secs").and_then(|v| v.as_u64()) {
        return Some(s);
    }
    let t = args
        .get("timeout")
        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64)))?;
    // Claude Code historically uses milliseconds for `timeout`.
    if t >= 1000 {
        Some((t / 1000).max(1))
    } else {
        Some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::tool::ToolCall;
    use serde_json::json;

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
            text.contains("escalated") || text.contains("sandbox: off"),
            "expected escalated banner, got:\n{text}"
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
        );
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
}
