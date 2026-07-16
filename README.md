# One

Rust 实现的编程 Agent，灵感来自 [Pi Agent](https://pi.dev)：极简核心、内置工具、可扩展、JSONL 树形会话格式。

## 特性

- **极简 Agent 核心**：短 system prompt + tool-calling 循环
- **内置工具**：`read` / `write` / `edit` / `bash` / `grep` / `find` / `ls` / `web_search` / `web_fetch`
- **工作区路径沙箱**：默认只能读写 `--cwd`（+ `--add-dir`）；`--full-access` 关闭边界
- **四种运行模式**：Interactive TUI / Print / JSON / RPC
- **会话持久化**：JSONL 树形 session，`--continue` 恢复
- **资源加载**：`AGENTS.md`、skills（含内置 `create-skill`）、`/prompt` 模板
- **扩展运行时**：Rust-native `Extension` trait
- **上下文压缩**：超阈值自动 compaction（基础版）

## 快速开始

```bash
# 编译（需要 Rust 工具链；推荐安装 build-essential）
cargo build -p one-cli

# Print 模式（Mock provider，无需 API key）
cargo run -p one-cli -- -p "list files in current directory"

# 交互模式
cargo run -p one-cli

# 继续上次 session
cargo run -p one-cli -- --continue

# JSON 事件流
cargo run -p one-cli -- --mode json -p "hello"

# RPC 模式（stdin JSONL）
cargo run -p one-cli -- --mode rpc --no-session

# 启用真实 LLM（需 API key + feature）
export ANTHROPIC_API_KEY=...
cargo run -p one-cli --features http-providers -- --provider anthropic -p "hello"
```

编译后的二进制名为 **`one`**（`target/debug/one` 或 `target/release/one`）。

## 项目结构

```
crates/
  one-core/       Agent loop、消息类型、compaction
  one-ai/         LLM Provider 抽象 + Mock/Anthropic/OpenAI
  one-tools/      内置 coding tools
  one-session/    JSONL 树形 session
  one-resources/  AGENTS.md / skills / prompts 加载
  one-ext/        Rust 扩展 trait + runtime
  one-tui/        交互式终端 UI
  one-cli/        CLI 入口（one 二进制）
docs/            设计与使用文档
```

## 文档

- [架构设计](docs/architecture.md)
- [CLI 参考](docs/cli.md)
- [Session 格式](docs/session-format.md)
- [扩展系统](docs/extensions.md)
- [Package / Suite 设计（草案，未实现）](docs/package-suites.md)
- [开发指南](docs/development.md)
- [路线图](docs/roadmap.md)

## 与官方 Pi 的关系

| 能力 | 官方 Pi (TS) | One |
|------|-------------|-----|
| 核心 4 tools | ✅ | ✅ |
| grep/find/ls | ✅ | ✅ |
| JSONL session 树 | ✅ | ✅（v3 子集） |
| Skills / prompts | ✅ progressive disclosure | ✅ catalog + `read` 按需加载 |
| Compaction | ✅ LLM | ✅ LLM + overflow 重试 |
| Thinking level | ✅ | ✅ `/settings thinking` / `/thinking` · stream · Ctrl+T · multi-provider |
| TypeScript 扩展 | ✅ | ❌（Rust 扩展） |
| MCP 内置 | ❌ | ✅ 平台基础能力（`one-mcp` / `rmcp`，见 docs/mcp.md） |
| Interactive TUI | ✅ 完整 | ✅ 可用（多行/用量 footer） |
| OAuth 登录 | ✅ | 🔜 计划中 |

更细的差距分析见 [docs/gap-vs-pi.md](docs/gap-vs-pi.md)。

## License

MIT