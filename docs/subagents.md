# 子 Agent（Sub-agent）设计

> **状态**：P1a+P1b 已落地（2026-07-19；CLI harness + TaskTool → harness::run；工程批判：并发限流、TUI 无子流、exit status、TaskTool 仅 cli、explore 白名单）  
> **背景**：architecture 曾将「子 Agent orchestrator」标为非目标。产品目标升级为：**可像 Claude 一样程序化执行 / 编排 agent 完成任务**。  
> **必读对照**：[claude-workflow-model.md](./claude-workflow-model.md) · [protocol.md](./protocol.md)（Agent≡Subagent）  
> **相关**：[architecture.md](./architecture.md) · [extensions.md](./extensions.md)

---

## 0. 在整体路线中的位置

```text
P0  CLI harness 单入口（完整 JSON + preset）  →  不经主模型也能跑
P1  封装 task 工具 → 调同一 harness              →  主会话内委托
P2  更多 preset / agents.md / RPC spawn
P3  外置脚本 workflow（默认）
P4  Teams 等（可选）
```

### 0.1 实现顺序（绑定）

```text
①  CLI 层先跑通 harness
      one run --preset explore -p "…"
      one run --spec ./agent.json -p "…"
      → RunResult JSON

②  再封装工具
      TaskTool.execute = harness::run(RunRequest::from_preset_or_spec(…))
      主模型只看见 tool，不看见第二套 runtime

③  基础 preset 的 harness JSON 入库
      builtin: explore（及日后 general…）
      用户可 --spec 覆盖完整 AgentSpec
```

**原则**：Subagent **不是** 先写一个特殊 TaskTool 再倒推出 CLI；而是 **CLI/harness 为一等能力**，工具只是薄封装。

Subagent **不是** workflow 本身；它是 workflow 与主 agent 共用的 **worker 单元**。

**同构契约**：根 Agent 与 Subagent 共用 [`AgentSpec` / `RunRequest`](./protocol.md)。  
**两种装载方式**（见 §0.2）：**完整 harness JSON** 与 **preset 名**。

### 0.2 完整 JSON vs Preset

| 方式 | 入口示例 | 解析结果 |
|------|----------|----------|
| **Preset** | `--preset explore` / `task agent=explore` / `agents.explore` 引用 | 加载内置或磁盘 preset → **展开为完整 AgentSpec** |
| **完整 JSON** | `--spec path.json` / RPC `agent: { …完整 AgentSpec }` / `agent_spec` 内联 | 直接反序列化为 AgentSpec |

运行时内存里 **永远是完整 AgentSpec**；preset 只是快捷方式 + 默认可分发模板。

```text
--preset explore  ──┐
                    ├──► AgentSpec ──► harness::run ──► RunResult
--spec foo.json  ───┘
task { agent: "explore" } ──► 同上
task { agent_spec: {…} }  ──► 同上（完整 JSON）
```

**基础 preset（MVP 至少提供）**

| preset id | 含义 | 默认 JSON 位置（概念） |
|-----------|------|------------------------|
| `explore` | 只读调研白名单 | 内置 + 可导出 `one agent dump explore` |
| `main` / `default` | 根 coding（可选） | 由 settings/flags 编译，不必文件 |

用户可把 dump 出的 JSON 改完再 `--spec`，或放到 `~/.one/agent/agents/*.md|json` 当自定义 preset。

---

## 1. 目标与非目标

### 1.1 目标

1. **程序化可调用** — 主模型 tool、CLI、`RPC spawn`、未来 SDK 共用同一 runner  
2. **隔离上下文** — 子任务的大量 `read`/`grep` 不污染主 session；只回 **摘要**  
3. **可复用角色** — `agents/*.md` frontmatter（对齐 Claude `.claude/agents`）  
4. **并行探索** — 一回合多个只读子任务（对齐 Claude Explore）  
5. **可观测** — TUI / Langfuse parent/child  
6. **安全默认** — 更严工具面；无 TTY 时 fail-closed

### 1.2 非目标（至少 MVP / v1）

| 非目标 | 原因 |
|--------|------|
| 任意深度树状编排 UI | 复杂度爆炸；Claude 也限深度 |
| 子 agent 独立长期会话 UX（`/resume` 子树） | 可后做；MVP 只返回 tool result |
| 跨进程隔离 / 远程 worker 池 | 过重；同进程 nested `Agent` 足够 |
| 自动任务分解 orchestrator（主模型不参与） | 与「极简 core」冲突；分解仍由主 LLM 决定 |
| 替代 skills / plan mode | skills 仍 progressive disclosure；plan 仍主会话 |

### 1.3 成功标准

**P1a（会话内 task）**

- [ ] 主模型可调用 `task`，`prompt` + `agent`/`mode`  
- [ ] 独立 messages；摘要 + **机器可读 status** 进 tool_result  
- [ ] explore：**硬白名单**工具；无 write/bash/MCP/嵌套 task  
- [ ] 逻辑并发上限 4 + **全局 Provider Semaphore**（可降级串行）  
- [ ] TUI：**无子 token 流**；仅 progress + 结束 summary  
- [ ] abort 传播；mock e2e（含并行不饿死 provider）

**P1b（定义文件）**

- [ ] 加载 `~/.one/agent/agents/*.md` 与 `.one/agents/*.md`  
- [ ] frontmatter：`name` `description` `tools` `model` `maxTurns`  

**与 P0/P2 联调（见 claude-workflow-model.md）**

- [ ] 宿主 `one agent run <name> -p "…" --output-format json`  
- [ ] RPC `spawn` 返回与 tool_result 同形的 envelope  

---

## 2. 方案对比

### A. 同进程 nested `Agent` + `task` Tool（**推荐**）

```text
Main Agent::run
  └─ tool_call: task { prompt, mode }
        └─ SubAgent::run (new messages, filtered tools, shared policy)
              └─ tool_result → parent (summary only)
```

| 优点 | 缺点 |
|------|------|
| 复用 `one-core::Agent` / Tool / Gate / Trace | 需小心 re-entrancy（gate/HITL） |
| 与现有 batch 并行模型契合 | 子 agent 占用同进程内存/连接 |
| 测试简单（mock provider 嵌套） | 不隔离崩溃（子 panic 影响主） |

### B. 子进程 `one --mode rpc`

| 优点 | 缺点 |
|------|------|
| 进程隔离 | 启动慢、序列化重、配置复制麻烦 |
| | 与 PathPolicy / HITL / session 难共享 |

### C. 仅 Prompt 技巧 / Skill（无真子 agent）

| 优点 | 缺点 |
|------|------|
| 零实现 | **不隔离上下文**；无并行；不满足「子 agent」 |

**决策：选 A。** B 可作为未来「沙箱子 agent」扩展，不进 MVP。

---

## 3. 产品形态

### 3.1 工具：`task`

对模型暴露的名字建议 **`task`**（对齐 Claude；短、好记）。别名可在 schema description 写 `agent`。

**输入 schema（草案）**

```json
{
  "name": "task",
  "description": "Spawn a sub-agent with a fresh context for a focused subtask. Prefer for large exploration so the main conversation stays small. Returns a text summary only.",
  "parameters": {
    "type": "object",
    "required": ["prompt"],
    "properties": {
      "prompt": {
        "type": "string",
        "description": "Full instructions for the sub-agent. Include goal, constraints, and what to return."
      },
      "description": {
        "type": "string",
        "description": "Short label for UI/trace (3–8 words)."
      },
      "mode": {
        "type": "string",
        "enum": ["explore", "general"],
        "description": "explore = read-only tools (default). general = coding tools without nesting task (v1.1)."
      },
      "model": {
        "type": "string",
        "description": "Optional provider:model override for the sub-agent (v1.2; MVP inherits parent)."
      }
    }
  }
}
```

**输出（tool result 文本 + details）**

父模型看到的文本（人类可读 + 机器 trailer）：

```text
[task · explore · status=success · 4 turns · 12.3s]
<summary>

<!-- one:task telemetry
status: success
turns: 4
tool_calls: 11
duration_ms: 12300
-->
```

| `status`（必填，机器可读） | 含义 | 父 agent 应如何理解 |
|---------------------------|------|---------------------|
| `success` | 正常结束 | 摘要可信 |
| `max_turns_exceeded` | 打满 max_turns | **调研不完整**，可再派 task 或自己补查 |
| `aborted` | 用户/宿主中止 | 忽略/重试 |
| `runtime_error` | provider/内部错误 | 看 message；可重试 |
| `incomplete_info` | 信息不足主动收束（见 §3.5） | 父 agent 补上下文后再派 |

`ToolOutput.details`（结构化，优先于解析 trailer）：

```json
{
  "status": "max_turns_exceeded",
  "agent": "explore",
  "turns": 16,
  "duration_ms": 12000,
  "ok": false
}
```

### 3.2 模式与工具集

| mode | 工具组装方式 | 阶段 |
|------|--------------|------|
| `explore`（默认） | **硬编码白名单**（§3.2.1），禁止「coding 减工具」 | **MVP** |
| `general` | coding profile − spawn 工具；PermissionGate | **v1.1** |
| `plan` | plan 工具集 | 延后 |

规则：

1. 子 agent **默认不带 `task`**（`spawn_policy.allow=[]`）。  
2. **不注册 MCP**（MVP）。  
3. **不加载 skills catalog / AGENTS.md**（MVP）。  
4. PathPolicy 继承父 workspace。  
5. **无 ask_user 工具**；HITL fail-closed（§3.5）。  
6. Plan 主会话：仅允许 explore。

#### 3.2.1 explore 工具：**白名单，不是减名单**（绑定）

**禁止** `coding_tools.filter(|t| !is_write)` 这类减法——漏网副作用工具（含未来 web_post、任意 shell 包装）。

```rust
// 唯一合法 explore 集合（名称级；实现与此表同步）
const EXPLORE_TOOLS: &[&str] = &[
    "read", "grep", "find", "ls",
    "web_search", "web_fetch",  // 仅 GET/搜索语义；无通用 POST 工具
];
// 明确排除：write, edit, bash, bash_*, ask_user, task, MCP, plan 写工具
```

装配时 **逐个 `Box::new(ReadTool…)`**，不从大列表 subtract。

### 3.3 并行语义 + Provider 限流（绑定，防死锁）

| 层 | 规则 |
|----|------|
| **逻辑并发** | 同一 assistant turn 内多个 explore `task`：逻辑上限默认 **4**（`spawn_policy.max_concurrent`） |
| **Provider 槽** | 全进程共享 **`ProviderPermit` Semaphore**（或令牌桶）；父 run 与所有子 run 的 **LLM 调用** 都 acquire 同一把锁 |
| **降级** | 若下游有效并发=1（如本地 Ollama、`ONE_LLM_CONCURRENCY=1`），4 个 task **自动串行排队**，主模型仍可一次发出多个 tool_call，执行层排队，**不**要求模型改策略 |
| **general** | 与 write/bash 同级 **串行**（写冲突） |
| **TPM/429** | 子 run 遇 429：指数退避；反复失败 → `status=runtime_error`，勿饿死父 run |

`SubAgentRunner` / factory **必须**注入：

- `Arc<dyn LlmConcurrency>`（或 `Semaphore`）  
- 父 `abort` flag  
- 同一 `TraceSink`（parent_run_id）

**测试门槛**：`e2e` 用 `concurrency=1` 的 mock provider 并行发 2 个 task → **必须完成且无死锁**。

### 3.4 用户可见行为（TUI：MVP 禁止子流）

| 面 | MVP（绑定） |
|----|-------------|
| TUI tool 行 | `▸ task · explore · <description>` |
| **子 LLM stream** | **禁止**刷到主 transcript / 多路 delta 穿层 |
| 进度 | 仅粗粒度：`ToolProgress` → `Turn 3/16`（可选 spinner），**无 token 碎片** |
| 结束 | 子 run End 后 **一次性** 把 summary 写入 tool_view（静态块） |
| abort | 主 Esc → 取消所有子 permit 等待 + 子 agent abort |
| session | 子消息默认不进主 JSONL |
| trace | Langfuse child under parent；TUI 不依赖子 stream |

Phase 3+ 若做子 stream：必须 **单路展开**（一次只看一个 task 的 transcript），禁止 4 路并行打同一 viewport。

### 3.5 ask_user / 反向提问污染（绑定）

1. **不注册** `ask_user` 工具。  
2. System prompt **硬契约**（见下）。  
3. `TaskTool` 包装层：若 summary 疑似向用户提问（启发式可选）或 status 已是 incomplete，**外层 envelope** 再交给父：

```text
[task · explore · status=incomplete_info]
Sub-agent could not finish without clarification (do not treat as a question to the user).
Reason: …
Partial findings:
…
```

父 agent 应 **自己** `ask_user` 或改写 prompt 再派，而不是把子文本当对话续写。

### 3.6 System prompt（explore 模板，绑定）

```text
You are a read-only sub-agent of One. Complete the delegated research task, then stop.

Rules:
- Use ONLY the tools provided. Prefer read/grep/find/ls over any shell.
- Do NOT ask questions — not via tools, not in thinking, not in the final answer.
- If you lack critical information or permission, end immediately with a single line:
  ERROR: <reason>
  then optional partial findings. Do not wait for a human reply.
- When successful, write a concise final answer: findings first, paths/symbols, residual risks.
- Do not restate the entire task prompt.
```

`TaskTool` 若检测到 final 以 `ERROR:` 开头 → `status=incomplete_info`（或 `runtime_error` 若是权限类）。

---

## 4. 架构

### 4.1 分层

```text
┌─────────────────────────────────────────────────────────┐
│ one-cli AppRuntime                                      │
│  · AgentSpec 编译 · ProviderPermit · TaskTool 就地装配  │
│  · harness::run(RunRequest) → RunResult                 │
└───────────────────────────┬─────────────────────────────┘
                            │ Arc<dyn Tool>  (meta-tool)
┌───────────────────────────▼─────────────────────────────┐
│ one-cli/src/runtime/task_tool.rs   ← 禁止放进 one-tools │
│  · args → RunRequest::child                             │
│  · envelope summary + status                            │
└───────────────────────────┬─────────────────────────────┘
                            │
┌───────────────────────────▼─────────────────────────────┐
│ one-core Agent::run（同构）                             │
│  · 新 messages + explore 白名单 tools                   │
│  · 每次 LLM 调用 acquire ProviderPermit                 │
│  · 无子 stream 事件到主 TUI（仅 progress 可选）         │
└─────────────────────────────────────────────────────────┘
```

**依赖纪律（绑定）**

| 位置 | 允许 |
|------|------|
| `one-tools` | 纯 IO：read/write/bash/grep… **禁止** `task` / 感知 Agent |
| `one-cli/runtime` | **元工具** `TaskTool`、factory、AgentSpec 装配 |
| `one-core` | 通用 `Agent` loop；可选共享 abort；**不**写死 task 业务名 |

`task` 是 **meta-tool**（绑死 AgentSpec、Runtime、token、permit），不是「系统 IO 工具」。

### 4.2 退出状态（绑定）

```rust
// one-cli protocol / runtime — 与 RunResult.stop_reason / details.status 对齐
pub enum TaskExitStatus {
    Success,
    MaxTurnsExceeded,  // 父 agent 必须能区分「查完了」vs「没查完」
    Aborted,
    RuntimeError,
    IncompleteInfo,    // ERROR: … 或信息不足
}
```

`ok` 字段：`Success` → true；其余 → false（但 summary 仍可带 partial findings）。

### 4.3 HITL

子 run：**不弹** ask / 审批列表。  
`dont_ask` + 无 ask_user 工具；gate 遇 ask → Deny 文案固定。

### 4.4 与 background bash 的关系

| | background bash | sub-agent（MVP） | sub-agent（计划） |
|--|-----------------|------------------|-------------------|
| 单位 | shell 进程 | LLM loop | 同左 |
| 完成通知 | `BackgroundTaskRegistry` notification_queue | **同步** tool_result | 同队列；**AgentJobRegistry** → `[job completed]` |
| 轮询 | `bash_output` / `bash_kill` | 无（阻塞到结束） | `job_output` / `job_kill` ✅ |
| 阻塞等待 | — | — | **`wait_tasks`**（mode=all/any；完成流）✅ |
| 立即返回 | `run_in_background=true` → id | 否 | `background=true` → `status=started` + `job_id` ✅ |

**实现计划**：[plans/2026-07-20-worktree-background.md](./plans/2026-07-20-worktree-background.md)。**BG0 已落地**（2026-07-20）。

要点（绑定）：

1. Agent job **不**塞进 `one-tools`；与 bash **共享** `Arc<Mutex<Vec<String>>>` notification queue。  
2. 通知只在 **父 turn 边界** `drain_notifications` 注入 User 消息。  
3. 终态 status 与同步 `task` 同形；`started` 仅表示已接受。  
4. 仍 **禁止** 子 token 流刷主 TUI。  
5. 默认 **同步**；`background=true` 显式开启。

### 4.4.1 Worktree 隔离（计划）

| `isolation` | 含义 | 默认 |
|-------------|------|------|
| `none` | 继承父 cwd / PathPolicy | explore；同步 general |
| `worktree` | `git worktree add`；子 PathPolicy root = 新路径 | **bg general 建议默认** |

- **不**自动 merge 回主树；envelope 带回 `path` / `branch`。  
- base = 当前 **HEAD**（非远程 default branch）。  
- 非 git 仓库 → 显式错误，不 silent 降级。  
- 详见 plan WT0–WT1 / CMB。

### 4.5 截断与预算

| 参数 | 默认 | 说明 |
|------|------|------|
| 子 `max_turns` | 16（低于主 32） | 可 settings `subagent.max_turns` |
| 子 summary 进主上下文 | 50KB / 2000 行（复用 truncate） | 防撑爆主 session |
| 并发 task 上限 | 4 | 超出串行或 deny |
| 总 wall time | 可选 300s | 超时 abort 子 run |

### 4.6 事件

扩展 `AgentEvent`（可选，MVP 可用现有 ToolExecution*）：

```text
SubAgentStart { call_id, mode, description }
SubAgentEnd   { call_id, ok, turns, duration_ms }
```

TUI 无此事件时，仍可仅显示 tool start/end。

Trace：

```text
parent run
  └ tool task
       └ child run (agent observation)
            └ child turns / tools
```

---

## 5. 分阶段实现

### Phase 0 — 决策落地（文档）

- [x] 本文档 + 工程批判修订  
- [x] architecture / roadmap 分阶段  
- [x] protocol.md Agent≡Subagent

### Phase 1 — MVP（先 CLI，再工具）

**1a — CLI harness（先做）**

1. `AgentSpec` + `harness::run(RunRequest)→RunResult`  
2. **Preset**：内置 `explore` harness JSON（可 dump）  
3. **完整 JSON**：`one run --spec file.json -p "…"` / `--output-format json`  
4. 快捷：`one agent run explore -p "…"` ≡ `--preset explore`  
5. explore 硬白名单 + ProviderPermit + status  
6. 测试：CLI 单测 / mock 跑通 explore preset 与 custom spec  

**1b — 封装 task 工具（CLI 稳定后）** ✅

1. [x] `TaskTool` **仅** `one-cli/runtime`，内部只调 `harness::run`  
2. [x] 支持 `agent: "explore"`（preset）与可选 `agent_spec` 内联完整 JSON  
3. [x] tool_result status trailer；TUI 无子 stream；并发 permit  
4. [x] 主 prompt 一行；e2e task→harness mock  

**刻意不做**：general、MCP 继承、background、嵌套 depth>1、子 stream UI、QuickJS。

### Phase 2 — general 模式 + 串行写

- mode=general 工具集 = coding 去 task  
- ask 策略 deny 或升级  
- 与 write/bash 一样串行  
- 更多 e2e  

### Phase 3 — 体验与观测

- TUI 折叠展示子 turns  
- Langfuse parent/child 属性打全  
- 可选 `ONE_SUBAGENT_SESSION=1` 落盘  
- settings：max_turns / max_concurrent / default_mode  
- `/settings` 展示  

### Phase 4 — 高级（按需）

- [x] `background=true` + 完成通知（**BG0**）  
- [x] BG1：粗进度 turns/max、父 abort→kill_all、`ONE_JOB_MAX_WALL_MS` wall-time  

### 4.4.2 后台 task 典型场景

| 场景 | 为何用 background |
|------|-------------------|
| 主 agent 边写代码边摸清大模块 | 派 explore 查 auth/billing，自己继续改入口 |
| 并行只读调研（一轮多个 bg task） | 多路摘要稍后以 `[job completed]` 回来，主 context 不塞满中间 grep |
| 长测试/构建日志分析（未来 general） | 不阻塞对话；先 started 再通知 |
| CI/脚本 | 仍更推荐外置 `one agent run`；bg 主要服务**交互主会话** |

**不适合**：需要立刻用结果才能下一步的决策（用默认同步 `task`）；单文件 `read`（别用 task）。

### 4.4.3 `wait_tasks`（前台无事时阻塞等待）

当前台已 `task(background=true)` 派完所有子任务、自己无其他 tool 可做时：

```text
wait_tasks()                    # 默认 mode=all，等全部 running
wait_tasks(mode="any")          # 等下一个完成就返回（可循环收）
wait_tasks(job_ids=["job_a","job_b"], wait_ms=120000)
```

- **阻塞** tool 直到条件满足或 `wait_ms` 超时  
- 返回 **completion stream**（按完成顺序带摘要）  
- 会 **吸收** 对应的 `[job completed]` 通知，避免下一 turn drain 重复  
- 模型在 **单次 tool_result** 里看到全部过程摘要（「每完成一个」体现在 stream 段落里）


- [ ] `isolation=worktree`（**WT0/WT1**；可写并行刚需）  
- 有限深度 2 + 子可 explore-only 再派  
- `model` 覆盖  
- inherit skills / MCP 开关  
- 与 Package profile：`tools.task = true`  

---

## 6. PR 切分建议

| PR | 标题 | 内容 | 依赖 |
|----|------|------|------|
| **PR1** | `feat(core/cli): SubAgentFactory + explore TaskTool` | 类型、factory、TaskTool、runtime 装配、explore 工具过滤 | — |
| **PR2** | `test: sub-agent mock e2e + concurrency budget` | e2e_mock、并发上限、abort 传播 | PR1 |
| **PR3** | `feat(tui/trace): sub-agent labels + parent_run_id` | 事件/trace/TUI 文案 | PR1 |
| **PR4** | `docs: sub-agents + architecture/roadmap` | 文档同步 | PR1 |
| **PR5** | `feat: task mode=general` | 写模式 + 串行 + 权限策略 | PR1–2 |

---

## 7. 实现要点（给编码 agent）

### 7.1 文件地图（MVP）

| 路径 | 动作 | 阶段 |
|------|------|------|
| `crates/one-cli/src/protocol.rs` | AgentSpec / RunRequest / TaskExitStatus | 1a |
| `crates/one-cli/src/runtime/harness.rs` | **`run(RunRequest)→RunResult`** + ProviderPermit | 1a |
| `crates/one-cli/src/runtime/presets.rs` | 内置 explore JSON / 加载 `--preset` / 磁盘 agents | 1a |
| `crates/one-cli/src/runtime/explore_tools.rs` | explore 硬白名单 | 1a |
| `crates/one-cli/src/run_cmd.rs` 或 `agent_cmd.rs` | `one run` / `one agent run` CLI | 1a |
| `presets/explore.json` 或内嵌 str | **基础 subagent harness JSON**（可 dump） | 1a |
| `crates/one-cli/src/runtime/task_tool.rs` | 薄封装 → harness（**禁止** one-tools） | **1b** |
| `crates/one-cli/src/runtime/build.rs` | 注册 task | 1b |
| `crates/one-cli/tests/…` | CLI + 后 task e2e | 1a→1b |

### 7.2 伪代码

```rust
// ① CLI / presets — first
let spec = match args {
    Preset(name) => presets::load(name)?,           // explore → 完整 AgentSpec
    SpecPath(p) => AgentSpec::from_json_file(p)?, // 完整 harness JSON
    Inline(v) => serde_json::from_value(v)?,
};
let req = RunRequest::new(spec, prompt);
let result = harness::run(req).await;
// print --output-format json

// ② TaskTool — thin wrapper only (after CLI works)
async fn execute(&self, args: Value) -> ToolOutput {
    let _slot = self.task_slots.acquire().await;
    let spec = if let Some(inline) = args.get("agent_spec") {
        serde_json::from_value(inline.clone())?
    } else {
        presets::load(args["agent"].as_str().unwrap_or("explore"))?
    };
    let req = RunRequest::child_from_spec(spec, prompt, parent_meta);
    let result = self.harness.run(req).await; // SAME entry as CLI
    format_task_tool_output(result)
}
```

### 7.3 测试清单（Phase 1 门槛）

| 测试 | 期望 |
|------|------|
| explore 工具名 **恰好** 白名单 | 无 write/bash/task/ask_user |
| `ONE_LLM_CONCURRENCY=1` 并行 2 task | 完成、无死锁、无 hang |
| max_turns 打满 | `status=max_turns_exceeded`，details 可解析 |
| final 以 `ERROR:` 开头 | `status=incomplete_info` + envelope |
| abort 父 | 子 `aborted` |
| 子无 task 工具 | 列表不含 task |

### 7.4 System prompt 主 agent 补一句

在 `DEFAULT_SYSTEM_PROMPT` 或 runtime overlay：

```text
- For large exploration across many files, use `task` (mode=explore) so findings return as a summary without bloating this conversation. Do not use task for trivial single-file reads.
```

---

## 8. 风险与缓解

| 风险 | 缓解（已写入规范） |
|------|-------------------|
| Provider 并发死锁 / 429 | 全局 Semaphore；逻辑 4 + 物理 1 降级串行 |
| TUI 多路 stream 抖动 | MVP **禁止**子 token 流；仅 progress + 终块 |
| 子 agent 反向提问污染父 | 硬 prompt + `ERROR:` + envelope + incomplete_info |
| 工具减名单漏副作用 | explore **硬白名单** |
| one-tools 循环依赖 | TaskTool **仅** one-cli |
| 父不知子是否查完 | `status=max_turns_exceeded` 等机器字段 |
| 费用爆炸 | max_turns 16 + concurrency settings + 默认 explore |

---

## 9. Key Decisions（含工程批判，绑定）

| # | 决策 | 理由 |
|---|------|------|
| 1 | 同进程 nested Agent | 复用 / 可测 |
| 2 | `task` + 默认 explore | 上下文隔离 + 并行调研 |
| 3 | **Agent≡Subagent**（AgentSpec） | 程序化封装 |
| 4 | **ProviderPermit 全局共享** | 防连接池/本地并发死锁 |
| 5 | **MVP 无子 TUI stream** | 防多路渲染穿层 |
| 6 | explore **白名单** | 防减名单漏网 |
| 7 | TaskTool **只在 one-cli** | 元工具 vs 纯 IO |
| 8 | **TaskExitStatus 机器可读** | 父可区分成功/打满 turns |
| 9 | ask 禁工具 + 反提问 envelope | 防 HITL 重入与逻辑污染 |
| 10 | general / 子 stream UI / QuickJS 后置 | YAGNI |

---

## 10. Open Questions（已默认）

| 问题 | 默认 |
|------|------|
| 工具名 | **`task`** |
| 子 thinking | **off** |
| general 进 MVP？ | **否** |
| 逻辑并发默认 | **4**；`ONE_LLM_CONCURRENCY` 控物理槽（默认合理值，本地可 1） |

---

## 11. 修订记录

| 日期 | 说明 |
|------|------|
| 2026-07-19 | 工程批判并入：并发限流、TUI、exit status、白名单、TaskTool 位置、反提问 envelope |
| 2026-07-19 | 初稿：方案 A、分阶段；Agent≡Subagent 协议 |
