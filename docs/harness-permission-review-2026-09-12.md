# Harness 权限审查（2026-09-12）

结论：**REQUEST CHANGES，权限尚未闭环。** 主执行链已有 gate、交互审批、非交互拒绝和工具路径检查，但授权范围能被 cwd、子代理和前缀缓存扩大。“有审批界面”还不能保证实际执行不越权。

范围：当前工作区代码，包含已有暂存和未暂存修改；按用户要求审查现状，不只评判本次 diff。以下为静态调用链确认的问题，未在宿主机执行越权写入。未修改实现。

## 主要发现

### 1. [P1] bash 的持久 cwd 会自动变成新的可写根

位置：`crates/one-tools/src/bash.rs:200`、`:992`；`crates/one-tools/src/os_sandbox.rs:121`。

`apply_persist` 只检查目录存在，保存 shell 结束时的 cwd；下一次调用将其写入 `sandbox.cwd`，而 `command_line` 无条件把 cwd 加入 writable roots。由于整个宿主文件系统已经只读挂载，首次 `cd /某个工作区外目录` 可以成功；第二次命令就能向这个目录写文件，不需要 require_escalated。宿主用户自身的权限仍然适用。

修复：执行 cwd 与授权 roots 分开保存。cwd 可以变化，但不得因此新增 RW bind；新增可写根必须经过授权。回归测试应执行两次调用：第一次切换至未授权目录，第二次写入仍失败。

### 2. [P1] task 的 cwd 参数允许子代理自行扩大权限

位置：`crates/one-cli/src/runtime/effective_config.rs:129`；`runtime/task_tool.rs:515`；`runtime/harness.rs:519`、`:537`。

模型传入的 cwd 经 `base_cwd.join` 接受，绝对路径和 `..` 都没有父权限边界校验。task 仅检查与 worktree 的互斥，harness 随后直接以该目录创建新的 `PathPolicy::workspace`。因此指定工作区外 cwd，子代理就获得该目录的读写范围；只读子代理也会扩大读取范围。父 gate 的路径阶段不检查 task.cwd。

修复：从父策略派生子策略，权限只能缩小；cwd 仅决定相对路径解析，不决定权限。worktree 由宿主显式授予其新建目录，resume 同样验证继承的 cwd。

### 3. [P1] 前缀授权可以吞掉复合命令，并跳过显式 deny

位置：`crates/one-tools/src/permissions.rs:370`；`crates/one-cli/src/approval.rs:503`。

`command_matches_prefix` 只做字符串前缀加空格匹配，不解析 shell。例如批准 `cargo test` 前缀后，`cargo test ; <另一条命令>` 也匹配；命令替换和重定向也不会单独检查。匹配后 gate 立即 Allow，甚至不会调用本应 deny 优先的 `evaluate_with_mode`。如果缓存的是 escalated 前缀，后续附加命令也在无 bwrap 环境执行。

修复：deny 和硬限制始终先于缓存授权；对复合语法保守拒绝前缀复用，或逐条解析并检查命令、替换和重定向。增加“已批准前缀 + 显式 deny 后缀命令”的测试。

### 4. [P1] 子代理丢失父级配置的 deny / ask 规则

位置：`crates/one-cli/src/runtime/build.rs:146`；`runtime/harness.rs:207`。

主代理从 settings 加载权限规则，harness 却构造 `PermissionRules::default()`。父级禁止的某个文件读取、命令或工具，在子代理工具集中存在时可能重新变成允许；DontAsk 只能拒绝剩余规则产生的 Ask，无法恢复已经丢失的 deny。

修复：把有效规则快照传入 HarnessOptions，并与子代理限制取更严格结果。不要在叶子执行器重新从空规则开始。

### 5. [P1] 缺少 bwrap 时默认放行无沙箱执行

位置：`crates/one-tools/src/os_sandbox.rs:85`。

workspace-write 模式下找不到 bwrap，只打印警告便直接运行 `bash -lc`。此时路径工具的 PathPolicy 无法限制 shell 中的写入，用户也没有批准这次沙箱降级。后续结果中的 UNSANDBOXED 提示晚于执行，不能替代授权。

修复：无法建立沙箱时拒绝执行；交互环境可请求明确的本次降级批准，非交互直接返回错误。显式 full-access 可保留现有行为。

### 6. [P1] 路径“仅一次”批准实际成为长期授权

位置：`crates/one-cli/src/approval.rs:301`、`:330`；`crates/one-tools/src/path_policy.rs:248`、`:276`、`:305`。

Once 调用 `grant_read_path` / `grant_write_path`，写入共享 DynamicGrants，没有消费或撤销逻辑。批准一次 write 后，同一路径的后续 edit/write 无须再问；目录授权覆盖后代。读取授权还会经 export_read_grants 传给子代理。当前测试只证明共享 policy 能看到授权，没有验证用完失效。

修复：Once 使用绑定调用 ID、规范化路径、访问类型的一次性执行凭证，不写入 session grants；仅 Session 可导出继承。失败、取消和重复调用也应验证凭证生命周期。

### 7. [P1] 路径别名在执行层有效，在权限规则层失效

位置：`crates/one-tools/src/permissions.rs:207`；`crates/one-tools/src/tool_args.rs` 的 `path_arg`。

工具接受 `path`、`file_path`、`filePath`，但 ParsedRule 仅读取 `path`。例如工作区内配置 `deny: ["read(.env)"]`，`read({"file_path":".env"})` 的规则匹配对象为空，随后路径边界允许该工作区文件，执行层再解析别名完成读取。路径边界正确不等于细粒度规则生效。

修复：先用共享解析器生成规范化调用，再进行规则匹配和执行；同时统一相对/绝对路径匹配语义，避免同一目标仅换表示方式就绕过规则。

## 已有的闭环部分与边界

- Agent 在执行前调用 gate；扩展正常执行链先改写参数，再交给权限 gate。
- 非交互 Ask 有拒绝路径；交互有审批反馈和取消 pending 的实现。
- 路径工具除 gate 外还做 PathPolicy 检查；read grant 与 write grant 有区分。
- escalated 和普通命令的授权缓存有区分。

这些是有效基础，但上面的跨调用、跨代理和失效分支会绕过它们。另外，OS sandbox 明确全盘只读挂载并保留网络；若产品承诺“工作区外不可读”或网络隔离，当前 bash 并不提供这种保证，应统一产品语义和实现。

## 验证与修复顺序

已运行：

- `cargo test -p one-tools --lib permissions -- --test-threads=1`：17 passed。
- `cargo test -p one-cli approval::tests -- --test-threads=1`：24 passed；编译存在 warnings，无测试失败。

测试通过只能证明既有断言成立。本次未新增漏洞复现测试，未运行完整 harness、ACP 或真实 bwrap 端到端测试，因此不作整体安全认证。

建议先修复 cwd 扩权、前缀复用和沙箱 fail-open，再统一父子权限快照、参数规范化与 Once 生命周期。验收标准应是：**任何实际执行的权限，必须能追溯到仍然有效且覆盖该操作的授权；工具改写、重试、后台化、子代理和恢复都不得扩大它。**
