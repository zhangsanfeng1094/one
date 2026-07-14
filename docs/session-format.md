# Session 格式

One 实现 JSONL session **v3 子集**，文件为 JSONL（每行一个 JSON 对象）。

## 文件位置

```
~/.one/agent/sessions/--<cwd-path>--/<timestamp>_<uuid>.jsonl
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
| `custom` | 扩展状态（不进入上下文） | ❌ |
| `custom_message` | 扩展注入消息 | ✅ |
| `label` | 书签标记 | ❌ |
| `model_change` | 模型切换记录 | 元数据 |
| `thinking_level_change` | 思考级别 | 元数据 |
| `session_info` | session 显示名称 | 元数据 |

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

`build_context_entries(leaf_id)` 从 leaf 走到 root，遇到 `compaction` 时：

1. 包含 compaction entry
2. 从 `firstKeptEntryId` 到 compaction 的 entries
3. compaction 之后的 entries

`build_session_context(leaf_id)` 将 entries 转为 `AgentMessage` 列表供 LLM 使用。

## 与官方 Pi 的差异

- HTML export / Gist share 已实现（`--export` / `--share` / `/export`）
- v1/v2 → v3 自动迁移已实现
- `custom_message` / `label` 类型已有，CLI 尚未暴露完整 label 操作
- 目录使用 `~/.one/agent/sessions/`（非 `~/.pi`）

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
```