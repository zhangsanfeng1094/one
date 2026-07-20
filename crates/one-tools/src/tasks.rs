//! Background bash task registry (Claude / Codex style).
//!
//! - `bash(run_in_background=true)` registers a task and returns immediately
//! - Completions are queued as plain-text notifications for the agent loop
//! - `bash_output` / `bash_kill` poll or stop tasks (no TUI status bar)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::time::timeout;

use crate::os_sandbox::OsSandbox;

const DEFAULT_OUTPUT_CHARS: usize = 50_000;

static TASK_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Completed,
    TimedOut,
    Killed,
    Failed,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Running => "running",
            TaskState::Completed => "completed",
            TaskState::TimedOut => "timed_out",
            TaskState::Killed => "killed",
            TaskState::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, TaskState::Running)
    }
}

#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub id: String,
    pub command: String,
    pub state: TaskState,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
    pub elapsed_ms: u64,
    pub notified: bool,
}

struct TaskInner {
    id: String,
    command: String,
    state: TaskState,
    exit_code: Option<i32>,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    error: Option<String>,
    started: Instant,
    finished: Option<Instant>,
    /// Delivered to agent notification queue.
    notified: bool,
    pid: Option<u32>,
    /// Process handle while running (for kill / wait).
    child: Option<Child>,
    /// Wakes waiters in `bash_output`.
    done: Arc<Notify>,
}

impl TaskInner {
    fn elapsed_ms(&self) -> u64 {
        let end = self.finished.unwrap_or_else(Instant::now);
        end.duration_since(self.started).as_millis() as u64
    }

    fn snapshot(&self) -> TaskSnapshot {
        TaskSnapshot {
            id: self.id.clone(),
            command: self.command.clone(),
            state: self.state,
            exit_code: self.exit_code,
            stdout: self.stdout.lock().expect("stdout lock").clone(),
            stderr: self.stderr.lock().expect("stderr lock").clone(),
            error: self.error.clone(),
            elapsed_ms: self.elapsed_ms(),
            notified: self.notified,
        }
    }
}

/// Shared registry for background shell tasks.
pub struct BackgroundTaskRegistry {
    tasks: Mutex<HashMap<String, TaskInner>>,
    /// Plain-text notifications drained by the agent before each LLM turn.
    notifications: Arc<Mutex<Vec<String>>>,
    /// OS sandbox applied to every spawned command.
    os_sandbox: Mutex<OsSandbox>,
}

impl Default for BackgroundTaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTaskRegistry {
    pub fn new() -> Self {
        Self::with_notification_queue(Arc::new(Mutex::new(Vec::new())))
    }

    /// Share a notification queue with other producers (e.g. agent jobs in one-cli).
    pub fn with_notification_queue(notifications: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            notifications,
            os_sandbox: Mutex::new(OsSandbox::disabled(std::env::temp_dir())),
        }
    }

    /// Update the OS sandbox used for new background tasks.
    pub fn set_os_sandbox(&self, sandbox: OsSandbox) {
        *self.os_sandbox.lock().expect("os_sandbox lock") = sandbox;
    }

    /// Queue the agent drains at turn boundaries (Claude-style notify).
    pub fn notification_queue(&self) -> Arc<Mutex<Vec<String>>> {
        self.notifications.clone()
    }

    fn next_id() -> String {
        let n = TASK_SEQ.fetch_add(1, Ordering::Relaxed);
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() % 0xFFFF)
            .unwrap_or(0);
        format!("bg_{ms:x}_{n}")
    }

    /// Spawn a background bash command. Returns task id immediately.
    pub async fn spawn(
        self: &Arc<Self>,
        command: String,
        cwd: PathBuf,
        timeout_secs: Option<u64>,
    ) -> Result<String, String> {
        let sandbox = self.os_sandbox.lock().expect("os_sandbox lock").clone();
        self.spawn_with_sandbox(command, cwd, timeout_secs, sandbox)
            .await
    }

    /// Spawn with an explicit OS sandbox (e.g. escalated / disabled bwrap).
    pub async fn spawn_with_sandbox(
        self: &Arc<Self>,
        command: String,
        cwd: PathBuf,
        timeout_secs: Option<u64>,
        sandbox: OsSandbox,
    ) -> Result<String, String> {
        let id = Self::next_id();
        let done = Arc::new(Notify::new());
        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

        let (prog, args) = sandbox.command_line(&command);
        let mut cmd = Command::new(&prog);
        cmd.args(&args)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(false);

        // Own process group so kill can reap descendants (sleep, pipelines, …).
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }

        let mut child = cmd
            .spawn()
            .map_err(|err| format!("failed to spawn: {err}"))?;

        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        {
            let mut tasks = self.tasks.lock().expect("tasks lock");
            tasks.insert(
                id.clone(),
                TaskInner {
                    id: id.clone(),
                    command: command.clone(),
                    state: TaskState::Running,
                    exit_code: None,
                    stdout: stdout_buf.clone(),
                    stderr: stderr_buf.clone(),
                    error: None,
                    started: Instant::now(),
                    finished: None,
                    notified: false,
                    pid,
                    child: Some(child),
                    done: done.clone(),
                },
            );
        }

        // Stream pipes into shared buffers (avoids pipe deadlock; allows mid-run snapshots).
        let out_target = stdout_buf.clone();
        tokio::spawn(async move {
            if let Some(mut out) = stdout {
                let mut bytes = Vec::new();
                let _ = out.read_to_end(&mut bytes).await;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if let Ok(mut g) = out_target.lock() {
                    *g = text;
                }
            }
        });
        let err_target = stderr_buf.clone();
        tokio::spawn(async move {
            if let Some(mut err) = stderr {
                let mut bytes = Vec::new();
                let _ = err.read_to_end(&mut bytes).await;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if let Ok(mut g) = err_target.lock() {
                    *g = text;
                }
            }
        });

        let registry = Arc::clone(self);
        let task_id = id.clone();

        tokio::spawn(async move {
            // Take child for exclusive wait.
            let mut child = {
                let mut tasks = registry.tasks.lock().expect("tasks lock");
                tasks
                    .get_mut(&task_id)
                    .and_then(|t| t.child.take())
            };

            let wait_result = async {
                match child.as_mut() {
                    Some(c) => c.wait().await.map(Some),
                    None => Ok(None), // already taken by kill
                }
            };

            let outcome = if let Some(secs) = timeout_secs {
                match timeout(Duration::from_secs(secs), wait_result).await {
                    Ok(Ok(status)) => Ok(status),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(_) => {
                        // Hard timeout: kill process group + child.
                        if let Some(ref mut c) = child {
                            force_kill(c);
                            let _ = c.wait().await;
                        } else {
                            registry.force_kill_pid(&task_id);
                        }
                        registry.finalize(
                            &task_id,
                            TaskState::TimedOut,
                            None,
                            Some(format!("background command timed out after {secs}s")),
                        );
                        done.notify_waiters();
                        return;
                    }
                }
            } else {
                wait_result.await.map_err(|e| e.to_string())
            };

            match outcome {
                Ok(Some(status)) => {
                    // Let pipe readers finish after EOF (process already reaped).
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    // If kill already finalized, keep Killed and only fill exit if missing.
                    let already = {
                        let tasks = registry.tasks.lock().expect("tasks lock");
                        tasks
                            .get(&task_id)
                            .map(|t| t.state.is_terminal())
                            .unwrap_or(true)
                    };
                    if already {
                        // Still record exit code if we have it and state is Killed.
                        let mut tasks = registry.tasks.lock().expect("tasks lock");
                        if let Some(t) = tasks.get_mut(&task_id) {
                            if t.exit_code.is_none() {
                                t.exit_code = status.code();
                            }
                        }
                    } else {
                        registry.finalize(
                            &task_id,
                            TaskState::Completed,
                            status.code(),
                            None,
                        );
                    }
                }
                Ok(None) => {
                    // Child taken by kill path — kill finalizes.
                }
                Err(err) => {
                    let already = {
                        let tasks = registry.tasks.lock().expect("tasks lock");
                        tasks
                            .get(&task_id)
                            .map(|t| t.state.is_terminal())
                            .unwrap_or(true)
                    };
                    if !already {
                        registry.finalize(&task_id, TaskState::Failed, None, Some(err));
                    }
                }
            }
            done.notify_waiters();
        });

        Ok(id)
    }

    fn force_kill_pid(&self, id: &str) {
        let mut tasks = self.tasks.lock().expect("tasks lock");
        if let Some(t) = tasks.get_mut(id) {
            if let Some(mut child) = t.child.take() {
                force_kill(&mut child);
            } else if let Some(pid) = t.pid {
                kill_pid(pid);
            }
        }
    }

    fn finalize(
        &self,
        id: &str,
        state: TaskState,
        exit_code: Option<i32>,
        error: Option<String>,
    ) {
        let mut tasks = self.tasks.lock().expect("tasks lock");
        let Some(task) = tasks.get_mut(id) else {
            return;
        };
        if task.state.is_terminal() {
            return;
        }
        task.state = state;
        task.exit_code = exit_code;
        task.error = error;
        task.finished = Some(Instant::now());
        task.child = None;

        if !task.notified {
            task.notified = true;
            let snap = task.snapshot();
            drop(tasks);
            self.push_notification(format_completion_notification(&snap));
        }
    }

    fn push_notification(&self, text: String) {
        self.notifications
            .lock()
            .expect("notifications lock")
            .push(text);
    }

    /// Snapshot a single task.
    pub fn get(&self, id: &str) -> Option<TaskSnapshot> {
        self.tasks
            .lock()
            .expect("tasks lock")
            .get(id)
            .map(|t| t.snapshot())
    }

    /// List all tasks.
    pub fn list(&self) -> Vec<TaskSnapshot> {
        let mut list: Vec<_> = self
            .tasks
            .lock()
            .expect("tasks lock")
            .values()
            .map(|t| t.snapshot())
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// Wait until task is terminal, or `timeout_secs` elapses (`None`/`0` = immediate snapshot).
    pub async fn wait(&self, id: &str, timeout_secs: Option<u64>) -> Result<TaskSnapshot, String> {
        let done = {
            let tasks = self.tasks.lock().expect("tasks lock");
            let task = tasks
                .get(id)
                .ok_or_else(|| format!("unknown task_id: {id}"))?;
            if task.state.is_terminal() {
                return Ok(task.snapshot());
            }
            task.done.clone()
        };

        let secs = timeout_secs.unwrap_or(0);
        if secs == 0 {
            return self
                .get(id)
                .ok_or_else(|| format!("unknown task_id: {id}"));
        }

        let _ = timeout(Duration::from_secs(secs), done.notified()).await;
        self.get(id)
            .ok_or_else(|| format!("unknown task_id: {id}"))
    }

    /// Kill a running task.
    pub async fn kill(&self, id: &str) -> Result<TaskSnapshot, String> {
        let (child, done, already_terminal) = {
            let mut tasks = self.tasks.lock().expect("tasks lock");
            let task = tasks
                .get_mut(id)
                .ok_or_else(|| format!("unknown task_id: {id}"))?;
            if task.state.is_terminal() {
                return Ok(task.snapshot());
            }
            let child = task.child.take();
            let pid = task.pid;
            let done = task.done.clone();
            // Mark killed immediately so wait path won't overwrite.
            task.state = TaskState::Killed;
            task.finished = Some(Instant::now());
            task.error = Some("killed by bash_kill".into());
            if !task.notified {
                task.notified = true;
                let snap = task.snapshot();
                drop(tasks);
                self.push_notification(format_completion_notification(&snap));
            } else {
                drop(tasks);
            }
            // If child was already taken by the wait task, kill by pid.
            if child.is_none() {
                if let Some(pid) = pid {
                    kill_pid(pid);
                }
            }
            (child, done, false)
        };
        let _ = already_terminal;

        if let Some(mut child) = child {
            force_kill(&mut child);
            tokio::spawn(async move {
                let _ = child.wait().await;
                done.notify_waiters();
            });
        } else {
            done.notify_waiters();
        }

        // Brief yield so wait() reapers can settle.
        tokio::task::yield_now().await;
        self.get(id)
            .ok_or_else(|| format!("unknown task_id: {id}"))
    }
}

fn force_kill(child: &mut Child) {
    if let Some(pid) = child.id() {
        kill_pid(pid);
    }
    let _ = child.start_kill();
}

fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        // Negative pid → process group (spawn used process_group(0)).
        unsafe {
            extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            const SIGKILL: i32 = 9;
            let _ = kill(-(pid as i32), SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

pub fn format_completion_notification(snap: &TaskSnapshot) -> String {
    let mut out = String::new();
    out.push_str("[Background task completed]\n");
    out.push_str(&format!("task_id: {}\n", snap.id));
    out.push_str(&format!("command: {}\n", snap.command));
    out.push_str(&format!("status: {}\n", snap.state.as_str()));
    match snap.exit_code {
        Some(c) => out.push_str(&format!("exit: {c}\n")),
        None if snap.state == TaskState::Killed => out.push_str("exit: killed\n"),
        None if snap.state == TaskState::TimedOut => out.push_str("exit: timeout\n"),
        None => out.push_str("exit: unknown\n"),
    }
    out.push_str(&format!("elapsed_ms: {}\n", snap.elapsed_ms));
    if let Some(err) = &snap.error {
        out.push_str(&format!("error: {err}\n"));
    }
    let body = format_output_body(&snap.stdout, &snap.stderr, DEFAULT_OUTPUT_CHARS);
    if !body.is_empty() {
        out.push_str(&body);
    }
    out
}

pub fn format_task_output(snap: &TaskSnapshot, max_chars: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!("task_id: {}\n", snap.id));
    out.push_str(&format!("command: {}\n", snap.command));
    out.push_str(&format!("status: {}\n", snap.state.as_str()));
    match snap.exit_code {
        Some(c) => out.push_str(&format!("exit: {c}\n")),
        None if snap.state == TaskState::Running => out.push_str("exit: (running)\n"),
        None if snap.state == TaskState::Killed => out.push_str("exit: killed\n"),
        None if snap.state == TaskState::TimedOut => out.push_str("exit: timeout\n"),
        None => out.push_str("exit: unknown\n"),
    }
    out.push_str(&format!("elapsed_ms: {}\n", snap.elapsed_ms));
    if let Some(err) = &snap.error {
        out.push_str(&format!("error: {err}\n"));
    }
    let body = format_output_body(&snap.stdout, &snap.stderr, max_chars);
    if !body.is_empty() {
        out.push_str(&body);
    } else if snap.state == TaskState::Running {
        out.push_str("(no output yet)\n");
    }
    out
}

fn format_output_body(stdout: &str, stderr: &str, max_chars: usize) -> String {
    let mut body = String::new();
    if !stdout.is_empty() {
        body.push_str("--- stdout ---\n");
        body.push_str(stdout.trim_end());
        body.push('\n');
    }
    if !stderr.is_empty() {
        body.push_str("--- stderr ---\n");
        body.push_str(stderr.trim_end());
        body.push('\n');
    }
    // Claude-style spill when huge; head preview + full path under tool-outputs.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let presented = crate::truncate::present_tool_output(
        &body,
        "bash_output",
        &cwd,
        crate::truncate::PreviewStyle::Head,
    );
    // Honor caller's max_chars as an extra safety net on the model-facing text.
    truncate_chars(&presented.text, max_chars)
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!(
        "{truncated}\n… [truncated, {} chars total]",
        s.chars().count()
    )
}

pub fn format_task_list(tasks: &[TaskSnapshot]) -> String {
    if tasks.is_empty() {
        return "No background tasks.".to_string();
    }
    let mut out = String::from("Background tasks:\n");
    for t in tasks {
        let exit = match t.exit_code {
            Some(c) => c.to_string(),
            None if t.state == TaskState::Running => "-".into(),
            None => t.state.as_str().into(),
        };
        // Compact one-liner like Codex `/ps`.
        let cmd = if t.command.len() > 60 {
            format!("{}…", &t.command[..57])
        } else {
            t.command.clone()
        };
        out.push_str(&format!(
            "  {}  {:10}  exit={}  {}ms  {}\n",
            t.id,
            t.state.as_str(),
            exit,
            t.elapsed_ms,
            cmd
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn background_task_completes_and_notifies() {
        let reg = Arc::new(BackgroundTaskRegistry::new());
        let id = reg
            .spawn(
                "echo hello-bg; echo err >&2; exit 0".into(),
                std::env::temp_dir(),
                Some(30),
            )
            .await
            .unwrap();

        let snap = reg.wait(&id, Some(10)).await.unwrap();
        assert_eq!(snap.state, TaskState::Completed);
        assert_eq!(snap.exit_code, Some(0));
        assert!(snap.stdout.contains("hello-bg"), "stdout={}", snap.stdout);

        let notes = reg.notification_queue().lock().unwrap().clone();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains(&id));
        assert!(notes[0].contains("[Background task completed]"));
    }

    #[tokio::test]
    async fn kill_running_task() {
        let reg = Arc::new(BackgroundTaskRegistry::new());
        let id = reg
            .spawn("sleep 30".into(), std::env::temp_dir(), Some(60))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(150)).await;
        let snap = reg.kill(&id).await.unwrap();
        assert_eq!(snap.state, TaskState::Killed);

        // wait should resolve quickly now
        let snap2 = reg.wait(&id, Some(5)).await.unwrap();
        assert_eq!(snap2.state, TaskState::Killed);
    }

    #[tokio::test]
    async fn list_and_output_format() {
        let reg = Arc::new(BackgroundTaskRegistry::new());
        let id = reg
            .spawn("echo x".into(), std::env::temp_dir(), Some(10))
            .await
            .unwrap();
        let _ = reg.wait(&id, Some(5)).await.unwrap();
        let list = reg.list();
        assert_eq!(list.len(), 1);
        let text = format_task_list(&list);
        assert!(text.contains(&id));
    }

    #[tokio::test]
    async fn non_zero_exit_still_completes() {
        let reg = Arc::new(BackgroundTaskRegistry::new());
        let id = reg
            .spawn("exit 7".into(), std::env::temp_dir(), Some(10))
            .await
            .unwrap();
        let snap = reg.wait(&id, Some(5)).await.unwrap();
        assert_eq!(snap.state, TaskState::Completed);
        assert_eq!(snap.exit_code, Some(7));
    }
}
