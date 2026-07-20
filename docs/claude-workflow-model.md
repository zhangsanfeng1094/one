# Claude 的 Agent / Workflow 模型调研（对照 One）

> **日期**：2026-07-19（§5.1 外置脚本 / 不内嵌 QuickJS 已拍板）  
> **目的**：说明 Claude Code 如何「程序化执行任务」，以及 One 应对齐哪一层、先做什么。  
> **相关**：[subagents.md](./subagents.md) · [architecture.md](./architecture.md)

官方入口：

- [Agent SDK](https://code.claude.com/docs/en/agent-sdk/overview)
- [Headless / `-p`](https://code.claude.com/docs/en/headless)
- [Subagents](https://code.claude.com/docs/en/sub-agents)
- [Agent Teams](https://code.claude.com/docs/en/agent-teams)（实验）
- [Dynamic Workflows](https://code.claude.com/docs/en/workflows)（research preview，社区对照文）

---

## 1. 一句话心智模型：谁拿着「下一步干什么」

| 原语 | 谁决定下一步 | 中间结果放哪 | 适合 |
|------|--------------|--------------|------|
| **单会话 Agent loop** | 主模型 turn-by-turn | 主 context | 日常 coding |
| **Subagents** | 主模型决定何时 spawn | 子 context；**摘要**回主 | 隔离探索 / 受限角色 |
| **Skills** | 主模型按说明走 | 主 context | 可复用指令包 |
| **Agent Teams** | **Lead** 模型 + 共享 task list | 各 teammate 独立 session + mailbox | 多视角协作、辩论式调查 |
| **Dynamic Workflows** | **脚本（代码）** | **脚本变量**（不进 LLM context） | 可重复、可 diff、可 scale 的编排 |
| **Agent SDK / `-p`** | 宿主程序 or 脚本 | 事件流 / JSON / session_id | CI、应用内嵌、管道 |

核心洞见（来自 Claude workflows 文档的对照逻辑）：

> 其它原语都让 **LLM 当控制流**；  
> **Workflow 把控制流交给代码**，Claude 只在节点里当 worker。

你要的「程序化执行 agent 实现任务」，在 Claude 世界对应的是：

```text
宿主程序 / CI / 脚本
    │
    ├─ 层 0：跑一个 agent 到结束（-p / Agent SDK query）
    ├─ 层 1：同一 agent 多轮 resume（session_id）
    ├─ 层 2：agent 内部再派 subagent（Agent 工具）
    ├─ 层 3：声明式角色库（.claude/agents/*.md）
    ├─ 层 4：脚本编排 N 个 agent（Workflow runtime）   ← 真正「workflow」
    └─ 层 5：多 session 协作（Agent Teams，实验）
```

**One 今天约在 层 0 的 MVP**（`-p` / RPC `prompt`），其余大多缺失。

---

## 2. Claude 各层怎么做的

### 2.1 程序化入口：CLI `-p` + Agent SDK

**CLI（headless）**

```bash
claude -p "Find and fix the bug" --allowedTools "Read,Edit,Bash"
claude --bare -p "Summarize" --allowedTools "Read"   # 不加载本地 hooks/skills/MCP 噪声
claude -p "…" --output-format json | jq -r '.result'
claude -p "…" --output-format stream-json --verbose
claude -p "continue…" --resume "$session_id"
```

要点：

| 能力 | 作用 |
|------|------|
| `-p` / print | 非交互跑完一轮 agent loop |
| `--bare` | 可复现：只吃显式 flags，不吃本机配置漂移 |
| `--allowedTools` / permission-mode | 无 TTY 时仍可控权限 |
| `--output-format text\|json\|stream-json` | 给脚本/CI 解析 |
| `--json-schema` | 结构化结果字段 |
| `--resume` / session_id | 多步程序化对话 |
| stdin pipe | 把 diff/log 喂给 agent |
| `--agents` JSON | **会话级**注入 subagent 定义（自动化友好） |

**Agent SDK（Python / TS）**

```python
async for message in query(
    prompt="Fix the bug in auth.py",
    options=ClaudeAgentOptions(
        allowed_tools=["Read", "Edit", "Bash"],
        agents={  # 程序化注册 subagent
            "code-reviewer": AgentDefinition(
                description="…",
                prompt="…",
                tools=["Read", "Glob", "Grep"],
            )
        },
    ),
):
    …
```

与「自己用 Messages API 写 tool loop」的区别：

- SDK = **Claude Code 的 harness 当库**（内置 tools、权限、session、hooks、subagents）
- Client SDK = 你自己实现 tool loop

其它语言：官方建议 **CLI `-p` + JSON** 当集成面。

### 2.2 Subagents（会话内 worker）

**定义**：Markdown + YAML frontmatter（项目 `.claude/agents/`、用户 `~/.claude/agents/`、CLI `--agents`、SDK `agents`）。

```markdown
---
name: code-reviewer
description: Use after code changes for quality review
tools: Read, Glob, Grep
model: sonnet
maxTurns: 20
permissionMode: default
background: true   # 可后台
isolation: worktree
---
You are a code reviewer. …
```

**运行时**：

- 工具名历史上叫 **Task**，现多为 **Agent**（`Task(...)` 仍作别名）
- 独立 context；**摘要**回主会话
- 内置：Explore（只读）、Plan、general-purpose
- 可限制 tools / model / hooks / mcpServers / skills preload
- 可 foreground 阻塞 或 background + 完成通知
- 主会话可用 `claude --agent <name>` 整会话扮演该角色
- 子 agent 默认 **不弹** AskUserQuestion 等主 UI 工具

**不是**：agent 之间互聊（那是 Teams）。

### 2.3 Agent Teams（实验）

```text
Lead session
  ├── Teammate A (独立 Claude 实例)
  ├── Teammate B
  └── Shared task list + mailbox (JSON 文件)
```

- 开启：`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`
- Teammate 可 **互相发消息**、claim task
- 成本高（每人独立 context）
- 适合：多假设调试、多视角 review
- **不适合**当通用 CI 编排默认路径

### 2.4 Dynamic Workflows（真正的「workflow」）

社区/文档对照摘要：

| 点 | 内容 |
|----|------|
| 控制流 | Claude **写 JS 编排脚本**，runtime 执行 |
| 节点 | 脚本调用多个 subagent；中间结果在 **脚本变量** |
| 规模 | 约 16 并发、每 run 上千 agent 上限（文档级） |
| 可重复 | 脚本可落盘、`~/.claude/workflows/`、可当 slash |
| 与 subagent 关系 | Workflow **建立在 subagent 之上**，不是替代 |

这就是「程序像 Claude workflow」的核心：**代码 orchestration + agent 当可调用单元**。

### 2.5 Hooks 作为 workflow 引擎胶水

社区实践：用 `SubagentStart` / `SubagentStop` / `PreToolUse` / `TaskCompleted` 等 hook **把状态机接到外部代码**，实现强制质量门、自定义编排。  
Claude 官方 hook 是「事件出口」；**workflow runtime 是「代码控制流」**；两者可叠。

---

## 3. One 现状对照

| Claude 能力 | One 现状 | 差距 |
|-------------|----------|------|
| `claude -p` 非交互 | `one -p` / print / json | 有基础 |
| stream-json / structured schema | `--mode json` 事件流 | 无统一 result envelope / json-schema 输出 |
| `--bare` 可复现 | 部分：`--no-mcp` `--no-skills` | 无完整 bare 语义文档化 |
| `--resume` session_id | `--continue` / `--session` / RPC | RPC 无结构化 session 协议文档 |
| Agent SDK 库 | crate 可嵌，**无 SDK 面** | 缺 `query()` 级公开 API |
| RPC 集成 | ping/prompt/abort/steer/… | 无 spawn_task / 事件订阅 / agent 定义 |
| Subagent 工具 | **无** | 需 `task`/`agent` 工具 |
| `.claude/agents` 声明 | **无**（仅 skills） | 需 `agents/*.md` 或等价 |
| Agent Teams | **无**（且曾非目标） | 后置 |
| Dynamic Workflows 脚本 runtime | **无** | 后置；依赖 subagent + 程序化 API |
| Langfuse / bench | ✅ One 更强一档 | 保留作观测底座 |

---

## 4. 对你目标的重新表述

**目标**：以后能 **用程序**（脚本 / CI / 自有服务）驱动 One 完成多步任务，体验接近 Claude 的：

1. **单 agent 任务** — `one -p` / SDK / RPC 稳定可解析  
2. **多 worker** — 主 agent 或宿主 spawn 多个 subagent  
3. **编排** — 最终用 **代码** 串步骤（workflow），而不是全靠主模型临场发挥  

因此实现顺序应是 **自下而上**（和 Claude 演进一致）：

```text
P0  程序化执行面（契约稳定）
P1  Subagent 运行时（task 工具 + 定义文件）
P2  宿主可直接 spawn（RPC/SDK，不经主模型）
P3  Workflow 脚本 runtime（代码控流）
P4  Teams（可选，协作型）
```

只做 P1 而不做 P0/P2，**达不到**「像 Claude workflow 一样程序化」；  
Workflow 只是 **P0+P1+P2 之上的编排层**。

---

## 5. 修订后的分阶段路线（对齐 Claude）

### Phase 0 — 程序化契约（Programmatic surface）

**目标**：任何宿主都能可靠跑完一个任务并拿到结果。

| 项 | 设计要点 |
|----|----------|
| **Result envelope** | print/json 统一：`session_id`, `result`, `usage`, `ok`, `error`, `duration_ms` |
| **RPC 增强** | `prompt` 已有；补 `subscribe`/`events`（或 stdout 事件+最终 result）、`set_tools`/`set_mode` 可选 |
| **CLI 脚本友好** | `--output-format text\|json\|stream-json`；`--max-turns`；`-y`；`--no-mcp`/`--no-skills` 合成 bare 文档 |
| **权限无 TTY** | 已 fail-closed + `-y`；文档化 CI 推荐 flags |
| **库 API（轻量）** | `one_cli` 或新 `one-sdk`：`async fn run_task(opts) -> TaskResult`（先 Rust，再考虑 FFI/JSON 子进程） |

验收：

```bash
one -p "list files" --provider mock -y --output-format json | jq -r '.result'
# CI: 非 0 exit on tool/provider hard fail
```

### Phase 1 — Subagent runtime（Claude Subagents 对齐）

见 [subagents.md](./subagents.md)，并升级为：

| 项 | Claude 对齐 |
|----|-------------|
| 工具名 | `task` 或 `agent`（Claude 现名 Agent，旧 Task） |
| 定义文件 | `~/.one/agent/agents/*.md` + `.one/agents/*.md`（frontmatter） |
| 内置类型 | `explore` / `general`（后 `plan`） |
| 字段 | `name`, `description`, `tools`/`disallowedTools`, `model`, `maxTurns`, `permissionMode` |
| 调用 | 主模型 tool_call **或** 宿主 `spawn` |
| 输出 | summary → tool_result；trace 带 `parent_run_id` |
| 后台 | v1.1：`background` + notification（对齐 Claude 完成通知） |

### Phase 2 — 宿主直接 spawn（不经主 LLM）

Claude 侧等价：`query(..., agents={...})` 或脚本调多个 `claude -p`。

One：

```text
RPC:  {"method":"spawn","params":{"agent":"explore","prompt":"…"}}
SDK:  run_subagent(AgentSpec { name, prompt, tools, … })
CLI:  one agent run explore -p "…" --json
```

这样 **workflow 脚本** 可以：

```text
r1 = spawn(explore, "auth 怎么工作")
r2 = spawn(explore, "billing 怎么工作")
r3 = spawn(general, f"根据 {r1}{r2} 改代码")
```

**不必**先有一个主 agent 来「决定」spawn。

### Phase 3 — Workflow runtime（代码控流）

**决策（2026-07-19，已拍板）**：P3 **默认外置脚本**；可选声明式 YAML；**不**把 QuickJS/V8 作为地基。详见 [§5.1](#51-p3-默认外置脚本--为何不内嵌-quickjs)。

| 选项 | 状态 | 说明 |
|------|------|------|
| **A. 外部脚本 + one CLI/RPC** | **P3 默认路径** | bash / Python / 任意语言编排；One 只提供原子 `run` / `spawn` |
| **B. 内置 workflow 文件** | **P3 增强（A 之后）** | `.one/workflows/*.yaml`：steps、parallel、depends_on |
| **C. 嵌入脚本（QuickJS 等）** | **非目标 / 远期可选** | 见 §5.1；仅当 A+B 不够且有明确用户时再评估 |

示例（A — 现在就能作为目标态文档）：

```bash
# 宿主语言控流；中间结果在 shell 变量，不进 LLM context
r1=$(one agent run explore -p "Locate auth bug" --output-format json -y)
r2=$(one agent run general -p "Fix using: $(echo "$r1" | jq -r .result)" --output-format json -y)
echo "$r2" | jq -r .result
```

示例（B — 目标态 YAML）：

```yaml
# .one/workflows/fix-and-review.yaml
name: fix-and-review
steps:
  - id: explore
    agent: explore
    prompt: "Locate the bug related to: ${input}"
  - id: fix
    agent: general
    needs: [explore]
    prompt: "Fix based on: ${explore.result}"
  - id: review
    agent: code-reviewer
    needs: [fix]
    prompt: "Review the diff for: ${fix.result}"
```

```bash
one workflow run fix-and-review --input "login 500" --json
```

### 5.1 P3 默认外置脚本 · 为何不内嵌 QuickJS

> **已决策**：One 的「workflow」优先 = **稳定 worker API + 宿主语言编排**；  
> **不**在 P0–P3 内嵌 QuickJS（或 V8/Deno）作为编排 runtime。

#### 心智对齐 Claude

| | Claude Dynamic Workflows | One（推荐） |
|--|--------------------------|-------------|
| 控制流 | 内嵌 JS runtime 执行脚本 | **外部** bash/Python/CI（或日后 YAML） |
| Worker | subagent | 同一套 `run` / `spawn` / `task` |
| 中间结果 | 脚本变量 | 宿主变量 / 文件 / jq |
| 可重复、可 diff | 落盘的 JS | 落盘的 sh/py/**yaml** |

Claude 内嵌 JS 是产品选择；**能力等价物**是「代码控流 + agent 当节点」。  
用外部脚本调 `one`，在工程上已是 **同构的轻量 workflow**，且：

- 不引入第二语言运行时  
- CI/IDE/现有工具链零学习成本  
- 与 `claude -p` 管道用法一致  

#### 「重」的分解（QuickJS）

| 维度 | 粗估 | 结论 |
|------|------|------|
| 二进制体积 | QuickJS 约几百 KB～1MB 级 | **不重** |
| 启动 / 内存 | 远轻于 V8/Deno | **轻** |
| 编译与绑定 | `rquickjs` 等，偶发平台坑 | **中** |
| **安全沙箱** | 禁 os/net/FFI、限时、限内存，须自建 | **重** |
| **产品 API** | 脚本能调哪些 `one.*`、取消/并发/错误模型 | **最重** |
| 工期 | hello-world 快；生产级 orchestration 慢 | **重** |

→ 内嵌 QuickJS **不是体积问题，是过早背上「第二运行时 + 沙箱 + 编排 API」**。  
在 P0 契约与 P1/P2 worker 未定型时嵌引擎，容易把错误抽象写死。

#### 若将来要「内置脚本」，优先级

| 方案 | 何时考虑 |
|------|----------|
| **A 外置脚本** | **现在**（P3 默认） |
| **B YAML/TOML steps** | A 稳定后；覆盖串行/并行/depends 的 80% 场景 |
| **Rhai / Lua** | 需要配置级逻辑、仍想全 Rust 生态时 |
| **QuickJS（feature `workflow-js`）** | 明确需要「图灵完备 + 开发者只写 JS」、且 YAML 不够；**默认 off** |
| **V8 / Deno / 完整 Node** | **不做**（过重） |
| **Wasmtime guest** | 强隔离多语言时再评估（工程更大） |

若做 QuickJS，**窄 API 铁律**（示意）：

```js
// 只允许编排，不允许变成第二个不受控 agent
const a = await one.spawn("explore", { prompt: "…" });
const b = await one.spawn("general", { prompt: a.summary });
return { result: b.summary };
// 禁止：任意 fs / net / child_process / 读密钥路径
```

#### 何时允许重新评估内嵌 JS

同时满足再开 design review：

1. P0 result envelope + P2 `spawn` 已稳定 ≥ 一个版本周期  
2. 外部脚本 / YAML 有真实用户反馈「编排能力不够」  
3. 有人愿意写沙箱与 API 兼容测试（非 demo）  
4. 以 **optional Cargo feature** 合并，默认构建不含引擎  

在此之前：文档与 roadmap 将 C 标为 **非目标（远期可选）**，避免范围蔓延。

### Phase 4 — Teams（可选）

仅在需要 **peer 互聊 / 共享任务板** 时做；多数「程序化任务」用 P0–P3 足够，且更便宜。

---

## 6. 与旧 subagent 草案的关系

| 旧草案（subagents.md） | 修订 |
|------------------------|------|
| 主目标：主模型委托 explore | **升为**：程序化流水线的 **worker 原语** |
| MVP 只有 tool `task` | 仍要，但是 **P1**；**P0 程序化契约并行或略前** |
| 不做宿主直接 spawn | **改为 P2 必做**（workflow 刚需） |
| 不提 workflow | **P3 为显式目标** |

---

## 7. 建议的实现切片（可开工顺序）

| PR 串 | 内容 | 解锁 |
|-------|------|------|
| **W0a** | `--output-format json` result envelope + exit code | CI 可接 |
| **W0b** | RPC/文档：session_id、status、稳定错误码 | 宿主集成 |
| **W1a** | nested Agent + explore `task` 工具 | 会话内委托 |
| **W1b** | `agents/*.md` 加载 + 按 name 调用 | 角色库 |
| **W2** | `one agent run` + RPC `spawn` | **无主模型的编排** |
| **W3a** | 示例：Python/bash 多步 workflow（**默认 P3**） | 外置编排，同构 Claude「代码控流」 |
| **W3b** | 可选：`one workflow run` YAML | 声明式 workflow |
| **W3c** | ~~内嵌 QuickJS~~ | **非目标**；条件见 §5.1 |
| **W4** | background task + Langfuse parent/child 打全 | 观测与并行体验 |

---

## 8. Key Decisions（已拍板）

1. **程序化入口优先于炫酷多 agent UI** — 先 JSON/RPC/CLI 契约。  
2. **Subagent 是 worker，不是 workflow** — workflow = 宿主/脚本/YAML 控流。  
3. **宿主可直接 spawn** — 对齐 SDK `agents` + 多进程 `claude -p` 模式。  
4. **声明文件兼容 Claude 形态** — frontmatter agents，降低迁移成本。  
5. **P3 默认外置脚本；YAML 次之；QuickJS/V8 不进地基** — 体积不是问题，沙箱与 API 才是；见 §5.1。  
6. **Teams 后置** — 多数程序化任务不需要 peer 互聊。  
7. **保留 One 差异** — Langfuse harness、多厂商、workspace 沙箱。

---

## 9. 你下一步可以怎么选

| 选择 | 含义 |
|------|------|
| **路径 α（推荐）** | W0 程序化契约 → W1 subagent → W2 spawn → **外置脚本 workflow**（§5.1） |
| **路径 β** | 只做 W1（会话内 task），程序化靠现有 `-p` | 快，但 workflow 弱 |
| **路径 γ** | 直接上 YAML workflow 引擎 | 缺 worker 原语时会空心 |
| **路径 δ（不推荐现阶段）** | 内嵌 QuickJS workflow runtime | 见 §5.1 否决理由 |

确认路径后，可将 `docs/subagents.md` 与 `roadmap.md` 按 §7 改勾选，并写实现 plan。
