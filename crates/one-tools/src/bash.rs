use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::os_sandbox::OsSandbox;
use crate::path_policy::{PathPolicy, SandboxMode};
use crate::tasks::BackgroundTaskRegistry;

const DEFAULT_TIMEOUT_SECS: u64 = 120;

pub struct BashTool {
    cwd: PathBuf,
    /// Kept for backwards-compat tests; high-risk asks are handled by ToolGate.
    auto_approve: bool,
    registry: Arc<BackgroundTaskRegistry>,
    sandbox_mode: SandboxMode,
    os_sandbox: OsSandbox,
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
        let os_sandbox = OsSandbox::from_policy(&policy);
        // Share sandbox settings with background registry.
        registry.set_os_sandbox(os_sandbox.clone());
        Self {
            cwd: policy.cwd().to_path_buf(),
            auto_approve,
            registry,
            sandbox_mode: policy.mode(),
            os_sandbox,
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
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        let boundary = match self.sandbox_mode {
            SandboxMode::WorkspaceWrite => {
                if self.os_sandbox.enabled && OsSandbox::bwrap_available() {
                    " File tools are workspace-scoped. Bash runs in a bubblewrap sandbox \
(workspace RW, $HOME RO). High-risk commands prompt for approval unless --yes."
                } else {
                    " File tools are workspace-scoped. High-risk bash commands need approval \
unless --yes / ONE_AUTO_APPROVE=1."
                }
            }
            SandboxMode::FullAccess => {
                " Full filesystem access (--full-access); bash is not OS-sandboxed."
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
Stdout/stderr returned to the model are capped (~2000 lines / 50KB; large output \
is spilled to disk with a ~4KB head preview + path for read/grep)."
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

        if run_in_background {
            let id = self
                .registry
                .spawn(command.to_string(), self.cwd.clone(), timeout_secs)
                .await
                .map_err(|err| tool_error("bash", err))?;

            let text = format!(
                "Background task started\n\
                 task_id: {id}\n\
                 command: {command}\n\
                 Use bash_output with this task_id to poll or wait; bash_kill to stop.\n\
                 A [Background task completed] notice will appear when it finishes."
            );
            let mut details = json!({
                "background": true,
                "task_id": id,
                "command": command,
                "ok": true,
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

        let (prog, args) = self.os_sandbox.command_line(command);
        let mut child = Command::new(&prog)
            .args(&args)
            .current_dir(&self.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| tool_error("bash", err.to_string()))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let wait_result = timeout(Duration::from_secs(timeout_secs), child.wait()).await;

        let status = match wait_result {
            Ok(Ok(status)) => status,
            Ok(Err(err)) => return Err(tool_error("bash", err.to_string())),
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(tool_error("bash", "command timed out"));
            }
        };

        let mut stdout_buf = String::new();
        let mut stderr_buf = String::new();

        if let Some(mut stdout) = stdout {
            stdout
                .read_to_string(&mut stdout_buf)
                .await
                .map_err(|err| tool_error("bash", err.to_string()))?;
        }
        if let Some(mut stderr) = stderr {
            stderr
                .read_to_string(&mut stderr_buf)
                .await
                .map_err(|err| tool_error("bash", err.to_string()))?;
        }

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

        let code_label = match exit_code {
            Some(c) => c.to_string(),
            None => "signal".into(),
        };
        let sandboxed = self.os_sandbox.enabled && OsSandbox::bwrap_available();
        // Always surface sandbox status in the text the model/user sees
        // (details-only was easy to miss in the TUI).
        let sandbox_line = if sandboxed {
            format!(
                "sandbox: bwrap · mode={} · writes limited to workspace (+ --add-dir)",
                self.sandbox_mode.as_str()
            )
        } else if self.os_sandbox.enabled {
            "sandbox: requested but bwrap missing — bash is UNSANDBOXED".to_string()
        } else {
            format!(
                "sandbox: off · mode={} (use workspace-write default or unset --full-access)",
                self.sandbox_mode.as_str()
            )
        };
        let mut output = format!("exit {code_label}\n{sandbox_line}");
        let mut truncated = false;
        let mut spill_path: Option<String> = None;
        if !body.is_empty() {
            // Claude-style: large stdout/stderr → full file on disk + head preview.
            // (Head preview matches Claude Code; tail is available via PreviewStyle::Tail.)
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
            "sandboxed": sandboxed,
            "sandboxMode": self.sandbox_mode.as_str(),
            "truncated": truncated,
            "fullOutputPath": spill_path,
        });
        if let Some(d) = description {
            details
                .as_object_mut()
                .unwrap()
                .insert("description".into(), json!(d));
        }
        Ok(ToolOutput::text_with_details(output, details))
    }
}

/// `timeout_secs` preferred; `timeout` accepted (Claude). Values ≥ 1000 on `timeout` are ms.
fn resolve_timeout_secs(args: &serde_json::Value) -> Option<u64> {
    if let Some(s) = args.get("timeout_secs").and_then(|v| v.as_u64()) {
        return Some(s);
    }
    let t = args.get("timeout").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_i64().map(|n| n.max(0) as u64))
    })?;
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
        let outside = std::env::temp_dir().join(format!(
            "one-bash-leak-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&outside);

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
                    "command": format!("echo leaked > {}", outside.display())
                }),
            })
            .await;

        assert!(
            !outside.exists(),
            "bash OS sandbox must not create host file {}",
            outside.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
