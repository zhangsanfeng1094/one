#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(12);
const TURN_TIMEOUT: Duration = Duration::from_secs(15);

struct NativeHarness {
    root: PathBuf,
    runtime_dir: PathBuf,
    sessions: Vec<String>,
}

impl NativeHarness {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "one-native-control-e2e-{}-{nonce}",
            std::process::id()
        ));
        let runtime_dir = root.join("runtime");
        fs::create_dir_all(&runtime_dir).unwrap();
        Self {
            root,
            runtime_dir,
            sessions: Vec::new(),
        }
    }

    fn spawn_one(&mut self, label: &str) -> (String, PathBuf, PathBuf) {
        let agent_dir = self.root.join(format!("agent-{label}"));
        self.spawn_one_with_agent(label, agent_dir)
    }

    fn spawn_one_with_agent(
        &mut self,
        label: &str,
        agent_dir: PathBuf,
    ) -> (String, PathBuf, PathBuf) {
        let cwd = self.root.join(format!("cwd-{label}"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&agent_dir).unwrap();

        let session = format!(
            "one-nctl-{label}-{}-{}",
            std::process::id(),
            unique_suffix()
        );
        let one = env!("CARGO_BIN_EXE_one");
        let command = format!(
            "env XDG_RUNTIME_DIR={} ONE_AGENT_DIR={} {} --provider mock --no-mcp --no-skills --no-memory --no-subagent",
            sh_quote(&self.runtime_dir),
            sh_quote(&agent_dir),
            sh_quote(Path::new(one)),
        );

        let status = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &session,
                "-x",
                "120",
                "-y",
                "40",
                "-c",
                cwd.to_str().unwrap(),
                &command,
            ])
            .status()
            .expect("launch tmux session");
        assert!(status.success(), "tmux new-session failed for {label}");
        self.sessions.push(session.clone());
        (session, cwd, agent_dir)
    }

    fn control_dir(&self) -> PathBuf {
        self.runtime_dir.join("one/control")
    }

    fn socket_statuses(&self) -> Vec<(PathBuf, Value)> {
        let Ok(entries) = fs::read_dir(self.control_dir()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|v| v.to_str()) == Some("sock"))
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?;
                let stem = name.strip_prefix("one-")?.strip_suffix(".sock")?;
                let (pid, process_start) = stem.split_once('-')?;
                let pid = pid.parse::<u32>().ok()?;
                rpc_call(
                    &path,
                    "handshake",
                    json!({
                        "protocol_version": 1,
                        "pid": pid,
                        "process_start": process_start,
                    }),
                )
                .ok()
                .map(|v| (path, v))
            })
            .collect()
    }

    fn wait_for_cwds(&self, expected: &[&Path]) -> HashMap<PathBuf, (PathBuf, Value)> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            let mut found = HashMap::new();
            for (socket, response) in self.socket_statuses() {
                if !response["ok"].as_bool().unwrap_or(false) {
                    continue;
                }
                if let Some(cwd) = response["result"]["cwd"].as_str() {
                    found.insert(PathBuf::from(cwd), (socket, response));
                }
            }
            if expected.iter().all(|cwd| found.contains_key(*cwd)) {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for native control sockets; found={found:#?}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for NativeHarness {
    fn drop(&mut self) {
        for session in &self.sessions {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", session])
                .status();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % 1_000_000_000
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn rpc_call(socket: &Path, method: &str, params: Value) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let request = json!({"id": method, "method": method, "params": params});
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(std::io::Error::other)
}

fn control_cli(runtime_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_one"))
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .args(["control"])
        .args(args)
        .output()
        .expect("run one control")
}

fn control_json(runtime_dir: &Path, args: &[&str]) -> Value {
    let output = control_cli(runtime_dir, args);
    assert!(
        output.status.success(),
        "one control {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON stdout")
}

#[test]
fn control_cli_empty_and_bad_endpoints() {
    let harness = NativeHarness::new();
    assert_eq!(
        control_json(&harness.runtime_dir, &["list", "--json"]),
        json!([])
    );
    let dir = harness.control_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("one-bad.sock"), "bad").unwrap();
    fs::write(dir.join("one-12345-67890.sock"), "not a socket").unwrap();
    let stale = dir.join(format!("one-{}-1.sock", u32::MAX));
    let _listener = std::os::unix::net::UnixListener::bind(&stale).unwrap();
    // A valid-looking socket that accepts connections but never answers must
    // have a bounded handshake timeout.
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let start = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .nth(19)
        .unwrap();
    let hanging = dir.join(format!("one-{}-{start}.sock", std::process::id()));
    let _hanging_listener = std::os::unix::net::UnixListener::bind(&hanging).unwrap();
    let started = Instant::now();
    assert_eq!(
        control_json(&harness.runtime_dir, &["list", "--json"]),
        json!([])
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "list stalled on invalid endpoints"
    );
    let output = control_cli(&harness.runtime_dir, &["status", "--json"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no live runtime"));
}

#[test]
fn control_cli_multi_runtime_lifecycle() {
    if !tmux_available() {
        eprintln!("SKIP control_cli_multi_runtime_lifecycle: tmux is unavailable");
        return;
    }
    let mut harness = NativeHarness::new();
    let (_, cwd_a, _) = harness.spawn_one("cli-a");
    let (_, cwd_b, _) = harness.spawn_one("cli-b");
    let sockets = harness.wait_for_cwds(&[&cwd_a, &cwd_b]);
    let (socket_a, hello_a) = sockets.get(&cwd_a).unwrap();
    let (socket_b, hello_b) = sockets.get(&cwd_b).unwrap();
    let pid_a = hello_a["result"]["pid"].as_u64().unwrap().to_string();
    let pid_b = hello_b["result"]["pid"].as_u64().unwrap().to_string();
    let list = control_json(&harness.runtime_dir, &["list", "--json"]);
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .any(|row| row["pid"] == pid_a.parse::<u32>().unwrap()
            && row["cwd"] == cwd_a.to_str().unwrap()));
    assert!(rows
        .iter()
        .any(|row| row["pid"] == pid_b.parse::<u32>().unwrap()
            && row["cwd"] == cwd_b.to_str().unwrap()));

    let ambiguous = control_cli(&harness.runtime_dir, &["status"]);
    assert!(!ambiguous.status.success());
    let error = String::from_utf8_lossy(&ambiguous.stderr);
    assert!(error.contains("ambiguous") && error.contains(&pid_a) && error.contains(&pid_b));
    let status_a = control_json(&harness.runtime_dir, &["status", "--pid", &pid_a, "--json"]);
    assert_eq!(status_a["cwd"], cwd_a.to_str().unwrap());
    assert_eq!(status_a["pid"], pid_a.parse::<u32>().unwrap());

    let marker_a = format!("CLI_A_{}", unique_suffix());
    let prompt = control_json(
        &harness.runtime_dir,
        &["prompt", "--pid", &pid_a, "--json", &marker_a],
    );
    assert_eq!(prompt["applied_as"], "native_prompt");
    let idle_a = wait_for_busy(socket_a, false);
    let idle_b = wait_for_busy(socket_b, false);
    let session_a = PathBuf::from(idle_a["session_path"].as_str().unwrap());
    assert!(wait_for_file_contains(&session_a, &marker_a).contains(&marker_a));
    assert_eq!(idle_b["session_id"], Value::Null);
    let marker_b = format!("CLI_B_{}", unique_suffix());
    assert_eq!(
        control_json(
            &harness.runtime_dir,
            &["prompt", "--pid", &pid_b, "--json", &marker_b]
        )["applied_as"],
        "native_prompt"
    );
    let idle_b = wait_for_busy(socket_b, false);
    let session_b = PathBuf::from(idle_b["session_path"].as_str().unwrap());
    assert_ne!(idle_a["session_id"], idle_b["session_id"]);
    assert_ne!(session_a, session_b);
    assert!(!wait_for_file_contains(&session_b, &marker_b).contains(&marker_a));
    assert!(!fs::read_to_string(&session_a).unwrap().contains(&marker_b));
    let list = control_json(&harness.runtime_dir, &["list", "--json"]);
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["pid"] == pid_a.parse::<u32>().unwrap()
            && row["session_id"] == idle_a["session_id"]
            && row["session_path"] == idle_a["session_path"]));
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["pid"] == pid_b.parse::<u32>().unwrap()
            && row["cwd"] == cwd_b.to_str().unwrap()
            && row["session_id"] == idle_b["session_id"]
            && row["session_path"] == idle_b["session_path"]));

    let long_prompt = format!("CLI_LONG_{} {}", unique_suffix(), "x".repeat(1200));
    control_json(
        &harness.runtime_dir,
        &["prompt", "--pid", &pid_a, "--json", &long_prompt],
    );
    wait_for_busy(socket_a, true);
    let busy = control_cli(
        &harness.runtime_dir,
        &["prompt", "--pid", &pid_a, "BUSY_MUST_NOT_QUEUE"],
    );
    assert!(!busy.status.success());
    assert!(busy.stdout.is_empty());
    let busy_error = String::from_utf8_lossy(&busy.stderr);
    assert!(
        busy_error.contains("busy")
            && busy_error.contains("steer")
            && busy_error.contains("follow_up")
            && busy_error.contains("abort")
    );
    let steer = format!("CLI_STEER_{}", unique_suffix());
    let follow = format!("CLI_FOLLOW_{}", unique_suffix());
    assert_eq!(
        control_json(
            &harness.runtime_dir,
            &["steer", "--pid", &pid_a, "--json", &steer]
        )["applied_as"],
        "native_steer"
    );
    assert_eq!(
        control_json(
            &harness.runtime_dir,
            &["follow-up", "--pid", &pid_a, "--json", &follow]
        )["applied_as"],
        "native_follow_up"
    );
    let transcript =
        wait_for_file_contains_with_timeout(&session_a, &follow, Duration::from_secs(35));
    assert!(transcript.contains(&steer));
    let after_follow = wait_for_busy_with_timeout(socket_a, false, Duration::from_secs(45));
    assert_eq!(after_follow["session_id"], idle_a["session_id"]);

    // Interrupt a separate turn so the queued follow-up above has completed.
    let abort_prompt = format!("CLI_ABORT_{} {}", unique_suffix(), "y".repeat(6000));
    control_json(
        &harness.runtime_dir,
        &["prompt", "--pid", &pid_a, "--json", &abort_prompt],
    );
    wait_for_busy(socket_a, true);
    // The busy flag is set before the turn clears the previous abort flag.
    // Give the newly spawned mock stream time to enter its active turn.
    thread::sleep(Duration::from_millis(250));
    let abort_started = Instant::now();
    assert_eq!(
        control_json(&harness.runtime_dir, &["abort", "--pid", &pid_a, "--json"])["applied_as"],
        "native_interrupt"
    );
    let after_abort = wait_for_busy(socket_a, false);
    assert!(
        abort_started.elapsed() < TURN_TIMEOUT,
        "abort did not interrupt the long turn"
    );
    assert_eq!(after_abort["session_id"], idle_a["session_id"]);
    assert_eq!(after_abort["session_path"], idle_a["session_path"]);
    assert!(socket_a.exists());
    assert_eq!(
        control_json(&harness.runtime_dir, &["status", "--pid", &pid_b, "--json"])["session_id"],
        idle_b["session_id"]
    );
    assert!(socket_b.exists());

    // After B stops, an omitted target selects A.
    Command::new("tmux")
        .args(["kill-session", "-t", &harness.sessions[1]])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let list = control_json(&harness.runtime_dir, &["list", "--json"]);
        if list
            .as_array()
            .is_some_and(|rows| rows.len() == 1 && rows[0]["pid"] == pid_a.parse::<u32>().unwrap())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B still appears live after its process exited: {list}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let auto = control_json(&harness.runtime_dir, &["status", "--json"]);
    assert_eq!(auto["pid"], pid_a.parse::<u32>().unwrap());
    let explicit = control_json(
        &harness.runtime_dir,
        &["ping", "--endpoint", socket_a.to_str().unwrap(), "--json"],
    );
    assert_eq!(explicit["pid"], auto["pid"]);
}

#[test]
fn session_browser_binds_two_live_sessions_and_routes_prompt() {
    if !tmux_available() {
        eprintln!(
            "SKIP session_browser_binds_two_live_sessions_and_routes_prompt: tmux unavailable"
        );
        return;
    }
    let mut harness = NativeHarness::new();
    let agent = harness.root.join("shared-agent");
    let (_, cwd_a, _) = harness.spawn_one_with_agent("browser-a", agent.clone());
    harness.wait_for_cwds(&[&cwd_a]);
    let (_, cwd_b, _) = harness.spawn_one_with_agent("browser-b", agent.clone());
    let sockets = harness.wait_for_cwds(&[&cwd_a, &cwd_b]);
    let socket_a = &sockets[&cwd_a].0;
    let socket_b = &sockets[&cwd_b].0;
    let marker_a = format!("BROWSER_A_{}", unique_suffix());
    let marker_b = format!("BROWSER_B_{}", unique_suffix());
    rpc_call(socket_a, "prompt", json!({"text": marker_a})).unwrap();
    let idle_a = wait_for_busy(socket_a, false);
    let path_a = PathBuf::from(idle_a["session_path"].as_str().unwrap());
    wait_for_file_contains(&path_a, &marker_a);
    rpc_call(socket_b, "prompt", json!({"text": marker_b})).unwrap();
    let idle_b = wait_for_busy(socket_b, false);
    let path_b = PathBuf::from(idle_b["session_path"].as_str().unwrap());
    wait_for_file_contains(&path_b, &marker_b);

    let session_cli = |cwd: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_one"))
            .env("XDG_RUNTIME_DIR", &harness.runtime_dir)
            .env("ONE_AGENT_DIR", &agent)
            .arg("--cwd")
            .arg(cwd)
            .arg("session")
            .args(args)
            .output()
            .unwrap()
    };
    let list = session_cli(&cwd_a, &["list", "--all", "--json"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(rows.len(), 2);
    for (status, cwd, path) in [(&idle_a, &cwd_a, &path_a), (&idle_b, &cwd_b, &path_b)] {
        let row = rows
            .iter()
            .find(|row| row["id"] == status["session_id"])
            .unwrap();
        assert_eq!(row["state"], "live_idle");
        assert_eq!(row["cwd"], cwd.to_str().unwrap());
        assert_eq!(row["path"], path.to_str().unwrap());
        assert_eq!(row["live"]["pid"], status["pid"]);
        assert_eq!(row["live"]["frontend"], "interactive");
    }
    let id_a = idle_a["session_id"].as_str().unwrap();
    let show = session_cli(&cwd_a, &["show", id_a, "--json"]);
    let row: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(row["state"], "live_idle");
    assert_eq!(row["live"]["pid"], idle_a["pid"]);
    let tui = session_cli(&cwd_a, &["tui", id_a]);
    assert!(!tui.status.success());
    assert!(String::from_utf8_lossy(&tui.stderr).contains("session is live"));

    let follow = format!("BROWSER_FOLLOW_{}", unique_suffix());
    let exec = session_cli(&cwd_a, &["exec", id_a, &follow]);
    assert!(
        exec.status.success(),
        "{}",
        String::from_utf8_lossy(&exec.stderr)
    );
    wait_for_file_contains(&path_a, &follow);
    assert!(!fs::read_to_string(&path_b).unwrap().contains(&follow));
    assert_eq!(
        wait_for_busy(socket_a, false)["session_id"],
        idle_a["session_id"]
    );

    let long_prompt = format!("BROWSER_LONG_{} {}", unique_suffix(), "x".repeat(900));
    rpc_call(socket_a, "prompt", json!({"text":long_prompt})).unwrap();
    wait_for_busy(socket_a, true);
    let busy_list = session_cli(&cwd_a, &["list", "--all", "--json"]);
    let busy_rows: Vec<Value> = serde_json::from_slice(&busy_list.stdout).unwrap();
    let busy_row = busy_rows.iter().find(|row| row["id"] == id_a).unwrap();
    assert_eq!(busy_row["state"], "live_busy");
    assert_eq!(busy_row["live"]["pid"], idle_a["pid"]);
    let busy_show = session_cli(&cwd_a, &["show", id_a, "--json"]);
    let busy_detail: Value = serde_json::from_slice(&busy_show.stdout).unwrap();
    assert_eq!(busy_detail["state"], "live_busy");
    let busy = session_cli(&cwd_a, &["exec", id_a, "MUST_NOT_QUEUE"]);
    assert!(!busy.status.success());
    let error = String::from_utf8_lossy(&busy.stderr);
    assert!(error.contains("steer") && error.contains("followup"));
    assert!(!fs::read_to_string(&path_a)
        .unwrap()
        .contains("MUST_NOT_QUEUE"));
}

fn result<'a>(response: &'a Value, method: &str) -> &'a Value {
    assert_eq!(
        response["ok"].as_bool(),
        Some(true),
        "{method} failed: {response}"
    );
    &response["result"]
}

fn wait_for_busy(socket: &Path, busy: bool) -> Value {
    wait_for_busy_with_timeout(socket, busy, TURN_TIMEOUT)
}

fn wait_for_busy_with_timeout(socket: &Path, busy: bool, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let response = rpc_call(socket, "status", json!({})).expect("status rpc");
        let status = result(&response, "status");
        if status["busy"].as_bool() == Some(busy) {
            return status.clone();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for busy={busy}; last={response}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_file_contains(path: &Path, needle: &str) -> String {
    wait_for_file_contains_with_timeout(path, needle, TURN_TIMEOUT)
}

fn wait_for_file_contains_with_timeout(path: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let text = fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {needle:?} in {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_socket_removed(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(6);
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "socket was not cleaned up after process exit: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_child_exit(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "One exec exited unsuccessfully: {status}");
                return;
            }
            Ok(None) => {}
            Err(err) => panic!("query One exec status: {err}"),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("One exec did not exit within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn native_control_multi_process_full_lifecycle() {
    if !tmux_available() {
        eprintln!("SKIP native_control_multi_process_full_lifecycle: tmux is unavailable");
        return;
    }

    let mut harness = NativeHarness::new();
    let (session_a, cwd_a, _agent_a) = harness.spawn_one("a");
    let (session_b, cwd_b, _agent_b) = harness.spawn_one("b");

    let statuses = harness.wait_for_cwds(&[&cwd_a, &cwd_b]);
    let (socket_a, hello_a) = statuses.get(&cwd_a).expect("A socket").clone();
    let (socket_b, hello_b) = statuses.get(&cwd_b).expect("B socket").clone();

    assert_ne!(socket_a, socket_b, "two One processes shared an endpoint");
    assert_eq!(result(&hello_a, "handshake")["protocol_version"], 1);
    assert_eq!(result(&hello_b, "handshake")["protocol_version"], 1);
    assert_ne!(
        result(&hello_a, "handshake")["pid"],
        result(&hello_b, "handshake")["pid"]
    );
    assert_ne!(
        result(&hello_a, "handshake")["process_start"],
        result(&hello_b, "handshake")["process_start"]
    );
    assert_eq!(
        fs::metadata(harness.control_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&socket_a).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Create distinct sessions through the native prompt path (no keyboard/Enter).
    let marker_a = format!("NATIVE_A_{}", unique_suffix());
    let marker_b = format!("NATIVE_B_{}", unique_suffix());
    let prompt_a = rpc_call(&socket_a, "prompt", json!({"text": marker_a})).unwrap();
    assert_eq!(result(&prompt_a, "prompt")["applied_as"], "native_prompt");
    let prompt_b = rpc_call(&socket_b, "prompt", json!({"text": marker_b})).unwrap();
    assert_eq!(result(&prompt_b, "prompt")["applied_as"], "native_prompt");
    let idle_a = wait_for_busy(&socket_a, false);
    let idle_b = wait_for_busy(&socket_b, false);

    let session_id_a = idle_a["session_id"].as_str().unwrap().to_owned();
    let session_id_b = idle_b["session_id"].as_str().unwrap().to_owned();
    let session_path_a = PathBuf::from(idle_a["session_path"].as_str().unwrap());
    let session_path_b = PathBuf::from(idle_b["session_path"].as_str().unwrap());
    assert_ne!(session_id_a, session_id_b);
    assert_ne!(session_path_a, session_path_b);
    let transcript_a = wait_for_file_contains(&session_path_a, &marker_a);
    let transcript_b = wait_for_file_contains(&session_path_b, &marker_b);
    assert!(
        !transcript_a.contains(&marker_b),
        "B marker leaked into A session"
    );
    assert!(
        !transcript_b.contains(&marker_a),
        "A marker leaked into B session"
    );

    // A long mock response gives a deterministic busy window without network/tools.
    let long_marker = format!("LONG_A_{}", unique_suffix());
    let long_prompt = format!("{long_marker} {}", "x".repeat(2400));
    let accepted = rpc_call(&socket_a, "prompt", json!({"text": long_prompt})).unwrap();
    assert_eq!(result(&accepted, "prompt")["applied_as"], "native_prompt");
    let _ = wait_for_busy(&socket_a, true);

    let busy_normal = rpc_call(
        &socket_a,
        "prompt",
        json!({"text": "BUSY_NORMAL_MUST_NOT_QUEUE"}),
    )
    .unwrap();
    assert_eq!(busy_normal["ok"], false);
    assert_eq!(busy_normal["error"]["code"], "busy");
    assert_eq!(
        busy_normal["error"]["details"]["allowed"],
        json!(["steer", "follow_up", "abort"])
    );

    let steer_marker = format!("STEER_CONSUMED_{}", unique_suffix());
    let follow_marker = format!("FOLLOWUP_CONSUMED_{}", unique_suffix());
    let steer = rpc_call(&socket_a, "steer", json!({"text": steer_marker})).unwrap();
    assert_eq!(result(&steer, "steer")["applied_as"], "native_steer");
    let follow = rpc_call(&socket_a, "follow_up", json!({"text": follow_marker})).unwrap();
    assert_eq!(
        result(&follow, "follow_up")["applied_as"],
        "native_follow_up"
    );

    // Abort A only. B must remain alive with the same session identity.
    let before_b = result(&rpc_call(&socket_b, "status", json!({})).unwrap(), "status").clone();
    let abort = rpc_call(&socket_a, "abort", json!({})).unwrap();
    assert_eq!(result(&abort, "abort")["applied_as"], "native_interrupt");
    assert_eq!(result(&abort, "abort")["session_preserved"], true);

    // A's queued steer/followup must be consumed by the same session, not merely
    // acknowledged by the socket server. The follow-up is drained after abort.
    let transcript_a = wait_for_file_contains(&session_path_a, &follow_marker);
    assert!(
        transcript_a.contains(&steer_marker),
        "native steer was acknowledged but never persisted/consumed"
    );
    let _ = wait_for_busy(&socket_a, false);

    let after_b = result(&rpc_call(&socket_b, "status", json!({})).unwrap(), "status").clone();
    assert_eq!(after_b["session_id"], before_b["session_id"]);
    assert_eq!(after_b["session_path"], before_b["session_path"]);
    assert!(socket_b.exists(), "aborting A removed B endpoint");

    // The same A session remains usable after abort.
    let after_abort_marker = format!("AFTER_ABORT_{}", unique_suffix());
    let after_abort = rpc_call(&socket_a, "prompt", json!({"text": after_abort_marker})).unwrap();
    assert_eq!(
        result(&after_abort, "prompt")["applied_as"],
        "native_prompt"
    );
    let final_a = wait_for_busy(&socket_a, false);
    assert_eq!(final_a["session_id"].as_str(), Some(session_id_a.as_str()));
    assert_eq!(
        final_a["session_path"].as_str(),
        Some(session_path_a.to_str().unwrap())
    );
    let final_transcript = wait_for_file_contains(&session_path_a, &after_abort_marker);
    assert!(final_transcript.contains(&follow_marker));

    // A naturally-completing `one exec` process exercises ControlServerGuard
    // destruction without mixing TUI key semantics into the socket lifecycle
    // assertion. Its endpoint must disappear while both live TUI endpoints stay.
    let exec_cwd = harness.root.join("cwd-exec-exit");
    let exec_agent = harness.root.join("agent-exec-exit");
    fs::create_dir_all(&exec_cwd).unwrap();
    fs::create_dir_all(&exec_agent).unwrap();
    let exec_prompt = format!("NATIVE_EXIT_CLEANUP {}", "z".repeat(220));
    let mut exec_child = Command::new(env!("CARGO_BIN_EXE_one"))
        .current_dir(&exec_cwd)
        .env("XDG_RUNTIME_DIR", &harness.runtime_dir)
        .env("ONE_AGENT_DIR", &exec_agent)
        .args([
            "exec",
            "--no-session",
            "--provider",
            "mock",
            "--no-mcp",
            "--no-skills",
            "--no-memory",
            "--no-subagent",
            &exec_prompt,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn One exec cleanup probe");

    let statuses = harness.wait_for_cwds(&[&cwd_a, &cwd_b, &exec_cwd]);
    let (socket_exec, hello_exec) = statuses
        .get(&exec_cwd)
        .expect("exec cleanup socket")
        .clone();
    assert_eq!(result(&hello_exec, "handshake")["frontend"], "print");
    // Unified status: one-shot print run reports prompt=false (no live prompt
    // consumer) while steer/follow_up/abort stay available. The handshake
    // snapshot races the print turn itself, so assert the invariant instead
    // of a fixed lifecycle value: the authoritative `state` field and the
    // derived busy flag must agree, and a mock print turn can only be
    // observed as running or idle.
    let exec_status = result(&hello_exec, "handshake");
    assert_eq!(exec_status["capabilities"]["prompt"], false);
    assert_eq!(exec_status["capabilities"]["steer"], true);
    assert_eq!(exec_status["capabilities"]["abort"], true);
    let exec_state = exec_status["state"].as_str().expect("state field");
    let exec_busy = exec_status["busy"].as_bool().expect("busy field");
    assert!(
        exec_state == "running" || exec_state == "idle",
        "unexpected exec state: {exec_state}"
    );
    assert_eq!(
        exec_state == "running",
        exec_busy,
        "state/busy must stay consistent"
    );
    assert!(socket_exec.exists());
    wait_for_child_exit(&mut exec_child, Duration::from_secs(45));
    wait_for_socket_removed(&socket_exec);

    assert!(socket_a.exists(), "exec exit removed A endpoint");
    assert!(socket_b.exists(), "exec exit removed B endpoint");
    let still_b = rpc_call(&socket_b, "status", json!({})).unwrap();
    assert_eq!(result(&still_b, "status")["session_id"], session_id_b);

    // TUI processes are intentionally left to the harness Drop guard.
    let _ = session_a;
    let _ = session_b;
}

#[test]
fn native_control_forced_exit_is_cleaned_on_next_start() {
    if !tmux_available() {
        eprintln!("SKIP native_control_forced_exit_is_cleaned_on_next_start: tmux is unavailable");
        return;
    }

    let mut harness = NativeHarness::new();
    let (_session, cwd, _agent) = harness.spawn_one("forced");
    let statuses = harness.wait_for_cwds(&[&cwd]);
    let (stale_socket, hello) = statuses.get(&cwd).expect("forced socket");
    let pid = result(hello, "handshake")["pid"].as_u64().unwrap();
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .expect("kill One process");
    assert!(status.success());
    assert!(stale_socket.exists(), "forced exit did not leave a socket");

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while PathBuf::from(format!("/proc/{pid}")).exists() {
        assert!(Instant::now() < deadline, "killed One process did not exit");
        thread::sleep(Duration::from_millis(20));
    }

    let stale_socket = stale_socket.clone();
    let (_replacement, next_cwd, _next_agent) = harness.spawn_one("replacement");
    let next = harness.wait_for_cwds(&[&next_cwd]);
    assert!(next.contains_key(&next_cwd));
    assert!(!stale_socket.exists(), "next startup kept stale endpoint");
}
