# Programmatic Surface + Subagents Implementation Plan

> **For agentic workers:** Implement task-by-task. Steps use checkbox (`- [ ]`) syntax. Prefer small PRs matching W0 → W1 → W2.  
> **Design specs:** [claude-workflow-model.md](../claude-workflow-model.md) · [subagents.md](../subagents.md)  
> **Decision:** P3 workflow = external scripts (not embedded QuickJS).

**Goal:** Make One reliably scriptable (CI/host), then add nested explore subagents, then host-direct `spawn` so multi-step workflows work without a lead LLM.

**Architecture:** **Agent ≡ Subagent** via shared [`AgentSpec` / `RunRequest`](../protocol.md). Single `harness::run`. **Order: CLI harness first → then TaskTool wraps it.** Both **full harness JSON** (`--spec`) and **preset** (`--preset explore` / `agent=explore`) resolve to the same `AgentSpec`.

**Protocol:** `docs/protocol.md` · types in `crates/one-cli/src/protocol.rs`.

**Implementation order (binding):**

```text
1. presets: builtin explore harness JSON + load(preset|file|inline)
2. harness::run(RunRequest) → RunResult  (+ ProviderPermit, explore whitelist)
3. CLI: one agent run / one run --preset|--spec --output-format json
4. TaskTool → only calls harness::run (preset name or full agent_spec)
```

**Phase-1 hard requirements:**

1. **CLI before tool** — no TaskTool until `one agent run explore` works.
2. **Full JSON + preset** — both first-class; preset expands to full AgentSpec.
3. **ProviderPermit** (`ONE_LLM_CONCURRENCY`); logical task ≤4 queues if needed.
4. **No subagent token stream to main TUI.**
5. **explore hard whitelist.**
6. **TaskTool only in one-cli.**
7. **TaskExitStatus** machine-readable.
8. Sub prompt forbids questions; `ERROR:` → incomplete_info.

**Tech stack:** Existing Rust workspace (`one-core`, `one-cli`, `one-tools`); mock provider for tests; no new runtime languages.

---

## File map (target)

| Path | Role |
|------|------|
| `crates/one-cli/src/result_envelope.rs` | **New** — `RunResult` / serde JSON shape |
| `crates/one-cli/src/cli.rs` | `--output-format`, maybe re-export |
| `crates/one-cli/src/modes/print.rs` | Emit envelope + exit codes |
| `crates/one-cli/src/modes/rpc.rs` | Same envelope on `prompt`; later `spawn` |
| `crates/one-cli/src/main.rs` | Wire format + exit status |
| `crates/one-cli/src/runtime/subagent.rs` | **New** — `SubAgentFactory` impl |
| `crates/one-cli/src/runtime/build.rs` / `tools.rs` | Register `task` tool |
| `crates/one-cli/src/agent_cmd.rs` | **New** — `one agent run` (W2) |
| `crates/one-cli/tests/e2e_mock.rs` | Envelope + nested task e2e |
| `docs/cli.md` | Document flags / RPC / agent run |
| `examples/workflows/` | **New** (W2/W3) bash/python multi-step |

**Dependency rule:** Keep `one-tools` free of `one-ai` / full runtime. Put `TaskTool` + factory trait in **`one-cli`** (or trait object callback), not in `one-tools` if that would pull provider deps.

---

## W0 — Programmatic result envelope

### Task W0.1: Define `RunResult` type

**Files:**
- Create: `crates/one-cli/src/result_envelope.rs`
- Modify: `crates/one-cli/src/main.rs` (mod + use)
- Test: unit tests in `result_envelope.rs`

- [ ] **Step 1: Add type**

```rust
// crates/one-cli/src/result_envelope.rs
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    pub ok: bool,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl RunResult {
    pub fn success(result: impl Into<String>, duration_ms: u64) -> Self { /* … */ }
    pub fn failure(error: impl Into<String>, duration_ms: u64) -> Self { /* … */ }
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).expect("RunResult serializes")
    }
}
```

- [ ] **Step 2: Unit test roundtrip**

```rust
#[test]
fn run_result_json_has_ok_and_result() {
    let j = RunResult::success("hello", 12).to_json_line();
    let v: serde_json::Value = serde_json::from_str(&j).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["result"], "hello");
    assert_eq!(v["duration_ms"], 12);
}
```

- [ ] **Step 3: `cargo test -p one-cli result_envelope` — pass**

- [ ] **Step 4: Commit** `feat(cli): add RunResult envelope type`

---

### Task W0.2: CLI `--output-format` + print mode

**Files:**
- Modify: `crates/one-cli/src/cli.rs`
- Modify: `crates/one-cli/src/modes/print.rs`
- Modify: `crates/one-cli/src/main.rs` (exit code)

- [ ] **Step 1: Add enum**

```rust
#[derive(Debug, Clone, ValueEnum, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    /// NDJSON events (existing --mode json behavior) + final RunResult line optional
    StreamJson,
}
```

CLI: `--output-format <text|json|stream-json>`  
Compatibility: `--mode json` continues to mean stream events; prefer documenting that `--output-format json` is the **script-friendly single object** (or final line).

**Recommended behavior (lock this in code comments):**

| Flag | stdout |
|------|--------|
| `text` (default for print) | final assistant text only |
| `json` | **one** JSON object = `RunResult` |
| `stream-json` | existing event lines, then optional final `RunResult` with `"type":"result"` **or** keep events and put envelope only on `--output-format json` |

**MVP lock:**  
- `--output-format json` → single `RunResult` on stdout (no event spam).  
- `--mode json` → keep current event stream for TUI-like consumers.  
- Do not break `one bench` / existing scripts that parse `"type":"final"`.

- [ ] **Step 2: print.rs**

```rust
pub async fn run_print(…, format: OutputFormat) -> Result<i32, Box<dyn Error>> {
    let t0 = Instant::now();
    // subscribe only for stream-json / legacy
    match runtime.prompt(provider, prompt).await {
        Ok(text) => {
            let mut rr = RunResult::success(text, t0.elapsed().as_millis() as u64);
            rr.session_id = runtime.session_id();
            rr.session_path = runtime.session_path_string();
            rr.usage = runtime.usage_snapshot();
            match format {
                OutputFormat::Text => { println!("{}", rr.result); Ok(0) }
                OutputFormat::Json => { println!("{}", rr.to_json_line()); Ok(0) }
                OutputFormat::StreamJson => { /* events already printed */ Ok(0) }
            }
        }
        Err(e) => {
            let rr = RunResult::failure(e.to_string(), …);
            if matches!(format, OutputFormat::Json) {
                println!("{}", rr.to_json_line());
            } else {
                eprintln!("{e}");
            }
            Ok(1)
        }
    }
}
```

- [ ] **Step 3: main uses exit code** `std::process::exit(code)`

- [ ] **Step 4: Manual check**

```bash
cargo run -p one-cli -- -p "list files" --provider mock -y --output-format json --no-session
# expect: {"ok":true,"result":"…","duration_ms":…}
echo $?  # 0
```

- [ ] **Step 5: Commit** `feat(cli): --output-format json RunResult envelope`

---

### Task W0.3: RPC prompt returns envelope

**Files:**
- Modify: `crates/one-cli/src/modes/rpc.rs`
- Modify: `docs/cli.md` (RPC table)

- [ ] Change `prompt` success shape to include at least:

```json
{
  "id": "1",
  "ok": true,
  "result": {
    "text": "…",
    "session_id": "…",
    "duration_ms": 123,
    "usage": { "input_tokens": 0, "output_tokens": 0 }
  }
}
```

Keep `result.text` for backward compatibility; add fields alongside.

- [ ] Document in `docs/cli.md`
- [ ] Commit `feat(rpc): richer prompt result envelope`

---

### Task W0.4: AppRuntime helpers for envelope fields

**Files:**
- Modify: `crates/one-cli/src/runtime/mod.rs` / `session.rs`

- [ ] Add `session_id()`, `session_path_display()`, `usage_snapshot()` if missing (read from agent token_usage + session header).
- [ ] Unit/integration as needed.
- [ ] Commit `feat(runtime): expose session/usage for RunResult`

---

## W1 — Explore subagent (`task` tool)

### Task W1.1: `SubAgentRequest` / `SubAgentResult` + factory trait

**Files:**
- Create: `crates/one-cli/src/runtime/subagent.rs`

```rust
#[derive(Debug, Clone, Copy)]
pub enum SubAgentMode { Explore }

pub struct SubAgentRequest {
    pub prompt: String,
    pub description: Option<String>,
    pub mode: SubAgentMode,
    pub parent_run_id: Option<String>,
    pub parent_call_id: Option<String>,
}

pub struct SubAgentResult {
    pub ok: bool,
    pub summary: String,
    pub turns: usize,
    pub duration_ms: u64,
    pub error: Option<String>,
}

#[async_trait]
pub trait SubAgentRunner: Send + Sync {
    async fn run(&self, req: SubAgentRequest) -> SubAgentResult;
}
```

- [ ] Commit types first if useful, or fold into W1.2.

---

### Task W1.2: Implement explore runner

**Files:**
- Modify: `crates/one-cli/src/runtime/subagent.rs`
- Modify: `crates/one-cli/src/runtime/build.rs` (wire later)

Behavior:

1. Build tools = `read_only_tools_with_policy(policy)` (+ web if network feature) — **no** `task`, write, bash, MCP.  
2. `AgentConfig { system_prompt: SUBAGENT_EXPLORE_PROMPT, max_turns: 16, thinking_level: Off }`.  
3. `agent.prompt(provider, &req.prompt).await`.  
4. Truncate summary (reuse `one_tools::truncate` or 50KB cap).  
5. Share abort: if `AppRuntime` has abort flag, poll or pass into child (minimal: check before/after; better: shared `Arc<AtomicBool>` if Agent already has abort_flag API — use `agent.abort_flag` clone if public, else add getter).

```rust
const SUBAGENT_EXPLORE_PROMPT: &str = r#"You are a read-only sub-agent of One.
Complete the delegated research task, then stop.
- Use only provided tools (read/search/list/web).
- Do not ask the user questions.
- Final message: concise findings, key paths/symbols, residual risks. No task restatement."#;
```

- [ ] Unit test: tool name set for explore excludes `write`, `bash`, `task`.
- [ ] Commit `feat(cli): SubAgentRunner explore implementation`

---

### Task W1.3: `TaskTool` as `one_core::Tool` ✅

**Files:**
- Create: `crates/one-cli/src/runtime/task_tool.rs` (preferred)  
  **or** `crates/one-tools/src/task_tool.rs` with `Arc<dyn SubAgentRunner>` injected — only if trait lives in `one-core` or a tiny shared place.

Recommended: **keep tool in one-cli** to avoid trait in core:

```rust
pub struct TaskTool {
    runner: Arc<dyn SubAgentRunner>,
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str { "task" }
    fn description(&self) -> &str { /* explore-focused */ }
    fn parameters_schema(&self) -> Value { /* prompt, description, mode */ }
    async fn execute(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let prompt = args["prompt"].as_str().ok_or(…)?.to_string();
        let mode = parse_mode(args.get("mode")); // default Explore
        if mode != SubAgentMode::Explore {
            return Ok(ToolOutput::text("only mode=explore is supported in MVP"));
        }
        let res = self.runner.run(SubAgentRequest { prompt, … }).await;
        Ok(ToolOutput::text(format_task_result(&res)))
    }
}
```

- [ ] Register in Act tool list via `runtime/tools.rs` / `build.rs`.  
- [ ] Plan mode: either omit `task` or force explore only (prefer **allow explore-only**).  
- [ ] Concurrent: ensure `task` classified with read-only tools in `run_tool_batch` if classification is name-based — **check** `one-core/agent.rs` for concurrent-safe tool names; add `"task"` if needed.

- [ ] Commit `feat(cli): task tool delegates to explore SubAgentRunner`

---

### Task W1.4: System prompt one-liner + docs

**Files:**
- Modify: `crates/one-core/src/agent.rs` `DEFAULT_SYSTEM_PROMPT` **or** runtime overlay in `prompt.rs` (prefer **runtime overlay** so core stays generic).

```text
- For large multi-file exploration, use the `task` tool (mode=explore) so findings return as a summary without bloating this conversation. Do not use task for a single trivial read.
```

- [ ] Update `docs/cli.md` tools table + `docs/subagents.md` checkboxes for P1a.  
- [ ] Commit `docs+prompt: when to use task tool`

---

### Task W1.5: E2E mock nested task

**Files:**
- Modify: `crates/one-cli/tests/e2e_mock.rs`

Scenario:

1. Mock provider: first turn parent emits `tool_call` name=`task` with prompt to read a fixture file.  
2. Child explore run needs either nested mock behavior or a **scripted** `SubAgentRunner` in test.

**Pragmatic test approach (pick one):**

| Approach | Pros |
|----------|------|
| **A. Unit-test TaskTool with FakeRunner** | Fast, no full LLM |
| **B. Full e2e with multi-script mock provider** | Higher fidelity, harder |

**MVP:** A + light integration:

```rust
#[tokio::test]
async fn task_tool_formats_runner_summary() {
    struct Fake;
    #[async_trait]
    impl SubAgentRunner for Fake {
        async fn run(&self, req: SubAgentRequest) -> SubAgentResult {
            SubAgentResult { ok: true, summary: format!("saw:{}", req.prompt), turns: 1, duration_ms: 1, error: None }
        }
    }
    let tool = TaskTool { runner: Arc::new(Fake) };
    let out = tool.execute(json!({"prompt":"find auth"})).await.unwrap();
    assert!(out.text.contains("saw:find auth"));
}
```

- [ ] Also test: explore tool list filter unit test.  
- [ ] `cargo test -p one-cli` green.  
- [ ] Commit `test(cli): task tool and explore tool filter`

---

## W2 — Host-direct spawn

### Task W2.1: `one agent run`

**Files:**
- Create: `crates/one-cli/src/agent_cmd.rs`
- Modify: `crates/one-cli/src/cli.rs` `Commands::Agent`

```bash
one agent run explore -p "Locate auth module" --provider mock -y --output-format json --cwd .
# optional: one agent run ./path/to/custom.md …
```

MVP agents: built-in name `explore` only (same as SubAgentMode::Explore).  
Output: `RunResult` JSON.

- [ ] Reuse `SubAgentRunner` — **do not** duplicate agent build.  
- [ ] Commit `feat(cli): one agent run explore`

---

### Task W2.2: RPC `spawn`

**Files:**
- Modify: `crates/one-cli/src/modes/rpc.rs`

```json
{"id":"1","method":"spawn","params":{"agent":"explore","prompt":"…"}}
→ {"id":"1","ok":true,"result":{"text":"…","duration_ms":…,"turns":…}}
```

- [ ] Document in `docs/cli.md`.  
- [ ] Commit `feat(rpc): spawn method for host orchestration`

---

### Task W2.3: Example external workflow

**Files:**
- Create: `examples/workflows/parallel-explore.sh`
- Create: `examples/workflows/README.md`

```bash
#!/usr/bin/env bash
set -euo pipefail
# Requires: one on PATH, real or mock provider
A=$(one agent run explore -p "Summarize src/ layout" --output-format json -y --no-session)
B=$(one agent run explore -p "List public APIs" --output-format json -y --no-session)
echo "A=$(echo "$A" | jq -r .result | head -c 200)"
echo "B=$(echo "$B" | jq -r .result | head -c 200)"
```

- [ ] Commit `docs: example external multi-agent workflow script`

---

### Task W2.4: Roadmap / architecture checkboxes

- [ ] Mark P0/P1/P2 items done in `docs/roadmap.md` as each ships.  
- [ ] architecture status: 📝 → ✅ for shipped rows.  
- [ ] Final commit `docs: mark programmatic subagent phases complete` (only when true).

---

## Out of scope (this plan)

| Item | Where |
|------|--------|
| `mode=general` write subagents | P1.x later |
| `agents/*.md` full frontmatter | P1b follow-up PR |
| background task + notify | P4 |
| YAML `one workflow run` | P3b |
| QuickJS / Teams | Non-goals |
| nested task depth > 1 | Non-goal MVP |

---

## PR sequence (merge order)

| PR | Title | Tasks |
|----|-------|--------|
| **PR1** | `feat(cli): RunResult --output-format json` | W0.1–W0.4 |
| **PR2** | `feat(cli): explore subagent + task tool` | W1.1–W1.5 |
| **PR3** | `feat(cli): one agent run + RPC spawn` | W2.1–W2.3 |
| **PR4** | `docs: phases complete + examples` | W2.4 + polish |

---

## Verification (definition of done)

```bash
# W0
cargo test -p one-cli
cargo run -p one-cli -- -p "hello" --provider mock -y --no-session --output-format json
# → ok:true JSON; exit 0

# W1
cargo test -p one-cli task_
# interactive optional: model calls task explore

# W2
cargo run -p one-cli -- agent run explore -p "list top-level files" --provider mock -y --output-format json --no-session
# examples/workflows/parallel-explore.sh with mock if supported
```

---

## Execution handoff

**Plan saved to** `docs/plans/2026-07-19-programmatic-subagents.md`.

**Suggested execution order:** PR1 (W0) first — unblocks CI scripting immediately even before subagents.

When implementing:

1. **Subagent-driven** — one PR/task cluster per subagent, review between, or  
2. **Inline** — execute W0→W2 in this repo with checkpoints after each PR-sized chunk.

Do **not** start QuickJS, Teams, or YAML engine in this plan.
