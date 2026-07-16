# 架构设计

## 设计原则

1. **极简核心**：Agent loop 保持短小，复杂能力通过扩展和资源配置实现
2. **Pi 兼容**：Session 格式、工具名称、配置目录尽量与官方 Pi 对齐
3. **crate 分层**：每层职责单一，可独立测试和嵌入
4. **默认工作区边界**：file tools 默认只能访问 `--cwd` + `--add-dir`；skill 发现根只读（`~/.one/agent`、`~/.agents/skills`、兼容 `~/.codex|claude|grok/skills`）；`--full-access` 显式关闭
5. **领域外置（规划中）**：编程/办公等工作流以 **Package / Suite** 装配，Core 不按领域名分支；见 [package-suites.md](./package-suites.md)（草案，暂未实现）

## Crate 依赖图

```
one-cli
 ├── one-tui          (Interactive 模式)
 ├── one-session      (会话持久化)
 ├── one-resources    (AGENTS.md / skills / prompts)
 ├── one-ext          (扩展 runtime)
 ├── one-tools        (内置 tools)
 ├── one-ai           (LLM providers)
 └── one-core         (Agent loop / messages / events)
```

## Agent Loop

```
用户输入
  → [可选] prompt 模板展开 (/deploy)
  → [可选] compaction 检查
  → LLM complete（text + thinking delta 事件）
  → 有 tool_call？
      是 → 执行 tool → 结果写入 messages → 继续循环
      否 → 返回最终文本
  → [可选] 持久化到 session JSONL
```

核心类型：

- `Agent`：维护 `messages`、`tools`、事件订阅
- `LlmProvider`：统一 `complete` / `complete_streaming`
- `ThinkingLevel`：统一 off/low/medium/high；各 provider 经 `one_ai::thinking` 映射到 budget / effort / think
- `ContentBlock::Thinking`：thinking 正文 + 可选 `signature`（多轮回传）+ `redacted`
- `Tool`：异步执行，返回 `ToolOutput`（内置含 `web_search` / `web_fetch`，`network` feature）
- `PathPolicy` / `SandboxMode`（`one-tools`）：路径 canonicalize 后校验 workspace 根；`workspace-write`（默认）vs `full-access`
- `ToolGate`（`one-core`）+ `PermissionGate`（`one-cli`）：工具执行前 allow/deny/ask；交互弹窗或 fail-closed
- `PermissionRules`：Claude 式 `Bash(git push *)` / `Write(**/.env*)` 规则
- `OsSandbox`：workspace-write 下 bash 经 `bwrap`（workspace RW、home RO）

## Session 树

每个 session 文件是 JSONL，第一行是 header，后续每行是一个 entry。

Entry 通过 `id` / `parentId` 构成树，`leaf` 指向当前分支位置。`branch(entry_id)` 可回到历史节点继续对话。

详见 [session-format.md](session-format.md)。

## 资源加载

`ResourceLoader::discover(cwd, agent_dir)` 加载：

| 资源 | 路径 |
|------|------|
| AGENTS.md | `~/.one/agent/AGENTS.md` + 从 cwd 向上 |
| Skills | 项目 `.one/skills` / `.agents/skills` → 用户 `~/.one/agent/skills` / **`~/.agents/skills`（跨客户端通用）** / 兼容 `~/.claude|codex|grok/skills` → **内置** `~/.one/agent/builtin-skills`；PathPolicy **只读 allowlist** 上述目录（agentskills progressive disclosure）；`skills_config` enable/disable（类 Codex） |
| Prompts | `~/.one/agent/prompts/*.md`、`.one/prompts/*.md` |

System prompt = 默认 prompt + AGENTS.md/CLAUDE.md + **skills catalog（仅 name/description/location）**。

Skills 正文不预加载；模型在相关任务上 `read` 对应 `SKILL.md`（progressive disclosure）。

## 扩展系统

Rust-native 扩展实现 `Extension` trait：

```rust
#[async_trait]
trait Extension {
    fn name(&self) -> &str;
    async fn on_load(&self, ctx: &ExtensionContext<'_>) -> Result<()>;
    async fn on_event(&self, event: &ExtensionEvent) -> Result<()>;
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}
```

与官方 Pi 的 TypeScript 扩展**不兼容**，但 API 理念相近。

## Package / Suite（规划中）

目标形态：启用 **coding** 包即可编程，启用 **office** 包即可办公；工具列表、skills、system overlay、权限预设由包声明式 merge，Core 只消费扁平 `RuntimeProfile`。

详见 **[package-suites.md](./package-suites.md)**（设计草案，**暂不实现**）。

## 运行模式

| 模式 | 入口 | 用途 |
|------|------|------|
| Interactive | `one` | TUI 多轮对话 |
| Print | `one -p "..."` | 脚本单次调用 |
| JSON | `one --mode json -p "..."` | 结构化事件流 |
| RPC | `one --mode rpc` | stdin/stdout JSONL 集成 |

RPC 协议见 [cli.md](cli.md#rpc-模式)。

## Plan Mode（内置）

Agent 在 **Plan** 与 **Act/Build** 之间切换：

| 状态 | 工具 | 产出 |
|------|------|------|
| Plan | 只读 + 仅可写 plan 文件 + `exit_plan_mode` | `~/.one/agent/plans/<uuid>.md` |
| Act | 完整 coding tools + 扩展 tools | 按批准的 plan 改代码 |

进入：`/plan`、`--plan`、**Shift+Tab**。退出并实现：`/act` / `/build`（注入 plan 正文并自动开一轮实现）；Shift+Tab 再切回 Build（不自动实现）。  
硬门控（无 bash / 不能写业务代码）+ system prompt overlay，对齐 Claude Code 工作流。