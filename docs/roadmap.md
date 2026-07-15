# 路线图

## 已完成 ✅

### Phase 1 — 最小 Agent
- [x] Workspace + crate 骨架
- [x] Agent loop（prompt → LLM → tool → loop）
- [x] 内置 tools：read / write / edit / bash
- [x] Mock provider
- [x] Print 模式 (`-p`)

### Phase 2 — Session
- [x] `one-session` crate
- [x] JSONL v3 树形格式（读写）
- [x] `--continue` / `--session` / `--no-session`
- [x] Context builder（compaction 路径）
- [x] v1/v2 session 自动迁移
- [x] `/tree` 分支导航（列表 + `/tree <id>` 切换）
- [x] session export（HTML + `--export`）
- [x] `--share` GitHub Gist 上传（需 `GITHUB_TOKEN` + `network` feature）

### Phase 3 — Provider & 工具扩展
- [x] Anthropic / OpenAI provider（feature-gated）
- [x] ModelRegistry + `models.json` 自定义模型
- [x] grep / find / ls tools
- [x] Streaming text delta 事件
- [x] 真正 SSE streaming（Anthropic / OpenAI / Ollama）
- [x] Ollama / OpenRouter

### Phase 4 — 交互 & 集成
- [x] Interactive TUI（基础 + 流式渲染）
- [x] JSON 事件流模式
- [x] RPC stdin/stdout 模式
- [x] Steer / Follow-up（运行中注入消息）
- [x] 模型切换 UI（`/model`）

### Phase 5 — 扩展 & 资源
- [x] `one-resources`：AGENTS.md / skills / prompts
- [x] `one-ext`：Rust Extension trait + builtin status
- [x] 基础 compaction
- [x] 扩展状态 `custom` entry 持久化
- [x] 扩展热重载（`/reload`）
- [x] 动态加载 `.so` 扩展（`dylib` feature，实验性）

### 安全 & 部署
- [x] 工具执行沙箱（阻断危险命令）
- [x] 权限确认流（`--yes` / `ONE_AUTO_APPROVE`）
- [x] 单二进制 release CI
- [x] `--version`（clap 内置）

### 测试 & 质量
- [x] 集成测试（mock provider e2e）
- [x] Session 迁移 / 分支往返测试
- [x] Bash 沙箱 / 审批测试

### 文档
- [x] README
- [x] architecture / cli / session-format / development / roadmap / extensions

---

### Phase A 补齐（P0 体验）— 2026-07-14
- [x] Skills（Agent Skills 标准）：XML catalog + 模型 `read` 按需加载 + 可选 `/skill:name` 强制
- [x] Compaction：extractive + LLM 摘要；context overflow 重试一次
- [x] Session UX：`/session` `/resume` `/new` `/name` + CLI `-r`
- [x] Footer 用量：`~N tok` 估算 + thinking 标签
- [x] Thinking level：`/settings` / `/thinking` + session 持久化 + Anthropic budget
- [x] Thinking 全链路：ContentBlock signature、各 provider 请求/解析/回放、TUI 流式 + Ctrl+T 折叠
- [x] TUI 多行：Ctrl+J / Shift+Enter；paste 保留换行
- [x] CLAUDE.md 与 AGENTS.md 一并加载
- [x] 联网：`web_search` / `web_fetch` 内置工具；沙箱不再硬拦 curl/wget

### Phase P0 日常替代感 — 2026-07-15
- [x] Provider 面：DeepSeek / Gemini 直连 + 内置 models 预设 + `--list-providers`
- [x] TUI：`@file` 注入 + Tab 路径补全；`-r` 交互 session 选择器
- [x] 精确 usage：provider input/output tokens + 粗估 cost footer
- [x] 统一 settings：`~/.one/agent/settings.json` + `/settings`

## 待完善 / 非阻塞 🔜

- [ ] OAuth subscription 登录（Pi 官方订阅模式）
- [ ] TS 扩展兼容层（QuickJS / WASM 评估）
- [ ] self-update 命令
- [ ] 性能基准套件
- [ ] TUI 差分渲染优化 / 贴图 / `@` 模糊搜索增强
- [ ] 与官方 Pi session 全量兼容性回归测试
- [ ] 更准的 per-model pricing 表

---

## 非目标（与官方 Pi 保持一致）

- 不内置 MCP
- 不内置子 Agent orchestrator

## Plan Mode（已内置 MVP）

- `/plan`、`--plan`，或 **空输入时 Space**：硬只读工具 + 仅可写 plan 文件 + `exit_plan_mode`
- `/act` / `/build`：批准计划并切到 Build 模式开始实现；Space 再按一次仅回到 Build（不自动实现）
- 计划落盘：`~/.one/agent/plans/<uuid>.md`
- Thinking 深度不占用快捷键，用 `/settings thinking …`