# 斜杠命令与快捷键 (Slash Commands & Shortcuts)

在 One 的交互式 TUI 中，你可以通过 `/` 命令和键盘快捷键高效管理会话状态、扩展与运行时。

---

## 常用斜杠命令

| 命令 | 说明 | 示例 / 参数 |
| :--- | :--- | :--- |
| **`/mcp`** | 打开 MCP 服务器管理面板（查看连接、开启/禁用、导入外源配置） | `/mcp` |
| **`/mcp import`** | 浏览并从 Claude / Codex / Cursor / `.mcp.json` 导入 MCP 服务 | `/mcp import` |
| **`/mcp enable <name>`** | 启用指定名称的 MCP 服务器 | `/mcp enable agy` |
| **`/mcp disable <name>`** | 禁用指定名称的 MCP 服务器 | `/mcp disable agy` |
| **`/plan`** | 切换至规划模式（只读探索，生成 PR 实施方案） | `/plan` |
| **`/act`** | 退出规划模式，进入代码执行模式 | `/act` |
| **`/login`** | 弹出账号登录与认证管理界面 | `/login` |
| **`/logout`** | 退出当前服务商登录态 | `/logout` |
| **`/model`** | 查看或切换当前使用的模型 | `/model anthropic:claude-3-7-sonnet-20250219` |
| **`/learn`** | 意图学习与规则管理（自动从会话学习或手动录入） | `/learn` / `/learn list` / `/learn status` |
| **`/compact`** | 手动触发上下文压缩与历史摘要 | `/compact` |
| **`/clear`** 或 **`/new`** | 清空当前对话并开始新会话（保持 MCP 连接池） | `/clear` |
| **`/reload`** | 热重载磁盘配置（MCP、Skills、Prompt 模板、Plugins） | `/reload` |
| **`/help`** | 显示帮助手册与命令列表 | `/help` |

---

## 键盘快捷键

| 快捷键 | 功能 |
| :--- | :--- |
| **`Shift + Tab`** | 快速在 Plan 模式与 Act 模式之间来回切换 |
| **`Ctrl + C`** | 中断当前正在执行的工具调用或模型流式生成 |
| **`Ctrl + D`** / **`Esc`** | 退出当前会话 / 关闭弹出菜单 |
| **`Up / Down`** | 浏览历史输入命令 |
| **`PageUp / PageDown`** | 上下滚动对话内容 |
