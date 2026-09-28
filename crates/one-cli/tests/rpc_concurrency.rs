use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Rpc {
    child: Child,
    input: ChildStdin,
    output: Receiver<Value>,
}

impl Rpc {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_one"))
            .args([
                "--mode",
                "rpc",
                "--provider",
                "mock",
                "--no-mcp",
                "--no-skills",
                "--no-memory",
                "--no-subagent",
            ])
            .env(
                "ONE_AGENT_DIR",
                std::env::temp_dir().join(format!("one-rpc-test-{}", std::process::id())),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start one --mode rpc");
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let line = line.expect("read rpc stdout");
                let value: Value =
                    serde_json::from_str(&line).expect("stdout must contain only JSON responses");
                tx.send(value).unwrap();
            }
        });
        Self {
            child,
            input,
            output: rx,
        }
    }

    fn send(&mut self, id: &str, method: &str, params: Value) {
        writeln!(
            self.input,
            "{}",
            json!({"id": id, "method": method, "params": params})
        )
        .unwrap();
        self.input.flush().unwrap();
    }

    fn recv(&self, timeout: Duration) -> Value {
        self.output
            .recv_timeout(timeout)
            .expect("timely RPC response")
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn control_requests_run_while_prompt_is_active() {
    let mut rpc = Rpc::start();
    rpc.send("ready", "ping", json!({}));
    assert_eq!(rpc.recv(Duration::from_secs(10))["id"], "ready");

    let long = "x".repeat(600);
    rpc.send("first", "prompt", json!({"text": long}));
    let start = Instant::now();
    rpc.send("status", "status", json!({}));
    let status = rpc.recv(Duration::from_secs(2));
    assert_eq!(status["id"], "status");
    assert_eq!(status["result"]["busy"], true);
    // Unified status: mid-turn lifecycle is explicit, not a bare bool.
    assert_eq!(status["result"]["state"], "running");
    assert_eq!(status["result"]["control"]["state"], "running");
    assert_eq!(status["result"]["control"]["capabilities"]["prompt"], true);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(status["result"]["control"]["pid"], rpc.child.id());

    rpc.send("second", "prompt", json!({"text": "second"}));
    let busy = rpc.recv(Duration::from_secs(2));
    assert_eq!(busy["id"], "second");
    assert_eq!(busy["code"], "busy");
    for (id, method, params) in [
        ("thinking", "thinking", json!({"level": "high"})),
        ("compact", "compact", json!({})),
        ("spawn", "spawn", json!({"prompt": "hello"})),
    ] {
        rpc.send(id, method, params);
        let response = rpc.recv(Duration::from_secs(2));
        assert_eq!(response["id"], id);
        assert_eq!(response["code"], "busy");
    }
    rpc.send("session", "session", json!({}));
    assert_eq!(rpc.recv(Duration::from_secs(2))["id"], "session");

    rpc.send("steer", "steer", json!({"text": "steer marker"}));
    rpc.send("follow", "follow_up", json!({"text": "followup marker"}));
    assert_eq!(rpc.recv(Duration::from_secs(2))["id"], "steer");
    assert_eq!(rpc.recv(Duration::from_secs(2))["id"], "follow");
    let first = rpc.recv(Duration::from_secs(15));
    assert_eq!(first["id"], "first");
    assert_eq!(first["ok"], true);
    assert!(first["result"]["text"]
        .as_str()
        .unwrap()
        .contains("steer marker"));
    rpc.send("after-follow", "status", json!({}));
    let usage = rpc.recv(Duration::from_secs(2));
    assert_eq!(usage["id"], "after-follow");
    assert!(usage["result"]["usage"]["output_tokens"].as_u64().unwrap() >= 32);
    // Unified status: derived busy=false + explicit lifecycle state, and
    // capabilities come from the real transport (rpc stdin supports prompt),
    // not from a frontend-label guess.
    assert_eq!(usage["result"]["busy"], false);
    assert_eq!(usage["result"]["state"], "idle");
    assert_eq!(usage["result"]["control"]["capabilities"]["prompt"], true);
    assert_eq!(usage["result"]["control"]["frontend"], "rpc");

    rpc.send("abort-me", "prompt", json!({"text": "y".repeat(600)}));
    rpc.send("busy-again", "status", json!({}));
    assert_eq!(rpc.recv(Duration::from_secs(2))["result"]["busy"], true);
    rpc.send("abort", "abort", json!({}));
    assert_eq!(rpc.recv(Duration::from_secs(2))["id"], "abort");
    let aborted = rpc.recv(Duration::from_secs(5));
    assert_eq!(aborted["id"], "abort-me");
    assert_eq!(aborted["ok"], false);
    assert!(aborted["error"].as_str().unwrap().contains("aborted"));
    rpc.send("after", "prompt", json!({"text": "usable again"}));
    let after = rpc.recv(Duration::from_secs(5));
    assert_eq!(after["id"], "after");
    assert_eq!(after["ok"], true);
    rpc.send("idle", "status", json!({}));
    let idle = rpc.recv(Duration::from_secs(2));
    assert_eq!(idle["id"], "idle");
    assert_eq!(idle["result"]["busy"], false);
    assert_eq!(idle["result"]["state"], "idle");
}
