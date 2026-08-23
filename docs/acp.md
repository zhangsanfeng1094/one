# Agent Client Protocol (ACP)

> **状态**：已实现（`one acp` / `--mode acp` / `one web`）
> **协议**：[Agent Client Protocol v1](https://agentclientprotocol.com/protocol/v1/overview)
> **SDK**：[`agent-client-protocol` 0.9](https://crates.io/crates/agent-client-protocol)（官方 trait API）

ACP 让 IDE / 编辑器 / Web 客户端把 One 当作驱动的 coding agent。
- **stdio 传输**：`one acp`（JSON-RPC 2.0 over stdio，用于 IDE 嵌入）；
- **WebSocket 传输**：`one web`（内置现代 Web UI，浏览器通过 WebSocket 建立 ACP 连接交互）。

---

## 快速开始

### 1. Web 网页模式（推荐体验）

```bash
# 启动内置 Web UI（默认监听 127.0.0.1:3000）
one web

# 指定端口并自动打开默认浏览器
one web --port 3000 --open

# 启用自动审批
one web --yolo --provider xai
```

### 2. IDE 嵌入模式 (stdio)

```bash
# IDE / 客户端配置的命令
one acp --cwd /path/to/project --provider xai -y

# 等价
one --mode acp --cwd /path/to/project --provider mock --yes

# 自动批准工具权限（yolo）
one acp --yolo --provider mock
```

### Zed / 通用 editor 配置示例

```json
{
  "agent_servers": {
    "one": {
      "command": "one",
      "args": ["acp", "--cwd", "/path/to/project", "--provider", "xai"],
      "env": {}
    }
  }
}
```

全局 flags（`--provider`、`--model`、`--cwd`、`-y`、`--no-mcp`、`--no-skills`、`--no-subagent`、`--no-memory`、`--read-only`、`--plan`、`--no-session` 等）在 `acp` 子命令下可用。

---

## 支持的方法

### Client → Agent（baseline + optional）

| Method | 状态 | 说明 |
|--------|------|------|
| `initialize` | ✅ | 协商 `protocolVersion=1`；声明 loadSession / modes / list / resume / prompt caps |
| `authenticate` | ✅ | no-op（凭证走 `one login` / env / `auth.json`） |
| `session/new` | ✅ | 新建会话；返回 `sessionId` + Act/Plan modes |
| `session/load` | ✅ | 按 session id / 路径加载；**重放** history 为 `session/update` |
| `session/resume` | ✅ | 加载但不重放 history |
| `session/list` | ✅ | 列出 cwd 下近期 One sessions |
| `session/prompt` | ✅ | 用户消息；流式 `session/update`；返回 `stopReason` |
| `session/cancel` | ✅ | 中止当前 turn（abort + 取消 pending 审批） |
| `session/set_mode` | ✅ | `act` ↔ `plan` |
| `session/set_model` | ✅ | 切换 model id（unstable） |
| `session/set_config_option` | ✅ | `thinking` = off\|low\|medium\|high |

### Agent → Client

| Method / notification | 状态 | 说明 |
|----------------------|------|------|
| `session/update` | ✅ | `agent_message_chunk` / `agent_thought_chunk` / `tool_call` / `tool_call_update` / `available_commands_update` / `current_mode_update` |
| `session/request_permission` | ✅ | 工具审批与 `ask_user` 桥接到 client |

### 未做 / 有意简化

| 能力 | 说明 |
|------|------|
| Client `fs/*` / `terminal/*` 反向执行 | 工具仍在 **agent 本机** 执行（PathPolicy + 可选 bwrap）；不把读写路由回 IDE |
| Client 注入 MCP servers（`session/new.mcpServers`） | 忽略并打日志；用 One 自己的 `mcp.json` / `one mcp import` |
| ACP `logout` / elicitation 一等 | 未实现；`ask_user` 降级为 permission 选项 |

---

## Session modes

| Mode id | 行为 |
|---------|------|
| `act`（默认） | 完整 coding 工具面 |
| `plan` | 只读探索 + plan 文件；不可改代码直到切回 act |

也可在 prompt 里发 slash：`/plan`、`/act`、`/compact`、`/thinking medium`。

---

## 权限

- 默认：敏感工具走 `session/request_permission`（Allow once / session / always / Deny）
- `--yes` / `--yolo` / `ONE_AUTO_APPROVE=1`：自动批准
- PathPolicy 与 Interactive 模式一致

---

## 实现位置

| 路径 | 职责 |
|------|------|
| `crates/one-cli/src/modes/acp.rs` | Agent trait、session 表、事件/审批桥 |
| `crates/one-cli/src/cli.rs` | `Commands::Acp`、`RunMode::Acp` |
| `crates/one-cli/src/main.rs` | `one acp` / `--mode acp` 入口 |
| `crates/one-ai/src/auth/storage.rs` | `resolve_api_key_blocking` LocalSet 安全（ACP spawn_local） |

设计原则：

1. **不另起一套 loop** — 每个 ACP session 是一个完整 `AppRuntime`（与 TUI 同构）
2. **stdout 纯净** — 仅 JSON-RPC；Langfuse / tracing 不写 stdout
3. **Session 持久化** — 默认创建 `~/.one/agent/sessions/…`，支持 load/list

---

## 本地 smoke

```bash
# initialize + session/new + prompt（mock）
python3 - <<'PY'
import json, subprocess, threading, queue, os
cmd = ["./target/debug/one","acp","--provider","mock","--yes","--cwd",os.getcwd(),"--no-mcp"]
# … send initialize / session/new / session/prompt …
PY
```

协议细节与 schema 见 <https://agentclientprotocol.com/protocol/v1/schema>。
