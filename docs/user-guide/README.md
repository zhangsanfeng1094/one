# One 用户指南 (User Guide)

欢迎阅读 One 编程 Agent 的官方使用手册与实战指南。

---

## 目录导航

- [01. 快速入门与安装](./01-getting-started.md)
  - 源码编译与安装
  - 账号登录与 API Key 配置
  - 交互模式、单次执行与会话恢复
- [04. 斜杠命令与快捷键](./04-slash-commands.md)
  - TUI 常用斜杠命令（`/mcp`、`/plan`、`/login`、`/reload` 等）
  - 键盘操作与快捷键
- [07. MCP 服务器集成与管理](./07-mcp-servers.md)
  - MCP 核心机制与架构
  - JSON 配置文件格式（用户级与项目级）
  - 本地 stdio 与远程 Streamable HTTP 服务添加
  - CLI 命令全集（`list` / `add` / `remove` / `doctor` / `import`）
  - 从 Claude / Codex / Cursor 一键迁移配置
  - 性能调优、超时控制与输出截断保护
- [09. 意图识别与规则自学习](./09-intent-learning.md)
  - LPG (Labeled Property Graph) 意图图谱架构与工作原理
  - CLI 管理全集（`one learn` 录入、`--list` 查询、`--test` 推理测试与 `--reset`）
  - 交互式 TUI 会话自学习与管理（`/learn` 系列命令）
  - 动态 `<system-reminder>` 上下文注入与最优工具推荐
  - 跨会话持久化存储与 Memory 知识库协作机制

---

## 更多架构与技术规范

深入了解底层实现与协议规范，请参考项目核心文档：
- [系统架构图与核心设计](../architecture.md)
- [CLI 完整参数参考](../cli.md)
- [MCP 平台能力技术专题](../mcp.md)
- [会话格式与分支机制](../session-format.md)
- [Subagent 与任务调度](../subagents.md)
