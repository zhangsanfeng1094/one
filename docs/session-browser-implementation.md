# One Session Browser 实现报告

## 命令

```text
one session list [--all] [--query TEXT] [--limit N] [--json]
one --cwd PATH session list
one session show SPEC [--all] [--json]
one session tui SPEC [--all]
one session exec SPEC PROMPT... [--all]
```

`list` 默认显示当前 cwd，`--all` 枚举 One 已知 session root 下的所有项目。`--cwd` 沿用全局选项；与 `--all` 同时使用时，`--all` 选择跨项目范围。`--query` 对 id、name、preview、path、cwd 不区分大小写筛选。默认上限 50。`show` 的 `--all` 允许跨项目解析；完整 id 和路径优先，其他多命中返回含 cwd、修改时间、id、label 的候选清单。

## 状态和进入规则

`live_busy` / `live_idle` 仅在 Native Control 握手已验证 endpoint 的 pid、process_start，且 status 中的 session_id、session_path、cwd 都与 session 文件信息相符时显示。其他文件显示 `dormant`。lock/presence 不用于推断 live。列表的发现失败仍返回历史文件；进入命令若发现有效进程的 endpoint 但握手失败，则保守拒绝。

`tui` 只进入 dormant session，沿用 `cli.session` 的既有 TUI open 路径。`exec` 对 dormant 沿用既有 print/exec open 路径；live idle 且支持 prompt 时经 Native Control 向同一 runtime 发送，并附 expected session id/path；live busy 提示 `one control steer` / `one control followup`，不支持 prompt 时明确报错。live TUI 不热 attach，也不启动第二个 writer。

dormant entry 在运行前通过 `SessionLock::acquire` 取得唯一 OS 文件锁，`main` 持有 guard 直到 TUI/exec 退出。OS lock 是占用的唯一依据；取得锁后会覆盖旧 JSON，因此 PID 复用、dead/malformed 元数据均可回收。Drop 仅在 lock path 仍指向 guard 持有的 inode 时删除它。当前生产写入路径仍是 `SessionManager`，未改 `SessionActor` 架构。`one resume` 和 TUI `/resume` 保持原行为。

## 修改文件

- `crates/one-cli/src/cli.rs`, `main.rs`, `session_cmd.rs`：命令定义、路由、浏览与进入。
- `crates/one-cli/src/runtime/control.rs`：严格 endpoint 发现与 expected session prompt 校验。
- `crates/one-session/src/manager.rs`, `presence.rs`：跨已知项目列举和 session lock。
- `crates/one-cli/tests/session_browser.rs`, `native_control_e2e.rs`, `crates/one-session/tests/grok_session_features.rs`：CLI、live runtime、并发锁回归。
- `docs/cli.md`：用法示例。

## 验证

- `cargo fmt --all`、`cargo fmt --all -- --check`：通过。
- `cargo test -p one-session`：34 个测试通过。
- `cargo test -p one-cli --test session_browser`：4 个测试通过，包括并发 dormant entry、lock 生命周期与 stale 回收。
- `cargo test -p one-cli --test native_control_e2e`：默认并行 full suite 连续两遍各 5/5 通过，包括两个 live runtime 的状态与 prompt 路由。
- `cargo test -p one-cli --test rpc_concurrency`：1 个测试通过。
- `cargo test -p one-cli --bin one resume`：5 个筛选测试通过。
- `cargo check -p one-cli --bin one`、`cargo build -p one-cli --bin one`：通过。
- `git diff --check`（本次相关已跟踪文件）：通过。

已知限制：本次不实现 hot attach 或进程迁移；`one session exec` 发往 live runtime 的 prompt 由原 frontend 消费，CLI 只返回 Native Control 接收确认，不等待 agent 结果。

此前默认并行运行的一次失败发生在新增 live 用例的 `wait_for_busy_with_timeout`：`status` RPC 立即返回 socket `ENOENT`，并非 timeout 断言。新增用例现将两个 TUI 的启动及首轮 mock prompt 依次进行，并把 busy 阶段的 prompt 从 1200 字符缩至 900；两个 runtime 仍同时 live。此后默认并行 full suite 连续两遍通过。
