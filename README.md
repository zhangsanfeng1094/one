# One

Rust 实现的编程 Agent，灵感来自 [Pi Agent](https://pi.dev)：极简核心、内置工具、可扩展、JSONL 树形会话格式。

## 特性

- **极简 Agent 核心**：短 system prompt + tool-calling 循环
- **内置工具**：`read` / `write` / `edit` / `bash` / `bash_output` / `bash_kill` / `grep` / `find` / `ls` / `ask_user` / `web_search` / `web_fetch`
- **工作区路径沙箱**：默认只能读写 `--cwd`（+ `--add-dir`）；`--full-access` 关闭边界；可选 bash bubblewrap
- **权限门控**：allow / deny / ask 规则 + 交互审批列表
- **四种运行模式**：Interactive TUI / Print / JSON / RPC
- **会话持久化**：JSONL 树形 session，`--continue` / `--resume` 恢复
- **资源加载**：`AGENTS.md` / `CLAUDE.md`、skills（progressive disclosure + 内置 `create-skill`）、`/prompt` 模板
- **MCP 平台客户端**：stdio + streamable HTTP；`one mcp` / `/mcp`；可从 Claude/Codex/Cursor 导入
- **扩展运行时**：Rust-native `Extension` + hooks.json + 本地 plugins
- **Plan / Act 模式**：`--plan` / Shift+Tab / `/plan` → `/act`
- **OAuth / 订阅登录**：Codex、xAI Grok、OpenCode Zen/Go（`one login` / `/login`）
- **上下文压缩**：LLM 摘要 + overflow 重试 + `/compact`
- **执行轨迹**：`--trace` → Langfuse（OTLP）+ `one bench` harness

## 快速开始

```bash
# 编译（需要 Rust 工具链；推荐安装 build-essential）
cargo build -p one-cli

# Print 模式（Mock provider，无需 API key）
cargo run -p one-cli -- -p "list files in current directory"

# 交互模式
cargo run -p one-cli

# 继续上次 session / 交互选择历史
cargo run -p one-cli -- --continue
cargo run -p one-cli -- --resume

# JSON 事件流
cargo run -p one-cli -- --mode json -p "hello"

# RPC 模式（stdin JSONL）
cargo run -p one-cli -- --mode rpc --no-session

# 真实 LLM（http-providers 已是 one-cli 默认 feature）
export ANTHROPIC_API_KEY=...
cargo run -p one-cli -- --provider anthropic -p "hello"

# 订阅登录后使用
cargo run -p one-cli -- login          # 交互选 Codex / xAI / OpenCode …
cargo run -p one-cli -- --provider openai-codex -p "hello"
cargo run -p one-cli -- --provider xai -p "hello"
```

编译后的二进制名为 **`one`**（`target/debug/one` 或 `target/release/one`）。

## 项目结构

```
crates/
  one-core/       Agent loop、Tool/ToolGate、compaction、trace
  one-ai/         LLM Provider + OAuth/auth + models.json compat
  one-tools/      内置 coding tools + PathPolicy / sandbox
  one-session/    JSONL 树形 session
  one-resources/  AGENTS.md / skills / prompts
  one-mcp/        MCP 平台客户端 → Tool
  one-ext/        扩展 runtime + plugins + hooks
  one-tui/        交互式终端 UI
  one-cli/        CLI 入口与 AppRuntime 装配（one 二进制）
benches/         harness 评测任务与 broken-kit
docs/            架构活文档与专题设计
```

## 文档

- **[架构图（活文档）](docs/architecture.md)** — 总览、能力状态矩阵、数据流、干净度评估  
- [CLI 参考](docs/cli.md)
- [Session 格式](docs/session-format.md)
- [扩展系统](docs/extensions.md)
- [MCP](docs/mcp.md)
- [Provider Compat](docs/compat.md)
- [Harness 埋点与能力对比](docs/harness-eval.md) — Langfuse `--trace` / `one bench` / 跨 agent 评测
- [Package / Suite 设计（草案，未实现）](docs/package-suites.md)
- [程序化 / Subagent / Workflow（对照 Claude）](docs/claude-workflow-model.md)
- [Harness JSON 协议（Agent≡Subagent）](docs/protocol.md)
- [子 Agent 设计](docs/subagents.md)
- [实现计划 W0–W2](docs/plans/2026-07-19-programmatic-subagents.md)
- [Worktree + Background 计划](docs/plans/2026-07-20-worktree-background.md)
- [开发指南](docs/development.md)
- [路线图](docs/roadmap.md)
- [与 Pi 差距](docs/gap-vs-pi.md)

## Harness 轨迹（评测）

观测后端为 **Langfuse（OpenTelemetry OTLP）**（需项目 API keys）：

```bash
export LANGFUSE_PUBLIC_KEY=pk-lf-...
export LANGFUSE_SECRET_KEY=sk-lf-...
export LANGFUSE_BASE_URL=https://us.cloud.langfuse.com   # 按区域

# 记录 agent 链路 → OTLP → Langfuse UI
one --trace -p "list files" --provider mock -y
# 含更大 LLM/tool I/O 预览
one --trace --trace-full -p "list files" --provider mock -y

# mock 可重复的 smoke 任务包
one bench --suite smoke

# 评测（broken-kit v0.2 更难；输出在 benches/out，已 gitignore）
./benches/run.sh smoke
./benches/run.sh verify
./benches/run.sh full          # 真模型，读 TUI 配置
```

详见 [docs/harness-eval.md](docs/harness-eval.md) · [benches/README.md](benches/README.md)。

## 与官方 Pi 的关系

| 能力 | 官方 Pi (TS) | One |
|------|-------------|-----|
| 核心 coding tools | ✅ | ✅（+ bash_output/kill、ask_user、web_*） |
| JSONL session 树 | ✅ | ✅（v3 子集） |
| Skills / prompts | ✅ progressive disclosure | ✅ catalog + `read` + `/skills` |
| Compaction | ✅ LLM | ✅ LLM + overflow 重试 |
| Thinking level | ✅ | ✅ `/settings` · `/thinking` · stream · Ctrl+T |
| TypeScript 扩展 | ✅ | ❌（Rust 扩展 + hooks/plugins） |
| MCP 内置 | ❌ | ✅ 平台基础能力（`one-mcp` / `rmcp`） |
| Plan Mode | ❌ | ✅ `--plan` / Shift+Tab / `/plan`→`/act` |
| Interactive TUI | ✅ 完整 | ✅ 可用（多行 / Settings / HITL） |
| OAuth 登录 | ✅ 多厂商 | ✅ Codex + xAI + OpenCode Zen/Go（Claude/Copilot 待） |
| Harness / 轨迹 | 不同路径 | ✅ Langfuse `--trace` + `one bench` |

更细的差距分析见 [docs/gap-vs-pi.md](docs/gap-vs-pi.md)。

## License

MIT
