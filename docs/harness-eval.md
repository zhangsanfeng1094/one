# Harness 能力对比与埋点评测

> 目标：收集 agent **完整执行链路**，用固定任务打分，再和对照 agent 对比，驱动 harness 改进。  
> 原则：**非破坏** — Core 只增加可选 `TraceSink`；默认路径零开销；不改分层与依赖纪律。

## 快速使用

### 1. 写轨迹（任意一次运行）

```bash
# 默认写入 ~/.one/agent/traces/trace_<ms>.jsonl
one --trace -p "list files in current directory" -y --provider mock

# 指定路径
one --trace ./run.jsonl -p "…" -y

# 环境变量
ONE_TRACE=1 one -p "…" -y
```

### 2. 看汇总

```bash
one trace-stats ./run.jsonl
one trace-stats ./run.jsonl --json
```

### 3. 跑 smoke 任务包（mock，可进 CI）

```bash
one bench --suite smoke
one bench --task mock-list-files
one bench --suite all --out ./benches/out/manual
```

输出目录含：

- `traces/<task>.jsonl` — 完整 span
- `summary.json` / `summary.md` — 通过率表

`suite=full` 的任务（如 `edit-marker`）需要真实模型才能过；当前 bench runner 默认仍走 **MockProvider**（确定性 smoke）。对 full 任务请用：

```bash
one --trace ./edit.jsonl --provider <real> -y -p "$(cat benches/tasks/edit-marker/prompt.md)" \
  --cwd /tmp/edit-ws
# 再人工或脚本按 rubric.json 判分
```

## 架构（加法）

```text
Agent::run
  │  optional TraceSink (None = no-op)
  ├─ run_start / run_end
  ├─ turn_start
  ├─ llm_request / llm_response  (latency_ms, ttft_ms, usage)
  ├─ tool_start / tool_end       (duration_ms, is_error)
  └─ gate                        (allow | rewrite | deny)
         │
         ▼
  JsonlTraceSink  ──►  *.jsonl
         │
         ▼
  TraceStats / one trace-stats / one bench scorer
```

| 层 | 职责 |
|----|------|
| `one-core::trace` | `TraceEvent` schema、`TraceSink`、`MemoryTrace`、`JsonlTraceSink`、`TraceStats` |
| `Agent` | 可选 `set_trace` / `set_trace_meta`；loop 内埋点 |
| `one-cli` | `--trace`、`trace-stats`、`bench` |
| `benches/tasks/*` | 任务包：fixture + prompt + rubric |

**不**改：`AgentEvent` 语义、session 格式、crate 依赖方向、默认运行行为。

## Trace 事件类型

| type | 含义 |
|------|------|
| `run_start` / `run_end` | 一次 prompt 循环边界；status = ok / aborted / max_turns / error |
| `turn_start` | 一轮 LLM |
| `llm_request` / `llm_response` | 请求规模 + 时延 + usage + stop_reason |
| `tool_start` / `tool_end` | 工具名、args 预览、时长、错误 |
| `gate` | 权限/扩展门控 |
| `compaction` | （预留）压缩前后 token |
| `score` | bench 写入的自动判分 |

## 主评测项目：`broken-kit` v0.2

路径：`benches/projects/broken-kit/`。比 v0.1 更难：跨模块依赖、源码**无** BUG 剧透。

| 模块 | 典型故障 |
|------|----------|
| `money` | f64 折扣、未 clamp、该 floor 却 round |
| `catalog` | upsert 变重复、get 误折叠大小写、search 漏 SKU |
| `promo` | 阶梯 `>`、券大小写、叠加而非 max、off 金额算错 |
| `cart` | 忽略 qty、税在折扣前、依赖 promo/money |
| `ledger` | hold/release 无 FIFO |
| `schedule` | 负区间、单点、闭区间 overlap/clamp |

- Spec：`README.md` · 维护者：`SOLUTIONS.md`（**不**拷给 agent）
- 任务：`kit-baseline` + `kit-fix-{discount,search,cart,ledger,parse,all}`
- **输出** `benches/out/` 已 gitignore，勿提交
- **可重复**：`full` 每次拷到 `out/.../workspace`，不改源树

```bash
./benches/run.sh verify
./benches/run.sh full
./benches/run.sh full kit-fix-ledger
./benches/run.sh codex
./benches/run.sh compare
```

脚本结构见 `benches/README.md`：`run.sh` 只做分发，`lib/` 公共，`cmd/{offline,one,codex,compare}.sh` 各管一块。

### 对照 agent 协议（Codex / 其它）

| 项 | 约定 |
|----|------|
| Fixture | 每次独立拷贝 stock 树，互不污染 |
| Prompt | 同一 `benches/tasks/<id>/prompt.md` |
| 打分 | 只认 `cargo test` exit 0（不认 agent 自述） |
| One | 白盒 TraceEvent（turns/tools/tokens/latency） |
| Codex | `codex exec --json` 事件流 + session JSONL + wall_ms |
| 公平性 | 都在临时 workspace、可写 sandbox、无 SOLUTIONS.md |

### Codex 结构化输出（已接到 `run.sh codex`）

官方支持（见 [Non-interactive mode](https://developers.openai.com/codex/non-interactive-mode)）：

```bash
codex exec --json -C "$WS" --sandbox workspace-write -o last.txt "…"
# stdout → JSONL: thread.started / turn.* / item.* (command_execution, file_change, …)
# turn.completed.usage → input/output/cached tokens
# 另有 ~/.codex/sessions/**/rollout-*.jsonl 全量 session
```

`./benches/run.sh codex` 会保存 `codex-events.jsonl` + `codex-stats.txt`，便于和 One 的 `run.jsonl` / `trace-stats.txt` 对照。

接入其它 agent：仿 `cmd_codex`，包「cwd + prompt + 结构化 log + cargo test」。

### Harness 行为约束（评测隔离）

| 信号 | 处理 |
|------|------|
| 小改用整文件 `write` | system prompt + tool 描述引导优先 `edit` |
| 全局 skill catalog 噪声 / 高 token | `full` 传 `--no-skills`；catalog 文案改为「仅明显匹配才 load」 |
| linker/cc bash error | 文档与 prompt 标明环境问题；`run.sh` 预检 `cargo`/`cc` |
| 污染 fixture | 只写 `out/.../workspace` 拷贝，源树只读 |

## 任务包格式

```text
benches/projects/<name>/   # 共享测试工程（推荐）
benches/tasks/<task-id>/
  task.json      # id, title, suite, project?, fixture?, test_filter?
  prompt.md      # 用户提示
  rubric.json    # 自动检查
  fixture/       # 可选本地 fixture（无 project 时）
```

`task.json` 可用 `"project": "broken-kit"` 指向共享工程，避免每题复制一份代码。

Shell 真模型路径（`./benches/run.sh full|codex|compare`）与 `one bench` 共用同一约定：

- workspace：`project` → `fixture` 字段 → `tasks/<id>/fixture/`
- 打分：`rubric` 的 `command` 检查 → 否则 `test_filter`（可多 pattern，每个单独 `cargo test`）→ 否则全量 test
- `test_filter` 支持 `"ledger::"`、`"money:: promo::"`（空格拆成多次）或 `["money::","promo::"]`
- 产物：`result.txt`（仅 `pass`/`fail`）、`score.log` / `score.json`

### Rubric checks

| type | 字段 | 说明 |
|------|------|------|
| `file_contains` | path, text | 工作区文件含文本 |
| `file_exists` | path | 文件存在 |
| `command` | cmd, exit_code | 在 fixture cwd 执行 |
| `max_turns` | n | 轨迹 turns ≤ n |
| `max_tool_errors` | n | 工具错误次数 |
| `trace_has_tool` | name | 调用过某工具 |
| `trace_status` | status | 如 `ok` |
| `final_text_contains` | text | 最终回复 |

## 指标（对比维度）

从 `TraceStats` / summary 表直接读：

- **pass rate**（主）
- turns / tool_calls / tool_error_rate
- wall_ms / llm latency / ttft_p50
- tokens (in/out/cache)
- gate denies

同任务对比不同 harness 时：固定 fixture、尽量同模型、非交互 auto-approve、N 次取中位。

## 对照 agent（后续）

1. One：原生 `--trace`  
2. 其他：导出 session/日志 → normalize 到同一 `TraceEvent` 子集（脚本即可，不必进 core）  
3. 输出并排 summary，按失败模式改 prompt / tool 描述 / compaction

## 相关代码

- `crates/one-core/src/trace.rs`
- `crates/one-core/src/agent.rs`（`set_trace`）
- `crates/one-cli/src/bench_cmd.rs` / `trace_cmd.rs`
- `crates/one-cli/src/runtime/build.rs`（`--trace` 装配）
