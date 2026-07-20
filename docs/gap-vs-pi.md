# One vs 官方 Pi 差异分析

> 对比基准  
> - **One**：本仓库 `one`（Rust，v0.1.0）  
> - **官方 Pi**：[@earendil-works/pi-coding-agent](https://github.com/earendil-works/pi)（TypeScript monorepo，`pi.dev`）  
>
> 统计日期：**2026-07-19**  
> 结论概览：**日常 coding agent 主路径可用** — skills / compaction / session UX / thinking / MCP / OAuth（Codex·xAI·OpenCode）/ Plan Mode / harness 埋点均已落地；主差距在 **TS 扩展生态、厂商 OAuth 全量、安装分发、完整 RPC/SDK、TUI 精修**。

---

## 1. 总体判断

| 维度 | 对齐程度 | 说明 |
|------|----------|------|
| 哲学与定位 | ⭐⭐⭐⭐ | 极简核心 + tools + 可扩展；**MCP 为平台基础能力**；无子 agent；**Plan Mode 已内置（Pi 无）** |
| Agent loop | ⭐⭐⭐⭐ | prompt → LLM → tool → loop；steer / follow-up / abort 已有 |
| 内置 tools | ⭐⭐⭐⭐⭐ | read/write/edit/bash + bash_output/bash_kill + grep/find/ls + ask_user + web_*（Pi 联网靠 skill） |
| Session JSONL 树 | ⭐⭐⭐⭐ | v3 子集 + 迁移 + branch + resume/new/name；fork/clone/交互式 tree UI 弱 |
| Providers | ⭐⭐⭐⭐ | mock / anthropic / openai / openai-codex / ollama / openrouter / deepseek / gemini + models.json `compat`；OAuth：Codex / xAI / OpenCode Zen·Go |
| Interactive TUI | ⭐⭐⭐ | 多行、@file、Tab 补全、Settings、模型 select、Plan/Build、HITL 列表；主题/keybindings/差分渲染弱于 Pi |
| 扩展系统 | ⭐⭐ | Rust Extension + hooks.json + plugins（本地）；**不兼容** TS 扩展 / 无 npm 包生态 |
| Skills / Prompts | ⭐⭐⭐⭐ | progressive disclosure（XML catalog + `read`）+ `/skill:name` + `/skills` 开关 |
| Compaction | ⭐⭐⭐⭐ | LLM 摘要 + overflow 重试 + `/compact`；阈值 70% context_window |
| RPC / SDK | ⭐⭐ | RPC：ping/prompt/abort/steer/follow_up/session/status/thinking/compact；无嵌入式 SDK 文档级 API |
| Harness / 观测 | ⭐⭐⭐⭐ | `--trace` → Langfuse OTLP + `one bench` + `benches/` 跨 agent 评测（Pi 侧不同路径） |
| 安装与分发 | ⭐ | 源码 `cargo build`；无 install.sh / self-update |
| 测试与兼容性 | ⭐⭐ | mock e2e、session 测试、bench smoke；缺 Pi session 全量兼容回归 |

**粗估完成度（2026-07-19）**

```
核心循环 ████████████████░░░░  80%
工具集   ████████████████████  95%
Session  ███████████████░░░░░  75%
Provider █████████████░░░░░░░  65%  (OAuth 部分 + models.json)
TUI      ████████████░░░░░░░░  55%
资源系统 ████████████████░░░░  80%  (skills / prompts / AGENTS)
扩展     ████████░░░░░░░░░░░░  40%  (Rust 面齐，生态无)
RPC/SDK  ██████░░░░░░░░░░░░░░  30%
生态/安装 ██░░░░░░░░░░░░░░░░░░  10%
─────────────────────────────
加权整体 ██████████████░░░░░░  ~70–75%
```

---

## 2. 已对齐（One ≈ Pi）

| 能力 | Pi | One | 备注 |
|------|----|-----|------|
| 默认 coding tools + 搜索 | ✅ | ✅ | + `bash_output` / `bash_kill` / `ask_user` |
| Agent tool-calling 循环 | ✅ | ✅ | `one-core::Agent`，`--max-turns` 默认 32 |
| Streaming text / thinking | ✅ | ✅ | SSE 多厂商 + TUI Ctrl+T 折叠 |
| 四种运行模式 | Interactive / Print / JSON / RPC | 同左 | RPC 方法面仍薄于 Pi |
| JSONL session 树 | ✅ v3 | ✅ v3 子集 | `~/.one/agent/sessions/` |
| `--continue` / `--resume` / `--session` / `--no-session` | ✅ | ✅ | `-r` 交互选 session |
| `/model` 切换 | ✅ | ✅ | Ctrl+L float select |
| `/tree` 分支 | ✅ 完整 TUI | ✅ 列表 + `/tree <id>` | 无折叠/过滤 UI |
| AGENTS.md + CLAUDE.md | ✅ | ✅ | 向上发现合并 |
| Skills progressive disclosure | ✅ | ✅ | catalog + `read` + `/skill:name` + enable/disable |
| Prompt templates `/name` | ✅ | ✅ | |
| Compaction | LLM + overflow | ✅ | `/compact` + 自动阈值 |
| Steer / Follow-up | Enter / Alt+Enter | Ctrl+S / Alt+Enter | 快捷键不同 |
| Abort / 退出 | Escape | Esc；Ctrl+C 渐进退出 | |
| models.json 自定义 + `compat` | ✅ | ✅ | Pi 形 providers 块 + wire `api` 枚举 |
| OpenAI Responses + Completions | ✅ | ✅ | 另：Anthropic Messages、Gemini generateContent |
| HTML export / Gist share | ✅ | ✅ | `--export` / `--share` / `/export` |
| Session v1/v2 → v3 迁移 | ✅ | ✅ | |
| OAuth / 订阅登录 | 多厂商 | 🟨 Codex + xAI + OpenCode Zen/Go | Claude Pro/Max、Copilot 待 |
| Thinking level | Shift+Tab 循环 | `/thinking` · `/settings` · session 持久化 | Shift+Tab = Plan/Build |
| Settings UI | ✅ | ✅ | Ctrl+G 居中面板 |

---

## 3. 主要差距（按影响排序）

### 3.1 P0 — 体验与认证剩余

| 差距 | 官方 Pi | One 现状 | 影响 |
|------|---------|----------|------|
| **OAuth 全量** | Claude Pro/Max、ChatGPT、Copilot、OpenCode… | ✅ Codex · xAI SuperGrok · OpenCode Zen/Go；❌ Claude / Copilot | 部分订阅用户仍须 API key |
| **Provider 覆盖** | 30+ 内置 | 内置十余 + 任意 models.json 自定义 | Bedrock/Azure/Vertex 等靠兼容端点或 OpenRouter |
| **TUI 精修** | 外部编辑器、模糊 `@`、主题热重载、完整 footer | 多行/`@`/Tab/图片粘贴已有；主题/keybindings 写死 | 质感与可定制性弱一档 |
| **Footer / cost** | token↑↓、cache、cost、context % | usage 估算 + thinking 标签；pricing 表粗 | 费用判断不如 Pi 准 |

### 3.2 P1 — 扩展生态与可编程面

| 差距 | 官方 Pi | One 现状 |
|------|---------|----------|
| 扩展语言 | TypeScript 一等公民 | Rust `Extension` trait + hooks.json + 本地 plugins |
| 热扩展 API | registerTool / Command / 自定义 UI / provider | tools / context / before·after / lifecycle；TUI 自定义 ❌ |
| 包管理 | `pi install` npm/git | 无（Package/Suite 仅设计，见 package-suites.md） |
| 动态加载 | 原生 TS | dylib 实验性（已知 builtin 名） |
| Keybindings / 主题 | 用户 JSON | 写死 |
| SDK | `createAgentSession` 等 | crate 库，无文档化嵌入 SDK |
| RPC | 完整 JSONL 协议 | 9 个 method（见 cli.md）；无事件订阅协议化 |

### 3.3 P2 — Session / 配置细节

| 差距 | 官方 Pi | One 现状 |
|------|---------|----------|
| `/fork` `/clone` | ✅ | ❌ |
| Tree UI | 搜索、折叠、label | 文本列表 |
| `SYSTEM.md` / `APPEND_SYSTEM.md` | ✅ | 默认 prompt + AGENTS + overlays |
| Tool 细粒度 CLI | `--tools` / `--exclude-tools` | `--read-only` / `--plan` / `--no-mcp` / `--no-skills` |
| `@file` CLI 注入 / stdin pipe | ✅ | 交互内 `@`；CLI 参数面弱 |
| Project trust | trust.json | 无（路径沙箱 + 审批替代） |

### 3.4 P3 — 工程质量与分发

| 差距 | 官方 Pi | One 现状 |
|------|---------|----------|
| 安装 | npm / install.sh | `cargo build -p one-cli` |
| self-update | ✅ | ❌ |
| 跨平台文档 | Windows / Termux 等 | 以 Linux 开发文档为主 |
| Pi session 互通回归 | 原生 | v3 子集，目录 `~/.one`，无全量回归集 |

---

## 4. 模块级对照

### 4.1 Crate / 包结构

| Pi (TS packages) | One (Rust crates) | 对齐 |
|------------------|-------------------|------|
| pi-ai | one-ai（含 auth / compat / 多厂商） | 较好 |
| pi-agent-core | one-core（+ trace） | 较好 |
| pi-coding-agent | one-cli + one-tools + one-session + one-resources | 拆分合理 |
| pi-tui | one-tui | 可用，精修差 |
| extensions | one-ext | 能力面 Codex 对齐，生态不对齐 |
| （无独立 MCP 包） | one-mcp | **One 多出来**：平台 MCP |
| — | benches/ + Langfuse | 评测路径不同 |

### 4.2 Slash 命令对照

| 命令 | Pi | One |
|------|----|-----|
| `/help` `/model` `/tree` `/export` `/reload` `/quit` | ✅ | ✅ |
| `/login` `/logout` | ✅ | ✅（Codex / xAI / OpenCode） |
| `/settings` `/thinking` | ✅ | ✅ |
| `/resume` `/new` `/name` `/session` `/rewind` | ✅ | ✅ |
| `/compact` | ✅ | ✅ |
| `/skill:name` `/skills` | ✅ | ✅ |
| `/plan` `/act` `/build` | — | ✅（One 独有） |
| `/mcp` | —（Pi 无内置 MCP） | ✅ |
| `/fork` `/clone` `/trust` | ✅ | ❌ |
| `/clear` | — | ✅ |

### 4.3 Provider 对照（摘要）

| 能力 | Pi | One |
|------|----|-----|
| Anthropic / OpenAI / Ollama / OpenRouter | ✅ | ✅ |
| Gemini 原生 | ✅ | ✅ |
| DeepSeek | ✅ | ✅ |
| openai-codex OAuth | ✅ | ✅ |
| xAI SuperGrok OAuth | ✅ | ✅ |
| OpenCode Zen / Go | ✅ | ✅ |
| Claude Pro / Copilot OAuth | ✅ | ❌ |
| Bedrock / Azure / Vertex 一等 | ✅ | models.json 自建 |
| Thinking / reasoning 全链路 | ✅ | ✅ multi-provider + `compat` |
| Token / cost | ✅ | 估算 + 粗 pricing |

### 4.4 RPC 对照

| Method | Pi | One |
|--------|----|-----|
| prompt / ping | ✅ | ✅ |
| abort / steer / follow_up | ✅ | ✅ |
| session / status | ✅ | ✅ |
| thinking / compact | ✅ | ✅ |
| 完整事件流订阅 / model 切换 | ✅ | ❌（打印机订阅，非协议） |

### 4.5 安全哲学差异（有意不同）

| 项 | Pi | One |
|----|----|-----|
| 路径边界 | 靠环境隔离 | **默认 workspace-write**；`--full-access` 关闭 |
| 权限弹窗 | 刻意不做 | 交互 Ask 列表（Always / Once / Fingerprint / No） |
| 细粒度规则 | 扩展 | `permissions.allow/deny/ask` |
| Bash OS 沙箱 | 环境隔离 | **bubblewrap**（可选） |
| MCP | 扩展生态 | **平台内置** `one-mcp` |

> One 对齐 Claude/Codex 的「工作区硬边界 + 审批 + 规则」；另加 Plan Mode 与 harness 轨迹。

---

## 5. 已实现且主路径可用（历史「半残」项）

| 功能 | 状态 |
|------|------|
| Skills | ✅ catalog + progressive `read` + force-load + enable/disable |
| Compaction | ✅ LLM 摘要 + overflow 重试 + `/compact` |
| Settings | ✅ `settings.json` + Ctrl+G + slash |
| Session UX | ✅ `/resume` `/new` `/name` `/session` `/rewind` · `-r` |
| Thinking | ✅ level + stream + signature 回放 + Ctrl+T |
| MCP | ✅ stdio + streamable HTTP + import + `/mcp` |
| OAuth | ✅ Codex / xAI / OpenCode（Claude/Copilot 待） |
| Extension 面 | ✅ gate + hooks + plugins + reload；dylib 仍实验 |
| Plan Mode | ✅ `--plan` / Shift+Tab / `/plan`→`/act` |
| Harness | ✅ Langfuse `--trace` + `one bench` + broken-kit |

---

## 6. One 相对 Pi 的「多出来」或不同选择

| 点 | 说明 |
|----|------|
| **Rust 单二进制** | 无 Node 运行时依赖 |
| **MCP 平台能力** | 内置客户端，非扩展 |
| **Plan / Act 模式** | 硬工具门控 |
| **路径沙箱 + bwrap + 审批规则** | 更保守默认 |
| **Harness → Langfuse** | OTLP 轨迹 + bench 打分 |
| **Mock 默认** | 无 key 可开发 / e2e |
| **配置目录 `~/.one`** | 不与 `~/.pi` 抢占 |

---

## 7. 建议补齐优先级

### 仍高价值

1. OAuth：Anthropic Claude Pro/Max、GitHub Copilot  
2. TUI：主题 / keybindings / footer cost 精修  
3. RPC/SDK 文档化（可嵌入集成）  
4. 安装脚本 + self-update  
5. Package / Suite MVP（coding profile 外置，见 package-suites.md）  
6. Pi session 全量兼容回归（可选）

### 明确不追 / 已改决策

- ~~内置 MCP~~ → **已是平台基础能力**  
- 内置 sub-agent orchestrator（仍不追）  
- ~~内置 plan mode~~ → **已做**  
- TS 扩展 1:1 兼容 → 评估中，非短期必须  

---

## 8. 一句话总结

> **One 已用 Rust 把 Pi 的「极简 agent 骨架」做成可日常使用的产品主路径**（loop、全套 coding tools、JSONL session、四模式、skills、compaction、thinking、MCP、部分 OAuth、Plan Mode、TUI、harness）。  
> 相对官方 Pi，最大洞在 **扩展/包生态（TS/npm）、剩余订阅 OAuth、安装分发、RPC/SDK 深度与 TUI 精修**，而不是「核心不能用」。

---

## 9. 参考

- One：`README.md`、`docs/architecture.md`、`docs/roadmap.md`、`docs/cli.md`、`docs/harness-eval.md`  
- 官方 Pi：https://pi.dev/ 、https://github.com/earendil-works/pi  
- 设计哲学：https://mariozechner.at/posts/2025-11-30-pi-coding-agent/
