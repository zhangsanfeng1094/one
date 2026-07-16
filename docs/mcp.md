# MCP（基础能力）

> **状态**：已实现（`crates/one-mcp`，官方 `rmcp`）。  
> **原生配置：仅 JSON**（`mcp.json`）。不维护 TOML 双格式。

---

## 配置加载

### One 原生（唯一格式：JSON）

| 范围 | 路径 |
|------|------|
| 用户 | `~/.one/agent/mcp.json` |
| 项目 | `.one/mcp.json`（cwd → git root，cwd 覆盖祖先） |

`one mcp add/remove` 只读写用户级 `mcp.json`。

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "enabled": true,
      "startup_timeout_sec": 30,
      "tool_timeout_sec": 120
    },
    "linear": {
      "url": "https://mcp.linear.app/mcp",
      "headers": { "Authorization": "Bearer ${LINEAR_TOKEN}" }
    }
  },
  "maxOutputBytes": 20000
}
```

字符串字段加载时做 `${VAR}` 展开。

### 兼容扫描（只读别人配置，不是第二套 One 格式）

优先级 **高 → 低**（同名整份替换）：

```text
One 项目 .one/mcp.json
One 用户 ~/.one/agent/mcp.json
Codex    ~/.codex/config.toml  [mcp_servers]   ← 只读 TOML
Claude   ~/.claude.json
Cursor   .cursor/mcp.json · ~/.cursor/mcp.json
标准     项目 .mcp.json
```

Codex 的 `tools.*.approval_mode` 等专有字段会忽略，只映射 `command` / `args` / `env` / `url` / `headers` 等连接字段。

关闭兼容：

```bash
ONE_CODEX_MCPS_ENABLED=0
ONE_CLAUDE_MCPS_ENABLED=0
ONE_CURSOR_MCPS_ENABLED=0
```

超时 / 输出：`ONE_MCP_STARTUP_TIMEOUT_SECS`、`MCP_TIMEOUT`（毫秒）、`ONE_MAX_MCP_OUTPUT_BYTES`。

---

## 异步连接与新对话

```text
AppRuntime::build
  → load_effective(cwd)     # 同步读盘
  → McpManager::spawn       # 后台连 server，不挡 TUI

每轮 prompt
  → 若 loading 且 0 tools：最多等 45s 等第一台
  → sync_mcp_tools()        # generation 变了就挂上新 tools

/new
  → 只清 messages；MCP 连接池保留

Plan mode
  → 不注册 MCP tools；/act 后再挂
```

工具命名：`{server}__{tool}`。

---

## TUI 管理

| 入口 | 作用 |
|------|------|
| **Settings**（Ctrl+G）→ **MCP servers** | 摘要状态（`3/5 ready · 1 loading · 1 off`） |
| **`/mcp`** | 管理面板：状态 + Enter 开关 |
| **`/mcp list`** | 只读列表 |
| **`/mcp enable\|disable <name>`** | 命令行开关 |

面板分 **Enabled / Disabled** 两组，每行显示 `name · source · status` 与 `transport · detail`。

关闭会写入用户 `mcp.json` 的 `disabledServers`（兼容来源如 Codex/Claude 也能关）。开启后若未连接会后台重连。

## CLI

```bash
one mcp list
one mcp add fs -- npx -y @modelcontextprotocol/server-filesystem /path
one mcp add r --transport http --url https://mcp.example.com/mcp
one mcp doctor
one mcp remove fs

one --no-mcp
```

---

## 尚未做

- OAuth 浏览器流  
- `/reload` 热重连  
- TUI `/mcps` 面板  
