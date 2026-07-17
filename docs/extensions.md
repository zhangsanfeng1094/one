# 扩展系统

One 提供 **Rust-native** 扩展运行时，架构对齐 [Codex extension-api / hooks / plugins](https://github.com/openai/codex) 的能力面，但不兼容 TypeScript / npm 扩展。

## 分层（对齐 Codex）

```text
┌──────────────────────────────────────────────────┐
│  Plugins（plugin.json）                           │
│  skills · prompts · hooks 文件 · 扩展引用 · MCP 声明 │
└────────────────────┬─────────────────────────────┘
                     │ discover
┌────────────────────▼─────────────────────────────┐
│  ExtensionRuntime                                 │
│  Registry + ExtensionData + HooksConfig           │
└────────────────────┬─────────────────────────────┘
                     │
     ┌───────────────┼───────────────┐
     ▼               ▼               ▼
 Extension trait   外部 hooks     Agent 接入
 tools/context     PreToolUse     tool_gate
 before/after      PostToolUse    AgentHooks
 lifecycle         Session*       system overlay
```

| Codex 概念 | One 对应 |
|------------|----------|
| `ExtensionRegistryBuilder` | `ExtensionRegistryBuilder` |
| `ToolContributor` | `Extension::tools` |
| `ContextContributor` | `Extension::contribute_context` |
| Thread / Turn lifecycle | `on_event` + `AgentHooks` 桥 |
| Tool lifecycle | `before_tool` / `after_tool` |
| PreToolUse rewrite/deny | `PreToolDecision` + 外部 `hooks.json` |
| `ExtensionData` | `one_ext::ExtensionData` |
| `plugin.json` | `.one-plugin/plugin.json` |
| Hooks 脚本 | `~/.one/agent/hooks.json` |

**MCP** 仍是平台基础能力（`one-mcp`），不经扩展插件路径实现协议；plugin 仅可**声明** MCP 配置片段（装配合并后续迭代）。

---

## Extension Trait

```rust
use async_trait::async_trait;
use one_ext::{Extension, ExtensionContext, ExtensionEvent, PreToolDecision, PromptFragment};

struct MyExtension;

#[async_trait]
impl Extension for MyExtension {
    fn name(&self) -> &str { "my-extension" }

    async fn on_load(&self, ctx: &ExtensionContext<'_>) -> one_ext::Result<()> {
        // ctx.cwd / ctx.session_file / ctx.data (type map)
        let _ = ctx;
        Ok(())
    }

    async fn on_event(&self, event: &ExtensionEvent) -> one_ext::Result<()> {
        match event {
            ExtensionEvent::ToolEnd { tool_call, is_error, .. } => {
                tracing::debug!(tool = %tool_call.name, is_error, "tool finished");
            }
            _ => {}
        }
        Ok(())
    }

    fn tools(&self) -> Vec<std::sync::Arc<dyn one_core::Tool>> {
        vec![/* Arc::new(MyTool) */]
    }

    fn contribute_context(&self) -> Vec<PromptFragment> {
        vec![PromptFragment {
            source: "my-extension".into(),
            text: "Guidance injected into the system prompt.".into(),
        }]
    }

    async fn before_tool(
        &self,
        call: &one_core::ToolCall,
    ) -> one_ext::Result<PreToolDecision> {
        if call.name == "bash" {
            // Allow | Rewrite { arguments } | Deny { message }
        }
        Ok(PreToolDecision::Allow)
    }

    async fn after_tool(
        &self,
        _call: &one_core::ToolCall,
        _output: &one_core::ToolOutput,
        _is_error: bool,
    ) -> one_ext::Result<()> {
        Ok(())
    }
}
```

### 生命周期事件

| 事件 | 时机 |
|------|------|
| `SessionStart` / `SessionEnd` | 每次 agent run 开始/结束（`AgentHooks`） |
| `TurnStart` / `TurnEnd` | 每个 LLM turn |
| `ToolStart` / `ToolEnd` | 工具前后（`ToolEnd` 由 runtime 在 after 路径发出） |
| `UserPromptSubmit` | CLI 收到用户输入、进入 model 前 |
| `PreCompact` / `PostCompact` | 预留（compaction 挂钩） |

### PreTool 管道（执行顺序）

```text
tool_call
  → Extension::before_tool（可 Rewrite / Deny）
  → hooks.json PreToolUse 脚本（可 Rewrite / Deny）
  → PermissionGate（allow / ask / deny）
  → Tool::execute
  → Extension::after_tool + PostToolUse 脚本
```

`ToolGateDecision::Rewrite` 由 `one-core` 在执行前写回 `ToolCall.arguments`。

---

## 注册与发现

### `~/.one/agent/extensions.json`

**数组（兼容旧格式）：**

```json
[
  { "name": "status", "builtin": true },
  { "name": "my_so", "path": "ext/libmine.so" }
]
```

**对象（推荐）：**

```json
{
  "extensions": [
    { "name": "status", "builtin": true }
  ],
  "disabledPlugins": ["untrusted-plugin"],
  "noDefaultStatus": false
}
```

未配置时默认加载内置 `status` 扩展（可用 `noDefaultStatus: true` 关闭）。

### Plugins（Codex 风格目录）

扫描顺序（同名 project 优先）：

| 范围 | 路径 |
|------|------|
| 项目 | `<cwd>/.one/plugins/<name>/` |
| 用户 | `~/.one/agent/plugins/<name>/` |

Manifest 路径（先匹配者）：

- `.one-plugin/plugin.json`
- `.codex-plugin/plugin.json`（兼容）
- `plugin.json`

```json
{
  "name": "demo",
  "version": "0.1.0",
  "description": "Demo plugin",
  "skills": ["skills"],
  "prompts": ["prompts"],
  "extensions": ["status"],
  "hooks": "hooks.json",
  "systemOverlay": "system.md",
  "mcpServers": {}
}
```

- **skills / prompts**：并入 `ResourceLoader` 发现根  
- **extensions**：builtin 名或相对 dylib 路径  
- **hooks**：相对路径的 hooks 配置  
- **systemOverlay**：追加到 system prompt  

### 外部 Hooks（`hooks.json`）

路径：`~/.one/agent/hooks.json` 或 plugin 声明文件。

```json
{
  "preToolUse": [
    {
      "name": "block-rm",
      "matcher": "bash",
      "command": ["python3", "/path/to/pre_bash.py"],
      "timeoutSec": 10
    }
  ],
  "postToolUse": [],
  "sessionStart": [],
  "sessionEnd": [],
  "userPromptSubmit": []
}
```

- **stdin**：JSON 请求（`hookEventName`, `toolName`, `toolInput`, `cwd`, …）  
- **stdout**（PreToolUse）：  
  - `{ "permissionDecision": "deny", "systemMessage": "…" }`  
  - `{ "updatedInput": { … } }`  
  - `{ "continue": false, "systemMessage": "…" }`  
- **matcher**：工具名 glob（`bash`、`ba*`、`*`）

---

## 运行时接入（one-cli）

`AppRuntime::build`：

1. `discover_all(cwd, agent_dir)` → extensions + plugins + hooks  
2. 合并 plugin skills/prompts/overlays  
3. `extensions.load_all`  
4. `agent.set_tool_gate(extensions.tool_gate(permission_gate))`  
5. `agent.set_hooks(extensions.agent_hooks())`  
6. Act 模式合并 `extensions.tools()`  

`/reload` 会 unload → 重新 discover → 重绑 gate/hooks → 重建 tools/prompt。

---

## ExtensionData

```rust
ctx.data.insert(MyConfig { .. });
let cfg: Option<MyConfig> = ctx.data.get();
ctx.data.with_mut::<MyConfig, _>(|c| { /* … */ });
```

类型图存于 `Arc<ExtensionData>`，与 runtime 同寿。

---

## 自定义命令

```rust
fn commands(&self) -> Vec<ExtensionCommand> {
    vec![ExtensionCommand {
        name: "hello".into(),
        description: "Say hello".into(),
        handler: |_| "hello from extension".into(),
    }]
}
```

（TUI 斜杠表可在后续迭代挂接 `runtime.commands()`。）

---

## 状态持久化

```rust
fn custom_state(&self) -> Option<(String, serde_json::Value)>;
fn restore_state(&self, custom_type: &str, data: &Value) -> Result<()>;
```

Session JSONL `custom` entry；resume 时 `restore_custom`。

---

## 示例

```bash
cargo run --example status_extension -p one-ext
cargo test -p one-ext
```

内置 `status`：提供 `status` tool + system 片段 + `ext.status` 状态。

---

## dylib（可选 feature）

```toml
one-ext = { path = "...", features = ["dylib"] }
```

导出 `extern "C" fn one_extension_name() -> *const c_char`，当前 ABI 仅映射到**已知 builtin 名**（完整跨 dylib trait 对象需稳定 C ABI，未做）。

---

## 与官方 Pi / Codex 的差异

| 能力 | Codex | Pi (TS) | One |
|------|-------|---------|-----|
| 语言 | Rust contributors | TypeScript | Rust `Extension` |
| Registry / Data | ✅ | 部分 | ✅ |
| PreTool rewrite/deny | hooks + 扩展 | 扩展 | ✅ 扩展 + hooks.json |
| plugin.json | ✅ marketplace | npm packages | 本地 plugins 目录 |
| 热重载 | 部分 | `/reload` | ✅ `/reload` |
| TUI 自定义渲染 | 有限 | ✅ | ❌ |
| npm / 应用商店安装 | marketplace | npm | ❌ |

---

## 未来方向

1. 稳定 dylib / WASM ABI（真正 out-of-tree 扩展）  
2. plugin MCP 片段合并进 `McpManager`  
3. TUI 挂接 `ExtensionCommand`  
4. Package / Suite 通过 `extensions.load` 引用（见 [package-suites.md](./package-suites.md)）  
5. Compaction 真正发出 `PreCompact` / `PostCompact`
