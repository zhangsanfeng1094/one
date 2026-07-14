use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use one_core::error::Result;
use one_core::tool::{invalid_args, tool_error, Tool, ToolCall, ToolDefinition, ToolOutput};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_TIMEOUT_SECS: u64 = 120;

pub struct BashTool {
    cwd: PathBuf,
    auto_approve: bool,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_auto_approve(cwd, true)
    }

    pub fn with_auto_approve(cwd: PathBuf, auto_approve: bool) -> Self {
        Self { cwd, auto_approve }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: "Execute a shell command in the project working directory.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer" }
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

        if let Some(pattern) = crate::sandbox::is_command_blocked(command) {
            return Err(tool_error(
                "bash",
                format!("blocked command pattern: {pattern}"),
            ));
        }

        if !self.auto_approve {
            if let Some(pattern) = crate::sandbox::requires_confirmation(command) {
                let approved = std::env::var("ONE_AUTO_APPROVE")
                    .or_else(|_| std::env::var("PI_AUTO_APPROVE"))
                    .ok()
                    .as_deref()
                    == Some("1");
                if !approved {
                    return Err(tool_error(
                        "bash",
                        format!(
                            "command requires approval (matched `{pattern}`). Re-run with --yes or ONE_AUTO_APPROVE=1"
                        ),
                    ));
                }
            }
        }

        let timeout_secs = call
            .arguments
            .get("timeout_secs")
            .and_then(|value| value.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let mut child = Command::new("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(&self.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| tool_error("bash", err.to_string()))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let wait_result = timeout(
            Duration::from_secs(timeout_secs),
            child.wait(),
        )
        .await;

        let status = match wait_result {
            Ok(Ok(status)) => status,
            Ok(Err(err)) => return Err(tool_error("bash", err.to_string())),
            Err(_) => return Err(tool_error("bash", "command timed out")),
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

        // `None` means killed by signal — treat as failure in details.
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

        // Always lead with exit line so the TUI can color / summarize reliably.
        let code_label = match exit_code {
            Some(c) => c.to_string(),
            None => "signal".into(),
        };
        let mut output = format!("exit {code_label}");
        if !body.is_empty() {
            output.push('\n');
            output.push_str(body.trim_end());
        }

        Ok(ToolOutput::text_with_details(
            output,
            json!({
                "exitCode": exit_code,
                "command": command,
                "ok": status.success(),
            }),
        ))
    }
}