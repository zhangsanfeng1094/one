# 意图识别与规则自学习 (Intent Graph & Learning)

One 内置了基于**带标签属性图 (Labeled Property Graph, LPG)** 的意图识别与规则自学习引擎。它负责在运行时对用户提问进行实时意图推理、实体提取、动态上下文约束（`<system-reminder>`）注入以及最优工具推荐，支持动态查询、推理测试、轨迹自学习与持久化更新。

---

## 核心工作原理

```
                           ┌──────────────────────────────────────────────┐
                           │            用户请求 (User Query)              │
                           └──────────────────────┬───────────────────────┘
                                                  │
                  ┌───────────────────────────────┴───────────────────────────────┐
                  ▼                                                               ▼
   ┌──────────────────────────────┐                                ┌──────────────────────────────┐
   │    LPG 意图图谱 (IntentGraph)  │                                │     分层记忆库 (Memory L1-L3)   │
   ├──────────────────────────────┤                                ├──────────────────────────────┤
   │ • 动宾匹配 (Verb-Object)     │                                │ • 跨 Session 架构知识与背景  │
   │ • 实体提取 (Entity Parse)    │                                │ • 用户代码风格与偏好约定     │
   │ • 冲突仲裁与冷却控制         │                                │ • 历史踩坑教训 (Lessons)     │
   └──────────────┬───────────────┘                                └──────────────┬───────────────┘
                  │                                                               │
                  ├───────────────────────────────┬───────────────────────────────┤
                  ▼                               ▼                               ▼
    ┌───────────────────────────┐   ┌───────────────────────────┐   ┌───────────────────────────┐
    │  JIT System Reminder 注入 │   │      TUI Alert / Toast    │   │     建议工具推荐列表      │
    │  (Mandatory / Recommended)│   │  (高亮提示命中意图与约束)  │   │ (tool-deepwiki, web_search)│
    └───────────────────────────┘   └───────────────────────────┘   └───────────────────────────┘
```

---

## CLI 命令行管理 (`one learn`)

通过 `one learn` 子命令，可在终端中直接管理、查询、测试和训练意图规则：

| 命令 | 说明 | 示例 |
| :--- | :--- | :--- |
| **`one learn "<规则>"`** | 录入自然语言或结构化意图规则（自动提取动词触发器、实体、提醒与推荐工具） | `one learn "当用户询问架构或源码实现时，建议优先使用 find 和 deepwiki"` |
| **`one learn --list`** | 查看当前所有已生效的自定义规则 | `one learn --list` |
| **`one learn --status`** | 查看图谱统计（总节点数、边数、触发器数、持久化路径） | `one learn --status` |
| **`one learn --test "<query>"`** | 模拟意图推理测试（输出匹配意图、提取实体、注入提醒与推荐工具） | `one learn --test "查一下 grok-build 的源码实现"` |
| **`one learn --reset`** | 清除所有自定义学习规则并重置为内置图谱 | `one learn --reset` |

### 规则录入格式示例

1. **自然语言格式**：
   ```bash
   one learn "当用户询问 Grok Build 或 grok-build 架构和源码时，建议优先使用 deepwiki 查阅 xai-org/grok-build 仓库"
   ```
2. **结构化格式**：
   ```bash
   one learn "意图: 数据库迁移 | 触发: 迁移,执行 + 数据库,db,migration | 提醒: 迁移数据库前必须先备份现有生产数据 | 级别: 强制 | 工具: bash"
   ```

### 意图推理测试输出示例
```bash
one learn --test "查一下 grok-build 的源码实现"
```
```text
Query: 查一下 grok-build 的源码实现

Matched intents:
  · 查阅外部库文档 (SearchExternalDocs)  confidence=0.59  evidence=trigger:trig-external-usage
  · 分析或调研框架架构、源码设计 (custom-intent-66db4832)  confidence=0.59  evidence=trigger:custom-trig-66db4832
  · 检索信息 (RetrieveInformation)  confidence=0.50  evidence=sub_intent:SearchExternalDocs

Entities: grok-build

Reminders:
  · 🟡 建议策略 [外部文档查询指引] (rem-deepwiki-docs)
    涉及第三方开源库（grok-build）的 API、用法或规范时，优先调用 deepwiki 查阅权威文档，不要盲目推测。

Suggested tools:
  · deepwiki (from SearchExternalDocs)
  · find (from SearchExternalDocs)
```

---

## 交互式 TUI 命令 (`/learn`)

在交互模式下，支持直接在对话过程中沉淀经验与偏好：

- **`/learn`**：**从当前会话轨迹自动学习**。自动提取最近一轮提问的动宾特征、最终使用的工具链组合并持久化。
- **`/learn <规则文本>`**：直接录入新规则。
- **`/learn list`**：列出已生效的自定义规则。
- **`/learn status`**：查看当前意图图谱统计。
- **`/learn reset`**：重置图谱。

---

## 跨会话持久化与 Memory 协作

1. **意图图谱持久化**：自定义规则会自动持久化至 `~/.one/agent/intent_graph/custom.json`，在启动和每次新会话时自动热合并。
2. **工具意图记忆 (`type: tool_intent`)**：Agent 内置的 `memory_search` 与 `memory_write` 工具支持 `tool_intent` 类型备忘录（如 `tool-intent-open-source-deepwiki`），用于沉淀特定开源库、API 查阅习惯与官方仓库地址。
