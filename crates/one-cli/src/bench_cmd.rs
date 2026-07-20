//! `one bench` — run harness capability tasks against mock (smoke) or real providers.
//!
//! Non-destructive: uses temp workspaces, never touches the repo under test
//! except reading `benches/tasks/`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use one_ai::MockProvider;
use one_core::agent::{Agent, AgentConfig, LlmProvider};
use one_core::message::now_ms;
use one_core::trace::{
    new_run_id, MemoryTrace, ScoreCheckResult, SharedTrace, TraceEvent, TraceStats,
};
use one_core::TraceRunMeta;
use one_tools::default_tools;
use serde::{Deserialize, Serialize};

use crate::cli::BenchCli;
use crate::langfuse::{LangfuseConfig, LangfuseTraceSink};

/// Rubric for automatic scoring.
#[derive(Debug, Clone, Deserialize)]
struct Rubric {
    #[serde(default)]
    checks: Vec<Check>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Check {
    FileContains {
        path: String,
        text: String,
    },
    FileExists {
        path: String,
    },
    Command {
        cmd: String,
        #[serde(default)]
        exit_code: i32,
    },
    MaxTurns {
        n: usize,
    },
    MaxToolErrors {
        n: usize,
    },
    TraceHasTool {
        name: String,
    },
    TraceStatus {
        status: String,
    },
    FinalTextContains {
        text: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct TaskMeta {
    id: String,
    #[serde(default)]
    title: String,
    /// `smoke` (mock-friendly) or `full` (needs real model).
    #[serde(default = "default_suite")]
    suite: String,
    #[serde(default)]
    prompt: Option<String>,
    /// Shared project under `benches/projects/<name>` (preferred over local `fixture/`).
    #[serde(default)]
    project: Option<String>,
    /// Relative path from the task dir to a fixture directory.
    #[serde(default)]
    fixture: Option<String>,
    /// Optional cargo test filter(s) for scoring.
    /// Accepts a single pattern (`"ledger::"`), whitespace-separated
    /// patterns (`"money:: promo::"`), or a JSON array of patterns.
    /// Each pattern becomes its own `cargo test <pat> -- --quiet` check
    /// (cargo only allows one filter per invocation).
    #[serde(default, deserialize_with = "deserialize_test_filter")]
    test_filter: Option<Vec<String>>,
}

fn deserialize_test_filter<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct TfVisitor;
    impl<'de> Visitor<'de> for TfVisitor {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("string, array of strings, or null")
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let parts: Vec<String> = v
                .split_whitespace()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            if parts.is_empty() {
                Ok(None)
            } else {
                Ok(Some(parts))
            }
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(s) = seq.next_element::<String>()? {
                let t = s.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
            if out.is_empty() {
                Ok(None)
            } else {
                Ok(Some(out))
            }
        }
    }

    deserializer.deserialize_any(TfVisitor)
}

fn default_suite() -> String {
    "smoke".into()
}

#[derive(Debug, Serialize)]
struct TaskResult {
    task_id: String,
    suite: String,
    pass: bool,
    score: f64,
    turns: usize,
    tool_calls: usize,
    wall_ms: u64,
    tokens: u64,
    /// Where traces went: `langfuse:<host>` or `memory` (offline, no credentials).
    trace: String,
    checks: Vec<ScoreCheckResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn run_bench(cli: BenchCli) -> Result<(), Box<dyn std::error::Error>> {
    let tasks_dir = resolve_tasks_dir(cli.tasks_dir.as_deref())?;
    let out_dir = resolve_out_dir(cli.out.as_deref())?;
    std::fs::create_dir_all(&out_dir)?;

    let suite_filter = cli.suite.to_ascii_lowercase();
    let mut tasks = discover_tasks(&tasks_dir)?;
    if let Some(only) = &cli.task {
        tasks.retain(|t| t.0.id == *only);
        if tasks.is_empty() {
            return Err(format!("task not found: {only}").into());
        }
    } else if suite_filter != "all" {
        tasks.retain(|t| t.0.suite.eq_ignore_ascii_case(&suite_filter));
    }

    if tasks.is_empty() {
        return Err(format!(
            "no tasks matched suite={suite_filter} under {}",
            tasks_dir.display()
        )
        .into());
    }

    let langfuse = LangfuseConfig::from_env();
    if let Some(cfg) = &langfuse {
        println!("bench: langfuse → {}", cfg.project_url_hint());
    } else {
        println!("bench: offline trace (set LANGFUSE_* keys to export)");
    }

    println!(
        "bench: {} task(s) from {} → {}",
        tasks.len(),
        tasks_dir.display(),
        out_dir.display()
    );

    let mut results = Vec::new();
    for (meta, task_dir) in &tasks {
        print!("  • {} … ", meta.id);
        let r = run_one_task(meta, task_dir, cli.max_turns, cli.keep, langfuse.as_ref()).await;
        match &r {
            Ok(tr) if tr.pass => println!("PASS (turns={} tools={})", tr.turns, tr.tool_calls),
            Ok(tr) => println!("FAIL score={:.2}", tr.score),
            Err(e) => println!("ERROR {e}"),
        }
        match r {
            Ok(tr) => results.push(tr),
            Err(e) => results.push(TaskResult {
                task_id: meta.id.clone(),
                suite: meta.suite.clone(),
                pass: false,
                score: 0.0,
                turns: 0,
                tool_calls: 0,
                wall_ms: 0,
                tokens: 0,
                trace: "none".into(),
                checks: vec![],
                error: Some(e.to_string()),
            }),
        }
    }

    let passed = results.iter().filter(|r| r.pass).count();
    let summary = serde_json::json!({
        "suite": suite_filter,
        "tasks_dir": tasks_dir.display().to_string(),
        "out": out_dir.display().to_string(),
        "passed": passed,
        "total": results.len(),
        "results": results,
    });
    let summary_path = out_dir.join("summary.json");
    std::fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)?;

    let mut md = String::new();
    md.push_str("# Harness bench summary\n\n");
    md.push_str(&format!(
        "Suite: `{suite_filter}` · {passed}/{} passed\n\n",
        results.len()
    ));
    md.push_str("| task | pass | score | turns | tools | wall_ms | tokens |\n");
    md.push_str("|------|------|-------|-------|-------|---------|--------|\n");
    for r in &results {
        md.push_str(&format!(
            "| {} | {} | {:.2} | {} | {} | {} | {} |\n",
            r.task_id,
            if r.pass { "✅" } else { "❌" },
            r.score,
            r.turns,
            r.tool_calls,
            r.wall_ms,
            r.tokens
        ));
    }
    let md_path = out_dir.join("summary.md");
    std::fs::write(&md_path, &md)?;

    println!();
    println!("summary: {}  ({passed}/{} passed)", summary_path.display(), results.len());
    println!("{md}");

    if passed < results.len() {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_one_task(
    meta: &TaskMeta,
    task_dir: &Path,
    max_turns: usize,
    keep: bool,
    langfuse: Option<&LangfuseConfig>,
) -> Result<TaskResult, Box<dyn std::error::Error>> {
    let work = make_workspace(task_dir, meta)?;
    let prompt = load_prompt(task_dir, meta)?;
    let mut rubric = load_rubric(task_dir)?;
    // If task sets test_filter and rubric has no command check, add one cargo
    // test invocation per filter pattern (cargo accepts only one TESTNAME).
    if let Some(filters) = &meta.test_filter {
        let has_cmd = rubric.checks.iter().any(|c| matches!(c, Check::Command { .. }));
        if !has_cmd {
            for filter in filters {
                rubric.checks.push(Check::Command {
                    cmd: format!("cargo test {filter} -- --quiet"),
                    exit_code: 0,
                });
            }
        }
    }

    // Single sink: Langfuse when credentials exist; otherwise in-memory for scoring only.
    let mut langfuse_handle: Option<Arc<LangfuseTraceSink>> = None;
    let (sink, events_fn, trace_label): (SharedTrace, Box<dyn Fn() -> Vec<TraceEvent>>, String) =
        if let Some(cfg) = langfuse {
            let lf = LangfuseTraceSink::start(cfg.clone());
            let label = format!("langfuse:{}", cfg.project_url_hint());
            let lf_events = lf.clone();
            langfuse_handle = Some(lf.clone());
            (
                lf,
                Box::new(move || lf_events.events()),
                label,
            )
        } else {
            let mem = Arc::new(MemoryTrace::new());
            let mem_events = mem.clone();
            (
                mem,
                Box::new(move || mem_events.events()),
                "memory".into(),
            )
        };

    let tools = default_tools(work.clone());
    let mut agent = Agent::new(
        AgentConfig {
            system_prompt: one_core::agent::DEFAULT_SYSTEM_PROMPT.into(),
            max_turns,
            thinking_level: one_core::ThinkingLevel::Off,
        },
        tools,
    );
    agent.set_trace(Some(sink.clone()));
    // Stable session id per task run so multi-turn harness traces group in Langfuse.
    let bench_session_id = format!("bench:{}:{}", meta.id, uuid::Uuid::new_v4().simple());
    agent.set_trace_meta(TraceRunMeta {
        task_id: Some(meta.id.clone()),
        agent_version: Some(env!("CARGO_PKG_VERSION").into()),
        config: Some(serde_json::json!({
            "max_turns": max_turns,
            "suite": meta.suite,
            "cwd": work.display().to_string(),
            "project": meta.project,
            "test_filter": meta.test_filter,
            "backend": if langfuse.is_some() { "langfuse" } else { "memory" },
            "harness": "one-bench",
        })),
        session_id: Some(bench_session_id),
        user_id: crate::langfuse::user_id_from_env(),
        trace_full: false,
    });

    // Smoke suite uses mock for determinism. Full suite also defaults to mock so
    // `one bench` stays offline; for real-model eval use:
    //   one --trace --provider <p> -y --cwd <fixture-copy> -p "$(cat prompt.md)"
    let provider: Box<dyn LlmProvider> = Box::new(MockProvider::new());
    let run_result = agent.prompt(provider.as_ref(), &prompt).await;
    let final_text = match run_result {
        Ok(t) => t,
        Err(e) => {
            let _ = e;
            String::new()
        }
    };

    let events = events_fn();
    let stats = TraceStats::from_events(&events);
    let (pass, score, checks) = score_task(&rubric, &work, &stats, &events, &final_text);

    // Score event → same sink (Langfuse scores, or memory-only offline).
    let run_id = stats.run_id.clone().unwrap_or_else(new_run_id);
    sink.record(TraceEvent::Score {
        ts_ms: now_ms(),
        run_id,
        task_id: Some(meta.id.clone()),
        pass,
        score,
        checks: checks.clone(),
        notes: None,
    });

    // Drain Langfuse worker before moving on / process exit.
    if let Some(lf) = langfuse_handle {
        lf.shutdown();
    }

    if !keep {
        let _ = std::fs::remove_dir_all(&work);
    }

    Ok(TaskResult {
        task_id: meta.id.clone(),
        suite: meta.suite.clone(),
        pass,
        score,
        turns: stats.turns,
        tool_calls: stats.tool_calls,
        wall_ms: stats.wall_ms,
        tokens: stats.usage.total(),
        trace: trace_label,
        checks,
        error: None,
    })
}

fn score_task(
    rubric: &Rubric,
    work: &Path,
    stats: &TraceStats,
    events: &[TraceEvent],
    final_text: &str,
) -> (bool, f64, Vec<ScoreCheckResult>) {
    if rubric.checks.is_empty() {
        // Default: run completed ok and produced something.
        let pass = matches!(
            stats.status,
            Some(one_core::TraceRunStatus::Ok)
        ) && (!final_text.is_empty() || stats.tool_calls > 0);
        return (
            pass,
            if pass { 1.0 } else { 0.0 },
            vec![ScoreCheckResult {
                name: "default_ok".into(),
                pass,
                detail: None,
            }],
        );
    }

    let mut results = Vec::new();
    for check in &rubric.checks {
        let (name, pass, detail) = match check {
            Check::FileContains { path, text } => {
                let p = work.join(path);
                let content = std::fs::read_to_string(&p).unwrap_or_default();
                let pass = content.contains(text);
                (
                    format!("file_contains:{path}"),
                    pass,
                    (!pass).then(|| format!("missing `{text}` in {}", p.display())),
                )
            }
            Check::FileExists { path } => {
                let p = work.join(path);
                let pass = p.exists();
                (
                    format!("file_exists:{path}"),
                    pass,
                    (!pass).then(|| format!("missing {}", p.display())),
                )
            }
            Check::Command { cmd, exit_code } => {
                let status = Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .current_dir(work)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                match status {
                    Ok(s) => {
                        let code = s.code().unwrap_or(-1);
                        let pass = code == *exit_code;
                        (
                            format!("command:{cmd}"),
                            pass,
                            (!pass).then(|| format!("exit {code}, expected {exit_code}")),
                        )
                    }
                    Err(e) => (format!("command:{cmd}"), false, Some(e.to_string())),
                }
            }
            Check::MaxTurns { n } => {
                let pass = stats.turns <= *n;
                (
                    format!("max_turns:{n}"),
                    pass,
                    (!pass).then(|| format!("turns={}", stats.turns)),
                )
            }
            Check::MaxToolErrors { n } => {
                let pass = stats.tool_errors <= *n;
                (
                    format!("max_tool_errors:{n}"),
                    pass,
                    (!pass).then(|| format!("errors={}", stats.tool_errors)),
                )
            }
            Check::TraceHasTool { name } => {
                let pass = stats.tool_names.iter().any(|t| t == name);
                (
                    format!("trace_has_tool:{name}"),
                    pass,
                    (!pass).then(|| format!("tools={:?}", stats.tool_names)),
                )
            }
            Check::TraceStatus { status } => {
                let got = stats
                    .status
                    .as_ref()
                    .map(|s| format!("{s:?}").to_ascii_lowercase())
                    .unwrap_or_else(|| "none".into());
                // status field is like "ok" / "Ok" debug
                let pass = got.contains(&status.to_ascii_lowercase());
                (
                    format!("trace_status:{status}"),
                    pass,
                    (!pass).then(|| format!("got={got}")),
                )
            }
            Check::FinalTextContains { text } => {
                let pass = final_text.to_ascii_lowercase().contains(&text.to_ascii_lowercase());
                (
                    format!("final_text_contains"),
                    pass,
                    (!pass).then(|| "text not found in final output".into()),
                )
            }
        };
        let _ = events; // reserved for richer checks
        results.push(ScoreCheckResult {
            name,
            pass,
            detail,
        });
    }

    let passed_n = results.iter().filter(|c| c.pass).count();
    let score = passed_n as f64 / results.len() as f64;
    let pass = results.iter().all(|c| c.pass);
    (pass, score, results)
}

fn discover_tasks(tasks_dir: &Path) -> Result<Vec<(TaskMeta, PathBuf)>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    if !tasks_dir.is_dir() {
        return Err(format!("tasks dir not found: {}", tasks_dir.display()).into());
    }
    let mut entries: Vec<_> = std::fs::read_dir(tasks_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for ent in entries {
        let dir = ent.path();
        let meta_path = dir.join("task.json");
        if !meta_path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&meta_path)?;
        let mut meta: TaskMeta = serde_json::from_str(&raw)?;
        if meta.id.is_empty() {
            meta.id = ent.file_name().to_string_lossy().into();
        }
        out.push((meta, dir));
    }
    Ok(out)
}

fn load_prompt(task_dir: &Path, meta: &TaskMeta) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(p) = &meta.prompt {
        return Ok(p.clone());
    }
    let path = task_dir.join("prompt.md");
    if path.exists() {
        return Ok(std::fs::read_to_string(path)?);
    }
    Err(format!("no prompt for task {}", meta.id).into())
}

fn load_rubric(task_dir: &Path) -> Result<Rubric, Box<dyn std::error::Error>> {
    let path = task_dir.join("rubric.json");
    if !path.exists() {
        return Ok(Rubric { checks: vec![] });
    }
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

fn make_workspace(task_dir: &Path, meta: &TaskMeta) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let work = std::env::temp_dir().join(format!("one-bench-{}-{ms}", meta.id));
    if work.exists() {
        let _ = std::fs::remove_dir_all(&work);
    }
    std::fs::create_dir_all(&work)?;

    let fixture_src = resolve_fixture_src(task_dir, meta)?;
    if let Some(src) = fixture_src {
        copy_dir_filtered(&src, &work)?;
    }
    Ok(work)
}

/// Resolve fixture directory: `project` → benches/projects/<name>, else `fixture` path, else task/fixture.
fn resolve_fixture_src(
    task_dir: &Path,
    meta: &TaskMeta,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    if let Some(name) = &meta.project {
        let projects = find_projects_dir(task_dir)?;
        let p = projects.join(name);
        if !p.is_dir() {
            return Err(format!("project not found: {}", p.display()).into());
        }
        return Ok(Some(p));
    }
    if let Some(rel) = &meta.fixture {
        let p = task_dir.join(rel);
        if !p.is_dir() {
            return Err(format!("fixture not found: {}", p.display()).into());
        }
        return Ok(Some(p));
    }
    let local = task_dir.join("fixture");
    if local.is_dir() {
        Ok(Some(local))
    } else {
        Ok(None)
    }
}

fn find_projects_dir(task_dir: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    // task_dir is .../benches/tasks/<id> → projects is .../benches/projects
    if let Some(tasks) = task_dir.parent() {
        let projects = tasks.parent().map(|b| b.join("projects"));
        if let Some(p) = projects {
            if p.is_dir() {
                return Ok(p);
            }
        }
    }
    // Fallback: walk from cwd / compile-time workspace.
    let cwd = std::env::current_dir()?;
    for c in [
        cwd.join("benches/projects"),
        cwd.join("../../benches/projects"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../benches/projects"),
    ] {
        if c.is_dir() {
            return Ok(c.canonicalize().unwrap_or(c));
        }
    }
    Err("could not find benches/projects".into())
}

/// Files never copied into agent workspaces (spoilers / maintainer notes).
const FIXTURE_DENY: &[&str] = &["SOLUTIONS.md", "SOLUTIONS", ".git", "target"];

fn copy_dir_filtered(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if FIXTURE_DENY.iter().any(|d| *d == name_str) {
            continue;
        }
        let ty = entry.file_type()?;
        let to = dst.join(&name);
        if ty.is_dir() {
            copy_dir_filtered(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

fn resolve_tasks_dir(explicit: Option<&Path>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    // Prefer ./benches/tasks, then walk up from CWD looking for Cargo.toml workspace.
    let cwd = std::env::current_dir()?;
    let candidates = [
        cwd.join("benches/tasks"),
        cwd.join("tasks"),
        // When run from crates/one-cli
        cwd.join("../../benches/tasks"),
    ];
    for c in candidates {
        if c.is_dir() {
            return Ok(c.canonicalize().unwrap_or(c));
        }
    }
    // Compile-time path relative to this crate → workspace root.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ws = manifest.join("../../benches/tasks");
    if ws.is_dir() {
        return Ok(ws.canonicalize().unwrap_or(ws));
    }
    Err("could not find benches/tasks (pass --tasks-dir)".into())
}

fn resolve_out_dir(explicit: Option<&Path>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let cwd = std::env::current_dir()?;
    let base = if cwd.join("benches").is_dir() {
        cwd.join("benches/out")
    } else {
        cwd.join("one-bench-out")
    };
    Ok(base.join(format!("{ms}")))
}
