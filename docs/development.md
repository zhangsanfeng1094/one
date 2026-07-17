# 开发指南

## 环境要求

- Rust 1.75+（推荐 stable）
- Linux/macOS
- 可选：`build-essential`（gcc + libc-dev）

```bash
# Ubuntu/Debian
sudo apt-get install -y build-essential pkg-config
```

> 若系统无 gcc，项目 `.cargo/config.toml` 提供了 Zig 作为临时 C 编译器的 workaround。正式开发建议安装 `build-essential` 后删除 Zig 配置。

## 构建

```bash
cargo build -p one-cli
cargo test
cargo run -p one-cli -- -p "hello"
```

### Features

| Feature | 说明 |
|---------|------|
| `http-providers` | 启用 Anthropic / OpenAI HTTP provider |

```bash
cargo build -p one-cli --features http-providers
```

## Crate 开发顺序建议

1. `one-core` — 改 Agent loop、消息类型
2. `one-tools` — 加/改内置 tool
3. `one-session` — session 格式与持久化
4. `one-ai` — 新 provider
5. `one-cli` — 新模式/新 flag

## 添加新 Tool

1. 在 `one-tools/src/` 新建模块，实现 `Tool` trait
2. 注册到 `coding_tools_with_options()` / `read_only_tools_with_policy()`；路径类工具必须走 `PathPolicy::resolve`
3. 补充单元测试

## 添加新 Provider

1. 在 `one-ai/src/` 实现 `LlmProvider`
2. 在 `one-cli/src/provider.rs` 注册
3. 在 `ModelRegistry` 添加默认模型条目

## 添加扩展

参考 `crates/one-ext/examples/status_extension.rs` 与 [extensions.md](./extensions.md)：

```rust
struct MyExtension;

#[async_trait]
impl Extension for MyExtension {
    fn name(&self) -> &str { "my-ext" }
    fn tools(&self) -> Vec<Arc<dyn Tool>> { vec![/* ... */] }
    fn contribute_context(&self) -> Vec<PromptFragment> { vec![] }
    async fn before_tool(&self, call: &ToolCall) -> one_ext::Result<PreToolDecision> {
        Ok(PreToolDecision::Allow)
    }
}
```

发现路径：`~/.one/agent/extensions.json`、`~/.one/agent/plugins/*/…`、或代码里 `ExtensionRegistryBuilder::install`。  
`AppRuntime` 通过 `discover_all` 加载，并绑定 `tool_gate` + `AgentHooks`。

## 架构文档维护

改分层、新 crate、或显著能力（MCP / Ext / Package）落地时，更新：

1. **[architecture.md](./architecture.md)** §2 状态矩阵、§3 依赖图、§7 模块地图、§9 简洁性评估  
2. 必要时 roadmap 勾选与专题文档（extensions / mcp / package-suites）

`one-cli` 编排逻辑在 `src/runtime/`（按 build / plan / tools / prompt / session / reload 分文件），不要重新合成单文件巨石。

## 测试

```bash
# 全部测试
cargo test

# 单个 crate
cargo test -p one-session
cargo test -p one-core
cargo test -p one-ext
```

## 调试

```bash
RUST_LOG=debug cargo run -p one-cli -- -p "test"
```