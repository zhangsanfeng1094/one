# 分层 Memory 设计

> **状态**：📝 仅设计，**暂不实现**（2026-07-21）  
> **问题**：跨 session 学到的偏好 / 教训 / 项目事实如何持久化，且 **不** 一股脑塞进 context。  
> **核心判断**：**存多少几乎不重要；关键是在正确时机加载正确的 memory。**  
> **相关**：[architecture.md](./architecture.md) §6.3 system prompt · [subagents.md](./subagents.md) · skills progressive disclosure（`one-resources`）

---

## 0. 动机与非目标

### 0.1 已有 vs 缺口

| 能力 | 现状 | 角色 |
|------|------|------|
| Session JSONL + compaction | ✅ | **会话内**工作记忆 |
| `AGENTS.md` / `CLAUDE.md` | ✅ | **人写**的静态项目约定 |
| Skills catalog + `read` body | ✅ | 工作流 **按需** 披露 |
| 跨 session 自动/半自动记忆 | ❌ | **本文设计对象** |

静态 `AGENTS.md` 解决「团队约定」；不解决「上次对话里用户纠正了偏好」「上周踩过的部署坑」——这些不该靠人手工抄进 AGENTS。

### 0.2 设计原则（绑定）

1. **容量不设硬上限**  
   磁盘上 memory 可无限增长；限制的是 **每个 turn 进入模型的工作集**。

2. **默认几乎不加载正文**  
   Session 启动只注入 **索引 / 路由层**；body 默认不在 system prompt。

3. **分层披露（progressive disclosure）**  
   对齐 skills：catalog 常驻 → body 按需 `read`/`grep`。Memory 复用同一范式，**不是**「又一个总是拼进 system 的大 blob」。

4. **索引稳定、内容可变**  
   常驻字节要 **可 prompt-cache**（session 内冻结）；动态召回放在 tool result 或 user 侧 block，**禁止**每 turn 改写 system 前缀。

5. **Memory 是 hint，不是权威**  
   尤其是代码路径 / API 行为：读 body 时应提醒 **point-in-time，使用前 verify**。

### 0.3 非目标（V1 明确不做）

| 非目标 | 原因 |
|--------|------|
| 向量库 / embedding RAG 作为默认路径 | 黑盒、难 debug、易打爆 cache；可后置为可选 MCP |
| Codex 式 6h 双模型离线 pipeline | 运维与模型依赖重；open CLI 首版过重 |
| 强制主 agent 写专用 memory API | 可用普通 write/edit + 约定；专用 tool 可选 |
| 跨用户 / 团队共享生成记忆 | 首版 per-user 本地；团队约定仍走 git 里的 AGENTS |
| Subagent 继承父级全量 memory | 过拟合旧偏好；见 §5 |

---

## 1. 两层概念：存储 vs 工作集

```text
存储层（disk，可任意大）          工作集（context，严格预算）
─────────────────────            ────────────────────────
全部 memory 文件                   本 turn 模型实际看见的子集
session 摘要档案                   = 索引 ∪ 本任务相关 body
旧 rollout 摘录                    ≠ 存储全集
```

**产品问题**不是「memory 放多少」，而是 **工作集选择（working-set selection）**：

> 正确的 memory = 当前任务 × 当前 scope（cwd/项目）× 当前意图。

选错比存少更糟：噪声稀释注意力，陈旧事实压过当前任务。

---

## 2. 五层模型（L0–L4）

从「总是在」到「几乎从不自动进」：

```text
L0  契约层          always · tiny
L1  项目静态层      session boot · bounded
L2  Memory 路由层   session boot · 极短索引
L3  任务工作集      按需 · 本任务相关 body
L4  深档案          几乎只 grep/read · 任意大
```

### 2.1 各层定义

| 层 | 内容 | 何时进入 context | 体量策略 | 谁决定加载 |
|----|------|------------------|----------|------------|
| **L0** | 默认 system、tool/安全 policy、「如何使用 memory」的短纪律 | 每 session 固定 | 很小 | 运行时 |
| **L1** | `AGENTS.md` / `CLAUDE.md`（已有） | boot 合并 | 中等，**有 cap / 截断** | 路径发现 |
| **L2** | Memory **索引**（id / type / scope / tags / 一句 description） | boot 注入；**session 内冻结** | 极短（行数或 token 上限） | 运行时 |
| **L3** | 相关 memory **正文**、可选：本 session 相关决策摘要 | turn 中按需 | 中；**每 turn budget** | 模型或轻量检索 |
| **L4** | 长 reference、旧 session 摘要、失败 postmortem | 需要证据时 | 任意大；**永不整页自动注入** | 模型 `grep`/`read` |

**L2 是地图，不是行李箱。**  
地图告诉 agent「有什么、去哪找」；行李箱（L3/L4）只在用到时打开。

### 2.2 与现有拼装的关系

当前 system 拼装（见 architecture §6.3）：

```text
DEFAULT_SYSTEM_PROMPT
  + AGENTS.md / CLAUDE.md          →  L1
  + skills catalog XML             →  范式同 L2
  + plugin / extension overlays
  + plan / task hints
```

落地后 **增量** 为：

```text
…现有…
  + memory catalog（L2，session 冻结）
  + （可选）L0 内短纪律：「先看索引再 read body；代码类须 verify」

turn 中：
  read / grep memory 路径          →  L3 / L4
  （可选）memory_search → 只返回索引行，不返回全文
```

**不**把 L3/L4 正文拼进 system。

### 2.3 对照 skills（复用范式）

| Skills（已有） | Memory（设计） |
|----------------|----------------|
| catalog：name + description | L2：id + type + scope + tags + desc |
| body：`read SKILL.md` | L3：`read` memory 文件 |
| 禁用/开关 | 可按 type/scope 过滤是否进 L2 |
| `/skill:name` 强制加载 | 可选 `/memory:id` 或用户 @ 引用 |

实现时应优先 **扩展 `one-resources` 披露模式**，避免第二套互不兼容的 prompt 机制。

---

## 3. 加载策略：正确时机

### 3.1 门控流水线

每个用户 turn（或新任务边界）可概念化为：

```text
用户消息 / 任务
      │
      ▼
① Scope gate      全局 vs 本 repo vs 本子目录？
      │
      ▼
② Intent gate     写代码 / 文档 / 调试 / 偏好类 / 无关？
      │
      ▼
③ Relevance       L2 索引中哪些 description/tag 命中？
      │
      ▼
④ Budget gate     本 turn 还剩多少 token？优先 L3，限制 L4
      │
      ▼
⑤ Inject site
   · L2 索引 → system（session 冻结）
   · L3/L4 body → tool result 或 user 侧 <memory-context>
```

### 3.2 三种触发（由严到松）

| 级别 | 含义 | 示例 | 默认动作 |
|------|------|------|----------|
| **T1 强制相关** | 用户或任务明确指向记忆 | 「按我上次说的」「别用连字符」；索引 `feedback` 命中当前动作类型 | 可自动把 **1～少量** L3 送入（仍受 budget） |
| **T2 任务相关** | 索引暗示与当前任务相关 | 在做 OAuth，有 `project_oauth` | **模型决定** `read` 0～N 个 body |
| **T3 证据相关** | 需要精确命令/路径/历史 | 「上次那个报错怎么修的」 | 只 `grep` L4 → 再 `read` 1～2 文件；**禁止扫全库** |

**默认产品策略：T2 为主，T1 为辅，T3 严格限步数**  
（建议：memory 查找步数上限，例如 ≤4–6 次再进入主工作，可配置。）

### 3.3 时机表（产品行为）

| 时机 | 加载 | 不加载 |
|------|------|--------|
| Session 启动 | L0 + L1 + **L2 索引** | 任何 memory body |
| 用户下达任务 | 可选 T1 命中的少量 L3 | 全库 |
| Agent 判断相关 | `read` 1～3 个 L3 | 扫 L4 |
| 需要精确证据 | L4 grep → 再 read | 用摘要冒充权威事实 |
| Compaction 后 | 会话摘要照旧；**不**重灌 memory 库 | — |
| Subagent / harness 子 run | 默认 **无 L2**，或仅 task 相关 1 条 | 父 agent 全量记忆 |
| `/new` 新 session | 重新读盘生成 L2；**可**包含上 session 新写入 | 旧 session 对话全文 |

### 3.4 注入位置与 prompt cache

| 内容 | 注入位置 | Session 内是否可变 |
|------|----------|--------------------|
| L2 索引 | system | **否**（boot 冻结；本 session 新写入的索引变更下 session 再生效） |
| L3/L4 正文 | tool result，或 user 侧 `<memory-context>` | 可 |
| 召回结果 | **禁止**写回 system 前缀 | — |

原则：**动态数据不进 system**。与 Hermes「snapshot 冻结」、Claude「索引 boot 加载」一致；避免每 turn 改 system 导致 cache miss。

读 L3/L4 body 时，runtime 宜在 tool 结果外包一层提醒（实现时）：

```text
This memory is N days old (written at …). Point-in-time observation.
Verify against current code/config before asserting as fact.
```

---

## 4. 存储布局（服务寻址，不限总量）

路径为建议形状；实现时可微调，但 **必须支持 scope 分层与索引/正文分离**。

```text
~/.one/agent/memory/
  _global/
    MEMORY.md                 # L2 索引（一行一条）
    feedback_*.md             # L3 body
    user_*.md
  projects/<cwd-hash>/
    MEMORY.md                 # 项目 L2
    project_*.md
    reference_*.md            # 偏 L4：可很长
  sessions/                   # 可选 L4 档案
    <date>-<id>.md            # session 摘要，供检索，不自动注入
```

### 4.1 索引行契约（Catalog schema）

L2 每一行（或 XML 一条）**至少**包含：

| 字段 | 含义 |
|------|------|
| `id` | 稳定标识（= 文件 stem 或显式 id） |
| `type` | `user` \| `feedback` \| `project` \| `reference` \| … |
| `scope` | `global` \| `project`（及可选 path 前缀） |
| `tags` | 逗号分隔，供 intent/relevance |
| `description` | **一句**话，仅用于是否打开 body |

示例（markdown 列表）：

```markdown
# Memory index (do not treat as full instructions; read bodies on demand)

- [feedback_no_hyphens] type=feedback scope=global tags=writing,style
  Never use hyphens in written content
- [project_oauth] type=project scope=project tags=auth,oauth
  Staging OAuth uses device code; do not use client_secret flow
- [ref_gateway] type=reference scope=project tags=routing,ops
  Gateway capacity portal + request form (verify paths before citing)
```

**索引禁止**夹带可执行的长指令正文；长内容必须落在独立 body 文件。

### 4.2 Body 文件（L3/L4）

建议 frontmatter（松约束，prompt 约定即可；校验器可选）：

```markdown
---
name: No hyphens in writing
type: feedback
scope: global
tags: [writing, style]
updated: 2026-07-21
---

Never use hyphens in written content (emails, documents, messages).

**Why:** User preference.

**How to apply:** Prefer alternative phrasing; avoid em dashes in drafts.
```

| type | 典型用途 | 默认层级 |
|------|----------|----------|
| `feedback` | 行为纠正、输出风格 | L3 |
| `user` | 稳定画像（少写） | L3，极少变 |
| `project` | 项目事实、约定补充 | L3 |
| `reference` | 长技术笔记 | L4 倾向 |

### 4.3 Scope 叠加（分层，不是单桶）

```text
Session boot in <cwd>:
  1. Load L2 from memory/_global/
  2. Overlay L2 from memory/projects/<hash(cwd or git root)>/
  3. 冲突时：project 覆盖 global（同 id）；或索引并列、由模型按 scope 选择
```

与 AGENTS 向上发现 **正交**：

- **AGENTS/CLAUDE**：人维护、可进仓库、团队共享  
- **Memory**：本机用户生成、默认可不进 git  

二者都进 L1/L2 工作流，但 **写入方与信任模型不同**。

### 4.4 写路径（设计倾向，非实现承诺）

| 方案 | 说明 | V1 倾向 |
|------|------|---------|
| **A. 主 agent 同步写** | 用 write/edit 写 body + 更新 MEMORY.md；用户可见 | **首选**（简单、可监督） |
| **B. 专用 memory tool** | 强制 schema / 防重复 | 可选增强 |
| **C. 异步 extract/consolidate** | Codex 双阶段 | **后置**，非默认 |

**写时纪律（进 L0 短提示）**：

- 不记 trivial / 仅单次任务的纠正  
- 不记 codebase 或 AGENTS 已明显写明的事实  
- 不记易变临时状态（live metrics）  
- 先 grep 索引，**更新**优于新建重复条  
- 默认倾向 **不写**（NO-OP）；只在未来 agent 会明显更好时写  

**不在 V1 强制**：usage 衰减、自动 prune。磁盘不限；工作集用 L2 行数/token cap + L3 budget 控制。可选后续：索引行 LRU、手动 `/memory prune`。

---

## 5. Subagent / Harness 边界

与 [subagents.md](./subagents.md) 一致：子 run 是 **窄任务 worker**。

| 角色 | Memory 策略 |
|------|-------------|
| 主 interactive agent | L0–L2 完整；L3/L4 按需 |
| `explore` 等只读 preset | **默认不加载 L2**；任务 prompt 自带所需事实 |
| 可写 general + worktree | 默认不加载；或 `AgentSpec.resources.memory: off \| index \| …` |
| 父 → 子 | **禁止**隐式继承父全量 memory；需要时由父 **显式** 写入子 prompt 的几句 |

避免：子 agent 带着全局 `feedback_*` 去「自由发挥」，偏离任务契约。

---

## 6. 与外部实现的关系（取舍）

| 来源 | 采纳 | 不采纳（V1） |
|------|------|----------------|
| **Claude Code** | 索引 always + body on demand；typed files；读时 age/verify；同步可监督写入 | 闭源路径细节 |
| **Skills in one** | catalog / progressive disclosure | — |
| **Hermes** | session 内 system 冻结护 cache | 过紧的 char 硬顶作为唯一闸门 |
| **Codex** | NO-OP 写门控；summary + lazy handbook 思想 | 6h 双模型 pipeline、强制 citation 衰减 |
| **Cursor Rules** | 静态规则分层（always/auto/manual）启发 intent gate | 社区 Memory Bank 整包灌入 |
| **Mem0 等 MCP** | 可作为 **可选 L4 后端** | 不作默认核心依赖 |

---

## 7. 配置面（实现时预留，现不落地）

示意 `settings.json`（**非**当前 schema）：

```json
{
  "memory": {
    "enabled": true,
    "load": "index",
    "index_max_lines": 200,
    "body_budget_tokens": 2000,
    "max_lookups_per_turn": 6,
    "scopes": ["global", "project"],
    "write": "agent",
    "subagent": "off"
  }
}
```

| 值 | 含义 |
|----|------|
| `load: off` | 不注入 L2 |
| `load: index` | 仅 L2（默认） |
| `load: off` + 用户 @memory | 纯手动 |
| `subagent: off \| index` | 子 run 默认 off |

CLI 示意：`--no-memory` / 未来 `/memory` 列表与编辑——**仅文档预留**。

---

## 8. 实现分期（备忘，非当前任务）

> 本文 **不要求** 开工。下列仅供日后拆 PR 时参考。

| 阶段 | 内容 | 依赖 |
|------|------|------|
| **M0** | 文档 + 目录约定 + L0 纪律文案定稿 | — |
| **M1** | 读路径：发现 `_global` + project 索引 → 注入 L2；session 冻结 | `prompt_compose` / `one-resources` |
| **M2** | 写路径：约定 + 可选校验；agent 可 write body/index | tools 已有 |
| **M3** | tool 结果 age 包装；lookup 步数/budget | runtime |
| **M4** | `AgentSpec.resources.memory`；subagent 默认 off | protocol |
| **M5** | 可选：`memory_search`、session 摘要进 L4、MCP 外挂 | — |

验收直觉（M1+）：

1. 空 memory 目录时行为与今天一致（仅 L0+L1+skills）。  
2. 有 100+ body 时，system **只**见索引，token 不随 body 数线性涨。  
3. 相关任务下 agent 能 `read` 正确 body；无关任务不主动扫 L4。  
4. 子 agent 默认看不到父级 memory 索引。

---

## 9. 决策摘要

| 议题 | 决策 |
|------|------|
| 存储容量 | **不硬限** |
| 默认进 context 的内容 | **仅 L2 索引**（+ 已有 L0/L1） |
| Body 加载 | **按需**，T2 为主 |
| 分层 | L0 契约 · L1 静态 AGENTS · L2 路由 · L3 工作集 · L4 档案 |
| 注入位置 | L2→system 冻结；L3/L4→tool/user，不进 system |
| 写 | 倾向主 agent 同步；强 NO-OP 纪律 |
| 子 agent | 默认不带 memory |
| 向量 RAG | 非默认 |
| 实现 | **暂缓**；本文为设计真源 |

---

## 10. 文档索引

| 文档 | 关系 |
|------|------|
| [architecture.md](./architecture.md) | 状态矩阵 · system 拼装 · 本文入口 |
| [subagents.md](./subagents.md) | 子 run 不继承全量 memory |
| [protocol.md](./protocol.md) | 日后 `AgentSpec.resources.memory` |
| [roadmap.md](./roadmap.md) | 待办勾选（实现启动时再勾） |
| `one-resources` skills | progressive disclosure 参考实现 |

---

## 11. 修订记录

| 日期 | 说明 |
|------|------|
| 2026-07-21 | 初稿：分层 L0–L4、工作集选择、加载门控、存储与非目标；明确暂不实现 |
