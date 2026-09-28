# Config Studio 全局配置界面调整

本轮直接修改 `/home/fxh/tools/one` 的已有 One Config Studio。任务初始目录 `remote-code` 不包含该页面；本轮没有修改 remote-code 的服务或任务生命周期。

## 使用方式

Agents 定义 Agent 的工作方式；Model Enhancers 修正模型行为；Effective Config 展示 Agent 定义与全局模型行为增强合成后的最终配置。

- 一级对象是 Agent 与 Enhancer，模型不常驻导航或列表页。
- Enhancer 列表展示行为说明、启用状态、适用模型数量与受影响 Agent。
- 详情支持搜索服务商/模型、多选、启用/禁用、选择 Agent 配置范围、恢复默认。搜索结果最多显示 40 条，支持继续缩小关键词。
- Research 等自定义 Agent 按其实际使用的配置归属显示影响范围；保留原有 preset 绑定语义，不新增伪 Agent 绑定字段。
- 默认仅显示全局配置。已移除项目选择器、写入层、项目覆盖操作和项目文档入口，旧项目深链接也不能通过当前 UI 访问项目文档。
- 来源链为 Built-in Default → Global Override → Effective。常规界面使用“内置默认 → 全局自定义 → 最终生效”。
- Agent 与最终配置默认显示可读预览。原始文本、JSON、增强文本编辑和编译 trace 位于高级入口。
- 桌面沿用现有视觉系统；390px 窄屏使用横向对象导航，模型仍只在详情中搜索。

## 实现与兼容

- `crates/one-web/web/src/config/ConfigStudio.tsx`：全局导航、默认预览、未保存修改离开提醒。
- `components/ModelEnhancers.tsx`：行为列表与详情、搜索多选、真实保存、影响范围、可读结果。
- `components/AgentPreview.tsx`：Agent 定义的真实编译预览，以及普通配置概览。
- `components/EffectivePanel.tsx`：移除项目覆盖操作，原始报告折叠到高级入口。
- UI 请求显式携带 `studio_global=1`；后端为这些请求限定文档目录和预览来源，避免“写全局但预览仍混入项目配置”。
- `prompt_compose` 与 enhancer resolver 增加显式来源链入口，沿用原有编译器。旧 API 请求和运行时仍保持原有行为；不迁移、删除或改写历史项目配置。
- 保存继续使用原有版本检查、备份与原子写入。多模型保存按版本顺序提交；部分失败会说明已保存数量，其余不自动重试。仅修改状态时保留每个模型已有的增强文本。
- 可读预览使用 React 文本节点，不执行配置中的 HTML；完整原文保留在高级入口。

## 验证

- `make build` 成功：新增 One 项目统一构建入口，依次执行前端 TypeScript/Vite 构建和 `cargo build -p one-cli --bin one`，产物 `target/debug/one` 嵌入最新前端。
- `cargo test -p one-cli --lib config_studio`：114 项通过。
- `cargo test -p one-cli --test enhancer_studio_test --test prompt_enhancer_test`：7 + 5 项通过。新增全局/项目隔离、旧运行时兼容、项目文档不可访问、全局 MCP 来源隔离回归。
- `cargo test -p one-web -p one-prompt`：24 项通过。
- Playwright 使用构建后的真实 One 服务、独立的临时 Agent 目录和项目目录验证：导航与默认文案、搜索多选、真实保存与刷新持久化、自定义增强进入真实预览、Research 可读预览、禁用保留文本但移除效果、恢复默认不影响其他模型、409 冲突提示持续可见。
- 1440px 桌面与 390px 窄屏截图已检查；无页面 JavaScript 错误，无页面横向溢出。
- 修改文件空白检查通过；构建保留仓库既有 Rust 未使用代码提示，无构建错误。

浏览器脚本：`/tmp/playwright-test-one-global.js`。

截图：`/tmp/one-global-ui/enhancers-desktop.png`、`effective-desktop.png`、`agent-desktop.png`、`enhancer-mobile.png`。

验证使用隔离的临时配置，不改用户全局配置。One 是 Rust 服务，不适用 remote-code 的 `./start-remotode.sh` Go 重启脚本；已启动并检查最新 One 二进制，未重启无关 Go 服务。此任务保持 ready_for_check，不自动 completed。
