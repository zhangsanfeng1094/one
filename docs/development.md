# 开发指南

## 环境要求

- Rust 1.75+（推荐 stable）
- Linux/macOS
- 可选：`build-essential`（gcc + libc-dev）；bubblewrap（bash OS 沙箱）

```bash
# Ubuntu/Debian
sudo apt-get install -y build-essential pkg-config
# 可选 OS 沙箱
sudo apt-get install -y bubblewrap
```

> 若系统无 gcc，项目 `.cargo/config.toml` 可能提供 Zig 作为临时 C 编译器的 workaround。正式开发建议安装 `build-essential`。

## 构建

```bash
cargo build -p one-cli          # 二进制 target/debug/one
cargo test
cargo run -p one-cli -- -p "hello"
cargo run -p one-cli -- --list-providers
```

`one-cli` **默认启用** `http-providers`（及 `network`），真实 LLM / `web_search` / `web_fetch` 无需再加 feature。  
仅在需要关掉网络相关依赖时再改 `Cargo.toml` features。

### Features

| Crate | Feature | 说明 |
|-------|---------|------|
| `one-cli` | `http-providers`（**default**） | 真实 HTTP providers + network tools |
| `one-cli` / `one-ai` / `one-tools` | `network` | reqwest；web tools、session share 等 |
| `one-ext` | `dylib` | 实验性 `.so` 扩展加载 |

```bash
# 默认即带 HTTP providers
cargo build -p one-cli

# 扩展 dylib 实验
cargo test -p one-ext --features dylib
```

## Crate 开发顺序建议

1. `one-core` — Agent loop、消息类型、ToolGate、trace、compaction  
2. `one-tools` — 加/改内置 tool、PathPolicy、sandbox  
3. `one-session` — session 格式与持久化  
4. `one-ai` — 新 provider、compat、OAuth/auth  
5. `one-mcp` / `one-resources` / `one-ext` — 平台能力  
6. `one-tui` — 交互与 slash  
7. `one-cli` — 装配、flags、modes、`AppRuntime`

## 添加新 Tool

1. 在 `one-tools/src/` 新建模块，实现 `Tool` trait  
2. 注册到 `coding_tools_with_options()` / `read_only_tools_with_ask()` / plan 工具集；路径类工具必须走 `PathPolicy::resolve`  
3. 需要后台任务时接 `BackgroundTaskRegistry`（见 `bash` / `bash_output` / `bash_kill`）  
4. 补充单元测试  

## 添加新 Provider

1. 在 `one-ai/src/` 实现 `LlmProvider`（及 streaming / thinking 如需要）  
2. 在 `one-cli/src/provider.rs` 与 `cli.rs` `ProviderKind` 注册（或仅 models.json 自定义）  
3. 在 `ModelRegistry` / builtin models 添加默认模型条目  
4. 需要 OAuth 时扩展 `one-ai/src/auth/` 与 `oauth_provider_catalog()`  

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

## 认证与配置目录

| 路径 | 用途 |
|------|------|
| `~/.one/agent/settings.json` | 统一设置（provider/model/thinking/sandbox/…） |
| `~/.one/agent/models.json` | providers + compat + 模型列表 |
| `~/.one/agent/auth.json` | OAuth / 订阅凭证（`0600`） |
| `~/.one/agent/mcp.json` | 用户 MCP 服务器 |
| `~/.one/agent/sessions/` | JSONL sessions |
| `~/.one/agent/cache-debug/` | prompt cache 调试（可用 `ONE_DEBUG_CACHE=0` 关） |

```bash
one login                 # 交互选 Codex / xAI / OpenCode
one login openai-codex
one login xai --device-code
one logout --all
```

## 架构文档维护

改分层、新 crate、或显著能力（MCP / Ext / OAuth / Package）落地时，更新：

1. **[architecture.md](./architecture.md)** §2 状态矩阵、§3 依赖图、§7 模块地图、§9 简洁性评估  
2. 必要时 [roadmap.md](./roadmap.md) 勾选与专题文档（extensions / mcp / gap-vs-pi / harness-eval）  
3. [cli.md](./cli.md) 的 flags、slash、RPC、环境变量  

`one-cli` 编排逻辑在 `src/runtime/`（build / plan / tools / prompt / session / reload 等），不要重新合成单文件巨石。

## 测试

```bash
# 全部测试
cargo test

# 单个 crate
cargo test -p one-session
cargo test -p one-core
cargo test -p one-ext
cargo test -p one-tools
cargo test -p one-mcp
cargo test -p one-cli

# mock e2e
cargo test -p one-cli --test e2e_mock
```

## Harness / 调试

```bash
# 日志
RUST_LOG=debug cargo run -p one-cli -- -p "test"

# Langfuse 轨迹（见 harness-eval.md）
export LANGFUSE_PUBLIC_KEY=...
export LANGFUSE_SECRET_KEY=...
one --trace -p "list files" --provider mock -y

# smoke bench（无 keys 也可离线打分）
one bench --suite smoke

# 完整 broken-kit 评测
./benches/run.sh smoke
./benches/run.sh full
```
