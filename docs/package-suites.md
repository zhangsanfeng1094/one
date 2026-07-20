# Package / Suite 设计（草案）

> **状态**：设计文档，**暂不实现**。  
> **目标**：配置「编程套件」即可编程，配置「办公套件」即可办公；底层 Agent 逻辑保持干净、通用。  
> **相关**：[architecture.md](./architecture.md) · [extensions.md](./extensions.md) · [roadmap.md](./roadmap.md)

---

## 1. 动机

One 今天把 **coding tools 装配、skills 发现、权限预设**  partially 写在 CLI/runtime 里。领域一变（办公、研究、运维），容易出现：

- Core / CLI 里 `if coding { ... }` 式分支；
- 新领域只能改源码，不能「换包」；
- 扩展（`one-ext`）、资源（skills/prompts）、工具列表三条线各自为政。

期望的产品形态：

```text
one --suite coding    →  软件工程工作流
one --suite office    →  文档/表格等工作流
one                   →  默认启用配置好的套件（建议默认 coding，兼容现状）
```

**口号**：

> **One Core = 通用 Agent 操作系统。**  
> **Package = 领域发行版（工具 + 策略 + 技能 + 可选扩展）。**  
> **用户只选发行版；底层从不按领域名分支。**

---

## 2. 非目标（本设计阶段）

- 不实现 package 安装器、应用商店、签名验证。
- 不引入第二套与 `Extension` 平行的「Plugin」运行时。
- **MCP 不由 Package 实现**：MCP 是平台基础能力（见 [mcp.md](./mcp.md)）；Package 最多 **过滤** 已连接 server/tools。不在 package 里塞协议客户端。
- 不把子 Agent orchestrator 做进 Core（子 agent 由 CLI 装配；见 [subagents.md](./subagents.md)，非 Package 职责）。
- 不要求 TypeScript 扩展兼容。
- 不做「全流程任意 hook」的巨型插件框架；只定义 **有限、稳定的装配面**。

---

## 3. 分层模型

```text
┌─────────────────────────────────────────────┐
│  L3  Package / Suite（领域发行版）            │
│  coding · office · research · …             │
│  package.toml + profile + skills + overlays │
└────────────────────┬────────────────────────┘
                     │ 声明式 merge
┌────────────────────▼────────────────────────┐
│  L2  Resources（弱代码 / 无代码）              │
│  Skills · Prompt templates · AGENTS 片段     │
└────────────────────┬────────────────────────┘
                     │
┌────────────────────▼────────────────────────┐
│  L1  Runtime hooks（代码扩展，可选）           │
│  one-ext：tools / commands / before_tool …  │
└────────────────────┬────────────────────────┘
                     │
┌────────────────────▼────────────────────────┐
│  L0  Core harness（通用、领域无关）            │
│  loop · session · Tool/Provider trait        │
│  policy 管道 · truncate/spill · compaction   │
└─────────────────────────────────────────────┘
```

| 层 | 职责 | 禁止 |
|----|------|------|
| **L0 Core** | 消息循环、session 树、统一 tool 执行与权限管道 | `if suite == "office"`、领域专用逻辑 |
| **L1 Extension** | 注册 tool、生命周期钩子、自定义命令 | 替换整个 agent loop（除非未来显式 API） |
| **L2 Resources** | 教模型怎么做、progressive disclosure | 直接执行任意系统调用（靠 tool） |
| **L3 Package** | 把 L1+L2+profile **打成可切换的发行版** | 实现第二套 runtime |

**原则**：能用 Skill/配置解决的，不做 Extension；能用 Extension 解决的，不改 Core。

---

## 4. 核心概念

### 4.1 Package（包）

磁盘上的一个目录 + `package.toml`（名称可议，实现时锁定），描述：

- 启用/禁用哪些 **tool id**；
- sandbox / 权限预设；
- 额外的 skills、prompts、system overlay 路径；
- 可选 extensions 加载列表；
- 对其他 package 的依赖（`requires`）。

### 4.2 Suite（套件）

面向用户的 **工作流别名**，通常对应一个顶层 package（或一组 package 的预设集合）。

- CLI：`one --suite coding`
- 配置：`default_suite = "coding"` 或 `enabled_packages = ["coding"]`

实现上 Suite 可以只是 package 的产品名；文档里两者可互换，schema 以 **package** 为准。

### 4.3 Profile（运行配置）

一次 session 生效的 **装配结果**（merge 之后的扁平配置）：

- tool allowlist / denylist
- `PathPolicy` / sandbox 模式
- permission ruleset
- system overlays（有序列表）
- resource 搜索根
- 已加载 extension 句柄

Package 是「源」；Profile 是「运行时视图」。

### 4.4 Tool id

内置与扩展工具统一用稳定字符串 id（如 `read`、`bash`、`docx_edit`）。  
Package **只引用 id**，不内嵌 tool 实现（实现来自 Core 内置注册表或 Extension）。

### 4.5 CLI Bridge（可选能力，非 MVP 必做）

Package 可声明「外部命令 → Tool」映射，由 Core 的通用 bridge 生成 `Tool`，避免每个领域都写 Rust。  
适合办公场景的 `pandoc`、脚本等。MVP 可只保留设计位，不实现。

---

## 5. 目录约定（草案）

### 5.1 发现路径（优先级高 → 低，同名策略见 §7）

| 范围 | 路径（示意） |
|------|----------------|
| 项目 | `<cwd>/.one/packages/<name>/` |
| 用户 | `~/.one/agent/packages/<name>/` |
| 内置 | 随二进制或 `~/.one/agent/builtin-packages/<name>/` |

与现有 skills 发现类似，**项目优先于用户，用户优先于内置**（实现时与 `one-resources` 对齐写死）。

### 5.2 包内布局

```text
coding/
  package.toml           # 元数据 + 装配入口
  profile.toml           # 可选：从 package.toml 拆出的 profile 段
  system.md              # system prompt overlay
  AGENTS.fragment.md     # 可选：追加到 context 的说明片段
  skills/                # Agent Skills（SKILL.md）
  prompts/               # /template 类提示
  extensions/            # 可选：动态库或日后 wasm
  tools/                 # 可选：CLI bridge 描述（未来）
  README.md              # 人读说明
```

---

## 6. Manifest 草案（`package.toml`）

字段名为示意，实现前可微调，但语义应保持。

```toml
[package]
name = "coding"
version = "0.1.0"
description = "Software engineering suite"
# 依赖其他 package，按拓扑序加载
requires = ["base"]

[profile]
# Core / 扩展已注册的 tool id
tools = [
  "read", "write", "edit", "bash",
  "bash_output", "bash_kill",
  "grep", "find", "ls", "ask_user",
]
# 即使其他包启用了，也强制关掉
disable_tools = []
# 与现有 SandboxMode / PathPolicy 对齐的策略名
sandbox = "workspace-write"          # read-only | workspace-write | full-access | …
# 权限规则预设：内置名或包内相对路径
permission_preset = "coding-dev"
# 可选：额外可写/可读根（办公包可指向 Documents）
# readable_roots = []
# writable_roots = []

[resources]
skills = ["skills"]                  # 相对包根的目录
prompts = ["prompts"]
system_overlay = "system.md"
agents_append = "AGENTS.fragment.md" # 可选

[extensions]
# 可选；MVP 可不支持加载，仅预留
load = []
# load = ["extensions/example.so"]

[ui]
welcome = "Coding suite loaded."     # 可选；TUI/print 提示，Core 不分支逻辑

# ----- 未来：CLI Bridge（非 MVP）-----
# [[tools.cli]]
# name = "convert_doc"
# description = "Convert documents with pandoc"
# command = ["pandoc", "{{input}}", "-o", "{{output}}"]
```

办公包对比示意：

```toml
[package]
name = "office"
version = "0.1.0"
requires = ["base"]

[profile]
tools = ["read", "write", "ask_user", "web_fetch"]
disable_tools = ["bash"]             # 或由更严 permission 限制 bash
sandbox = "documents-write"          # 需 Core 支持命名策略或 roots 配置
permission_preset = "office-safe"

[resources]
skills = ["skills"]
system_overlay = "system.md"

[extensions]
load = []                            # 日后：docx/sheet 等扩展 tool
```

`base` 包示意（最小可运行）：

```toml
[package]
name = "base"
version = "0.1.0"
description = "Minimal chat harness"

[profile]
tools = ["ask_user"]
sandbox = "read-only"
```

---

## 7. Merge 规则（多包叠加）

**默认模型：多 package 可同时启用，按依赖序 merge 成一个 Profile。**

| 维度 | 规则 |
|------|------|
| **加载顺序** | `requires` 拓扑序；同层按启用列表顺序 |
| **tools** | **并集**；之后应用所有包的 `disable_tools`（禁用优先） |
| **system overlays** | 按加载序 **append**（base → … → 顶层 suite） |
| **skills / prompts 根** | **并集** 加入 ResourceLoader 搜索路径 |
| **sandbox / 写权限** | **取更严**（例如 read-only > workspace-write > full-access） |
| **permission rules** | 合并列表；**deny 优先于 allow**（与现有 PermissionRules 精神一致） |
| **同名 tool id** | **启动失败**（报错指出冲突包），避免静默覆盖 |
| **同名 skill** | 与现有 skills 发现一致：**先发现者胜出**（路径优先级已定时） |
| **extensions** | 按序 `on_load`；tool 注册进入同一 ToolRegistry |

### 7.1 `--suite` 语义（产品）

推荐：

- `enabled_packages`：用户长期启用的包列表（可多选叠加）。
- `--suite <name>`：**以该 suite 为主配置会话**——至少启用 `name` 及其 `requires`；是否 **暂时忽略** 其他已启用包，由实现选择其一并写进 CLI 文档：
  - **选项 A（推荐 MVP）**：`--suite X` = 仅 `X + requires`（会话级隔离，好理解）；
  - **选项 B**：`--suite X` = 在已启用列表上确保包含 X（叠加）。

文档定案倾向 **选项 A**，减少「办公会话误带 coding bash」的意外。

### 7.2 默认启用

- 为兼容当前 One 用户：未配置时默认等价 **`coding`（或内置 coding profile）**。
- 架构上 `coding` 只是一个 package，不是 Core 特例。

---

## 8. Core 必须暴露的插槽（实现清单）

领域无关的最小接口：

1. **ToolRegistry**  
   - 按 id 注册内置 tool；扩展可 `register`；profile 做 allow/deny 过滤。

2. **Profile 构建器**  
   - 输入：启用 package 列表 + 全局 settings；输出：扁平 `RuntimeProfile`。

3. **ResourceRoots**  
   - 将 package 内 skills/prompts/agents 片段路径并入现有 `one-resources` 发现。

4. **SystemOverlay[]**  
   - 有序字符串，拼进 system prompt（在默认 prompt 与 AGENTS.md 之间或之后，实现时固定一处）。

5. **Policy 管道（已有，包只选预设）**  
   ```text
   tool_call
     → extension before_tool（未来）
     → PermissionRules
     → PathPolicy / OsSandbox
     → execute
     → truncate / spill
     → extension after_tool（未来）
   ```

6. **Session 元数据**  
   - header 或首条 meta 记录：`packages: [{name, version}, …]`，保证可复现与分享。

7. **Extension 加载点（可晚于 MVP）**  
   - 沿用 [extensions.md](./extensions.md) 的 `Extension` trait，由 package `extensions.load` 触发。

**明确不开放（初期）**：替换 agent loop、任意改写 session 树、完整自定义 TUI 组件树。

---

## 9. 与现有代码的映射（不实现，仅对齐）

| 现状 | 将来归属 |
|------|----------|
| `coding_tools` / `read_only_tools` / `plan_mode_tools` | 命名 profile / package 的 `tools` 列表 + mode 覆盖 |
| `one-resources` skills/prompts | package `resources.*` + 全局路径 |
| `PathPolicy` / `SandboxMode` | `profile.sandbox` + roots |
| `PermissionRules` | `permission_preset` |
| `one-ext::Extension` | package `extensions.load` |
| Plan Mode | **Core 模式**（工具 allowlist 切换）；可由 coding 包的 overlay 强化文案，但不独占 Core |
| 默认启动行为 | 默认启用 `coding` package |

渐进策略（实现阶段再做）：

```text
今天：代码里 coding_tools()
  → 抽出 tool id 列表到 profiles/coding.toml（仍可内嵌发行）
  → 迁到 packages/coding/
  → one pkg enable / install（更晚）
```

---

## 10. 官方包规划（产品，非实现承诺）

| Package | 意图 |
|---------|------|
| `base` | 最小对话 + `ask_user`；可测 Core |
| `coding` | 现有编程工具 + 工程向 skills/overlay |
| `coding.web` | 可选：`web_search` / `web_fetch`（network feature） |
| `office` / `office.docs` | 文档向 skills + 更严 sandbox；tool 以扩展/CLI 为主 |
| `research` | 只读 + 网络 + 笔记目录有限写入 |

用户可只装片段包（如仅 `office.docs`），不必一次装巨型 office。

---

## 11. MVP 验收标准（将来实现时）

仅当下列成立，才认为方向落地（**当前未做**）：

1. 存在 `PackageManifest` 解析与目录发现（用户/项目/内置）。
2. Merge 出 `RuntimeProfile`，驱动 tool 列表与 system overlay 与 skills 根。
3. 两个官方包可切换：
   - **coding**：行为与今天默认编码模式基本一致；
   - **office-lite**（或 `base`+文档 skill）：默认不能（或极难）对任意仓库 `bash`/`write` 乱改。
4. CLI：`one --suite <name>`（语义见 §7.1 选项 A）。
5. Session 记录启用的 package 名与版本。

**MVP 不做**：远程安装、dylib 强制、CLI bridge、应用商店、TS 插件。

---

## 12. 安全与信任

- Package 与 Pi package 类似：**扩展代码 = 本机全权**；skills = 可诱导模型执行危险操作。
- 项目级 `.one/packages` 应有 **信任提示**（对齐日后 project trust），避免阴路径投毒。
- 权限与 sandbox **不可**被 skill 文本绕过；只能通过 tool 管道与 preset 收紧/放宽。
- 多包 merge 时 **更严 sandbox 优先**，防止「办公包 + 编程包」叠加后意外 full-access。

---

## 13. 与「全流程插件系统」的关系

| 想法 | 本设计的取舍 |
|------|----------------|
| 配置包就能切换领域 | ✅ 一等公民（Package/Suite） |
| 底层干净通用 | ✅ Core 无领域分支 |
| 每个生命周期都能深度改写 | ❌ 不做上帝插件总线 |
| 代码扩展 | ✅ 继续 `Extension`，由 package 引用 |
| 分发安装 | ⏳ Package 稳定后再做 |

灵活 harness = **可配置装配 + 有限钩子 + 资源层 progressive disclosure**；  
全流程插件 = 生态成熟后的可选项，不是本阶段目标。

---

## 14. 待决问题（实现前拍板）

1. **`--suite` 是否会话级独占**（§7.1 选项 A vs B）——文档倾向 A。  
2. **无配置时默认 `coding` 还是 `base`**——倾向 `coding` 以兼容现状。  
3. **命名策略**：`documents-write` 等 sandbox 是枚举扩展还是通用 `writable_roots`？倾向 **通用 roots + 少量内置 preset 名**。  
4. **manifest 格式**：TOML vs JSON——倾向 TOML（与人类编辑的 package 友好）。  
5. **Plan mode** 是否也做成 package profile 变体，抑或保持 Core 一等模式——倾向 **Core 模式 + package overlay 文案**。

---

## 15. 文档状态与修订

| 版本 | 日期 | 说明 |
|------|------|------|
| 0.1 | 2026-07-16 | 初稿：分层、manifest、merge、MVP、与现状映射；不实现 |

实现启动时：将本文件状态改为「实施中」，并在 [roadmap.md](./roadmap.md) 增加对应 phase；CLI 标志以 [cli.md](./cli.md) 为准同步。
