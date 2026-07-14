# 扩展系统

One 提供 **Rust-native** 扩展运行时，API 理念对齐官方 Pi，但不兼容 TypeScript 扩展。

## Extension Trait

```rust
use async_trait::async_trait;
use one_ext::{Extension, ExtensionContext, ExtensionEvent};

struct MyExtension;

#[async_trait]
impl Extension for MyExtension {
    fn name(&self) -> &str {
        "my-extension"
    }

    async fn on_load(&self, ctx: &ExtensionContext<'_>) -> one_ext::Result<()> {
        let _ = ctx.cwd;
        Ok(())
    }

    async fn on_event(&self, event: &ExtensionEvent) -> one_ext::Result<()> {
        match event {
            ExtensionEvent::ToolExecutionStart { tool_name } => {
                println!("tool starting: {tool_name}");
            }
            _ => {}
        }
        Ok(())
    }
}
```

## 注册工具

扩展可返回额外 `Tool` 实例，合并进 Agent 工具列表：

```rust
fn tools(&self) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(MyTool)]
}
```

## 事件

| 事件 | 时机 |
|------|------|
| `AgentStart` | 每次 prompt 开始 |
| `AgentEnd` | prompt 完成 |
| `TurnStart` | LLM turn 开始 |
| `TurnEnd` | turn 结束 |
| `ToolExecutionStart` | 工具调用前 |
| `ToolExecutionEnd` | 工具调用后 |

## 示例

```bash
cargo run --example status_extension -p one-ext
```

## 与官方 Pi 扩展的差异

| 能力 | 官方 Pi (TS) | One |
|------|-------------|---------|
| 语言 | TypeScript | Rust |
| 热重载 `/reload` | ✅ | ✅（扩展 + skills/prompts） |
| TUI 自定义渲染 | ✅ | ❌ |
| 状态持久化 `custom` entry | ✅ | ✅（trait `custom_state`） |
| 从 npm 安装扩展包 | ✅ | ❌ |

## 未来方向

1. **dylib 动态加载**：编译为 `.so`，运行时加载
2. **WASM 沙箱**：安全隔离的扩展执行
3. **TS 兼容层**：评估 QuickJS 运行官方扩展的可行性