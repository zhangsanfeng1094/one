//! Per-process native control plane for a live One runtime.
//!
//! Each process exposes one user-only Unix socket. The socket controls the
//! *same* AppRuntime as the active frontend; it never creates a sidecar runtime.

use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Notify};

use super::{jobs, AppRuntime, TaskToolHost};

pub const CONTROL_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ControlSessionInfo {
    pub id: Option<String>,
    pub path: Option<String>,
}

impl ControlSessionInfo {
    pub fn from_runtime(runtime: &AppRuntime) -> Self {
        match runtime.session.as_ref() {
            Some(session) => Self {
                id: Some(session.header().id.clone()),
                path: session
                    .session_file()
                    .map(|path| path.display().to_string()),
            },
            None => Self::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ControlCapabilities {
    pub prompt: bool,
    pub steer: bool,
    pub follow_up: bool,
    pub abort: bool,
    pub status: bool,
    pub session: bool,
}

#[derive(Debug)]
pub struct ControlPrompt {
    pub text: String,
    pub accepted: oneshot::Sender<()>,
}

pub type ControlPromptReceiver = mpsc::UnboundedReceiver<ControlPrompt>;

#[derive(Clone)]
pub struct RuntimeControlHandle {
    steering_queue: Arc<Mutex<Vec<String>>>,
    followup_queue: Arc<Mutex<Vec<String>>>,
    abort_flag: Arc<AtomicBool>,
    input_waker: Arc<Notify>,
    task_host: Option<Arc<TaskToolHost>>,
    prompt_tx: mpsc::UnboundedSender<ControlPrompt>,
    /// Unified runtime lifecycle status — single source of truth.
    status: super::status::RuntimeStatusStore,
    session: Arc<RwLock<ControlSessionInfo>>,
    pid: u32,
    process_start: String,
    cwd: String,
    frontend: String,
    run_mode: String,
    /// Whether this handle's transport can actually deliver a prompt into a
    /// frontend turn loop. Decided by the **frontend wiring** (interactive
    /// native-prompt channel / RPC stdin loop), never inferred from the
    /// `frontend` label string.
    prompt_transport: ControlPromptTransport,
}

/// How a `prompt` request reaches a live frontend loop on this handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPromptTransport {
    /// `one --mode rpc`: prompts arrive on the RPC stdin request loop and are
    /// answered with a response line — the transport supports prompting.
    RpcStdin,
    /// Interactive TUI consumes native prompts through `prompt_rx`.
    InteractiveChannel,
    /// No prompt consumer on this transport (print / json one-shot runs).
    None,
}

impl ControlPromptTransport {
    fn supports_prompt(self) -> bool {
        !matches!(self, Self::None)
    }

    fn label(self) -> &'static str {
        match self {
            Self::RpcStdin => "rpc_stdin",
            Self::InteractiveChannel => "interactive_channel",
            Self::None => "none",
        }
    }
}

impl RuntimeControlHandle {
    /// RPC transport: prompts come in as RPC requests on stdin.
    pub fn for_rpc(runtime: &AppRuntime) -> Self {
        Self::for_runtime_with_transport(runtime, "rpc", "rpc", ControlPromptTransport::RpcStdin)
    }

    /// Native control socket bound to the interactive TUI prompt channel.
    pub fn for_interactive(
        runtime: &AppRuntime,
        prompt_tx: mpsc::UnboundedSender<ControlPrompt>,
    ) -> Self {
        Self::build(
            runtime,
            "interactive",
            "interactive",
            ControlPromptTransport::InteractiveChannel,
            prompt_tx,
        )
    }

    /// Native control socket for one-shot frontends (print / json): control
    /// methods work, prompts are not consumable.
    pub fn for_oneshot(runtime: &AppRuntime, frontend: &str) -> Self {
        let (prompt_tx, _prompt_rx) = mpsc::unbounded_channel();
        Self::build(
            runtime,
            frontend,
            frontend,
            ControlPromptTransport::None,
            prompt_tx,
        )
    }

    /// Legacy entry point retained for tests: explicit transport.
    fn for_runtime_with_transport(
        runtime: &AppRuntime,
        frontend: &str,
        run_mode: &str,
        transport: ControlPromptTransport,
    ) -> Self {
        let (prompt_tx, _prompt_rx) = mpsc::unbounded_channel();
        Self::build(runtime, frontend, run_mode, transport, prompt_tx)
    }

    fn build(
        runtime: &AppRuntime,
        frontend: &str,
        run_mode: &str,
        transport: ControlPromptTransport,
        prompt_tx: mpsc::UnboundedSender<ControlPrompt>,
    ) -> Self {
        Self {
            steering_queue: runtime.steering_queue.clone(),
            followup_queue: runtime.followup_queue.clone(),
            abort_flag: runtime.abort_flag.clone(),
            input_waker: runtime.input_waker.clone(),
            task_host: runtime.task_host.clone(),
            prompt_tx,
            status: runtime.status.clone(),
            session: runtime.control_session.clone(),
            pid: std::process::id(),
            process_start: process_start_identity(),
            cwd: runtime.cwd.display().to_string(),
            frontend: frontend.to_string(),
            run_mode: run_mode.to_string(),
            prompt_transport: transport,
        }
    }

    /// Legacy alias used by tests: interactive prompt channel.
    pub fn for_runtime(runtime: &AppRuntime, frontend: &str) -> Self {
        match frontend {
            "interactive" => {
                let (prompt_tx, _prompt_rx) = mpsc::unbounded_channel();
                Self::build(
                    runtime,
                    frontend,
                    frontend,
                    ControlPromptTransport::InteractiveChannel,
                    prompt_tx,
                )
            }
            _ => Self::for_oneshot(runtime, frontend),
        }
    }

    /// Derived compatibility busy flag (turn in flight ⇔ busy).
    pub fn is_busy(&self) -> bool {
        self.status.is_busy()
    }

    /// Begin an in-flight turn on the unified status store (RPC prompt loop).
    pub fn busy_guard(&self) -> super::status::TurnGuard {
        self.status
            .turn_guard_fixed(super::status::TurnOutcome::Completed)
    }

    pub fn update_session(&self, session: ControlSessionInfo) {
        if let Ok(mut slot) = self.session.write() {
            *slot = session;
        }
    }

    pub fn status(&self) -> Value {
        let session = self.session.read().map(|v| v.clone()).unwrap_or_default();
        let state = self.status.state();
        let activity = self
            .status
            .activity()
            .map(|a| serde_json::to_value(a).unwrap_or(Value::Null));
        json!({
            "protocol_version": CONTROL_PROTOCOL_VERSION,
            "pid": self.pid,
            "process_start": self.process_start,
            "cwd": self.cwd,
            "frontend": self.frontend,
            "run_mode": self.run_mode,
            // Authoritative lifecycle (idle/running/waiting_input/waiting_work/abort_requested).
            "state": state.as_str(),
            // Compatibility derived field: busy ⇔ turn in flight.
            "busy": self.status.is_busy(),
            "activity": activity,
            "current_turn": self.status.current_turn(),
            "last_outcome": self.status.last_outcome().map(|o| o.as_str()),
            "session_id": session.id,
            "session_path": session.path,
            "capabilities": ControlCapabilities {
                prompt: self.prompt_transport.supports_prompt(),
                steer: true,
                follow_up: true,
                abort: true,
                status: true,
                session: true,
            },
        })
    }

    pub fn steer(&self, text: String) {
        if let Ok(mut queue) = self.steering_queue.lock() {
            queue.push(text);
        }
        self.input_waker.notify_one();
    }

    pub fn follow_up(&self, text: String) {
        if let Ok(mut queue) = self.followup_queue.lock() {
            queue.push(text);
        }
        self.input_waker.notify_one();
    }

    pub fn abort(&self) {
        self.abort_flag.store(true, Ordering::SeqCst);
        self.status.set_abort_requested();
        self.input_waker.notify_one();
        if let Some(host) = &self.task_host {
            host.jobs()
                .kill_all_with_reason(jobs::KillReason::ParentAbort);
        }
    }
}

pub struct ControlServerGuard {
    endpoint: PathBuf,
    server_task: tokio::task::JoinHandle<()>,
    prompt_rx: Option<ControlPromptReceiver>,
    handle: RuntimeControlHandle,
}

impl ControlServerGuard {
    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    pub fn handle(&self) -> RuntimeControlHandle {
        self.handle.clone()
    }

    pub fn take_prompt_rx(&mut self) -> Option<ControlPromptReceiver> {
        self.prompt_rx.take()
    }
}

impl Drop for ControlServerGuard {
    fn drop(&mut self) {
        self.server_task.abort();
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

pub fn process_start_identity() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(start) = linux_process_start(std::process::id()) {
            return start;
        }
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn parse_endpoint_name(name: &str) -> Option<(u32, &str)> {
    let stem = name.strip_prefix("one-")?.strip_suffix(".sock")?;
    let (pid, start) = stem.split_once('-')?;
    if pid.is_empty()
        || start.is_empty()
        || !pid.bytes().all(|b| b.is_ascii_digit())
        || !start.bytes().all(|b| b.is_ascii_digit())
        || pid.starts_with('0')
        || start.starts_with('0')
    {
        return None;
    }
    let parsed = pid.parse::<u32>().ok()?;
    let parsed_start = start.parse::<u64>().ok()?;
    if parsed == 0 || parsed_start == 0 {
        return None;
    }
    Some((parsed, start))
}

#[cfg(target_os = "linux")]
fn linux_process_start(pid: u32) -> io::Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat
        .rfind(')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid proc stat"))?;
    // After comm, token 0 is field 3 (state); starttime is field 22.
    stat[close + 1..]
        .split_whitespace()
        .nth(19)
        .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_owned)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing proc starttime"))
}

#[cfg(target_os = "linux")]
fn cleanup_stale_endpoints(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some((pid, start)) = parse_endpoint_name(&name) else {
            continue;
        };
        let path = entry.path();
        if !entry.file_type()?.is_socket() {
            continue;
        }
        let stale = match linux_process_start(pid) {
            Ok(actual) => actual != start,
            Err(err) if err.kind() == io::ErrorKind::NotFound => true,
            Err(err) => {
                tracing::debug!(path = %path.display(), error = %err, "cannot validate native control endpoint");
                false
            }
        };
        if stale {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_effective_uid() -> io::Result<u32> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let line = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing proc uid"))?;
    line.split_whitespace()
        .nth(2)
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid effective uid"))
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|meta| meta.uid())
        .or_else(|_| {
            std::env::var("UID")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "uid unavailable"))
        })
        .unwrap_or(0)
}

pub fn control_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("one").join("control");
    }
    #[cfg(unix)]
    {
        return PathBuf::from(format!("/tmp/one-{}", effective_uid())).join("control");
    }
    #[allow(unreachable_code)]
    std::env::temp_dir().join("one-control")
}

/// Start the per-process native control socket.
///
/// `transport` declares how prompts can reach a live frontend loop on this
/// process (interactive channel / rpc stdin / none) — capabilities report it
/// verbatim instead of guessing from the frontend label.
pub async fn start_control_server(
    runtime: &AppRuntime,
    frontend: &str,
    transport: ControlPromptTransport,
) -> io::Result<ControlServerGuard> {
    use std::os::unix::fs::PermissionsExt;

    let dir = control_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;

    let pid = std::process::id();
    let process_start = process_start_identity();
    let endpoint = dir.join(format!("one-{pid}-{process_start}.sock"));
    #[cfg(target_os = "linux")]
    cleanup_stale_endpoints(&dir)?;

    let listener = UnixListener::bind(&endpoint)?;
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600))?;

    let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
    let handle = RuntimeControlHandle::build(runtime, frontend, frontend, transport, prompt_tx);

    let server_handle = handle.clone();
    let server_task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let connection_handle = server_handle.clone();
            tokio::spawn(async move {
                if let Err(err) = serve_connection(stream, connection_handle).await {
                    tracing::debug!(error = %err, "native control connection ended");
                }
            });
        }
    });

    Ok(ControlServerGuard {
        endpoint,
        server_task,
        prompt_rx: Some(prompt_rx),
        handle,
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct ControlRequest {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// An endpoint whose filename and live server agreed on protocol and process identity.
#[derive(Debug, Clone, Serialize)]
pub struct LiveControlEndpoint {
    pub endpoint: PathBuf,
    #[serde(flatten)]
    pub status: Value,
}

fn endpoint_identity(path: &Path) -> io::Result<(u32, String)> {
    if path.parent() != Some(control_dir().as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "endpoint is outside the current user's Native Control directory",
        ));
    }
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or_default();
    let (pid, start) = parse_endpoint_name(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Native Control endpoint name",
        )
    })?;
    if !std::fs::symlink_metadata(path)?.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Native Control endpoint is not a socket",
        ));
    }
    #[cfg(target_os = "linux")]
    if linux_process_start(pid)? != start {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Native Control endpoint has a stale process identity",
        ));
    }
    Ok((pid, start.to_owned()))
}

async fn client_round_trip(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    method: &str,
    params: Value,
) -> io::Result<Value> {
    let request = ControlRequest {
        id: json!(method),
        method: method.to_owned(),
        params,
    };
    let mut encoded = serde_json::to_vec(&request).map_err(io::Error::other)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    let line = lines.next_line().await?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Native Control closed the connection",
        )
    })?;
    let response: Value = serde_json::from_str(&line).map_err(io::Error::other)?;
    if response.get("id") != Some(&json!(method)) || !response["ok"].is_boolean() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Native Control response",
        ));
    }
    if response["ok"] == false {
        let error = &response["error"];
        let code = error["code"].as_str().unwrap_or("native_error");
        let message = error["message"]
            .as_str()
            .unwrap_or("Native Control request failed");
        let allowed = error["details"]["allowed"].as_array().map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        });
        let detail = allowed
            .map(|v| format!("; allowed: {v}"))
            .unwrap_or_default();
        return Err(io::Error::other(format!("{code}: {message}{detail}")));
    }
    response.get("result").cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Native Control response has no result",
        )
    })
}

/// Connect, validate a strict handshake, then run one method on that same connection.
pub async fn control_call(path: &Path, method: &str, params: Value) -> io::Result<(Value, Value)> {
    control_call_with_timeout(path, method, params, Duration::from_secs(4)).await
}

async fn control_call_with_timeout(
    path: &Path,
    method: &str,
    params: Value,
    timeout: Duration,
) -> io::Result<(Value, Value)> {
    let (pid, start) = endpoint_identity(path)?;
    tokio::time::timeout(timeout, async {
        let stream = UnixStream::connect(path).await?;
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let status = client_round_trip(
            &mut lines,
            &mut writer,
            "handshake",
            json!({
                "protocol_version": CONTROL_PROTOCOL_VERSION, "pid": pid, "process_start": start,
            }),
        )
        .await?;
        if status["protocol_version"] != CONTROL_PROTOCOL_VERSION
            || status["pid"] != pid
            || status["process_start"] != start
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Native Control handshake identity mismatch",
            ));
        }
        let result = if method == "handshake" {
            status.clone()
        } else {
            client_round_trip(&mut lines, &mut writer, method, params).await?
        };
        Ok((status, result))
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Native Control request timed out"))?
}

pub async fn discover_live_endpoints() -> io::Result<Vec<LiveControlEndpoint>> {
    let dir = control_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut live = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if let Ok((status, _)) =
            control_call_with_timeout(&path, "handshake", Value::Null, Duration::from_millis(750))
                .await
        {
            live.push(LiveControlEndpoint {
                endpoint: path,
                status,
            });
        }
    }
    live.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
    Ok(live)
}

/// Discovery for session entry: a socket owned by a currently valid process
/// whose handshake fails is an uncertain runtime, so callers must not open a
/// second writer until it can be resolved.
pub async fn discover_live_endpoints_strict() -> io::Result<Vec<LiveControlEndpoint>> {
    let dir = control_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut live = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if endpoint_identity(&path).is_err() {
            continue; // stale endpoint or unrelated file
        }
        let (status, _) =
            control_call_with_timeout(&path, "handshake", Value::Null, Duration::from_millis(750))
                .await
                .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", path.display())))?;
        live.push(LiveControlEndpoint {
            endpoint: path,
            status,
        });
    }
    live.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
    Ok(live)
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({"id": id, "ok": true, "result": result})
}

fn error_response(id: Value, code: &str, message: impl Into<String>, extra: Value) -> Value {
    json!({
        "id": id,
        "ok": false,
        "error": {"code": code, "message": message.into(), "details": extra}
    })
}

async fn serve_connection(stream: UnixStream, handle: RuntimeControlHandle) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let peer_uid = stream.peer_cred()?.uid();
        if peer_uid != linux_effective_uid()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "native control peer uid mismatch",
            ));
        }
    }
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<ControlRequest>(&line) {
            Ok(request) => handle_request(&handle, request).await,
            Err(err) => error_response(Value::Null, "invalid_request", err.to_string(), json!({})),
        };
        let mut encoded = serde_json::to_vec(&response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        encoded.push(b'\n');
        writer.write_all(&encoded).await?;
        writer.flush().await?;
    }
    Ok(())
}

async fn handle_request(handle: &RuntimeControlHandle, request: ControlRequest) -> Value {
    let id = request.id;
    match request.method.as_str() {
        "ping" | "status" => ok_response(id, handle.status()),
        "handshake" => {
            if let Some(version) = request.params.get("protocol_version") {
                if version.as_u64() != Some(CONTROL_PROTOCOL_VERSION as u64) {
                    return error_response(
                        id,
                        "protocol_mismatch",
                        "native control protocol version mismatch",
                        json!({"expected": CONTROL_PROTOCOL_VERSION, "received": version}),
                    );
                }
            }
            let expected_pid = request.params.get("pid");
            let expected_start = request.params.get("process_start");
            match (expected_pid, expected_start) {
                (None, None) => {}
                (Some(pid), Some(start)) => {
                    if pid.as_u64() != Some(handle.pid as u64)
                        || start.as_str() != Some(handle.process_start.as_str())
                    {
                        return error_response(
                            id,
                            "identity_mismatch",
                            "native control endpoint identity mismatch",
                            json!({"expected_pid": handle.pid, "expected_process_start": handle.process_start}),
                        );
                    }
                }
                _ => {
                    return error_response(
                        id,
                        "invalid_params",
                        "pid and process_start must be provided together",
                        json!({}),
                    );
                }
            }
            ok_response(id, handle.status())
        }
        "capabilities" => {
            let status = handle.status();
            ok_response(
                id,
                status
                    .get("capabilities")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            )
        }
        "session" => {
            let status = handle.status();
            ok_response(
                id,
                json!({
                    "session_id": status.get("session_id").cloned().unwrap_or(Value::Null),
                    "session_path": status.get("session_path").cloned().unwrap_or(Value::Null),
                }),
            )
        }
        "steer" => match text_param(&request.params) {
            Some(text) => {
                handle.steer(text);
                ok_response(id, json!({"applied_as": "native_steer"}))
            }
            None => error_response(id, "invalid_params", "params.text required", json!({})),
        },
        "follow_up" | "followup" => match text_param(&request.params) {
            Some(text) => {
                handle.follow_up(text);
                ok_response(id, json!({"applied_as": "native_follow_up"}))
            }
            None => error_response(id, "invalid_params", "params.text required", json!({})),
        },
        "abort" => {
            handle.abort();
            ok_response(
                id,
                json!({
                    "applied_as": "native_interrupt",
                    "scope": "current_turn",
                    "session_preserved": true,
                }),
            )
        }
        "prompt" => {
            let Some(text) = text_param(&request.params) else {
                return error_response(id, "invalid_params", "params.text required", json!({}));
            };
            // Session Browser pins a prompt to the identity selected by the
            // user. Manual `one control prompt` omits these optional fields.
            if let Some(expected) = request.params.get("expected_session_id") {
                let status = handle.status();
                if status["session_id"] != *expected
                    || status["session_path"] != request.params["expected_session_path"]
                {
                    return error_response(
                        id,
                        "session_changed",
                        "runtime switched sessions after selection",
                        json!({}),
                    );
                }
            }
            if handle.is_busy() {
                return error_response(
                    id,
                    "busy",
                    "runtime is busy; choose steer, follow_up, or abort explicitly",
                    json!({"allowed": ["steer", "follow_up", "abort"]}),
                );
            }
            if !handle.prompt_transport.supports_prompt() {
                return error_response(
                    id,
                    "unsupported",
                    "native prompt needs a live prompt consumer (interactive TUI or rpc stdin)",
                    json!({"allowed": ["steer", "follow_up", "abort"]}),
                );
            }
            let (accepted_tx, accepted_rx) = oneshot::channel();
            if handle
                .prompt_tx
                .send(ControlPrompt {
                    text,
                    accepted: accepted_tx,
                })
                .is_err()
            {
                return error_response(
                    id,
                    "frontend_closed",
                    "interactive frontend is unavailable",
                    json!({}),
                );
            }
            match tokio::time::timeout(Duration::from_secs(2), accepted_rx).await {
                Ok(Ok(())) => {
                    ok_response(id, json!({"applied_as": "native_prompt", "accepted": true}))
                }
                Ok(Err(_)) => error_response(
                    id,
                    "frontend_closed",
                    "interactive frontend closed",
                    json!({}),
                ),
                Err(_) => error_response(
                    id,
                    "frontend_timeout",
                    "interactive frontend did not accept prompt",
                    json!({}),
                ),
            }
        }
        other => error_response(
            id,
            "method_not_found",
            format!("unknown method: {other}"),
            json!({}),
        ),
    }
}

fn text_param(params: &Value) -> Option<String> {
    params
        .get("text")
        .or_else(|| params.get("prompt"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_process_start_identity_is_stable() {
        let a = process_start_identity();
        let b = process_start_identity();
        #[cfg(target_os = "linux")]
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn endpoint_dir_is_process_local_namespace() {
        let dir = control_dir();
        assert!(dir.ends_with("one/control") || dir.ends_with("control"));
    }

    #[test]
    fn malformed_endpoint_names_are_rejected() {
        for name in [
            "one-1.sock",
            "one-0-123.sock",
            "one-01-123.sock",
            "one-1-0123.sock",
            "one-1-nope.sock",
            "one-1-123.sock.bak",
            "other-1-123.sock",
            "one-4294967296-123.sock",
        ] {
            assert!(parse_endpoint_name(name).is_none(), "accepted {name}");
        }
        assert_eq!(parse_endpoint_name("one-12-345.sock"), Some((12, "345")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_endpoint_cleanup_preserves_live_identity_and_unrelated_entries() {
        let dir = std::env::temp_dir().join(format!(
            "one-control-cleanup-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let pid = std::process::id();
        let start = linux_process_start(pid).unwrap();
        let wrong_start = if start == "1" { "2" } else { "1" };
        let absent_pid = u32::MAX;
        assert_eq!(
            linux_process_start(absent_pid).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        let missing = dir.join(format!("one-{absent_pid}-1.sock"));
        let wrong = dir.join(format!("one-{pid}-{wrong_start}.sock"));
        let live = dir.join(format!("one-{pid}-{start}.sock"));
        let malformed = dir.join("one-bad-name.sock");
        let regular = dir.join(format!("one-{absent_pid}-2.sock"));
        let _listeners = [
            std::os::unix::net::UnixListener::bind(&missing).unwrap(),
            std::os::unix::net::UnixListener::bind(&wrong).unwrap(),
            std::os::unix::net::UnixListener::bind(&malformed).unwrap(),
        ];
        let live_listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
        drop(live_listener); // A live process identity must survive even when connect fails.
        std::fs::write(&regular, b"keep").unwrap();

        cleanup_stale_endpoints(&dir).unwrap();
        assert!(!missing.exists());
        assert!(!wrong.exists());
        assert!(live.exists());
        assert!(malformed.exists());
        assert!(regular.exists());

        drop(_listeners);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn fake_handle(prompt_supported: bool) -> (RuntimeControlHandle, ControlPromptReceiver) {
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        (
            RuntimeControlHandle {
                steering_queue: Arc::new(Mutex::new(Vec::new())),
                followup_queue: Arc::new(Mutex::new(Vec::new())),
                abort_flag: Arc::new(AtomicBool::new(false)),
                input_waker: Arc::new(Notify::new()),
                task_host: None,
                prompt_tx,
                status: super::super::status::RuntimeStatusStore::new(),
                session: Arc::new(RwLock::new(ControlSessionInfo::default())),
                pid: 42,
                process_start: "99".into(),
                cwd: "/tmp/test".into(),
                frontend: "interactive".into(),
                run_mode: "interactive".into(),
                prompt_transport: if prompt_supported {
                    ControlPromptTransport::InteractiveChannel
                } else {
                    ControlPromptTransport::None
                },
            },
            prompt_rx,
        )
    }

    #[tokio::test]
    async fn handshake_keeps_empty_params_compatible_and_validates_expected_identity() {
        let (handle, _rx) = fake_handle(true);
        let handshake = |params| ControlRequest {
            id: json!("handshake"),
            method: "handshake".into(),
            params,
        };

        let legacy = handle_request(&handle, handshake(json!({}))).await;
        assert_eq!(legacy["ok"], true);
        assert_eq!(
            legacy["result"]["protocol_version"],
            CONTROL_PROTOCOL_VERSION
        );
        assert_eq!(legacy["result"]["pid"], 42);
        assert_eq!(legacy["result"]["process_start"], "99");

        let full = handle_request(
            &handle,
            handshake(json!({"protocol_version": CONTROL_PROTOCOL_VERSION, "pid": 42, "process_start": "99"})),
        )
        .await;
        assert_eq!(full["ok"], true);
        assert_eq!(full["result"], legacy["result"]);

        let wrong_version = handle_request(
            &handle,
            handshake(json!({"protocol_version": CONTROL_PROTOCOL_VERSION + 1})),
        )
        .await;
        assert_eq!(wrong_version["error"]["code"], "protocol_mismatch");

        for params in [
            json!({"pid": 43, "process_start": "99"}),
            json!({"pid": 42, "process_start": "100"}),
        ] {
            let wrong_identity = handle_request(&handle, handshake(params)).await;
            assert_eq!(wrong_identity["error"]["code"], "identity_mismatch");
        }
        let partial = handle_request(&handle, handshake(json!({"pid": 42}))).await;
        assert_eq!(partial["error"]["code"], "invalid_params");
    }

    #[tokio::test]
    async fn busy_prompt_is_rejected_instead_of_becoming_followup() {
        let (handle, mut rx) = fake_handle(true);
        let _busy = handle.busy_guard();
        let response = handle_request(
            &handle,
            ControlRequest {
                id: json!("p1"),
                method: "prompt".into(),
                params: json!({"text":"next"}),
            },
        )
        .await;
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "busy");
        assert!(rx.try_recv().is_err());
        assert!(handle.followup_queue.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn steer_followup_and_abort_apply_without_frontend_polling() {
        let (handle, _rx) = fake_handle(true);
        let steer = handle_request(
            &handle,
            ControlRequest {
                id: json!(1),
                method: "steer".into(),
                params: json!({"text":"change course"}),
            },
        )
        .await;
        assert_eq!(steer["result"]["applied_as"], "native_steer");
        assert_eq!(
            handle.steering_queue.lock().unwrap().as_slice(),
            ["change course"]
        );

        let follow = handle_request(
            &handle,
            ControlRequest {
                id: json!(2),
                method: "follow_up".into(),
                params: json!({"text":"then test"}),
            },
        )
        .await;
        assert_eq!(follow["result"]["applied_as"], "native_follow_up");
        assert_eq!(
            handle.followup_queue.lock().unwrap().as_slice(),
            ["then test"]
        );

        let abort = handle_request(
            &handle,
            ControlRequest {
                id: json!(3),
                method: "abort".into(),
                params: json!({}),
            },
        )
        .await;
        assert_eq!(abort["result"]["applied_as"], "native_interrupt");
        assert!(handle.abort_flag.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn idle_native_prompt_is_delivered_over_channel() {
        let (handle, mut rx) = fake_handle(true);
        let h = handle.clone();
        let request = tokio::spawn(async move {
            handle_request(
                &h,
                ControlRequest {
                    id: json!("native"),
                    method: "prompt".into(),
                    params: json!({"text":"hello native"}),
                },
            )
            .await
        });
        let prompt = rx.recv().await.expect("native prompt");
        assert_eq!(prompt.text, "hello native");
        let _ = prompt.accepted.send(());
        let response = request.await.unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["applied_as"], "native_prompt");
    }

    // ── Real socket round-trip ─────────────────────────────────────────

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn sock_roundtrip(endpoint: &Path, requests: &[Value]) -> Vec<Value> {
        let stream = tokio::net::UnixStream::connect(endpoint)
            .await
            .expect("connect control socket");
        let (reader, mut writer) = stream.into_split();
        for req in requests {
            let mut line = serde_json::to_vec(req).unwrap();
            line.push(b'\n');
            writer.write_all(&line).await.unwrap();
        }
        let mut lines = BufReader::new(reader).lines();
        let mut out = Vec::new();
        for _ in requests {
            let line = lines.next_line().await.unwrap().expect("response line");
            out.push(serde_json::from_str(&line).unwrap());
        }
        out
    }

    #[tokio::test]
    async fn socket_roundtrip_ping_status_and_methods() {
        let dir = std::env::temp_dir().join(format!(
            "one-control-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let steering_queue = Arc::new(Mutex::new(Vec::new()));
        let followup_queue = Arc::new(Mutex::new(Vec::new()));
        let abort_flag = Arc::new(AtomicBool::new(false));
        let session = Arc::new(RwLock::new(ControlSessionInfo {
            id: Some("sess-A".into()),
            path: Some("/tmp/sessA.jsonl".into()),
        }));
        let (prompt_tx, _prompt_rx) = mpsc::unbounded_channel();
        let handle = RuntimeControlHandle {
            steering_queue: steering_queue.clone(),
            followup_queue: followup_queue.clone(),
            abort_flag: abort_flag.clone(),
            input_waker: Arc::new(Notify::new()),
            task_host: None,
            prompt_tx,
            status: super::super::status::RuntimeStatusStore::new(),
            session,
            pid: std::process::id(),
            process_start: process_start_identity(),
            cwd: "/tmp".into(),
            frontend: "interactive".into(),
            run_mode: "interactive".into(),
            prompt_transport: ControlPromptTransport::InteractiveChannel,
        };

        let endpoint = dir.join("one-test.sock");
        let listener = UnixListener::bind(&endpoint).unwrap();
        let server_handle = handle.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let h = server_handle.clone();
                tokio::spawn(async move {
                    let _ = serve_connection(stream, h).await;
                });
            }
        });

        let responses = sock_roundtrip(
            &endpoint,
            &[
                json!({"id":"1","method":"ping"}),
                json!({"id":"2","method":"status"}),
                json!({"id":"3","method":"steer","params":{"text":"go left"}}),
                json!({"id":"4","method":"follow_up","params":{"text":"then right"}}),
                json!({"id":"5","method":"session"}),
                json!({"id":"6","method":"capabilities"}),
                json!({"id":"handshake","method":"handshake","params":{"protocol_version": CONTROL_PROTOCOL_VERSION,"pid": handle.pid,"process_start": handle.process_start}}),
                json!({"id":"wrong-version","method":"handshake","params":{"protocol_version": CONTROL_PROTOCOL_VERSION + 1,"pid": handle.pid,"process_start": handle.process_start}}),
                json!({"id":"wrong-pid","method":"handshake","params":{"protocol_version": CONTROL_PROTOCOL_VERSION,"pid": handle.pid + 1,"process_start": handle.process_start}}),
            ],
        )
        .await;

        assert_eq!(responses[0]["ok"], true);
        assert_eq!(responses[0]["result"]["pid"], std::process::id() as u64);
        let status = &responses[1];
        assert_eq!(status["result"]["protocol_version"], 1);
        assert_eq!(status["result"]["frontend"], "interactive");
        assert_eq!(status["result"]["run_mode"], "interactive");
        assert_eq!(status["result"]["busy"], false);
        assert_eq!(status["result"]["session_id"], "sess-A");
        assert!(
            status["result"]["process_start"].is_string()
                && !status["result"]["process_start"]
                    .as_str()
                    .unwrap()
                    .is_empty()
        );
        assert_eq!(responses[2]["result"]["applied_as"], "native_steer");
        assert_eq!(responses[3]["result"]["applied_as"], "native_follow_up");
        assert_eq!(
            steering_queue.lock().unwrap().as_slice(),
            ["go left".to_string()]
        );
        assert_eq!(
            followup_queue.lock().unwrap().as_slice(),
            ["then right".to_string()]
        );
        assert_eq!(responses[4]["result"]["session_id"], "sess-A");
        assert_eq!(responses[5]["result"]["prompt"], true);
        assert_eq!(
            responses[6]["ok"], true,
            "same-user peer credential was rejected"
        );
        assert_eq!(responses[7]["error"]["code"], "protocol_mismatch");
        assert_eq!(responses[8]["error"]["code"], "identity_mismatch");

        // abort over the socket flips the shared flag immediately.
        let responses = sock_roundtrip(&endpoint, &[json!({"id":"7","method":"abort"})]).await;
        assert_eq!(responses[0]["result"]["scope"], "current_turn");
        assert!(abort_flag.load(Ordering::SeqCst));

        server.abort();
        let _ = std::fs::remove_file(&endpoint);
        let _ = std::fs::remove_dir(&dir);
    }

    #[tokio::test]
    async fn prompt_on_unsupported_frontend_returns_unsupported_not_busy() {
        // print/json frontends: idle prompt → unsupported (no interactive loop
        // to consume it); busy steer/follow_up/abort still apply.
        let (handle, _rx) = fake_handle(false);
        let response = handle_request(
            &handle,
            ControlRequest {
                id: json!(1),
                method: "prompt".into(),
                params: json!({"text":"hi"}),
            },
        )
        .await;
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "unsupported");

        let steer = handle_request(
            &handle,
            ControlRequest {
                id: json!(2),
                method: "steer".into(),
                params: json!({"text":"mid-turn"}),
            },
        )
        .await;
        assert_eq!(steer["ok"], true);
    }

    #[test]
    fn endpoint_filename_encodes_pid_and_process_start() {
        // Two processes (or PID-reuse) must never collide: the filename is
        // derived from pid + process_start identity.
        let dir = control_dir();
        let start = process_start_identity();
        let name = format!("one-{}-{start}.sock", std::process::id());
        let endpoint = dir.join(&name);
        assert!(endpoint.ends_with(&name));
        assert!(name.starts_with(&format!("one-{}-", std::process::id())));
    }

    #[test]
    fn control_dir_permissions_are_user_only() {
        let dir = control_dir();
        if let Ok(meta) = std::fs::metadata(&dir) {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o077, 0);
        }
    }
}
