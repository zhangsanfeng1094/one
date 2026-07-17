# MCP（基础能力）

> **状态**：已实现（`crates/one-mcp`，官方 `rmcp`）。  
> **原生配置：仅 JSON**（`mcp.json`）。One **只加载自己的配置**；其他 agent 的 MCP 通过显式 **导入** 进入 One。

---

## 配置加载（运行时）

| 范围 | 路径 | 优先级 |
|------|------|--------|
| 用户 | `~/.one/agent/mcp.json` | 低 |
| 项目 | `.one/mcp.json`（cwd → git root） | 高（近者覆盖） |

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
  "maxOutputBytes": 20000,
  "disabledServers": []
}
```

字符串字段加载时做 `${VAR}` 展开。

### 不再自动合并外源

默认 **不会** 读取 Claude / Codex / Cursor / 项目 `.mcp.json`。  
避免「别人配置悄悄进 One」、disable 列表和 provenance 纠缠。

逃生阀（不推荐）：`ONE_MCP_MERGE_FOREIGN=1` 恢复旧版多源自动 merge。

超时 / 输出：`ONE_MCP_STARTUP_TIMEOUT_SECS`、`MCP_TIMEOUT`（毫秒）、`ONE_MAX_MCP_OUTPUT_BYTES`。

---

## 从其他 Agent 导入

扫描（只读）→ 写入 `~/.one/agent/mcp.json` → 运行时连接。

| 来源 | 路径 |
|------|------|
| Codex | `~/.codex/config.toml` `[mcp_servers]` |
| Claude | `~/.claude.json`（含 project 级 mcpServers） |
| Cursor | `~/.cursor/mcp.json`、`.cursor/mcp.json` |
| 标准 | 项目 `.mcp.json` |

### CLI

```bash
one mcp import --list              # 只列候选
one mcp import                     # 导入所有尚未在 One 中的
one mcp import context-mode        # 导入指定名字
one mcp import --source codex      # 只从 Codex
one mcp import foo --force         # 覆盖 One 里已有同名
```

### TUI

| 入口 | 作用 |
|------|------|
| **`/mcp`** | 管理面板 |
| Actions → **Import from other agents** | 打开候选列表，Enter 导入一项（已有则 force 重同步） |
| Actions → **Import all available** | 导入全部未拥有项 |
| **`/mcp import`** | 同 Import 面板 |
| **`/mcp import all`** | 同 Import all |

---

## 异步连接与新对话

```text
AppRuntime::build
  → load_effective(cwd)     # 仅 One user + project
  → McpManager::spawn       # 后台连 server，不挡 TUI

每轮 prompt
  → 若 loading 且 0 tools：最多等 45s 等第一台
  → sync_mcp_tools()

/new
  → 只清 messages；MCP 连接池保留

Plan mode
  → 不注册 MCP tools；/act 后再挂

Import
  → 写 user mcp.json + 后台 connect 新 server + 抬 generation
```

工具命名：`{server}__{tool}`。

---

## TUI 管理

| 入口 | 作用 |
|------|------|
| **Settings**（Ctrl+G）→ **MCP servers** | 摘要状态 |
| **`/mcp`** | 面板：Import 动作 + 开关 |
| **`/mcp list`** | 只读列表 |
| **`/mcp enable\|disable <name>`** | 命令行开关 |

关闭会写入用户 `mcp.json` 的 `disabledServers`。

## CLI

```bash
one mcp list
one mcp add fs -- npx -y @modelcontextprotocol/server-filesystem /path
one mcp add r --transport http --url https://mcp.example.com/mcp
one mcp doctor
one mcp remove fs
one mcp import --list

one --no-mcp
```

---

## 尚未做

- OAuth 浏览器流  
- `/reload` 热重连全部 MCP  
- 导入时预览完整 argv / secrets 遮罩 UI  
