# One Harness JSON 协议（Agent ≡ Subagent）

> **版本**：`one.protocol.v1`  
> **核心命题**：**根 Agent 与 Subagent 同构**——都是一次「带规格的 harness run」。  
> JSON 不只描述「输出长什么样」，而是 **约束 harness 输入面**：system prompt、工具集、模型、权限、可再派谁、工作区……  
> 封装 SDK / workflow 脚本 / 主模型 `task` 工具，**共用同一套 schema**。

相关：[subagents.md](./subagents.md) · [claude-workflow-model.md](./claude-workflow-model.md) · [plans/2026-07-19-programmatic-subagents.md](./plans/2026-07-19-programmatic-subagents.md)

---

## 0. 心智模型

```text
                    ┌─────────────────────────────────────┐
                    │           AgentSpec                 │
                    │  name · system_prompt · tools ·     │
                    │  model · max_turns · permissions ·  │
                    │  skills · mcp · spawn_policy · …    │
                    └─────────────────┬───────────────────┘
                                      │ 同一形状
              ┌───────────────────────┼───────────────────────┐
              ▼                       ▼                       ▼
        根会话 Agent            Subagent (task)         `one agent run`
        (TUI / -p)              (父 run 内嵌套)          (宿主直接 spawn)
              │                       │                       │
              └───────────────────────┴───────────────────────┘
                                      │
                                      ▼
                              RunRequest  →  harness  →  RunResult
```

| 错误直觉 | 正确直觉 |
|----------|----------|
| Subagent 是另一种协议 | Subagent = **又一次** `RunRequest`，多了 `parent` 指针 |
| JSON 只是 stdout 结果 | JSON 先约束 **harness 配置 + 工具目录**，结果是附属 |
| 提示词/工具是实现细节 | **必须**出现在 spec 里，否则无法程序化复现与封装 |
| 主 agent 特殊 | 根也是 `AgentSpec`；默认 coding profile 只是内置模板 |

对齐 Claude：`AgentDefinition` / `.claude/agents/*.md` / SDK `agents={}` / `--agent` **同一角色描述**；  
spawn 路径（主模型调 Agent 工具 vs 宿主 `query`）不同，**规格同构**。

### 实现顺序（绑定）

```text
① CLI harness（preset + 完整 JSON）→ RunResult
② TaskTool 薄封装 → 同一 harness::run
③ 主会话默认 main 也可编译为 AgentSpec
```

**两种装载**：`AgentRef = "explore" | { …完整 AgentSpec }`（见 `protocol.rs`）。

---

## 1. 对象总表

| 对象 | 作用 | 谁产生 |
|------|------|--------|
| **`AgentSpec`** | 一次 run 的 harness 规格（prompt/tools/…） | 磁盘 agents.md、内置 profile、CLI、父 agent 的 `agents` 表、RPC |
| **`ToolSpec`** | 单个工具的 name/description/parameters | harness 根据 allow/deny 展开 |
| **`RunRequest`** | 启动一次 run 的完整输入 | 宿主 / 父 agent 的 `task` 工具 / CLI |
| **`RunResult`** | 一次 run 的输出 + 回显所用 spec 摘要 | harness |
| **`StreamEvent`** | 运行中事件（可选） | harness |
| **`RpcEnvelope`** | 长连接方法调用 | 宿主 |

---

## 2. `AgentSpec`（同构核心）

**根 agent、子 agent、命名角色文件，都是这个对象（或它的子集 + 引用）。**

```json
{
  "name": "explore",
  "description": "Read-only codebase research. Use for large exploration so the parent context stays small.",
  "system_prompt": "You are a read-only research sub-agent of One.\n…",
  "append_system_prompt": null,
  "tools": {
    "profile": "read_only",
    "allow": ["read", "grep", "find", "ls", "web_search", "web_fetch"],
    "deny": ["write", "edit", "bash", "bash_output", "bash_kill", "task"],
    "mcp": false
  },
  "model": {
    "provider": null,
    "id": null,
    "thinking": "off",
    "inherit": true
  },
  "max_turns": 16,
  "permission_mode": "dont_ask",
  "sandbox": "workspace-write",
  "cwd": null,
  "add_dirs": [],
  "skills": {
    "enabled": false,
    "catalog": false,
    "preload": []
  },
  "resources": {
    "agents_md": false,
    "claude_md": false
  },
  "spawn_policy": {
    "allow": [],
    "max_depth": 0,
    "max_concurrent": 0
  },
  "agents": {},
  "meta": {}
}
```

### 2.1 字段说明

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `name` | string | ✅* | 角色 id（`explore`、`general`、自定义）。根会话可用 `"main"` / `"default"` |
| `description` | string | | 给**父模型**看的委托说明（对齐 Claude frontmatter `description`） |
| `system_prompt` | string\|null | | 完整替换默认 system；`null` 表示用 profile 默认模板 |
| `append_system_prompt` | string\|null | | 追加在默认/主 prompt 后 |
| `tools` | object | ✅ | 见 [§2.2](#22-tools工具约束) |
| `model` | object | | 见 [§2.3](#23-model) |
| `max_turns` | number | | 本 run 的 tool-loop 上限；子 agent 通常小于根 |
| `permission_mode` | string | | 见 [§2.4](#24-permission_mode--sandbox) |
| `sandbox` | string | | 路径/OS 沙箱档位 |
| `cwd` | string\|null | | `null` = 继承父 / CLI |
| `add_dirs` | string[] | | 额外可读写根 |
| `skills` | object | | 是否注入 catalog / 预加载 skill 正文 |
| `resources` | object | | 是否加载 AGENTS.md / CLAUDE.md |
| `spawn_policy` | object | | **本 agent 能否再 spawn**；见 [§2.5](#25-spawn_policy--agents) |
| `agents` | object | | 子角色表：`{ "name": AgentSpec \| AgentRef }`，**值仍是同构 AgentSpec** |
| `meta` | object | | 扩展袋 |

\* 匿名内联 spec 可省略 `name`，harness 生成 `anonymous-<shortid>`。

### 2.2 `tools`（工具约束）

工具是 harness 的一等公民，**必须**可序列化描述。

```json
{
  "profile": "read_only",
  "allow": ["read", "grep", "find", "ls"],
  "deny": ["bash", "write"],
  "mcp": false,
  "mcp_allow": [],
  "extra": []
}
```

| 字段 | 说明 |
|------|------|
| `profile` | 内置起点：`coding` \| `read_only` \| `plan` \| `none`（仅 allow 列表） |
| `allow` | 在 profile 之上的 **白名单**；非空时最终集合 = `profile ∩ allow` 或 `allow` only（实现定一种，v1 建议：**若 allow 非空则仅 allow**，profile 只作文档默认） |
| `deny` | 从结果集中剔除 |
| `mcp` | 是否挂载 MCP 工具 |
| `mcp_allow` | MCP server 名或 `server__tool` 模式；空 = 全部已连接 |
| `extra` | 扩展/插件工具名 |

**解析算法（v1 规范，实现必须遵守）：**

```text
base = tools_for_profile(profile)   // coding / read_only / plan / []
if allow is non-empty:
  base = allow                      // 显式 allow 覆盖 profile 成员资格
base = base - deny
if not mcp: strip all mcp__* / server__*
else: filter by mcp_allow if non-empty
base += extra
→ ToolRegistry.materialize(base) → 最终 Tool[] 注入本 run 的 Agent
```

**实现（2026-07）：** 工具名 → 实现由 `one_tools::ToolRegistry` 统一注册；
harness / CLI 只做 `ToolsSpec` 解析 + `materialize`。新增 builtin：在 registry 注册工厂即可，无需改 harness match。
子 agent 默认 `read_only` 空 allow 走 **Explore** 硬白名单（无 ask_user）。
主会话 Act 工具同样经 registry 物化；MCP/extension 以 instance 挂入，并同步到 `TaskToolHost.dynamic_tools`，子 run 的 `tools.mcp: true` 可用。
含 write/edit/bash 的 harness run（**且** isolation=none）会占用进程级 **write lock**；`isolation=worktree` 时可并行写各自树。

**主会话 Act：** 工具列表由 **main AgentSpec.tools**（`.one/agents/main.json` 或 builtin）物化，不再写死 coding 全套。`--read-only` 仍强制 read_only 面。`task` 由 `spawn_policy` 注入，不依赖 tools.allow。

**isolation：**

| 值 | 含义 |
|----|------|
| `none` | 与父/CLI 共享 cwd |
| `worktree` | `git worktree add` 到 `.one/worktrees/<id>`；成功删除，失败保留；**不**自动 merge |

CLI：`one agent run … --isolation worktree`  
task：`isolation=worktree`；background + 可写工具默认升 worktree。

#### 磁盘 agent 形态

| 路径 | 说明 |
|------|------|
| `.one/agents/<name>.json` | 完整 AgentSpec JSON |
| `.one/agents/<name>.md` | YAML frontmatter + body→`system_prompt` |
| `~/.one/agent/agents/*` | 用户级；项目同名覆盖用户 |

`main` / `default` 为父 agent 自身；其余文件自动进入 `spawn_policy.allow` + `agents` 表。

**同构含义**：子 agent 的 `tools` 与根 **同一 schema**；不是另一套「subagent 工具枚举」。

#### 内置 profile（文档约定，与代码对齐时改一处）

| profile | 典型工具 |
|---------|----------|
| `coding` | read, write, edit, bash, bash_output, bash_kill, grep, find, ls, ask_user, web_*, task? |
| `read_only` | read, grep, find, ls, ask_user?, web_* |
| `plan` | read_only + plan 文件写 + exit_plan_mode |
| `none` | 空，仅 `allow` |

`task`（或 `agent`）是否出现在 `coding` 由 `spawn_policy` 决定：  
`spawn_policy.allow` 非空才注册 spawn 工具。

### 2.3 `model`

```json
{
  "provider": "anthropic",
  "id": "claude-sonnet-4-20250514",
  "thinking": "off",
  "inherit": false
}
```

| 字段 | 说明 |
|------|------|
| `inherit` | `true`：provider/id/thinking 从父 run 继承（子 agent 默认） |
| `provider` / `id` | `inherit=false` 时必填（或走 settings） |
| `thinking` | `off` \| `low` \| `medium` \| `high` |

### 2.4 `permission_mode` · `sandbox`

| permission_mode | 含义（对齐 Claude 语义） |
|-----------------|--------------------------|
| `default` | 规则 + 交互 ask（无 TTY 则 fail-closed） |
| `accept_edits` | 工作区内编辑类自动批 |
| `dont_ask` | 不弹窗；未 allow 的 deny |
| `plan` | 只读 + plan 工具 |
| `bypass` | 跳过审批（危险；仅显式） |

| sandbox | 含义 |
|---------|------|
| `workspace-write` | 默认 PathPolicy |
| `read_only` | 文件工具只读（可与 profile 叠加） |
| `full-access` | 关闭路径边界 |

### 2.5 `spawn_policy` · `agents`

**再派子 agent 不是魔法**，是 spec 的一部分：

```json
{
  "spawn_policy": {
    "allow": ["explore", "code-reviewer"],
    "max_depth": 1,
    "max_concurrent": 4
  },
  "agents": {
    "explore": {
      "name": "explore",
      "description": "…",
      "system_prompt": "…",
      "tools": { "profile": "read_only", "mcp": false },
      "max_turns": 16,
      "permission_mode": "dont_ask",
      "skills": { "enabled": false, "catalog": false },
      "spawn_policy": { "allow": [], "max_depth": 0, "max_concurrent": 0 }
    },
    "code-reviewer": { "$ref": "file:~/.one/agent/agents/code-reviewer.md" }
  }
}
```

| 规则 | |
|------|--|
| 子 run 的 `AgentSpec` | 从 `agents[name]` 取出（或内联），**再**走同一 harness |
| 深度 | 父 depth=0；子 depth=parent+1；`depth >= max_depth` 禁止再 spawn |
| 空 `allow` | 不注册 `task`/`agent` 工具（默认子 agent） |
| `$ref` | 仅引用解析为完整 AgentSpec；运行时内存里总是展开后的同构对象 |

### 2.6 Preset vs 完整 harness JSON

| 形态 | 说明 |
|------|------|
| **Preset id** | 短名 `explore`；解析为完整 `AgentSpec`（内置表或磁盘） |
| **完整 JSON** | 任意合法 `AgentSpec` 文件/内联；**不**依赖 preset 名 |
| **Markdown agents** | frontmatter + body → 同一 `AgentSpec`（preset 的磁盘形态之一） |

**解析优先级（命名加载时）**

```text
1. 内联 agent_spec / --spec 文件          → 完整 JSON，最高优先
2. 项目 .one/agents/<name>.json|.md
3. 用户 ~/.one/agent/agents/<name>.json|.md
4. 内置 presets（explore …）
```

**基础内置 preset（MVP）**：至少提供 **`explore`** 的完整 harness JSON（代码内嵌或 `presets/explore.json`），保证：

```bash
one agent run explore -p "…" --output-format json
one run --preset explore -p "…" --output-format json
one run --spec ./my-custom.json -p "…" --output-format json   # 完整 JSON，可与 preset 无关
```

`one agent dump explore > explore.json`（可选）便于用户改完整 JSON 再 `--spec`。

#### Markdown 磁盘形态示例

`~/.one/agent/agents/explore.md`：

```markdown
---
name: explore
description: Read-only research…
tools:
  profile: read_only
  mcp: false
max_turns: 16
permission_mode: dont_ask
model:
  inherit: true
  thinking: off
skills:
  enabled: false
---
You are a read-only research sub-agent…
```

frontmatter + body → **同一个 `AgentSpec`**。

---

## 3. `ToolSpec`（工具目录项）

harness 在 run 开始时根据 `AgentSpec.tools` **物化** 出列表，并应可导出：

```json
{
  "name": "read",
  "description": "Read a file…",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string" },
      "offset": { "type": "integer" },
      "limit": { "type": "integer" }
    },
    "required": ["path"]
  }
}
```

| 用途 |
|------|
| 注入 LLM tools 数组 |
| `system/init` 或 `RunResult.tools` 回显，便于封装层缓存 |
| 权限规则 `Tool(name)` 匹配 |

**spawn 工具本身**也是 ToolSpec，例如：

```json
{
  "name": "task",
  "description": "Run a sub-agent (same harness). args.agent must be in spawn_policy.allow.",
  "parameters": {
    "type": "object",
    "required": ["prompt"],
    "properties": {
      "prompt": { "type": "string" },
      "description": { "type": "string" },
      "agent": {
        "type": "string",
        "description": "Name under parent AgentSpec.agents, or inline mode alias like explore"
      },
      "agent_spec": {
        "type": "object",
        "description": "Optional inline AgentSpec override (must still pass spawn_policy)"
      }
    }
  }
}
```

`task` 的 execute = 构造子 **`RunRequest`**（parent 指回当前 run）→ 递归 harness → 把 `RunResult.result` 写成 tool_result。

---

## 4. `RunRequest`（harness 输入）

**任何**启动方式最终都归一到这个对象。

```json
{
  "protocol": "one.protocol.v1",
  "protocol_version": 1,
  "type": "run_request",
  "agent": {
    "name": "explore",
    "system_prompt": "…",
    "tools": { "profile": "read_only", "mcp": false },
    "max_turns": 16,
    "permission_mode": "dont_ask",
    "model": { "inherit": true },
    "spawn_policy": { "allow": [], "max_depth": 0, "max_concurrent": 0 }
  },
  "prompt": {
    "role": "user",
    "text": "Locate how auth sessions are stored",
    "images": []
  },
  "session": {
    "mode": "ephemeral",
    "id": null,
    "path": null
  },
  "parent": null,
  "run_id": null,
  "meta": {}
}
```

### 4.1 子 agent 时的 `parent`（同构差异仅此）

```json
{
  "parent": {
    "run_id": "run_parent_uuid",
    "session_id": "…",
    "tool_use_id": "call_task_01",
    "agent_name": "main",
    "depth": 1
  }
}
```

| 字段 | 说明 |
|------|------|
| `tool_use_id` | 父 turn 里 `task` 的 call id；流事件 `parent_tool_use_id` |
| `depth` | 嵌套深度 |
| 其余 | 与根 run **相同**：仍有完整 `agent` + `prompt` |

**没有**单独的 `SubAgentRequest` 类型——只有 `RunRequest` + optional `parent`。

### 4.2 `session.mode`

| mode | 行为 |
|------|------|
| `ephemeral` | 不落盘（子 agent 默认） |
| `persist` | 写入 `~/.one/agent/sessions/…` |
| `resume` | 使用 `id` / `path` 继续 |

### 4.3 从各入口映射到 `RunRequest`

| 入口 | 映射 |
|------|------|
| `one` 交互 / `-p` | `agent` = default coding AgentSpec（settings + flags 合并）；`prompt` = 用户输入 |
| `one agent run explore -p "…"` | `agent` = 加载 named AgentSpec；`parent` = null |
| 主模型 `task` | `agent` = `agents[name]`；`prompt` = args.prompt；`parent` = 当前 run |
| RPC `prompt` | 当前会话的 AgentSpec + params.text |
| RPC `spawn` / SDK `run(spec, prompt)` | 显式 AgentSpec + prompt |
| Workflow 脚本 | 多次 `RunRequest`，代码控流 |

---

## 5. `RunResult`（harness 输出）

```json
{
  "protocol": "one.protocol.v1",
  "protocol_version": 1,
  "type": "result",
  "ok": true,
  "result": "Findings: auth tokens live in …",
  "error": null,
  "run_id": "run_child_uuid",
  "session_id": null,
  "session_path": null,
  "duration_ms": 2345,
  "turns": 4,
  "stop_reason": "end_turn",
  "status": "success",
  "usage": {
    "input_tokens": 8000,
    "output_tokens": 600,
    "cache_read_tokens": 0,
    "cache_write_tokens": 0
  },
  "agent": {
    "name": "explore",
    "tools": ["read", "grep", "find", "ls", "web_search", "web_fetch"],
    "model": { "provider": "anthropic", "id": "…", "thinking": "off" },
    "max_turns": 16,
    "permission_mode": "dont_ask",
    "depth": 1
  },
  "parent": {
    "run_id": "run_parent_uuid",
    "tool_use_id": "call_task_01"
  },
  "children": [],
  "meta": {}
}
```

### 5.1 为何 result 里回显 `agent`

封装层要能回答：

- 这次 run **实际**开了哪些工具？  
- 用的什么 system 角色？  
- 是不是子 agent？  

不回显就无法做录制/回放/评测对齐。  
`agent.tools` 这里是 **物化后的名字列表**（不是完整 parameters，省体积）；完整 ToolSpec[] 可放 `meta.tool_defs` 或单独 API。

### 5.2 `children`

可选：父 run 结束时汇总子 run 的短摘要（id、name、ok、duration_ms）。  
全文仍在各自 ephemeral 上下文；默认不嵌套完整 messages。

### 5.3 Exit code（CLI）

| ok | exit |
|----|------|
| true | 0 |
| false | 1 |

---

## 6. Stream 事件（可选，仍挂在同构 run 上）

每条带：

```json
{
  "protocol": "one.protocol.v1",
  "type": "tool_call",
  "run_id": "…",
  "session_id": null,
  "parent_tool_use_id": "call_task_01",
  "agent_name": "explore",
  "depth": 1
}
```

| type | payload 要点 |
|------|----------------|
| `system` / `init` | 回显 AgentSpec 摘要 + `tools: ToolSpec[]` |
| `content_delta` | text/thinking |
| `tool_call` | id, name, **arguments object** |
| `tool_result` | tool_use_id, content[], details, ok |
| `turn_start` / `turn_end` | turn index |
| `result` | 完整 RunResult |

**工具 I/O**（与 LLM 一致）：

```json
{
  "type": "tool_call",
  "id": "call_1",
  "name": "read",
  "arguments": { "path": "src/auth.rs" }
}
```

```json
{
  "type": "tool_result",
  "tool_use_id": "call_1",
  "name": "read",
  "ok": true,
  "content": [{ "type": "text", "text": "…" }],
  "details": {}
}
```

---

## 7. 默认内置 AgentSpec（文档模板）

### 7.1 `main` / `default`（根 coding）

```json
{
  "name": "main",
  "description": "Default interactive coding agent",
  "system_prompt": null,
  "tools": {
    "profile": "coding",
    "allow": [],
    "deny": [],
    "mcp": true
  },
  "model": { "inherit": false, "thinking": null },
  "max_turns": 32,
  "permission_mode": "default",
  "sandbox": "workspace-write",
  "skills": { "enabled": true, "catalog": true, "preload": [] },
  "resources": { "agents_md": true, "claude_md": true },
  "spawn_policy": {
    "allow": ["explore"],
    "max_depth": 1,
    "max_concurrent": 4
  },
  "agents": {
    "explore": { "$ref": "builtin:explore" }
  }
}
```

`system_prompt: null` = 使用 `DEFAULT_SYSTEM_PROMPT` + AGENTS.md + skills catalog（resources/skills 打开时）。

### 7.2 `explore`（只读子 agent）

```json
{
  "name": "explore",
  "description": "Fast read-only exploration. Prefer for multi-file research.",
  "system_prompt": "You are a read-only sub-agent of One.\nComplete the research task, then stop.\n- Only use provided tools.\n- Do not ask the user questions.\n- Final answer: findings, paths/symbols, residual risks. Be concise.",
  "tools": {
    "profile": "read_only",
    "mcp": false,
    "deny": ["ask_user"]
  },
  "model": { "inherit": true, "thinking": "off" },
  "max_turns": 16,
  "permission_mode": "dont_ask",
  "sandbox": "workspace-write",
  "skills": { "enabled": false, "catalog": false },
  "resources": { "agents_md": false, "claude_md": false },
  "spawn_policy": { "allow": [], "max_depth": 0, "max_concurrent": 0 }
}
```

### 7.3 `general`（可写子 agent，后置）

与 `main` 类似但 `skills.catalog=false`、`spawn_policy.allow=[]`、`max_turns` 更小、`permission_mode` 继承或 `dont_ask`。

---

## 8. RPC / CLI 如何吃这套 JSON

### 8.1 RPC（目标）

```json
{
  "id": "1",
  "method": "run",
  "params": {
    "agent": { "...": "AgentSpec" },
    "prompt": { "text": "…" },
    "session": { "mode": "ephemeral" }
  }
}
```

兼容旧方法：

| method | 等价 |
|--------|------|
| `prompt` | `run` + 当前会话已绑定的 AgentSpec |
| `spawn` | `run` + named/builtin AgentSpec + ephemeral |
| `status` | 返回当前 AgentSpec 摘要 + tools 列表 |

### 8.2 CLI（目标；**先于 task 工具实现**）

```bash
# 根：隐式 main AgentSpec（现有交互 / -p）
one -p "…" --output-format json

# Preset harness（基础 subagent）
one agent run explore -p "map auth" --output-format json -y --no-session
one run --preset explore -p "map auth" --output-format json

# 完整 harness JSON（任意 AgentSpec，不限于 preset）
one run --spec ./my-agent.json -p "map auth" --output-format json
one run --spec - -p "…" < agent.json

# 调试：展开 preset → 完整 JSON / 物化 tools
one agent dump explore          # 打印内置 harness JSON
one agent inspect explore       # tools + prompt 摘要
```

**顺序**：先保证上表 CLI 与 `harness::run` 正确，再让 `task` 工具调用同一路径。

### 8.3 宿主封装伪代码

```python
# 与是否 subagent 无关
def run(agent_spec: dict, prompt: str, parent=None) -> dict:
    req = {
        "protocol": "one.protocol.v1",
        "type": "run_request",
        "agent": agent_spec,
        "prompt": {"text": prompt},
        "parent": parent,
        "session": {"mode": "ephemeral" if parent else "persist"},
    }
    return one_rpc("run", req)

# 根
run(load_spec("main"), "fix the bug")

# 子（workflow 层，不经主模型）
run(load_spec("explore"), "map auth", parent={"depth": 1, ...})
```

主模型路径：harness 把 `task` 的 arguments 编成同样的 `run(...)`。

---

## 9. Error

```json
{
  "code": "permission_denied",
  "message": "…",
  "retryable": false,
  "details": {}
}
```

与 harness 相关的 code：

| code | 场景 |
|------|------|
| `invalid_agent_spec` | tools/profile 无法解析 |
| `unknown_agent` | spawn 名不在 `agents` / allow |
| `spawn_depth_exceeded` | 超过 max_depth |
| `spawn_not_allowed` | policy 禁止 |
| `tool_not_in_spec` | 模型调用未授权工具（不应发生；门控） |
| `auth_required` / `provider_error` / `max_turns` / `aborted` | 同前 |

---

## 10. 版本与兼容

- 新增 AgentSpec 可选字段：版本不变。  
- 改 tools 解析算法：升 `protocol_version`。  
- **禁止**再发明 `SubAgentConfig` 平行类型；只加 `RunRequest.parent`。

---

## 11. 工程约束（与 subagents 批判对齐，绑定）

| # | 约束 |
|---|------|
| **LLM 并发** | 全进程 `ProviderPermit`（`ONE_LLM_CONCURRENCY`）；逻辑 task 并发与物理 LLM 槽分离；槽满则排队，不死锁 |
| **TUI** | 子 run **禁止** content_delta 进主 viewport；仅 progress + 终块 summary |
| **explore 工具** | **白名单** `read/grep/find/ls` + 可选 `web_search/web_fetch`；禁止 coding 减名单 |
| **TaskTool 位置** | 仅 `one-cli`（元工具）；`one-tools` 保持纯 IO |
| **exit status** | `RunResult.status`：`success` \| `max_turns_exceeded` \| `aborted` \| `runtime_error` \| `incomplete_info` |
| **反提问** | 子 prompt 禁止提问；`ERROR:` → incomplete_info；tool 层 envelope 再交给父 |

详见 [subagents.md](./subagents.md) §3.2–3.6、§9。

---

## 12. 实现落地顺序（按此契约）

| 步 | 内容 |
|----|------|
| 1 | 类型：`AgentSpec` / `RunRequest` / `RunResult` / `TaskExitStatus` ✅ 初版 |
| 2 | **内置 explore harness JSON（preset）** + 加载器（preset 名 / 文件 / 内联） |
| 3 | `ProviderPermit` + `harness::run` + explore 白名单 |
| 4 | **CLI**：`one agent run` / `one run --preset|--spec` + `--output-format json` |
| 5 | 测试：CLI preset + custom spec |
| 6 | **再** `TaskTool` 薄封装同一 `harness::run`（preset + agent_spec） |
| 7 | 测试：主→task；permit=1 双 task |

---

## 13. 修订

| 日期 | 说明 |
|------|------|
| 2026-07-19 | **CLI 先于 task 工具**；preset + 完整 harness JSON 双路径；`AgentRef` |
| 2026-07-19 | 并入工程批判：permit、TUI、白名单、status、TaskTool 位置 |
| 2026-07-19 | **重写**：Agent≡Subagent、AgentSpec 约束 harness |
