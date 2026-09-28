#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

struct Fixture {
    root: PathBuf,
    agent: PathBuf,
    a: PathBuf,
    b: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "one-session-browser-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agent = root.join("agent");
        let a = root.join("project-a");
        let b = root.join("project-b");
        for dir in [&agent, &a, &b] {
            fs::create_dir_all(dir).unwrap();
        }
        Self { root, agent, a, b }
    }

    fn session(&self, cwd: &Path, id: &str, name: &str) -> PathBuf {
        let dir = self.agent.join("sessions").join(format!(
            "--{}--",
            cwd.display().to_string().replace('/', "-")
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("20260927_000000_{id}.jsonl"));
        let header = json!({"type":"session", "version":3, "id":id,
            "timestamp":"2026-09-27T00:00:00Z", "cwd":cwd});
        let title = json!({"type":"session_info", "id":"entry1", "parentId":null,
            "timestamp":"2026-09-27T00:00:01Z", "name":name});
        let message = json!({"type":"message", "id":"entry2", "parentId":"entry1",
            "timestamp":"2026-09-27T00:00:02Z", "message":{"role":"user", "content":format!("preview for {id}"), "timestamp":1}});
        fs::write(&path, format!("{header}\n{title}\n{message}\n")).unwrap();
        path
    }

    fn one(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_one"))
            .env("ONE_AGENT_DIR", &self.agent)
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .arg("--cwd")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn list_query_show_and_ambiguity() {
    let f = Fixture::new();
    let a1 = f.session(&f.a, "alpha-111", "shared title");
    let _a2 = f.session(&f.a, "alpha-222", "second title");
    let b1 = f.session(&f.b, "beta-111", "shared title");
    let local = f.one(&f.a, &["session", "list", "--json"]);
    assert!(
        local.status.success(),
        "{}",
        String::from_utf8_lossy(&local.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&local.stdout).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|r| r["cwd"] == f.a.to_str().unwrap() && r["state"] == "dormant"));
    let all = f.one(&f.a, &["session", "list", "--all", "--json"]);
    assert!(
        all.status.success(),
        "{}",
        String::from_utf8_lossy(&all.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows
        .iter()
        .any(|r| r["id"] == "beta-111" && r["path"] == b1.to_str().unwrap()));
    for query in [
        "alpha-111",
        "shared title",
        "project-b",
        "20260927_000000_beta",
        "preview for beta",
    ] {
        let out = f.one(
            &f.a,
            &["session", "list", "--all", "--query", query, "--json"],
        );
        let rows: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
        assert!(!rows.is_empty(), "query {query}");
    }
    let show = f.one(&f.a, &["session", "show", "alpha-111", "--json"]);
    let row: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(row["path"], a1.to_str().unwrap());
    assert_eq!(row["live"], Value::Null);
    let ambiguous = f.one(&f.a, &["session", "show", "shared title", "--all"]);
    assert!(!ambiguous.status.success());
    let message = String::from_utf8_lossy(&ambiguous.stderr);
    assert!(
        message.contains("ambiguous")
            && message.contains("alpha-111")
            && message.contains("beta-111")
    );
}

#[test]
fn dormant_exec_continues_same_file() {
    let f = Fixture::new();
    let path = f.session(&f.a, "resume-111", "resumable");
    let out = f.one(
        &f.a,
        &[
            "--provider",
            "mock",
            "--no-mcp",
            "--no-skills",
            "--no-memory",
            "--no-subagent",
            "session",
            "exec",
            "resume-111",
            "continue this session",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("continue this session"));
    assert!(text.contains("resume-111"));
    let rows: Vec<Value> =
        serde_json::from_slice(&f.one(&f.a, &["session", "list", "--json"]).stdout).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["path"], path.to_str().unwrap());
}

#[test]
fn malformed_and_stale_locks_do_not_mark_live() {
    let f = Fixture::new();
    let path = f.session(&f.a, "stale-111", "stale lock");
    let lock = path.with_extension("lock");
    fs::write(&lock, "bad json").unwrap();
    let show = f.one(&f.a, &["session", "show", "stale-111", "--json"]);
    let row: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(row["state"], "dormant");
    fs::write(
        &lock,
        json!({"session_id":"stale-111", "pid":4294967294u32,
        "created_at":"2026-09-27T00:00:00Z", "updated_at":"2026-09-27T00:00:00Z",
        "hostname":"localhost", "activity":"idle"})
        .to_string(),
    )
    .unwrap();
    let show = f.one(&f.a, &["session", "show", "stale-111", "--json"]);
    let row: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(row["state"], "dormant");
}

#[test]
fn running_dormant_exec_owns_lock_until_exit() {
    let f = Fixture::new();
    let path = f.session(&f.a, "locked-111", "locked session");
    let lock_path = path.with_extension("lock");
    let long_prompt = format!("hold the mock stream {}", "x".repeat(1600));
    let mut first = Command::new(env!("CARGO_BIN_EXE_one"))
        .env("ONE_AGENT_DIR", &f.agent)
        .env("XDG_RUNTIME_DIR", f.root.join("runtime"))
        .arg("--cwd")
        .arg(&f.a)
        .args([
            "--provider",
            "mock",
            "--no-mcp",
            "--no-skills",
            "--no-memory",
            "--no-subagent",
            "session",
            "exec",
            "locked-111",
            &long_prompt,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while !lock_path.exists() {
        assert!(
            first.try_wait().unwrap().is_none(),
            "first exec exited before acquiring lock"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "lock was not acquired"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let data: Value = serde_json::from_slice(&fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(data["session_id"], "locked-111");
    assert_eq!(data["pid"], first.id());
    for args in [
        vec!["session", "exec", "locked-111", "second writer"],
        vec!["session", "tui", "locked-111"],
    ] {
        let second = f.one(&f.a, &args);
        assert!(!second.status.success(), "second writer was allowed");
    }
    assert!(
        lock_path.exists(),
        "failed entrant removed the owner's lock"
    );
    let status = first.wait().unwrap();
    assert!(status.success());
    assert!(!lock_path.exists(), "owner did not clean its lock on exit");
    fs::write(&lock_path, "stale malformed lock").unwrap();
    let next = f.one(
        &f.a,
        &[
            "--provider",
            "mock",
            "--no-mcp",
            "--no-skills",
            "--no-memory",
            "--no-subagent",
            "session",
            "exec",
            "locked-111",
            "after stale lock",
        ],
    );
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
    assert!(!lock_path.exists());
}
