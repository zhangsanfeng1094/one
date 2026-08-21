# 意图识别与工具意图匹配进度

> **状态**：🟨 **P0 已完成，P1–P3 待规划实施**  
> **更新时间**：2026-08-21  
> **适用范围**：One 的跨 session memory、`tool_intent` 规则与运行时工具建议注入

本文记录 One 当前的意图识别能力、最近完成的改动、已知边界，以及后续演进计划。

---

## 1. 结论摘要

One 当前**没有独立的通用 intent classifier 或前置 router**。它不会先把用户请求分类为「问答 / 编码 / 调试 / 审查 / 规划」再交给不同执行器。

当前实际采用的是两层机制：

1. **主 Agent loop 由模型结合 system prompt、当前模式和可用工具自行判断任务意图**。
2. **Memory 中的 `tool_intent` 规则提供工具选择偏好建议**，帮助模型决定是否优先查询某个工具或 MCP 服务。

因此，当前能力更准确的名称是：

> **基于 memory 的工具意图偏好匹配（tool-intent preference matching）**，而不是完整的通用用户意图识别系统。

`Plan / Act` 是显式运行模式，不是通用意图分类器。Plan 模式负责约束执行流程和审批边界，不能替代 intent classification。

---

## 2. 当前已完成的能力

### 2.1 Memory 规则匹配

运行时会从全局和项目级 `MEMORY.md` 加载 `tool_intent`、`intent`、`tool` 类型，或带有相关标签的 memory 条目，并针对当前用户请求计算匹配结果。

当前支持的匹配信号包括：

| 信号 | 作用 |
|---|---|
| `triggers` | 显式触发短语，命中时给予较高权重 |
| `negative_triggers` | 负向触发短语，命中后直接排除规则 |
| `tags` | 通过标签匹配任务上下文 |
| `id` | 通过规则 id 中的关键词匹配 |
| `description` | 通过索引描述中的关键词匹配 |
| `priority` | 在规则已经匹配成功后增加排序分数 |

规则 body 使用轻量 frontmatter 保存额外元数据，例如：

```markdown
---
name: DeepWiki Tool Intent
type: tool_intent
scope: global
tags: [docs, library, opensource]
updated: 2026-08-21
triggers: ["开源库", "第三方库", "opensource library"]
negative_triggers: ["当前项目源码"]
priority: 5
---

当用户询问第三方开源库的 API、配置、源码或文档时，优先查询 DeepWiki。
```

### 2.2 匹配结果包含证据和置信度

匹配结果 `ToolIntentHit` 现在包含：

- `score`：综合匹配分数；
- `confidence`：归一化置信度，供运行时展示；
- `evidence`：具体命中证据，例如 `trigger:第三方库`、`tag:文档`；
- `entry`：原始 memory 索引条目；
- `body_excerpt`：按需读取的简短策略摘要。

这使工具建议不再是不可解释的「命中 / 不命中」，而是可以向模型说明命中的原因。

### 2.3 负向规则和安全边界

负向触发词在普通评分前处理。例如，一条规则可能建议查询外部开源库文档，但对「查看当前项目源码」设置排除条件。这样可以减少宽泛规则错误覆盖当前任务的情况。

运行时注入的文本明确声明：

- 工具意图只是**可选建议**，不是强制路由；
- 用户的显式指令优先；
- 当前模式和权限 / 安全策略优先；
- 如果建议与用户请求冲突，不应调用推荐工具。

### 2.4 `memory_write` 支持规则元数据

专用 `memory_write` 工具已经扩展以下参数：

```json
{
  "triggers": ["..."],
  "negative_triggers": ["..."],
  "priority": 5
}
```

写入 `tool_intent` 条目时，运行时会生成对应 frontmatter，并继续原子更新 body 和 `MEMORY.md` 索引。

### 2.5 旧规则兼容

没有新 frontmatter 元数据的历史规则仍然可以依靠原来的 `id`、`tags` 和 `description` 进行匹配。

新增机制不会要求用户立即迁移已有 memory，也不会因为缺少 `triggers` 字段而让旧规则全部失效。

### 2.6 验证状态

当前相关验证均已通过：

```text
cargo test -p one-resources memory --lib
13 passed; 0 failed

cargo test -p one-cli --bin one runtime::memory_write_tool
2 passed; 0 failed

cargo check -p one-cli --no-default-features
Finished successfully
```

同时已运行 Rust 格式化和 `git diff --check`。

---

## 3. 关键实现位置

| 文件 | 职责 |
|---|---|
| `crates/one-resources/src/memory.rs` | Memory 索引解析、body frontmatter、工具意图评分和匹配结果 |
| `crates/one-resources/src/lib.rs` | 导出 `match_tool_intent_rules`、`ToolIntentHit` 等 API |
| `crates/one-cli/src/runtime/memory_write_tool.rs` | `memory_write` schema、参数解析和原子写入调用 |
| `crates/one-cli/src/runtime/tools.rs` | 在 Agent 回合前注入工具意图建议和匹配证据 |
| `crates/one-tools/src/memory_search.rs` | Memory 搜索工具 |
| `crates/one-tools/src/memory_write.rs` | Memory 写入工具 |
| `crates/one-cli/src/runtime/features.rs` | memory feature 和工具能力门控 |
| `crates/one-cli/src/runtime/plan.rs` | Plan / Act 模式，不承担通用意图分类 |
| `docs/memory.md` | 分层 Memory 的总体设计、边界和阶段状态 |

> 注：`one-tools` 中 memory 工具的具体路径应以当前代码树为准；如果工具仍由 `one-cli/src/runtime/` 装配，则以运行时实现为事实来源。

---

## 4. 当前工作流

```text
用户请求
   │
   ├─ 读取 session 内固定 system prompt、模式和工具能力
   │
   ├─ 从当前 session 已加载的 memory catalog 中筛选 tool_intent 规则
   │
   ├─ 读取匹配规则 body 的 frontmatter
   │
   ├─ 检查 negative_triggers
   │
   ├─ 计算 triggers / tags / id / description / priority 分数
   │
   ├─ 生成 confidence + evidence
   │
   ├─ 注入“可选工具建议”提醒
   │
   └─ 由模型结合用户指令和安全策略决定是否调用工具
```

这个流程是**建议式路由**，不是硬路由：匹配器不会直接调用 MCP，也不会绕过权限门控。

---

## 5. 已知边界和风险

### 5.1 不是通用意图分类器

目前没有独立的意图类别体系，也没有稳定的分类输出，例如：

```text
coding / debugging / review / planning / research / question
```

如果未来需要这些能力，应单独设计分类 schema、置信度和失败回退策略，而不是继续把所有逻辑堆到 `tool_intent` memory 中。

### 5.2 当前匹配主要是词法匹配

匹配器主要使用大小写归一化后的短语包含和关键词命中：

- 中文分词能力有限；
- 同义词、改写和隐含意图可能无法命中；
- 英文形态变化、缩写和拼写变体没有专门处理；
- 复杂否定、转折和长上下文的语义关系仍由模型判断。

### 5.3 `confidence` 是启发式指标

当前置信度由匹配分数归一化得到，不是经过标注数据校准的概率。

因此它适合：

- 展示匹配强弱；
- 进行简单排序或阈值过滤；
- 调试规则质量；

不适合直接作为安全决策、自动执行或统计意义上的概率。

### 5.4 规则冲突处理仍较简单

当前主要使用：

- 负向触发词排除；
- 分数排序；
- priority 作为附加分；
- 限制返回数量。

还没有专门的规则冲突解释、互斥组、覆盖关系或人工确认流程。

### 5.5 不会自动从每轮对话抽取意图规则

当前仍然坚持 memory 写入的 NO-OP 原则：只有未来 Agent 明显受益的稳定偏好、项目事实或工具偏好才应该写入 memory。

没有默认启用的：

- 每轮对话自动抽取；
- 后台记忆 consolidation；
- embedding / vector RAG；
- 跨用户共享规则。

---

## 6. 未来路线图

### P1：提高规则匹配质量

目标：在不引入重量级模型依赖的情况下，让现有工具意图规则更稳定、更可解释。

计划包括：

1. **补充匹配单元测试**
   - 触发词命中；
   - 负向触发词优先级；
   - legacy 规则兼容；
   - 多条规则排序；
   - priority 不得单独触发规则；
   - 中英文和标点变体。

2. **改进短语归一化**
   - Unicode 大小写和空白归一化；
   - 中英文标点统一；
   - 连字符、下划线和空格变体；
   - 安全处理 frontmatter 中的引号和反斜杠。

3. **增加规则质量诊断**
   - 记录未命中的原因；
   - 检测过宽的 tag / description；
   - 检测重复 triggers 和永远不会命中的 negative triggers；
   - 在调试模式下显示候选规则和淘汰原因。

4. **完善冲突策略**
   - 支持规则互斥组；
   - 区分“推荐工具”和“禁止工具”；
   - 对高冲突结果要求模型先澄清，而不是直接推荐工具。

### P2：建立轻量通用意图层

目标：增加可选的、可解释的通用任务分类，而不改变当前 Agent loop 的最终决策权。

候选类别：

```text
question / coding / debugging / review / planning /
research / documentation / configuration / conversation
```

设计要求：

1. 分类结果必须是**建议**，不能绕过权限或用户指令；
2. 需要明确的 `unknown` / `mixed` / `ambiguous` 回退类别；
3. 混合请求必须允许多标签，而不是强制单分类；
4. 分类失败时回退到现有 LLM 主循环；
5. 不应为了分类额外调用昂贵模型；
6. 需要将分类结果与 Plan / Act、工具 profile 和子 Agent policy 解耦。

可能的实现顺序：

```text
规则 / slash command 显式信号
        ↓
轻量词法与模式匹配
        ↓
可选本地或同一 LLM 的结构化分类
        ↓
候选工具 / 工作模式建议
        ↓
主 Agent loop 最终决策
```

### P3：评测和自适应改进

目标：用可重复数据验证意图匹配是否真正改善了工具选择，而不是只增加 prompt 噪声。

计划包括：

- 构造包含中文、英文、混合请求和否定语境的 intent benchmark；
- 统计 precision、recall、false positive、false negative；
- 统计推荐工具被接受、拒绝和纠正的比例；
- 使用 trace / harness 对比“启用 memory intent”和“关闭 memory intent”；
- 增加规则版本、来源和更新时间，便于回溯；
- 对用户明确纠正进行受控反馈，而不是自动把每次纠正写成长期规则；
- 在足够数据后再评估 embedding 或小型分类器，避免过早引入复杂基础设施。

---

## 7. 推荐的下一步拆分

如果继续开发，建议按以下顺序拆分 PR：

| PR | 内容 | 风险 |
|---|---|---|
| PR-1 | 补充匹配和排序测试，固定现有行为 | 低 |
| PR-2 | 统一短语归一化和 frontmatter 解析 | 中 |
| PR-3 | 调试输出与规则质量诊断 | 低 |
| PR-4 | 规则冲突 / 互斥组模型 | 中 |
| PR-5 | 轻量通用意图 schema，默认关闭 | 中高 |
| PR-6 | harness benchmark 和 trace 指标 | 中 |
| PR-7 | 根据评测结果决定是否引入语义检索或分类器 | 高，暂不提前承诺 |

在 P2 之前，不建议实现“自动把每条用户消息分类并强制选择工具”的硬路由器。当前建议式机制更容易调试，也更符合 One 现有的安全门控和 Agent loop 设计。

---

## 8. 相关文档

- [分层 Memory 设计](./memory.md)：L0–L4、索引 / body 分离、写入纪律和 M1–M6 实现状态
- [系统架构](./architecture.md)：system prompt、运行时装配和能力边界
- [Subagent 设计](./subagents.md)：子 Agent 的资源和 memory 隔离策略
- [Harness 评测](./harness-eval.md)：trace、benchmark 和能力对比
- [路线图](./roadmap.md)：项目级后续计划

---

## 9. 修订记录

| 日期 | 说明 |
|---|---|
| 2026-08-21 | 记录 P0 工具意图匹配增强：triggers、negative_triggers、priority、evidence、confidence，以及 P1–P3 路线图 |
