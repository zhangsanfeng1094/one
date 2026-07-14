# 架构设计

## 设计原则

1. **极简核心**：Agent loop 保持短小，复杂能力通过扩展和资源配置实现
2. **Pi 兼容**：Session 格式、工具名称、配置目录尽量与官方 Pi 对齐
3. **crate 分层**：每层职责单一，可独立测试和嵌入

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
  → LLM complete（支持 text delta 事件）
  → 有 tool_call？
      是 → 执行 tool → 结果写入 messages → 继续循环
      否 → 返回最终文本
  → [可选] 持久化到 session JSONL
```

核心类型：

- `Agent`：维护 `messages`、`tools`、事件订阅
- `LlmProvider`：统一 `complete` / `complete_with_deltas`
- `Tool`：异步执行，返回 `ToolOutput`（内置含 `web_search` / `web_fetch`，`network` feature）

## Session 树

每个 session 文件是 JSONL，第一行是 header，后续每行是一个 entry。

Entry 通过 `id` / `parentId` 构成树，`leaf` 指向当前分支位置。`branch(entry_id)` 可回到历史节点继续对话。

详见 [session-format.md](session-format.md)。

## 资源加载

`ResourceLoader::discover(cwd, agent_dir)` 加载：

| 资源 | 路径 |
|------|------|
| AGENTS.md | `~/.one/agent/AGENTS.md` + 从 cwd 向上 |
| Skills | 项目 `.one/skills` / `.agents/skills` → 用户 `~/.one/agent/skills` / `~/.agents/skills` → **内置** `~/.one/agent/builtin-skills`（二进制 `include_str!`，如 `create-skill`） |
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

## 运行模式

| 模式 | 入口 | 用途 |
|------|------|------|
| Interactive | `one` | TUI 多轮对话 |
| Print | `one -p "..."` | 脚本单次调用 |
| JSON | `one --mode json -p "..."` | 结构化事件流 |
| RPC | `one --mode rpc` | stdin/stdout JSONL 集成 |

RPC 协议见 [cli.md](cli.md#rpc-模式)。