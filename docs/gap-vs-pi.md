# One vs 官方 Pi 差异分析

> 对比基准  
> - **One**：本仓库 `one`（Rust，约 10.8k LOC，v0.1.0）  
> - **官方 Pi**：[@earendil-works/pi-coding-agent](https://github.com/earendil-works/pi)（TypeScript monorepo，`pi.dev`）  
>
> 统计日期：2026-07-14（Phase A 补齐后更新）  
> 结论概览：**骨架 + P0 体验约 65–75%**；skills/compaction/session UX/thinking/多行输入已补；生态层（扩展/包/OAuth/RPC/SDK）仍是主差距。

---

## 1. 总体判断

| 维度 | 对齐程度 | 说明 |
|------|----------|------|
| 哲学与定位 | ⭐⭐⭐⭐ | 极简核心 + tools + 可扩展；不内置 MCP / 子 agent；**Plan Mode 已内置（Pi 无）** |
| Agent loop | ⭐⭐⭐⭐ | prompt → LLM → tool → loop；steer / follow-up / abort 已有 |
| 内置 tools | ⭐⭐⭐⭐⭐ | 7 coding tools + `web_search` / `web_fetch`（Pi 则靠 skill） |
| Session JSONL 树 | ⭐⭐⭐⭐ | v3 子集 + 迁移 + branch；fork/clone/交互式 tree UI 弱 |
| Providers | ⭐⭐⭐ | Mock/Anthropic/OpenAI/Ollama/OpenRouter + `compat`（Pi 形 models.json）；缺 OAuth 与大量内置厂商 |
| Interactive TUI | ⭐⭐ | 可用，但编辑器/快捷键/footer/主题远弱于 Pi |
| 扩展系统 | ⭐ | Rust trait + 实验性 dylib；**不兼容** TS 扩展 / 无 npm 包生态 |
| Skills / Prompts | ⭐⭐ | prompts 可展开；skills **仅发现、未接入运行时** |
| Compaction | ⭐ | 字符估算 + Debug dump，无 LLM 摘要 / overflow 重试 |
| RPC / SDK | ⭐ / ⭐ | RPC 仅 3 个 method；无嵌入式 SDK 文档级 API |
| 安装与分发 | ⭐ | 无 install.sh / self-update / package manager |
| 测试与兼容性 | ⭐⭐ | mock e2e、session 测试有；缺 Pi session 全量兼容回归 |

**粗估完成度**

```
核心循环 ████████████████░░░░  80%
工具集   ████████████████████  95%
Session  ██████████████░░░░░░  70%
Provider ████████░░░░░░░░░░░░  40%
TUI      ██████░░░░░░░░░░░░░░  30%
资源系统 ████████░░░░░░░░░░░░  40%  (skills 半残)
扩展     ███░░░░░░░░░░░░░░░░░  15%
RPC/SDK  ██░░░░░░░░░░░░░░░░░░  10%
生态/安装 █░░░░░░░░░░░░░░░░░░░   5%
─────────────────────────────
加权整体 ███████████░░░░░░░░░  ~55–65%
```

---

## 2. 已对齐（One ≈ Pi）

这些是「能当 coding agent 用」的基础，One 已基本具备。

| 能力 | Pi | One | 备注 |
|------|----|-----|------|
| 默认 4 tools + 扩展 3 tools | ✅ | ✅ | read/write/edit/bash + grep/find/ls |
| Agent tool-calling 循环 | ✅ | ✅ | `one-core::Agent`，max_turns=32 |
| Streaming text delta | ✅ | ✅ | SSE（Anthropic/OpenAI/Ollama） |
| 四种运行模式 | Interactive / Print / JSON / RPC | 同左 | RPC 协议深度差很多 |
| JSONL session 树（id/parentId） | ✅ v3 | ✅ v3 子集 | 路径 `~/.one/agent/sessions/` |
| `--continue` / `--session` / `--no-session` | ✅ | ✅ | 缺 `-r` 交互选 session、`--fork` |
| `/model` 切换 | ✅ | ✅ | float picker + Ctrl+L |
| `/tree` 分支 | ✅ 完整 TUI | ✅ 列表 + `/tree <id>` | 无折叠/过滤/label 交互 |
| AGENTS.md 上下文 | ✅ | ✅ | 另支持 CLAUDE.md？**未确认/可能无** |
| Prompt templates `/name` | ✅ | ✅ | `expand_prompt` |
| Steer / Follow-up 队列 | Enter / Alt+Enter | Ctrl+S / Alt+Enter | 快捷键不同；delivery mode 配置无 |
| Abort | Escape | Esc 取消生成；Ctrl+C 渐进退出（关浮层/清输入/双击退出） | 单次 Ctrl+C 不退出，防误触 |
| models.json 自定义 | `~/.pi/agent/models.json` | `~/.one/agent/models.json` | providers 块 + legacy 扁平列表 |
| OpenAI Responses + Completions | ✅ | ✅ | 对齐 Pi `api` 字段语义 |
| HTML export / Gist share | ✅ | ✅ | `--export` / `--share` / `/export` |
| Session v1/v2 → v3 迁移 | ✅ | ✅ | `one-session::migrate` |
| 只读模式 | 工具 allowlist | `--read-only` | Pi 用 `--tools` / `--exclude-tools` 更细 |
| 非目标一致 | 无 MCP / 无子 agent / 无 plan mode | 无 MCP / 无子 agent；**有 Plan Mode** | One 额外做了 bash 沙箱 + 内置 Plan |

---

## 3. 主要差距（按影响排序）

### 3.1 P0 — 用户日常体验断层

| 差距 | 官方 Pi | One 现状 | 影响 |
|------|---------|----------|------|
| **OAuth 订阅登录** | `/login`：Claude Pro/Max、ChatGPT、Copilot | 仅 API key | 大量用户无法「开箱即用」 |
| **Provider 覆盖** | 30+ 内置（Gemini、Bedrock、Azure、xAI、Groq、DeepSeek、MiniMax…） | mock / anthropic / openai / ollama / openrouter | 非 OpenRouter 路径要自己拼 models.json |
| **TUI 编辑器** | 多行、Shift+Enter、`@` 文件模糊搜索、Tab 路径补全、Ctrl+G 外部编辑器、图片粘贴 | 多行 + `@`/Tab 路径 + 图片粘贴（路径/data-URI/**剪贴板位图 Ctrl+V**） | 质感接近；OAuth/footer 仍差 |
| **Footer / 用量** | token↑↓、cache R/W、cost、context %、model、cwd | 基础状态条 | 无法判断上下文压力与费用 |
| **Thinking level** | Shift+Tab 循环；session `thinking_level_change` | `/settings` / `/thinking`；**Shift+Tab 用于 Plan/Build** | 已接线 |
| **Session 管理 UX** | `/resume` 选择历史、`/new`、`/name`、`/session` 详情、`-r` | 仅 `--continue` / `--session` | 多任务切换困难 |
| **`/compact` 手动 + 智能自动压缩** | LLM 摘要；overflow 恢复重试；可自定义 instructions | 字符/4 估算；把旧消息 `Debug` 拼成 summary（**非模型摘要**） | 长会话质量不可用 |
| **Skills 真正生效** | catalog + 模型 `read` SKILL.md + 可选 `/skill:name` | ✅ progressive disclosure（XML catalog + location + force-load） | 已对齐标准路径 |

### 3.2 P1 — 扩展生态与可编程面

| 差距 | 官方 Pi | One 现状 |
|------|---------|----------|
| 扩展语言 | TypeScript 一等公民 | Rust `Extension` trait |
| 热扩展 API | registerTool / Command / 事件 / **自定义 UI** / provider | on_load / on_event / tools / 极简 commands |
| 事件模型 | 丰富（tool_call 可拦截、project_trust…） | 6 个粗粒度事件 |
| 包管理 | `pi install` npm/git、`pi list/update/config` | 无 |
| 动态加载 | 原生 TS require | dylib 实验性，且 **硬编码只认 `status` 扩展名** |
| 主题 | dark/light + 用户主题热重载 | 静态 Theme 结构体 |
| Keybindings | `keybindings.json` 可定制 | 写死在 app.rs |
| SDK | `createAgentSession` 等正式 API | 仅 crate 库，无文档化嵌入 SDK |
| RPC | 完整 JSONL 协议（见 docs/rpc.md） | `prompt` / `session` / `ping` 三个 method |

### 3.3 P2 — Session / 资源 / 配置细节

| 差距 | 官方 Pi | One 现状 |
|------|---------|----------|
| Session 路径 | `~/.pi/agent/sessions/` | `~/.one/agent/sessions/`（有意分家） |
| `/fork` `/clone` | 新文件复制分支 | 无 |
| Tree UI | 搜索、折叠、filter、label 书签 | 文本列表 + id 切换 |
| `SYSTEM.md` / `APPEND_SYSTEM.md` | 可替换/追加 system prompt | 仅默认 prompt + AGENTS.md 合并 |
| Context 发现 | 向上 walk + CLAUDE.md | AGENTS.md walk；CLAUDE.md 未见 |
| Skills 发现路径 | 全局/项目/父目录 `.agents/skills` | `~/.one` + `.one/skills` + cwd `.agents/skills`（无向上 walk） |
| Settings | `settings.json` + `/settings` | 无统一 settings；preferences 有限 |
| Project trust | trust.json + 启动确认 | 无（安全模型不同） |
| Tool 细粒度 CLI | `--tools` / `--exclude-tools` / `--no-tools` | 仅 `--read-only` |
| `@file` CLI 注入 | `pi @files... messages` | 无 |
| stdin pipe 进 print | ✅ | 未见 |
| `--offline` / 版本检查 / telemetry | 有完整策略 | 无 |

### 3.4 P3 — 工程质量与分发

| 差距 | 官方 Pi | One 现状 |
|------|---------|----------|
| 安装 | npm 全局 / `curl \| install.sh` | 源码 `cargo build` |
| self-update | `pi update --self` | 无 |
| 跨平台文档 | Windows / Termux / tmux 专门文档 | 基础 Linux 开发文档 |
| 兼容性测试 | 真实 session / 多 provider | mock e2e + session 单测 |
| 与 Pi session 互通 | 原生 | 文档称 v3 子集，**无全量兼容回归**；目录名不同 |
| 文档内一致性 | 成熟 | 部分过时（如 `extensions.md` 仍写热重载/custom 为 🔜，实际 roadmap 已勾选；`session-format.md` 仍写 export/migrate 未实现） |

---

## 4. 模块级对照

### 4.1 Crate / 包结构

| Pi (TS packages) | One (Rust crates) | 对齐 |
|------------------|-------------------|------|
| pi-ai | one-ai | 部分 |
| pi-agent-core | one-core | 较好 |
| pi-coding-agent（CLI/session/tools） | one-cli + one-tools + one-session + one-resources | 拆分合理，能力薄 |
| pi-tui | one-tui | 骨架有，能力薄 |
| extensions 运行时 | one-ext | 概念对齐，生态不对齐 |
| （无独立） | — | Pi 的 package manager 在 coding-agent 内 |

### 4.2 Slash 命令对照

| 命令 | Pi | One |
|------|----|-----|
| `/help` | ✅ | ✅ |
| `/model` | ✅ | ✅ |
| `/tree` | ✅ 完整 UI | ✅ 基础 |
| `/export` | ✅ | ✅ |
| `/reload` | ✅ 资源全量 | ✅ 扩展热重载 |
| `/quit` | ✅ | ✅ |
| `/login` `/logout` | ✅ | ❌ |
| `/settings` | ✅ | ❌ |
| `/scoped-models` | ✅ | ❌ |
| `/resume` `/new` `/name` `/session` | ✅ | ❌（`-n` CLI 有） |
| `/trust` | ✅ | ❌ |
| `/fork` `/clone` | ✅ | ❌ |
| `/compact` | ✅ | ❌（自动 compaction 很糙） |
| `/copy` | ✅ | ❌ |
| `/import` `/share` | ✅ | share 仅 CLI `--share` |
| `/hotkeys` `/changelog` | ✅ | ❌ |
| `/skill:name` | ✅ | ❌（skills 未接线） |
| 扩展/模板动态命令 | ✅ | prompts `/name` 有；扩展 commands 极弱 |
| `/clear` | — | ✅（One 独有清屏） |

### 4.3 Provider 对照（摘要）

| 能力 | Pi | One |
|------|----|-----|
| Anthropic Messages + SSE | ✅ | ✅ |
| OpenAI Responses / Completions | ✅ | ✅ |
| Ollama | ✅ | ✅（network feature） |
| OpenRouter | ✅ | ✅（http-providers） |
| Google / Vertex / Bedrock / Azure | ✅ | ❌（可经 OpenRouter 或自建 OpenAI 兼容） |
| OAuth（Claude / Codex / Copilot） | ✅ | ❌ |
| Thinking / reasoning blocks | ✅ 完整 | ✅ level + stream + UI + multi-provider wire |
| Token / cost 统计 | ✅ | ❌ |
| 跨 provider 上下文 handoff | ✅ | 未做专项 |
| 内置模型清单随版本更新 | ✅ | 依赖 models.json / 硬编码默认 |

### 4.4 RPC 对照

| Method / 能力 | Pi | One |
|---------------|----|-----|
| prompt | ✅ | ✅ |
| 完整事件流订阅 | ✅ | 打印机订阅，非协议化 |
| abort / steer / follow-up | ✅ | 库内有，**RPC 未暴露** |
| session 元数据 / branch | ✅ | 仅返回 path |
| model 切换 | ✅ | ❌ |
| ping | 可能有 | ✅ |
| 严格 LF JSONL 文档 | ✅ | 无完整协议文档 |

### 4.5 安全哲学差异（有意不同）

| 项 | Pi | One |
|----|----|-----|
| 路径边界 | 靠环境隔离 | **默认 workspace-write**：file tools 限 cwd + `--add-dir`；skill 发现根只读 allowlist（`.agents/skills` 等）；`--full-access` 关闭 |
| 权限弹窗 | **刻意不做**（容器 / 扩展自行处理） | 交互 Ask 弹窗（y/a/n）；print 模式 fail-closed；`-y` 自动批 |
| 细粒度规则 | 扩展自行 | `permissions.allow/deny/ask`（Claude 式 `Bash(git push *)`） |
| Bash OS 沙箱 | 靠环境隔离 | **bubblewrap**（workspace RW / home RO）；无 bwrap 时降级 |
| Project trust | 加载项目扩展/设置前确认 | 无 |
| 危险命令 | 靠环境隔离 | denylist + 规则 + OS sandbox |
| Skills 路径 | 任意 | `~/.one/agent` 默认可读，写仍限 workspace |

> One 对齐 Claude/Codex 的「工作区硬边界 + 审批 + 规则」；OS 沙箱用 bwrap（非 landlock ABI）。

---

## 5. 已实现但质量不够（「有」≠「齐」）

| 功能 | 状态 | 问题 |
|------|------|------|
| Compaction | 有 API + session entry | 摘要 = `format!("{message:?}")` 截断，不是 LLM 压缩；无 overflow 重试 |
| Skills | 能读 SKILL.md | 未进 prompt、无 `/skill:`、agent 不会自动选 skill |
| Extension custom state | trait 有 custom_state / restore | 生态与事件面太窄，难做真扩展 |
| dylib 扩展 | feature 有 | 仅 `status` 名硬编码，不是通用插件 ABI |
| `/tree` | 能切 branch | 无交互树视图、无 branch_summary 用户流 |
| Thinking 类型 | ContentBlock / StreamEvent / session entry | ✅ level + signature replay + TUI 折叠 Ctrl+T |
| 图片消息类型 | `TextOrImage::Image { path }` | ✅ 只存 path；发 API 再读文件→base64；无 session 内联 base64 |
| Markdown 渲染 | TUI 有表格等 | 差分渲染 / 同步输出 / flicker 控制弱于 pi-tui |
| 文档 | 有一套 | 与代码漂移（export/migrate/reload 状态不一致） |

---

## 6. One 相对 Pi 的「多出来」或不同选择

| 点 | 说明 |
|----|------|
| **Rust 单二进制** | 部署与性能路径不同；无 Node 运行时依赖 |
| **Crate 分层** | 可嵌入其他 Rust 应用（但 SDK 文档未成形） |
| **Bash 沙箱默认开启** | 与 Pi 哲学不同，更保守 |
| **Mock provider 默认** | 便于无 key 开发与 e2e；Pi 默认要认证 |
| **配置目录 `~/.one`** | 不与 `~/.pi` 抢占；也意味着 **不能直接共用 Pi 扩展/包** |

---

## 7. 建议补齐优先级（路线图建议）

### Phase A — 达到「日常可替用」

1. **Skills 接线**：注入 skill 列表到 system 或实现 `/skill:name` + 自动加载  
2. **Compaction 升级**：用 LLM 生成 summary；context overflow 时 compact 后重试  
3. **TUI 编辑器**：多行、`@` 文件、路径 Tab、图片粘贴  
4. **Session UX**：`/resume`、`/new`、`/name`、`/session`；可选 `-r`  
5. **Thinking level**：`/settings` / `/thinking` + provider 传参 + session 记录  
6. **Footer 用量**：至少 input/output tokens + context 占用估算  
7. **文档与代码对齐**（export/migrate/reload/skills 真实状态）

### Phase B — 认证与模型面

1. OAuth 或「订阅 token」通路（至少 Anthropic / OpenAI 一条）  
2. 扩展 models.json 文档 + 常见厂商预设  
3. Token/cost 记账写入 session / footer  
4. `--tools` / `--exclude-tools` / stdin pipe / `@file`

### Phase C — 生态

1. 定义稳定 Extension ABI（或 WASM），去掉 dylib 硬编码  
2. 评估 QuickJS / WASM TS 兼容层（高成本，可选）  
3. 包管理雏形：`one install git:...`（不必先兼容 npm pi-package）  
4. RPC 协议补齐到可集成（参考 OpenClaw 类场景）  
5. 安装脚本 + self-update  
6. 与官方 Pi session 样例的兼容性测试集

### 明确不追（与 Pi 一致）

- 内置 MCP  
- 内置 sub-agent orchestrator  
- 内置 plan mode / todo 工具  

---

## 8. 一句话总结

> **One 已经把 Pi 的「极简 agent 骨架」用 Rust 搭起来了（loop、7 tools、JSONL 树 session、四模式、基础 TUI、部分 provider），但离官方 Pi 的「可日常重度使用 + 可扩展生态」还差大约半个产品：**  
> 最大洞在 **OAuth/多 provider、TUI 编辑体验、真 compaction、skills 接线、扩展/包生态、完整 RPC/SDK**。  
> 若目标是「Pi 兼容的 Rust 实现」，优先修 **session/资源兼容 + skills + compaction + TUI**；若目标是「独立 Rust agent」，可接受扩展生态分叉，仍需补齐 **体验层 P0**。

---

## 9. 参考

- One：`README.md`、`docs/architecture.md`、`docs/roadmap.md`、`docs/cli.md`  
- 官方 Pi：https://pi.dev/ 、https://github.com/earendil-works/pi/blob/main/packages/coding-agent/README.md  
- 设计哲学：https://mariozechner.at/posts/2025-11-30-pi-coding-agent/
