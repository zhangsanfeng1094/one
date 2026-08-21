# MCP 服务器 (MCP Servers)

MCP（Model Context Protocol）用于为 One 扩展外部工具与数据源。配置后，MCP 工具将作为 One 的扩展能力供模型在对话中调度。

---

## 核心机制

1. **Rust 原生支持**：基于 `rmcp` 官方协议库，支持 stdio 本地进程与 Streamable HTTP / SSE 远程传输协议。
2. **Deferred 工具按需暴露**：默认模式下，模型初始仅加载 `search_tool` 与 `use_tool` 元工具及轻量 server 列表公告，不浪费系统上下文；需要时动态检索 Schema 并调用。
3. **独立配置与跨客户端导入**：One 维护自身独立配置文件，同时提供对 Grok、Claude、Codex、Cursor 等主流 Agent 配置的一键扫描与导入功能。

---

## 配置文件路径

| 范围 | 配置文件路径 | 说明 |
| :--- | :--- | :--- |
| **用户全局** | `~/.one/agent/mcp.json` | 默认配置，全局生效（`one mcp add/remove` 默认操作此处） |
| **项目本地** | `.one/mcp.json` | 项目级配置（从当前目录向上查找至 git 根目录），优先级高于用户配置 |

### 配置示例 (`~/.one/agent/mcp.json`)

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
    "agy": {
      "url": "https://agy-web-search-mcp.ngominhbinh708.workers.dev/mcp",
      "headers": {
        "X-Agy-Refresh-Token": "YOUR_AGY_REFRESH_TOKEN"
      },
      "enabled": true
    },
    "linear": {
      "url": "https://mcp.linear.app/mcp",
      "headers": {
        "Authorization": "Bearer ${LINEAR_API_KEY}"
      },
      "enabled": true
    }
  },
  "maxOutputBytes": 51200,
  "disabledServers": [],
  "toolExposure": "deferred"
}
```

> **注意**：配置中的字符串字段支持 `${VAR}` 环境变量占位符自动展开。

---

## CLI 命令行管理

### 1. 查看已配置服务器 (`list`)

```bash
one mcp list
one mcp list --json          # JSON 格式输出
```

### 2. 添加本地 stdio 服务器 (`add`)

以 `--` 隔离启动命令及参数：

```bash
# 添加文件系统服务
one mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /path/to/dir

# 携带环境变量
one mcp add postgres -e DATABASE_URL=postgres://localhost/mydb -- npx -y @modelcontextprotocol/server-postgres
```

### 3. 添加远程 HTTP / Streamable HTTP 服务器 (`add`)

支持短参数 `-H`（Header 支持冒号 `:` 或等号 `=` 分隔并自动 trim），支持位置参数 URL（自动推断 `http` 传输）：

```bash
# 最简添加（附带鉴权 Header 与直接 URL）
one mcp add agy -H "X-Agy-Refresh-Token: YOUR_AGY_REFRESH_TOKEN" https://agy-web-search-mcp.ngominhbinh708.workers.dev/mcp

# 显式指定 -t http
one mcp add agy -t http -H "X-Agy-Refresh-Token: YOUR_TOKEN" https://agy-web-search-mcp.ngominhbinh708.workers.dev/mcp

# Bearer 鉴权服务
one mcp add remote-api -H "Authorization: Bearer YOUR_TOKEN" https://mcp.example.com/mcp
```

### 4. 诊断与健康检查 (`doctor`)

通过 `doctor` 子命令检查网络连通性、协议握手与已暴露工具列表：

```bash
# 检查全部服务并列出工具
one mcp doctor
```

### 5. 从其他客户端一键导入 (`import`)

支持自动扫描 **Grok** (`~/.grok/config.toml`)、**Claude** (`~/.claude.json`)、**Codex** (`~/.codex/config.toml`)、**Cursor** (`~/.cursor/mcp.json`) 及 `.mcp.json`：

```bash
# 列出所有可导入的候选服务
one mcp import --list

# 从 Grok 导入指定服务
one mcp import grok agy

# 仅从 Claude 导入所有服务
one mcp import --source claude

# 导入所有检测到的新服务
one mcp import

# 强制覆盖已有同名配置
one mcp import context-mode --force
```

### 6. 移除服务器 (`remove`)

```bash
one mcp remove agy
```

---

## TUI 交互式管理

在交互终端中随时执行以下操作：

- **`/mcp`**：打开 MCP 服务管理面板，查看状态与可用工具。
- **`/mcp import`**：交互式选择并导入外源 MCP 服务。
- **`/mcp enable <name>` / `/mcp disable <name>`**：启用或临时禁用指定服务。
- **`/reload`**：热重载配置并重新建立 MCP 连接池。

---

## 环境变量与高级调优

- **启动超时**：`ONE_MCP_STARTUP_TIMEOUT_SECS=60` 或 `MCP_TIMEOUT=60000`（毫秒）。
- **工具输出截断上限**：`ONE_MAX_MCP_OUTPUT_BYTES=102400`（字节），默认 50 KiB，超出部分自动落盘保存。
- **单会话禁用 MCP**：启动时添加 `--no-mcp` 参数。
