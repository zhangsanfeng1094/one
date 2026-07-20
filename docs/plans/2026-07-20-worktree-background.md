# Worktree Isolation + Background Subagent Implementation Plan

> **For agentic workers:** Implement task-by-task. Steps use checkbox (`- [ ]`) syntax. Prefer small PRs matching **BG0 → BG1 → WT0 → WT1 → COMBINE**.  
> **Design specs:** [subagents.md](../subagents.md) · [claude-workflow-model.md](../claude-workflow-model.md) · prior discussion (worktree / background optimization)  
> **Depends on:** P1a/P1b landed — `harness::run`, `TaskTool`, `TaskExitStatus`, explore whitelist, `BackgroundTaskRegistry` (bash only).

**Goal:** Unblock long subagent work without freezing the parent turn (**background + notifications**), and make parallel **writable** children safe via **git worktree isolation** — without cloning Claude Teams UI or embedding a second runtime.

**Non-goals (this plan):**

- Multi-thread TUI like Codex `/agent` steer of live children (optional later `/jobs` list only).
- Auto-merge worktree back into parent tree.
- Nested spawn depth > 1.
- Putting agent/job harness into `one-tools` (meta-tools stay in `one-cli`).
- CSV fan-out / Agent Teams.

**Binding principles:**

1. **Agent ≡ Subagent** — still one `harness::run(RunRequest) → RunResult`.
2. **Background reuses bash notification semantics** — drain queue at parent turn boundary; no mid-stream injection.
3. **JobRegistry lives in one-cli** — share the *same* notification drain as bash by composing queues at runtime build, not by pushing LLM into `one-tools`.
4. **Worktree only when writes matter** — explore defaults `isolation=none`; general + parallel/background prefers `worktree`.
5. **No child token stream to main TUI** — progress only (`Turn n/max`); final summary once.
6. **Machine-readable status** — extend `TaskExitStatus` with `Started` for immediate bg ack; terminal statuses unchanged.

**Recommended ship order:**

```text
BG0  background explore (started + notify + task_output/kill)
BG1  progress + parent abort cancels jobs + TUI labels
WT0  WorktreeManager + CLI --isolation worktree
WT1  task isolation= + PathPolicy on worktree cwd + envelope path/branch
CMB  rules: bg general → default worktree; parallel write warn
```

BG0 is independently valuable on today's explore-only MVP. WT* can wait until `general` exists, but WT0 CLI can land early for host scripts.

---

## File map (target)

| Path | Role |
|------|------|
| `crates/one-cli/src/runtime/jobs.rs` | **New** — `AgentJobRegistry` (spawn/poll/kill/notify) |
| `crates/one-cli/src/runtime/task_tool.rs` | `background`, `isolation` args; started envelope |
| `crates/one-cli/src/runtime/task_output.rs` | **New** — `task_output` / `task_kill` tools (or `job_*`) |
| `crates/one-cli/src/runtime/worktree.rs` | **New** — `WorktreeManager` (git worktree add/remove) |
| `crates/one-cli/src/runtime/harness.rs` | Apply isolation → cwd; cleanup policy |
| `crates/one-cli/src/runtime/build.rs` | Wire JobRegistry + shared notify drain; register tools |
| `crates/one-cli/src/protocol.rs` | `IsolationMode`, `JobId`, status `started`, result `worktree` fields |
| `crates/one-tools/src/tasks.rs` | **Minimal change** — optional shared `NotificationBus` trait *or* leave bash as-is and merge drains in cli |
| `crates/one-core` agent loop | Drain notifications before each LLM call (already for bash — verify path works for agent jobs) |
| `crates/one-cli/tests/e2e_mock.rs` | bg task + notify; worktree mock/git fixture |
| `docs/subagents.md` | § background + isolation binding |
| `docs/cli.md` | flags / tools |

**Dependency rule:** `one-tools` stays free of agent harness. If notification merge needs a tiny shared type, prefer `Arc<Mutex<Vec<String>>>` composition in `one-cli` runtime over expanding `BackgroundTaskRegistry` with LLM jobs.

---

## Protocol deltas

### Isolation

```rust
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    #[default]
    None,
    Worktree,
    // Copy, // later: non-git fallback
}
```

On `AgentSpec`:

```rust
pub isolation: IsolationMode, // default None
```

On `RunResult` / task `details` (optional block):

```json
"worktree": {
  "path": "/tmp/one-wt-task_abc",
  "branch": "one/task-abc",
  "base_ref": "abc1234",
  "kept": false
}
```

### Background / jobs

Extend `TaskExitStatus`:

| status | when |
|--------|------|
| `started` | background spawn ack only (`ok` may be true meaning "accepted") |
| existing terminal | success / max_turns_exceeded / aborted / runtime_error / incomplete_info |

`task` tool args:

```json
{
  "prompt": "…",
  "agent": "explore",
  "description": "optional",
  "background": false,
  "isolation": "none"
}
```

Immediate background tool_result:

```text
[task · explore · status=started · id=task_…]
Background sub-agent started. Continue other work; result arrives as a notification or via task_output.
```

```json
{
  "ok": true,
  "status": "started",
  "job_id": "task_…",
  "background": true,
  "agent": "explore"
}
```

Terminal notification (parent drain):

```text
[task · explore · status=success · id=task_… · bg]
<summary>
```

### Cleanup policy (worktree)

```rust
pub enum WorktreeCleanup {
    DropAlways,      // default for success-only short tasks
    KeepOnError,     // default recommended
    KeepAlways,      // debug / host asked
}
```

MVP: `KeepOnError` + settings later. **Never auto-merge.**

---

## BG0 — Background explore (MVP)

### Task BG0.1: `AgentJobRegistry`

**Files:**
- Create: `crates/one-cli/src/runtime/jobs.rs`
- Modify: `crates/one-cli/src/runtime/mod.rs`

- [ ] **Step 1: Types**

```rust
pub struct AgentJobRegistry {
    jobs: Mutex<HashMap<String, AgentJob>>,
    notifications: Arc<Mutex<Vec<String>>>, // same shape as bash queue entries
    task_slots: Arc<Semaphore>,             // shared logical concurrency
}

pub struct AgentJob {
    pub id: String,
    pub agent: String,
    pub description: Option<String>,
    pub state: JobState, // Running | Completed | Aborted | Failed
    pub result: Option<RunResult>,
    pub abort: AbortHandle, // or AtomicBool + select in harness later
    pub started: Instant,
}

impl AgentJobRegistry {
    pub fn notification_queue(&self) -> Arc<Mutex<Vec<String>>>;
    pub async fn spawn_agent(/* RunRequest build + harness */) -> Result<String, ProtocolError>;
    pub fn snapshot(&self, id: &str) -> Option<JobSnapshot>;
    pub fn list(&self) -> Vec<JobSnapshot>;
    pub async fn kill(&self, id: &str) -> Result<(), ProtocolError>;
}
```

- [ ] **Step 2: Spawn path**
  - Acquire `task_slots` permit **owned** across job lifetime.
  - `tokio::spawn` → `harness::run`.
  - On finish: store `RunResult`, push notification string via `format_task_output` (+ `· bg` + `id=`).
  - Release permit.

- [ ] **Step 3: Unit tests** — fake harness completes; queue has one notification; concurrent slot = 1 still finishes two jobs.

- [ ] **Step 4: Commit** `feat(cli): AgentJobRegistry for background subagents`

### Task BG0.2: Wire notification drain with bash

**Files:**
- Modify: `crates/one-cli/src/runtime/build.rs` (or wherever bash registry + agent notifications attach)
- Inspect: `one-core` agent loop notification drain (bash path)

- [ ] **Step 1:** At runtime build, either:
  - **A (preferred):** single `Arc<Mutex<Vec<String>>>` shared by bash registry + agent jobs, or
  - **B:** drain both queues into one list before each LLM turn.

- [ ] **Step 2:** Confirm parent agent sees bg completion as model-visible text (same as bash notify tests).

- [ ] **Step 3: Commit** `feat(cli): merge agent job notifications into turn drain`

### Task BG0.3: `task(background=true)`

**Files:**
- Modify: `crates/one-cli/src/runtime/task_tool.rs`
- Modify: `crates/one-cli/src/protocol.rs` (`Started` status if needed)

- [ ] **Step 1:** Parse `background` bool (default false).
- [ ] **Step 2:** If false → existing sync `harness.run`.
- [ ] **Step 3:** If true → `registry.spawn_agent` → return `status=started` + `job_id` immediately (do **not** wait for summary).
- [ ] **Step 4:** Reject background if spawn_policy forbids / depth exceeded (same as sync).
- [ ] **Step 5:** Tests:
  - sync path unchanged
  - bg returns started without waiting
  - after job done, notification contains summary + terminal status
- [ ] **Step 6: Commit** `feat(cli): task background=true returns started job_id`

### Task BG0.4: `task_output` + `task_kill`

**Files:**
- Create: `crates/one-cli/src/runtime/task_output.rs`
- Modify: `crates/one-cli/src/runtime/build.rs` — register only on parents with `can_spawn()`

- [ ] **Step 1:** `task_output`
  - args: `task_id?`, `wait_ms?` (optional short wait)
  - omit id → list running/done jobs (Codex `/ps` vibe, text table)
  - with id → snapshot; if completed, same body as sync task_result

- [ ] **Step 2:** `task_kill`
  - set abort; harness must observe abort (wire `OneError::Aborted` if not already for nested)

- [ ] **Step 3:** Prompt hint one-liner for main agent.

- [ ] **Step 4: e2e** scripted parent: start bg task → other noop tool → next turn sees notify **or** explicit `task_output`.

- [ ] **Step 5: Commit** `feat(cli): task_output and task_kill tools`

### Task BG0.5: Docs + roadmap checkboxes

- [ ] Update `docs/subagents.md` §4.4 (sync → bg optional; registry)
- [ ] `docs/roadmap.md` P4 background → split BG0 done items
- [ ] Commit `docs: background subagent BG0`

**BG0 acceptance:**

| Test | Expect |
|------|--------|
| `task(background=true, agent=explore)` | `status=started`, `job_id` set, returns &lt; 100ms after spawn (mock) |
| Job finishes | notification drained next parent turn; status terminal |
| `task_output(id)` after done | summary matches |
| `task_kill` mid-flight | `aborted` |
| `ONE_LLM_CONCURRENCY=1` + 2 bg | both complete, no deadlock |
| Sync `task` | unchanged behavior |

---

## BG1 — Progress, abort, TUI polish

### Task BG1.1: Coarse progress

- [ ] Child harness emits `ToolProgress`-equivalent or job registry `turns` counter (optional poll).
- [ ] Parent TUI tool row: `▸ task · explore · bg · turn 3/16` — **no tokens**.
- [ ] Commit `feat(cli/tui): background task progress label`

### Task BG1.2: Parent abort cancels all jobs

- [ ] On session Esc / RPC abort: `registry.kill_all()`.
- [ ] Test: parent abort → job `aborted`.
- [ ] Commit `feat(cli): abort propagates to agent jobs`

### Task BG1.3: Budgets

- [ ] Optional `max_wall_ms` per job (default from settings or 300_000).
- [ ] Timeout → `runtime_error` or dedicated status later; notification still fires.
- [ ] Commit `feat(cli): background job wall-time budget`

---

## WT0 — WorktreeManager + CLI

### Task WT0.1: `WorktreeManager`

**Files:**
- Create: `crates/one-cli/src/runtime/worktree.rs`

- [ ] **Step 1: API**

```rust
pub struct WorktreeHandle {
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
    pub repo_root: PathBuf,
}

pub struct WorktreeManager { /* temp root under std::env::temp_dir()/one-worktrees or .one/worktrees */ }

impl WorktreeManager {
    pub fn create(&self, repo: &Path, job_id: &str) -> Result<WorktreeHandle, WorktreeError>;
    pub fn remove(&self, handle: &WorktreeHandle, force: bool) -> Result<(), WorktreeError>;
    pub fn gc_stale(&self, max_age: Duration) -> usize;
}
```

- [ ] **Step 2: Implementation notes**
  - Require `.git` (or worktree git-common-dir); else `WorktreeError::NotAGitRepo`.
  - Prefer: `git worktree add -b one/task-<id> <path> HEAD` (base = **current HEAD**, not remote default branch).
  - Path: `{cache}/one-wt-<id>` under user cache or repo `.one/worktrees/` (document choice: **repo `.one/worktrees/`** for easy discoverability, gitignore that dir).
  - Add `.one/worktrees/` to example gitignore docs if needed.

- [ ] **Step 3: Unit tests** — use `tempfile` + `git init` fixture; create + remove roundtrip.

- [ ] **Step 4: Commit** `feat(cli): git WorktreeManager`

### Task WT0.2: CLI flag

- [ ] `one agent run explore|general -p "…" --isolation worktree`
- [ ] Print worktree path in JSON envelope.
- [ ] Cleanup per policy after run.
- [ ] Commit `feat(cli): --isolation worktree for agent run`

---

## WT1 — Task + harness isolation

### Task WT1.1: Protocol + harness

- [ ] `AgentSpec.isolation` / `RunRequest` carry mode.
- [ ] `harness::run`: if `Worktree`, create handle, set `opts.cwd` + PathPolicy workspace to handle.path, run, attach `worktree` to `RunResult`, cleanup.
- [ ] explore + isolation=worktree: **allow but document as low value**; do not force.
- [ ] Commit `feat(cli): harness honors isolation=worktree`

### Task WT1.2: `task` argument

- [ ] Parse `isolation`: `none` | `worktree`.
- [ ] Pass into child `RunRequest`.
- [ ] Envelope / tool details include `worktree` block when used.
- [ ] Child system prompt append when worktree: "You are in an isolated git worktree; do not assume parent tree paths outside this cwd."
- [ ] Tests: mock harness receives cwd under worktree path (inject fake manager in tests).
- [ ] Commit `feat(cli): task isolation=worktree`

### Task WT1.3: Safety rails

- [ ] If `isolation=worktree` and not a git repo → `runtime_error` with clear message (no silent none).
- [ ] Max concurrent worktrees = `spawn_policy.max_concurrent` (reuse).
- [ ] `one agent worktree gc` optional CLI (can defer).
- [ ] Commit `fix(cli): worktree errors and concurrency cap`

**WT1 acceptance:**

| Test | Expect |
|------|--------|
| git fixture + isolation worktree | child writes only under worktree path |
| main tree clean after drop cleanup | no leftover branch if remove success (or orphan branch documented) |
| non-git | explicit error |
| tool_result details | `worktree.path` present |

---

## CMB — Combine rules (general + bg + wt)

> Depends on future **general** mode (writable tools). If general not landed, ship rules as code comments + docs only; enable when `is_supported_child` expands.

### Task CMB.1: Default matrix

| agent | background | isolation default |
|-------|------------|-------------------|
| explore | false/true | `none` |
| general | false | `none` |
| general | true | **`worktree`** |
| general | any, and parent already has other running write job | require worktree or queue serial |

- [ ] Implement defaults in `build_child_request` / TaskTool (explicit user isolation wins).
- [ ] Parallel two general with `none` → warn in tool_result or deny second (prefer **warn once** in MVP, **deny** if `ONE_STRICT_ISOLATION=1`).
- [ ] Commit `feat(cli): isolation defaults for background general`

### Task CMB.2: Docs + examples

- [ ] `docs/subagents.md` matrix + cleanup policy.
- [ ] Example: external script two `one agent run --isolation worktree` (host-level parallel, no task tool).
- [ ] Commit `docs: worktree + background operator guide`

---

## PR cut plan

| PR | Title | Scope |
|----|-------|-------|
| **PR-BG0a** | `feat(cli): AgentJobRegistry + notify drain` | BG0.1–0.2 |
| **PR-BG0b** | `feat(cli): task background + task_output/kill` | BG0.3–0.5 |
| **PR-BG1** | `feat(cli): bg progress, abort, wall budget` | BG1.* |
| **PR-WT0** | `feat(cli): WorktreeManager + --isolation` | WT0.* |
| **PR-WT1** | `feat(cli): task/harness isolation=worktree` | WT1.* |
| **PR-CMB** | `feat(cli): isolation defaults with general` | CMB.* (after general) |

---

## Testing checklist (all phases)

| # | Case | Phase |
|---|------|-------|
| 1 | bg explore started + notify summary | BG0 |
| 2 | task_output list / get | BG0 |
| 3 | task_kill → aborted | BG0 |
| 4 | concurrency=1 two bg jobs | BG0 |
| 5 | parent abort kills jobs | BG1 |
| 6 | wall timeout notifies error | BG1 |
| 7 | git worktree create/remove | WT0 |
| 8 | harness cwd = worktree | WT1 |
| 9 | non-git isolation errors | WT1 |
| 10 | bg general defaults worktree | CMB |
| 11 | sync task regression | always |

Mock strategy: inject `TaskHarness` / fake `WorktreeManager` trait for unit tests; one git integration test behind `#[cfg]` or always if cheap.

---

## Risk register

| Risk | Mitigation |
|------|------------|
| Notification races (double-deliver) | mark job `notified`; drain pop; bash pattern |
| Deadlock LLM permit + bg | parent never holds whole-run permit if can_spawn (already); jobs acquire per child only |
| Worktree disk leak | KeepOnError + gc command; cap concurrent |
| Model ignores notifications | prompt hint + task_output fallback |
| TUI spam | no child tokens; one line progress |
| Scope creep to Teams UI | explicitly out of plan |

---

## Open defaults (locked for implementers)

| Question | Default |
|----------|---------|
| Tool names | `task_output`, `task_kill` (not bash_output overload) |
| Job id prefix | `task_` |
| Worktree base | current `HEAD` |
| Worktree path | `<repo>/.one/worktrees/<id>` (+ gitignore) |
| Auto-merge | **never** |
| explore + worktree | allowed, not default |
| bg + explore | isolation none |
| Status `started` | `ok: true` means accepted, not "research done" |
| Child stream | still forbidden |

---

## Relation to existing roadmap

```text
P1b task tool          ✅ done
P2  RPC spawn / agents.md   independent track
P4  background              → this plan BG0–BG1
    worktree                → this plan WT0–WT1 (also enables safe general parallel)
general mode                → prerequisite for CMB value; can land between BG0 and WT1
```

Implement **BG0 first** even if general/worktree slip — it improves today's explore UX and CI-style long research without blocking the parent.

---

## Revision

| Date | Note |
|------|------|
| 2026-07-20 | Initial plan from architecture comparison + bash registry reuse strategy |
