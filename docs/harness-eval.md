# Harness 能力对比与埋点评测

> 目标：收集 agent **完整执行链路**，用固定任务打分，再和对照 agent 对比，驱动 harness 改进。  
> 原则：**非破坏** — Core 只增加可选 `TraceSink`；默认路径零开销；观测后端 **仅 Langfuse**。

## 快速使用

### 1. 配置 Langfuse

在 [Langfuse](https://langfuse.com) 项目设置里创建 API keys，然后：

```bash
export LANGFUSE_PUBLIC_KEY=pk-lf-...
export LANGFUSE_SECRET_KEY=sk-lf-...
# 可选（默认 EU cloud）
# export LANGFUSE_BASE_URL=https://cloud.langfuse.com   # EU
# export LANGFUSE_BASE_URL=https://us.cloud.langfuse.com # US
# export LANGFUSE_BASE_URL=http://localhost:3000         # self-host
# export LANGFUSE_TRACING_ENVIRONMENT=dev
# export LANGFUSE_RELEASE=0.1.0
```

`ONE_LANGFUSE=0` 可强制关闭（即使 keys 已设置）。

### 2. 打开轨迹

```bash
# 任意一次运行 → OTLP → Langfuse
one --trace -p "list files in current directory" -y --provider mock

# 含 LLM/tool I/O 预览（更大 payload）
one --trace --trace-full -p "…" -y

# 环境变量等价
ONE_TRACE=1 one -p "…" -y

# 别名
one --langfuse -p "…" -y
```

可选身份：

```bash
export LANGFUSE_USER_ID=alice          # 或 ONE_USER_ID；默认回退 $USER
export LANGFUSE_TRACING_ENVIRONMENT=dev
export LANGFUSE_RELEASE=0.1.0
```

传输：**OpenTelemetry OTLP/HTTP** → `{BASE}/api/public/otel/v1/traces`（官方非 SDK 语言推荐路径）。  
控制台可见：agent → turn(chain) → generation / tool；同一 One session 的多次 run 共享 `sessionId`；bench 会写 harness scores。

### 3. 跑 smoke 任务包（mock，可进 CI）

```bash
one bench --suite smoke
one bench --task mock-list-files
one bench --suite all --out ./benches/out/manual
```

- **有** `LANGFUSE_*` keys：每个 task 的事件与 harness score 写入 Langfuse  
- **无** keys：仅内存打分（不联网），适合离线 CI  

`suite=full` 的任务（如 `edit-marker`）需要真实模型才能过；当前 bench runner 默认仍走 **MockProvider**。对 full 任务请用：

```bash
one --trace --provider <real> -y -p "$(cat benches/tasks/edit-marker/prompt.md)" \
  --cwd /tmp/edit-ws
```

## 架构

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
  LangfuseTraceSink (OTEL)
         ├─ OTLP/HTTP  →  /api/public/otel/v1/traces
         └─ Scores API →  /api/public/scores
         ▼
  Langfuse UI（唯一观测面）
```

| 层 | 职责 |
|----|------|
| `one-core::trace` | `TraceEvent` schema、`TraceSink`、`MemoryTrace`、`TraceStats` |
| `Agent` | 可选 `set_trace` / `set_trace_meta`；loop 内埋点 |
| `one-cli::langfuse` | `LangfuseTraceSink` → OTEL OTLP + Scores API |
| `one-cli` | `--trace` / `--langfuse`、`one bench` |
| `benches/tasks/*` | 任务包：fixture + prompt + rubric |

**不**改：`AgentEvent` 语义、session 格式、crate 依赖方向、默认运行行为。

## 与 Langfuse 官方对齐

| 官方项 | One 现状 |
|--------|----------|
| Env：`LANGFUSE_PUBLIC_KEY` / `SECRET_KEY` / `BASE_URL` | ✅（`LANGFUSE_HOST` 别名） |
| Auth：Basic `pk:sk` | ✅ |
| **OTLP `/api/public/otel/v1/traces`** | ✅ **当前路径** |
| `x-langfuse-ingestion-version: 4` | ✅ |
| `langfuse.observation.type` agent/chain/generation/tool | ✅ |
| `gen_ai.*` model + usage | ✅ |
| `langfuse.observation.usage_details` | ✅ |
| 短生命周期 `force_flush` / shutdown | ✅ |
| Scores API（BOOLEAN 0/1 + NUMERIC） | ✅ flush OTLP → POST scores → join workers |
| Trace 属性传播到所有 span | ✅ session/user/tags/metadata/release/env |
| `langfuse.trace.tags` 为 string[] | ✅ OTEL string array |
| `sessionId` | ✅ One session header id / bench 每次独立 id |
| `userId` | ✅ `LANGFUSE_USER_ID` / `ONE_USER_ID` / `USER` |
| Generation I/O 文本 | ✅ 默认仅长度；`--trace-full` 上报预览（至 16k 字符） |
| Experiments dataset 属性 | ⚠️ 未接；bench 用 tags + harness scores |

## Trace 事件 → OTEL / Langfuse

| `TraceEvent` | OTEL span | Langfuse 映射 |
|--------------|-----------|---------------|
| `run_start` | root span | observation **agent** |
| `run_end` | end root | status / output / tokens |
| `turn_start` | child | observation **chain** |
| `llm_request` | child | observation **generation**（开始） |
| `llm_response` | end generation | model + `gen_ai.usage.*` |
| `tool_start` / `tool_end` | child | observation **tool** |
| `gate` | span event | event on tool/turn |
| `compaction` | child span | span |
| `score` | — | `POST /api/public/scores` |

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

有 `LANGFUSE_*` 时，`./benches/run.sh full` 会自动加 `--trace`。

### 对照 agent 协议（Codex / 其它）

| 项 | 约定 |
|----|------|
| Fixture | 每次独立拷贝 stock 树，互不污染 |
| Prompt | 同一 `benches/tasks/<id>/prompt.md` |
| 打分 | 只认 `cargo test` exit 0（不认 agent 自述） |
| One | 白盒 TraceEvent → Langfuse |
| Codex | `codex exec --json` 事件流 + session JSONL + wall_ms |
| 公平性 | 都在临时 workspace、可写 sandbox、无 SOLUTIONS.md |

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
- 产物：`result.txt`（仅 `pass`/`fail`）、`score.log` / `score.json`；One 链路指标看 Langfuse

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

从 Langfuse UI / Scores：

- **pass rate**（主，`harness_pass` score）
- turns / tool_calls / tool errors（span 元数据）
- wall_ms / llm latency / ttft
- tokens (in/out/cache)
- gate denies

同任务对比不同 harness 时：固定 fixture、尽量同模型、非交互 auto-approve、N 次取中位。

## 相关代码

- `crates/one-core/src/trace.rs`
- `crates/one-core/src/agent.rs`（`set_trace`）
- `crates/one-cli/src/langfuse.rs`
- `crates/one-cli/src/bench_cmd.rs`
- `crates/one-cli/src/runtime/build.rs`（`--trace` 装配）
