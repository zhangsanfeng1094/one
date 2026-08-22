# 基于属性图 (LPG) 的意图识别与动态提醒系统

> **状态**：🟢 **LPG 意图图谱与动态注入已收敛完成**  
> **更新时间**：2026-08-22  
> **适用范围**：One 的运行时意图推理、动态 `<system-reminder>` 注入、工具路由建议、UI 告警卡片与 `/learn` 规则学习

---

## 1. 架构总览与收敛设计

One 已完成从传统扁平 Memory 规则向**带标签属性图 (Labeled Property Graph, LPG)** 意图引擎的收敛：

```text
                           ┌──────────────────────────────────────────────┐
                           │            用户请求 (User Query)              │
                           └──────────────────────┬───────────────────────┘
                                                  │
                  ┌───────────────────────────────┴───────────────────────────────┐
                  ▼                                                               ▼
   ┌──────────────────────────────┐                                ┌──────────────────────────────┐
   │    LPG 意图图谱 (IntentGraph)  │                                │     分层记忆库 (Memory L1-L3)   │
   │ crates/one-resources/intent_ │                                │ crates/one-resources/memory  │
   ├──────────────────────────────┤                                ├──────────────────────────────┤
   │ • 动宾分解 (Verb-Object)     │                                │ • 跨 Session 架构知识与背景  │
   │ • 意图推理 (Infer Engine)    │                                │ • 用户代码风格与偏好约定     │
   │ • 冲突仲裁 (Veto / Conflict) │                                │ • 历史踩坑教训 (Lessons)     │
   │ • 概念提问抑制 / Meta 过滤   │                                │                              │
   └──────────────┬───────────────┘                                └──────────────┬───────────────┘
                  │                                                               │
                  ├───────────────────────────────┬───────────────────────────────┤
                  ▼                               ▼                               ▼
    ┌───────────────────────────┐   ┌───────────────────────────┐   ┌───────────────────────────┐
    │  JIT System Reminder 注入 │   │      TUI Alert / Toast    │   │     建议工具推荐列表      │
    │  (Mandatory/Recommended)  │   │  (高亮提示命中意图与约束)  │   │ (tool-deepwiki, web_search)│
    └───────────────────────────┘   └───────────────────────────┘   └───────────────────────────┘
```

### 职责边界收敛原则
1. **LPG 意图图谱 (Intent Graph)**：专职负责**意图识别、动作意图关联、工作流约束 (Reminders)、工具推荐建议 (Tool Suggestions) 以及安全守卫 (Guardrails)**。通过内置规则库及 `one learn` / `/learn` 命令动态维护。
2. **记忆系统 (Memory L1–L3)**：专职负责**跨 Session 的长效知识积累**（项目架构事实、业务背景、个人编码偏好、重要结论），不再承担工具意图匹配与规则注入的冗余职责。

---

## 2. LPG 意图图谱数据模型

LPG 模型位于 `crates/one-resources/src/intent_graph.rs`，包含以下核心要素：

### 2.1 节点类型 (Node Types)
- **`Trigger`**：触发条件节点，支持：
  - `VerbObject(verb, object)`：动宾匹配（如 `查/搜` + `文档/开源库`）
  - `Substring(pattern)`：子串模糊匹配
  - `Exact(phrase)`：精确短语匹配
- **`Intent`**：意图定义节点（如 `SearchExternalDocs`, `GitDestructiveAction`, `WebFactSearch`, `RunTestsAndVerification`）。
- **`Entity`**：实体节点，用于上下文实体解析与同义词泛化。
- **`Reminder`**：提醒/约束节点，携带动态注入文本与级别（`Mandatory` 强制 / `Recommended` 推荐 / `Info` 提示）。
- **`Tool`**：工具节点（如 `tool-deepwiki`, `tool-web_search`），包含优先级 `priority`。
- **`Context`**：上下文依赖节点。

### 2.2 边类型 (Edge Types)
- `Triggers`：触发器指向意图。
- `SubIntentOf`：子意图继承父意图。
- `SynonymOf`：同义词映射与动宾泛化。
- `InjectsReminder`：意图激活时注入对应提醒。
- `SuggestsTool`：意图激活时推荐优先使用的工具。
- `RequiresContext`：意图生效需满足的上下文。
- `VetoedBy`：意图被负向条件/冲突排除。
- `ConflictsWith`：规则冲突互斥。

---

## 3. 推理与抗噪能力

### 3.1 Token 匹配（不再用裸 `contains`）
- ASCII 词用词边界（`search` 不会命中 `research` 里的片段；`test` 不会命中 `contest`）。
- 单字 CJK 走**屏蔽复合词**：「查」不在「检查」里开火，「库」不在「仓库 / 数据库」里开火；「查一下」「这个库」「开源库」仍可命中。
- 检索动词在匹配期做家族扩展（`查 = 搜 = 看 = 分析 = search`），学习规则只记录用户原词，避免把同义词写进 trigger。

### 3.2 动宾提取与实体
- 学习路径提取 `(动词, 宾语)` 时**不再默认塞「查看」**；没有动词就退回子串 trigger。
- 拉丁标识符（`tokio` / `reqwest` / `hermes`）视为库实体，写入 `{{library}}` 并可作为外部文档意图的宾语。

### 3.3 概念确认提问抑制 (`is_conceptual_clarification`)
当用户提出概念探讨或确认式问题（如「我可以理解为...吗？」「是不是说...」「你的意思是...」）且未明确表达执行动作时：
- 自动抑制非强制类（`Recommended` / `Info`）的搜索工具提示与探索提醒。
- 仅保留 `Mandatory` 级别的安全守卫。
- 「这个是不是 bug」这类任务句**不会**被当成概念确认。

### 3.4 冲突、冷却与工具可用性
- `ConflictsWith`：两个意图同时激活时保留分数更高（同分比 priority）的一方。
- `cooldown_turns`：同一 reminder 在随后 N 个用户回合内不再注入。
- 已连接 MCP 服务器列表非空（或 MCP 关闭）时，按可用工具过滤建议；`deepwiki` 未连接则不会推荐。
- Plan 模式仍注入 **Mandatory** 安全提醒（如 force-push），但丢掉推荐级工具提示。

### 3.5 元提示与 XML 污染净化 (`strip_xml_and_meta`)
- 过滤 prompt 中包含的 `<system-reminder>`、`<context>`、`<env>` 等系统级 XML 标签，防止自反馈死循环和错误学习。

---

## 4. 运行时与 UI 呈现

### 4.1 JIT `<system-reminder>` 注入
- 运行时在 `crates/one-cli/src/runtime/prompt.rs` 与 `interactive.rs` 中调用 `inject_graph_intent_reminder`。
- 命中意图后，将约束与工具推荐以 `<system-reminder>` 块的形式即时注入模型会话上下文。

### 4.2 TUI 告警卡片 (Alert Card) 与 Toast 通知
- 在 TUI 界面中（`crates/one-tui/src/tool_view.rs` 与 `crates/one-tui/src/ui/chat.rs`）：
  - `Mandatory` 级别：红色边框告警卡片与顶部警示。
  - `Recommended` 级别：黄色边框建议卡片。
  - `Info` 级别：青色边框提示卡片。

---

## 5. 规则学习与管理 (`learn`)

用户可通过 CLI 或交互式 TUI 快速查看和扩充意图图谱：

### CLI 命令
```bash
# 查看图谱统计与节点状态
one learn --status

# 模拟意图推理（不写规则）
one learn --test "帮我查一下 reqwest 的用法"
one learn --test "git push --force"

# 从自然语言或结构化文本学习新规则
one learn "当用户询问架构或源码实现时，建议优先使用 find 和 deepwiki 定位与查阅"
one learn "意图: 数据库迁移 | 触发: 迁移,执行 + 数据库,db | 提醒: 务必先备份数据 | 级别: 强制 | 工具: bash"
```

### 交互式命令
```text
/learn                       # 从当前会话最近的用户提问中提取意图并学习
/learn <规则文本>            # 自然语言或结构化规则
/learn list                  # 列出自定义规则
/learn status                # 查看图谱节点数与自定义规则
/learn reset                 # 清除自定义规则
```

规则持久化保存在 `~/.one/agent/intent_graph/custom.json` 中，支持跨会话热加载与合并。
