# Session 格式

One 实现 JSONL session **v3 子集**，文件为 JSONL（每行一个 JSON 对象）。

附加 **派生 sidecar**（`*.summary.json` 等）用于列表加速与可观测性；**不是**消息真源，缺失时行为与旧版一致。

## 文件位置

```
~/.one/agent/sessions/--<cwd-path>--/
  <timestamp>_<uuid>.jsonl              # 权威会话树
  <timestamp>_<uuid>.summary.json       # 派生索引（可选）
  <timestamp>_<uuid>.system_prompt.txt  # 大 system prompt spill（可选）
  prompt_history.jsonl                  # 跨 session 输入历史（非 session）
```

`<cwd-path>` 为工作目录路径，将 `/` 替换为 `-`。

## Header（第一行）

```json
{
  "type": "session",
  "version": 3,
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "timestamp": "2026-07-14T10:00:00.000Z",
  "cwd": "/home/user/project"
}
```

## Entry 类型

| type | 说明 | 参与 LLM 上下文 |
|------|------|----------------|
| `message` | 用户/助手/工具结果消息 | ✅ |
| `compaction` | 上下文压缩摘要 | ✅（作为摘要） |
| `branch_summary` | 分支切换摘要 | ✅ |
| `custom` | 扩展 / One 元数据（不进入上下文） | ❌ |
| `custom_message` | 扩展注入消息 | ✅ |
| `label` | 书签标记 | ❌ |
| `model_change` | 模型切换记录 | 元数据 |
| `thinking_level_change` | 思考级别 | 元数据 |
| `session_info` | session 显示名称 | 元数据 |
| `rewind_marker` | 会话回退断点标记与模式 | ❌（对齐 Grok Build） |

## Grok-Build 架构体系支持

### 1. 回退标记（`RewindMarker` 与 `RewindMode`）

对齐 Grok Build 的显式回退分支追踪机制，当发生 `/rewind` 或分支回滚时，在事件流中记录 `RewindMarker`：

```json
{
  "type": "rewind_marker",
  "id": "r1e2w3d4",
  "parentId": "prev1234",
  "timestamp": "2026-08-21T10:00:00.000Z",
  "targetEntryId": "target_user_prompt_id",
  "mode": "all",
  "promptIndex": 2,
  "revertedFiles": ["src/main.rs", "src/lib.rs"]
}
```

- `mode` 支持三种模式：
  - `all`：回滚对话上下文与文件变更
  - `conversation_only`：仅回退对话上下文，保留文件修改
  - `files_only`：仅回滚文件修改，保留对话上下文

### 2. 结构化 Sidecar 体系

除核心 `*.jsonl` 事件流外，会话状态通过独立 Sidecar 文件持久化，避免大型工作区状态污染对话上下文：

| 文件 | 说明 |
|------|------|
| `<stem>.summary.json` | 索引与 Token 累计统计（快速恢复 UI 状态） |
| `<stem>.todo.json` | 会话关联的任务列表状态（`TodoSidecar`） |
| `<stem>.plan.json` | 架构规划与审核状态（`PlanSidecar`） |
| `<stem>.hunks.json` | 针对各 prompt_index 的文件修改快照（`HunkSnapshotsSidecar`） |
| `<stem>.lock` | 会话活跃锁与进程存活信息（PID / 状态机） |

### 3. 会话状态生命周期（`SessionPresence`）与崩溃检测

对齐 Grok Build `SessionPresence`：
- `Resident { activity: Idle | Working }`：当前进程正在运行的活跃会话
- `Attaching`：重连挂载中
- `Evicted`：从内存缓存卸载
- `Closed`：正常退出保存
- `DeadFailed { error }`：进程异常退出/崩溃的会话（自动识别并支持一键恢复）
- `Dormant`：磁盘休眠会话

### 4. 异步持久化通道（`SessionActor`）

通过 Tokio 异步 Actor（`SessionActor` + `PersistenceMsg`）将文件追加、Sidecar 写入与磁盘刷盘解耦，彻底避免 I/O 阻塞主事件循环与 TUI 渲染。

## 树结构

每个 entry（除 header）包含：

```json
{
  "type": "message",
  "id": "a1b2c3d4",
  "parentId": "prev1234",
  "timestamp": "2026-07-14T10:00:01.000Z",
  "message": { "role": "user", "content": "Hello" }
}
```

- 第一个 entry 的 `parentId` 为 `null`
- `branch(entry_id)` 将 leaf 移到历史节点，从该点继续产生新分支

## Context 构建

`build_context_entries(leaf_id)` 从 leaf 走到 root，取路径上**最新**的 `compaction`：

1. 包含该 compaction entry **一次**（作为摘要消息）
2. 从 `firstKeptEntryId` 起、到该 compaction **之前**的 entries（近期窗口）
3. 该 compaction **之后**的 entries（压缩后又产生的消息）

`firstKeptEntryId` 必须是「保留窗口里最旧一条」的 entry id（不是当前 leaf）。  
写入时用 `SessionManager::first_kept_entry_id_for_tail(kept.len())`。

压缩时 `compaction.keep_recent`（默认 2）按 **用户回合** 计：一轮 = 一条 User 及其后的 assistant/工具消息。`/compact` 与自动压缩都按这个配置保留最近 N 轮原文，更早的回合收进摘要。TUI 在压缩期间保持可滚动，并显示 compacting 标记；完成后换成结果（保留回合数、压缩前后 token）。`details` 可含 `kept_turns` 与 `tokens_after`。

`build_session_context(leaf_id)` 将 entries 转为 `AgentMessage` 列表供 **LLM** 使用。

`build_transcript_entries(leaf_id)` 返回 leaf→root **完整路径**（含 compaction 之前的消息）。TUI `/resume` 用这条路径重画对话：compaction 只显示一行标记，**不**用摘要替换屏幕上的历史。压缩改的是模型 context，不是用户看到的 session。

**`SessionEntry::Custom` 永不进入 LLM 上下文**（含下方 `one.*` 元数据）。

## One 元数据 `custom_type`（`one.*`）

均通过 `append_custom` / 专用 helper 写入；旧客户端打开时当作普通 Custom 忽略类型名即可。

| custom_type | 写入时机 | data 要点 |
|-------------|----------|-----------|
| `one.usage` | 每次用户 prompt 跑完（含失败/中断） | `delta` / `total` TokenUsage、`context_size_tokens`、`provider`、`model`、`prompt_index` |
| `one.tool_audit` | 同上（有工具时） | `tools[]`：`tool_call_id`、`name`、`duration_ms?`、`is_error`（**无** stdout 正文） |
| `one.error` | 该轮 agent **终端失败**（EmptyResponse / provider / abort / max_turns…） | `kind`、`message`、`stop_reason`（`error`\|`aborted`）、`prompt_index`、可选 `provider`/`model` |
| `one.prompt_snapshot` | session 创建、`/new`、`/reload`、resume 且 system prompt hash 变化 | `hash`、`byte_len`、小文本 inline 或 `path` spill |

Resume 时用最新 `one.usage.total` **恢复 UI 累计 token**（不写回消息列表）。

`one.error` 对齐 **Grok Build**：失败写在 session 旁路（Grok 的 `updates.jsonl` / `turn_completed stop_reason:error`），**不**注入下一轮模型的 chat history，避免 conversation pollution。

### 示例：`one.error`

```json
{
  "type": "custom",
  "id": "e1f2a3b4",
  "parentId": "prev",
  "timestamp": "2026-08-04T13:13:19Z",
  "custom_type": "one.error",
  "data": {
    "schema": 1,
    "kind": "empty_response",
    "message": "empty model response (no text or tool calls) after 11 attempt(s)",
    "stop_reason": "error",
    "prompt_index": 1,
    "provider": "ziyong-gpt",
    "model": "gpt-5.6-luna"
  }
}
```

### 示例：`one.usage`

```json
{
  "type": "custom",
  "id": "a1b2c3d4",
  "parentId": "prev",
  "timestamp": "2026-08-02T10:00:00Z",
  "custom_type": "one.usage",
  "data": {
    "schema": 1,
    "kind": "run",
    "delta": { "input_tokens": 100, "output_tokens": 20, "cache_read_tokens": 0, "cache_write_tokens": 0 },
    "total": { "input_tokens": 500, "output_tokens": 80, "cache_read_tokens": 0, "cache_write_tokens": 0 },
    "context_size_tokens": 100,
    "provider": "ziyong",
    "model": "grok-4.5",
    "prompt_index": 0
  }
}
```

## Sidecar：`*.summary.json`

与 JSONL **同 stem**，例如 `20260802_….jsonl` → `20260802_….summary.json`。

- **派生数据**：由 `SessionManager::write_summary` 从内存树重建
- 字段：`id`、`cwd`、`name`、`preview`、`leaf_id`、`entry_count`、`message_count`、`model`、`provider`、`usage_total`、`tool_call_count`、`tools_used`、`system_prompt_hash` 等
- `/resume` 列表**优先**读 sidecar；损坏或缺失则 **fallback** 到 JSONL 前缀扫描（旧 session 不受影响）
- 写入失败只打日志，**不**影响对话

## 与官方 Pi 的差异

- HTML export / Gist share 已实现（`--export` / `--share` / `/export`）
- v1/v2 → v3 自动迁移已实现
- `custom_message` / `label` 类型已有，CLI 尚未暴露完整 label 操作
- 目录使用 `~/.one/agent/sessions/`（非 `~/.pi`）
- 元数据 sidecar 与 `one.*` Custom 为 One 扩展（不进模型上下文）

## API 示例

```rust
use one_session::SessionManager;

// 创建
let mut sm = SessionManager::create("/path/to/project").await?;

// 追加消息
sm.append_message(AgentMessage::user_text("hello")).await?;

// 恢复
let sm = SessionManager::continue_recent("/path/to/project").await?;

// 分支
sm.branch("a1b2c3d4")?;

// 构建上下文
let ctx = sm.build_session_context();

// 元数据（不进上下文）
sm.append_usage(&usage_meta).await?;
sm.write_summary()?;
```
