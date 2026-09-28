# 提示词组件与 TOML DSL

`one-prompt` 是不依赖 One 运行时、工具实现或模型客户端的 Rust crate。文件加载和纯编译分离：`PromptSpec::load(path)` 加载 TOML 与相对 Markdown；`compile(&spec, &registry, &context)` 只处理内存数据，返回 `CompiledPrompt { text, slots, matched_rules }`。模型请求仍使用 `system_prompt: String`。

## AgentSpec 配置

默认省略 `prompt` 时使用代码预设。显式选择通用预设：

```json
{"prompt": {"preset": "general"}}
```

引用 DSL 文件：

```json
{"prompt": {"file": "prompts/main.toml"}}
```

例如 `.one/agents/main.json` 内的 `prompts/main.toml` 指 `.one/agents/prompts/main.toml`。嵌套子 Agent 的路径也以声明文件所在目录为准，不受子 Agent 工作目录或 worktree 影响。内联 AgentSpec 没有声明文件时，以宿主 cwd 为基准。DSL 中的 Markdown 路径以 **TOML 文件**所在目录为准。

`prompt.preset`、`prompt.file`、`prompt.spec` 三选一；`spec` 是供程序调用的完整内联 DSL 对象。`prompt.operations` 可在所选配置的普通操作之后添加操作，其正文文件以 AgentSpec 声明目录为准。Markdown Agent 的 frontmatter 使用相同配置，正文转换成最后一个 `role` 插槽 `replace` 操作。

## DSL v1

```toml
version = 1
preset = "code"

[[operations]]
slot = "extra"
op = "append"
body = { file = "team.md" }

[[rules]]
provider = "example-provider"
model = "example-model-*"
[[rules.operations]]
slot = "style"
op = "replace"
body = { text = "\nKeep simple answers concise.\n" }
```

正文 `body` 必须且只能使用 `text` 或 `file`。`disable` 禁止正文，`append` / `replace` 必须有正文。渲染保留正文原样，**不自动插入换行**；请在插槽正文或追加内容中明确写分隔符。

编译顺序固定：展开预设和自定义组件 → 普通操作 → 按声明顺序应用匹配模型规则 → 检查能力和模式 → 渲染。`replace` 清空先前内容并启用插槽；`disable` 关闭插槽；向已关闭插槽 `append` 报错，后续 `replace` 可以重新启用。每次编译都从原始配置开始，不修改配置或注册表。

模型规则的 provider 与 model **同时匹配**才生效，大小写敏感，`*` 匹配任意长度文本（包括空串），其他字符按字面值匹配。使用实际 provider ID 与实际 model ID，而非显示名。无规则匹配时使用基础配置。多条规则可同时命中，后声明的操作后执行。

## 组件与插槽

代码预设按下表顺序输出；保留迁移前默认代码提示词正文和顺序。可选能力说明仅在宿主实际提供相应能力时输出。

| 组件 | 插槽 | 条件 |
| --- | --- | --- |
| code_role | role | 无 |
| safety | safety | 无 |
| tools | tools | 无 |
| background | background | monitor |
| output | output | 无 |
| formatting | formatting | 无 |
| user_guide | user_guide | 无 |
| project | project | 无 |
| resources | resources | 无 |
| environment | environment | 无 |
| memory_catalog | memory_catalog | memory |
| subagent | subagent | task |
| memory_write | memory_write | memory、memory_write；act |
| style | style | 无 |
| planning | planning | plan；plan 模式 |
| extra | extra | 无 |

通用预设顺序为 `role`、`resources`、`environment`、`memory_catalog`、`subagent`、`memory_write`、`planning`、`extra`，采用通用角色、委派、规划和记忆说明，不含代码 Agent、git、One CLI 等专属要求。仅代码预设存在的插槽不能在通用预设中覆盖。

自定义组件追加在预设组件之后，组件和插槽数组顺序就是输出顺序：

```toml
[[components]]
id = "research"
[[components.slots]]
id = "research.sources"
body = { text = "\nVerify current claims with web_search.\n" }
[components.slots.when]
capabilities = ["web_search"]
modes = ["act"]
```

能力列表为 AND，模式列表为 OR；空列表不限制。操作不能修改插槽条件，因此模型规则不能重新启用宿主未提供能力的受限插槽。编译器不分析自然语言，也无法识别用户把工具说明写入无条件插槽的语义；能力说明应始终放在带条件的插槽中。

Rust 宿主还可用 `ComponentRegistry::register` 注册组件，用 `preset(name, component_ids)` 定义显式组件顺序。预设展开后组件 ID 和插槽 ID 必须唯一；不同备选预设可以共用插槽名。

## 上下文、来源和错误

正文中的 `{{variable}}` 是必需变量。缺失变量报错；关闭或条件未满足的插槽不渲染，也不要求其变量。插入的变量值不递归解析，Markdown 正文不作为 DSL 执行。

One 注入 `resources`（AGENTS、技能目录、扩展）、`environment`（环境快照）、`memory_catalog` 和 Plan 模式的 `plan_path`。前三项在独立 crate 的默认上下文中是空文本。外部 Rust 宿主可通过 `CompileContext.variables` 提供自己的变量。实际工具名作为能力集合；One 另外派生 `memory`（有记忆目录）和 `plan`（有计划文件路径）。

`CompiledPrompt.slots` 记录组件、插槽、初始正文来源、是否输出、操作顺序、操作配置来源及模型规则索引；`matched_rules` 记录全部命中的规则索引。顺序和索引从 0 开始，输出完全确定。

`PromptError` 提供 `kind`、`source_location` 和 `message`。版本错误、未知字段、未知预设或插槽、重复 ID、非法操作、未加载文件、缺失文件或变量都返回错误。配置错误可定位到文件和组件/插槽或操作索引；TOML 语法错误保留解析器的行列信息。未匹配规则中的未知插槽和非法正文形状也会报错。

初始化失败直接返回错误。模型切换先构造候选 provider，再按候选实际模型与工具编译；成功后才更新 provider 和全局模型偏好。Plan/Act、资源重载、子 Agent 都调用同一个宿主编译适配器。运行时编译失败保留上次成功的模型/提示词组合；资源重载可能已刷新资源对象和扩展，修复配置后再次 `/reload` 即可重编译。

## 从旧配置迁移

`AgentSpec.system_prompt` 和 `append_system_prompt` 已移除，即使值为 `null` 也报迁移错误。provider 请求中的 `system_prompt: String` 不受影响。

旧完整角色正文迁移为通用预设的 `role` 替换：

```json
{
  "prompt": {
    "preset": "general",
    "operations": [
      {"slot": "role", "op": "replace", "body": {"text": "You are a read-only research agent."}},
      {"slot": "extra", "op": "append", "body": {"text": "\n\nReturn findings with source paths."}}
    ]
  }
}
```

仅追加代码 Agent 指令时，选择 `code` 并对 `extra` 使用 `append`。旧 `system_prompt: null` 改为省略 `prompt` 或 `{"preset":"code"}`。旧追加正文改为 `extra` 操作；不要保留旧字段。

完整示例见 [代码预设与模型差异](../crates/one-prompt/examples/prompts/code.toml)、[Markdown 正文](../crates/one-prompt/examples/prompts/team.md)、[通用预设](../crates/one-prompt/examples/prompts/general.toml) 和 [外部 Rust Agent](../crates/one-prompt/examples/external_agent.rs)。模型差异示例仅供显式引用，不默认启用。

验证：`cargo test -p one-prompt`；CLI 运行时测试可使用 `ONE_AGENT_DIR=/tmp/one-prompt-test-agent cargo test -p one-cli` 隔离用户资源和会话目录；最后运行 `cargo check --workspace`。

## 本次验证结果

| 范围 | 通过测试数 |
| --- | ---: |
| one-prompt（含已交付示例编译） | 14 |
| one-cli（单元、WebSocket、mock 端到端） | 173 |
| one-core | 82 |
| one-tools | 195 |
| one-resources | 52 |
| 合计 | 516 |

上述测试均以 `--offline` 运行；CLI 使用临时 `ONE_AGENT_DIR`，WebSocket 测试在允许监听本地端口的环境中运行。`cargo check --workspace --offline`、外部 Rust Agent 示例和 `git diff --check` 通过。仓库仍有既存的 unused/dead-code 编译警告。

运行时回归覆盖模型 A → B → A、Plan/Act 恢复、能力关闭、Markdown 重载、失败保留提示词、初始化失败、相对路径和嵌套配置、旧字段迁移错误、子 Agent 独立模型与资源开关。子 Agent 的资源扫描同时新增了可在 `Send` 任务中递归发现技能的回归测试。
