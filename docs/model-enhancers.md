# Model Enhancers

状态：实现与验证进行中；外部验收前只进入 `ready_for_check`，不自动标记 Task 完成。

打开 `one config --port 3335` 打印的链接，选择左侧 **提示词 / 模型增强**。
从模型目录选择具体模型，点击 StopsEarly 等条目的 **创建覆盖**，勾选启用、编辑文本并保存。
下方 Effective System Prompt 会重新编译。刷新仍保留；点击 **编辑覆盖 → 恢复继承 / 删除覆盖** 可删除当前层该项覆盖。
内置提示词始终只读。一个 enhancer 的修改或删除不会修改同一模型的其他 enhancer。

## 正式配置文件

- 全局：`~/.one/agent/prompts/enhancers.json`，遵循 `ONE_AGENT_DIR` / `ONE_DATA_DIR` 的 agent home 约定。
- 项目：`<cwd>/.one/prompts/enhancers.json`。页面直接显示绝对保存位置。
- 运行时还读取从 Git 根目录到 cwd 的祖先 `.one/prompts/enhancers.json`，由远到近覆盖。

优先级：内置文本和 registry quirks → 全局文件 → 项目祖先 → 当前项目。
每层中，具体 preset 绑定覆盖该模型的所有-profile 绑定。`enabled` 和 `prompt` 独立继承。
项目层优先于全局层，即使全局绑定更具体。

文件不存在表示无用户覆盖；不存在的字段继承下一层。
`enabled: false` 显式禁用；删除该 enhancer 对象恢复继承；删除 `prompt` 字段恢复继承文本但保留启用状态。
若低层还有覆盖，可切到对应层删除，最终回到内置文本与 registry 启用状态。
无效文件会报告带路径的错误，不静默回退默认值。

可复制 [示例](examples/model-enhancers.json)，格式见 [JSON Schema](schemas/model-enhancers.schema.json)。
例如修改 Gemini 的 StopsEarly 只需要：

```json
{
  "version": 1,
  "bindings": [
    {
      "provider": "gemini",
      "model": "gemini-2.5-pro",
      "enhancers": {
        "stops_early": {
          "enabled": true,
          "prompt": "继续执行所有必要步骤，完成验证后再报告结果。"
        }
      }
    }
  ]
}
```

`provider` 和 `model` 是 registry 的精确标识，不是 glob。Web UI 自动填写。
可选 `preset: "code"` 将绑定限定到该 preset；省略时对所有 profiles 生效。
第一版 UI 支持具体模型，没有编造模型系列列表。支持的 preset 来自现有 one-prompt registry，未新增空 Research/Video 模板。

## 编译与生效

正式 runtime、主 Agent、Task/harness 子 Agent 都通过 `compile_host` 读取配置，然后交给 **one-prompt** 编译。
每次编译从原始 PromptConfig 和当前 provider/model 重新解析，不从上一个模型的已合成文本追加。
保存立即用于预览；运行中的会话在 reload、模型切换或下一次编译时读取，新会话会直接读取。
每轮请求不会自动读取文件，因此正在执行中的请求不会被编辑页面改写。

所有四项增强注入稳定语义锚点 `behavior_hooks`。自定义 profile 没有该锚点时，host 补一个专用 behavior component；不会回退到 extra、首个 slot 或业务 slot。
原有 provider/model ModelRule DSL 保留。高级规则仍可显式修改 hook，trace 会显示它们的操作和规则编号。
Enhancer 的正文是 one-prompt body，沿用编译器变量语法；未知 `{{变量}}` 会产生编译错误。

Effective API `/api/config/prompts/preview` 使用与 runtime 相同的 `compile_host_resolved` 编译路径，返回本次编译实际使用的 enhancer 解析结果及每个 slot 的操作来源。
界面预览上下文明确为：所选内置 Prompt preset、act 模式、内置 main 工具目录、当前目录真实环境；不附加运行中会话的 resources、memory、MCP 或 extension 快照。
这是所选 profile/model/context 的真实编译结果，不是运行中会话的快照，也不会为预览调用模型。

Config Studio 的增强保存复用文档 API：校验 → 版本冲突检测 → 备份 → 原子替换。
Advanced 文档源码入口可直接编辑正式 JSON，保留备份/恢复能力。
普通模型调整路径不要求理解 slot、operation 或 provider glob。

## 边界

- AgentSpec 继续负责通用 Agent 能力并引用 PromptConfig，没有增加 enhancer 字段。
- PromptProfile 决定业务结构；Model Enhancer 只描述模型行为差异。
- ModelRegistry 的 quirks 继续提供默认绑定，独立配置提供用户覆盖。
- one-prompt 仍是最终编译器；one-ai/provider 的 WireCompat 仍负责协议兼容。

验证结果与逐项 fidelity check 见后续验收记录。
