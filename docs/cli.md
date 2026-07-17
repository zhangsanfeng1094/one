# CLI 参考

## 基本用法

```bash
one                          # 交互模式（自动创建 session）
one -p "explain this repo"   # print 模式
one --continue / -c          # 继续最近 session
one --resume / -r            # 交互：打开 session 选择器；非交互：最近 session
one --session PATH           # 打开指定 session 文件
one --no-session             # 不持久化
one --read-only              # 只启用 read/grep/find/ls
one --plan                   # 以 Plan 模式启动（探索 + 写计划，/act 后实现）
one --cwd /path/to/project   # 指定工作目录（workspace 根）
one --add-dir /other/path    # 额外可读写目录（可重复）
one --full-access            # 关闭路径边界（危险；仅容器/可信环境）
one -n "refactor auth"        # 设置 session 名称
one -y                       # 自动批准高风险 bash 命令（不关闭路径边界）
one --export out.html        # 导出 session 为 HTML
one --share                  # 上传 session 到 GitHub Gist（需 GITHUB_TOKEN）
one --list-models            # 列出可用模型
one --list-providers         # 列出内置 + models.json 自定义 provider

# 订阅登录
one login                    # 交互选择：Codex / xAI Grok / OpenCode …
one login openai-codex       # ChatGPT Plus/Pro OAuth
one login xai                # SuperGrok / X Premium+ OAuth（别名 grok）
one login xai --browser      # 仅浏览器 PKCE（localhost:56121）
one login xai --device-code  # 无头 device code
one login opencode           # OpenCode Zen（控制台 API key）
one login opencode-go        # OpenCode Go 订阅（可 import CLI auth.json）
one logout opencode-go
one logout --all
one --provider openai-codex -p "hello"
one --provider opencode-go --model deepseek-v4-flash -p "hello"
```

凭证存 `~/.one/agent/auth.json`（`0600`）。Codex OAuth 过期自动 refresh。OpenCode 与 `OPENCODE_API_KEY` 共用。

交互模式：`/login`（弹出选择）· `/login opencode-go` · `/logout` · `/model opencode-go:deepseek-v4-flash`

## 权限与路径沙箱

默认 **`workspace-write`**：`read` / `write` / `edit` / `grep` / `find` / `ls` 只能访问：

| 范围 | 权限 |
|------|------|
| `--cwd`（工作区根） | 读 + 写 |
| `--add-dir` / settings `additional_directories` | 读 + 写 |
| `~/.one/agent`（plans / builtin-skills / one skills） | **仅读** |
| `~/.agents/skills`（跨客户端通用 skill 安装位） | **仅读** |
| `~/.codex/skills` · `~/.claude/skills` · `~/.grok/skills`（兼容） | **仅读** |
| 已发现 skill 的 package 目录（含 symlink 真实路径） | **仅读** |
| 其它绝对路径 / `../` 逃逸 | **拒绝** |

```bash
# 默认：只能改当前项目
one --cwd ~/code/myapp

# 允许再写一个共享目录
one --add-dir ~/shared/proto

# 关闭边界（等同 Codex danger-full-access）
one --full-access
```

| 标志 / 设置 | 作用 |
|-------------|------|
| （默认） | 工作区路径硬边界 |
| `--add-dir DIR` | 扩展可读写根 |
| `--full-access` | 关闭路径边界 |
| `-y` / `auto_approve` | 仅跳过**高危 bash** 确认，不放宽路径 |
| `--read-only` | 去掉写工具与 bash |
| `--plan` | 只能写 plan 文件 |

持久化（`~/.one/agent/settings.json`）：

```json
{
  "sandbox": "workspace-write",
  "additional_directories": ["/home/me/shared"]
}
```

交互：`/settings sandbox full-access` · `/settings add_dir /path/a,/path/b`

### 交互审批（人在环）

高危 bash（如 `sudo`、`rm -rf`、`git push`）或命中 `ask` 规则时，TUI 弹出 **列表式单选**（Codex 风格）：

| # | 选项 | 语义 |
|---|------|------|
| 1 | Yes, and don't ask again for anything | 本进程剩余时间 always-approve（不写 disk） |
| 2 | Yes, proceed | 仅本次 |
| 3 | Yes, and don't ask again for this | 同 fingerprint 本进程免问 |
| 4 | No, reject (type to add feedback) | 拒绝；可键入反馈给模型 |

快捷键：`↑/↓` 或 `1–4` 移动 · `Enter` 确认 · `Ctrl+o` 直接 Always · `y`/`a`/`n` 兼容 · `Esc` 取消。

| 模式 | 行为 |
|------|------|
| Interactive | 上表列表选择 |
| Print / JSON / RPC | 直接拒绝（除非 `-y` / `ONE_AUTO_APPROVE=1`） |

### ask_user（澄清问题）

模型可调用 `ask_user` 做结构化单选/多选（对齐 Claude `AskUserQuestion`）：

```json
{
  "questions": [
    {
      "question": "Which test runner?",
      "header": "Runner",
      "options": [
        { "label": "Jest", "description": "…" },
        { "label": "Vitest", "description": "…" }
      ],
      "multi_select": false
    }
  ]
}
```

- 1–4 题，每题 2–4 选项；`multi_select: true` 为多选（Space 勾选）。
- 始终可走 **Other** 键入自由文本；**Tab** 直接跳到 Other 并进入输入。
- 快捷键：`↑/↓` 或 `1–n` 选中 · `Enter` 确认 · `Tab` Other · `Esc` 取消。
- 仅 Interactive 可用；print/RPC 下 fail-closed。

### 细粒度规则

`settings.json`：

```json
{
  "permissions": {
    "allow": ["Bash(cargo *)", "Bash(git status*)"],
    "deny": ["Bash(git push *)"],
    "ask": ["Write(**/.env*)", "Bash(rm *)"]
  },
  "bash_sandbox": true
}
```

规则语法：`Tool` 或 `Tool(specifier)`，`*` 通配。求值顺序：**deny → ask → allow → 内置默认**。

交互追加：

```text
/settings allow Bash(cargo test *)
/settings deny Bash(git push *)
/settings ask Write(**/.env*)
```

### Bash OS 沙箱（bubblewrap）

`workspace-write` 下 bash 默认经 `bwrap` 启动：

- 工作区 + `--add-dir`：**读写**
- `$HOME` / 系统路径：**只读**
- 网络：保留（cargo/npm/curl）
- `--full-access` 或 `bash_sandbox: false` / `ONE_BASH_SANDBOX=0`：关闭


## Provider 与模型配置

```bash
one --provider mock          # 默认，本地测试
one --provider ollama        # 本地 Ollama
one --provider anthropic     # ANTHROPIC_API_KEY
one --provider openai        # OPENAI_API_KEY
one --provider openrouter    # OPENROUTER_API_KEY
one --provider deepseek      # DEEPSEEK_API_KEY（OpenAI-compat）
one --provider gemini        # GEMINI_API_KEY 或 GOOGLE_API_KEY

one --model gpt-4o           # 模型 id（-m）
one --base-url https://api.openai.com/v1
one --api-key sk-...
one --openai-api openai-responses   # 或 openai-completions / anthropic-messages / gemini-generate-content
```

### 配置优先级（高 → 低）

| 字段 | CLI | models.json | 环境变量 | 默认 |
|------|-----|-------------|----------|------|
| provider | `--provider` | — | — | `mock` |
| model | `--model` / `-m` | provider 下第一个 model / entry | `OPENROUTER_MODEL` 等 | 见下表 |
| baseUrl | `--base-url` | `providers.*.baseUrl` 或 model `baseUrl` | `OPENAI_BASE_URL` / `OLLAMA_HOST` / … | 官方 URL |
| api / providerType | `--openai-api` | `api` 或 `providerType`（固定枚举） | `ONE_OPENAI_API` | openai→responses，anthropic→messages，gemini→generate-content，其它→completions |
| apiKey | `--api-key` | `apiKey`（支持 `$ENV`） | `OPENAI_API_KEY` 等 | — |

默认 model：

| provider | default model |
|----------|----------------|
| mock | `mock-v1` |
| openai | `gpt-4o` |
| anthropic | `claude-sonnet-4-20250514` |
| ollama | `llama3.2` |
| openrouter | `anthropic/claude-sonnet-4` |
| deepseek | `deepseek-chat` |
| gemini | `gemini-2.5-flash` |

### 统一设置 `~/.one/agent/settings.json`

持久化 interactive 偏好（也会从旧 `preferences.json` 迁移）：

```json
{
  "provider": "deepseek",
  "model": "deepseek-chat",
  "thinking": "off",
  "auto_approve": false,
  "context_window": 128000,
  "sandbox": "workspace-write",
  "additional_directories": []
}
```

交互内：

```text
/settings                  # 查看
/settings thinking high    # 写入并立即生效（thinking）
/settings auto_approve true
```

### Wire protocol（请求/响应编解码）

`api` / `providerType` 是**固定枚举**（TUI 里点选，不可自由填写）。选中后决定如何拼装请求、解析流式/非流式响应：

| 值 | Endpoint | 说明 |
|----|----------|------|
| `openai-responses`（OpenAI 默认） | `POST {baseUrl}/responses` | 官方 OpenAI Responses |
| `openai-completions` | `POST {baseUrl}/chat/completions` | 最广兼容（Ollama / 代理 / DeepSeek） |
| `anthropic-messages`（Anthropic 默认） | `POST {baseUrl}/v1/messages` | Anthropic Messages（Claude） |
| `gemini-generate-content`（Gemini 默认） | `POST {baseUrl}/models/{model}:generateContent` | Gemini 原生（含 SSE stream） |

别名（写入时会规范成上表）：`openai-compatible` → `openai-completions`；`anthropic` → `anthropic-messages`；`gemini` → `gemini-generate-content`。

```bash
# Responses（默认）
export OPENAI_API_KEY=sk-...
cargo run -p one-cli --features http-providers -- --provider openai

# Completions + 自定义 base
cargo run -p one-cli --features http-providers -- \
  --provider openai \
  --openai-api openai-completions \
  --base-url http://127.0.0.1:8000/v1 \
  --model my-local-model

# Gemini 原生
export GEMINI_API_KEY=...
cargo run -p one-cli --features http-providers -- --provider gemini

# 任意自定义 provider 走 Anthropic / Gemini 协议
# models.json: { "api": "anthropic-messages", "baseUrl": "https://proxy/…", … }
# models.json: { "api": "gemini-generate-content", "baseUrl": "https://generativelanguage.googleapis.com/v1beta", … }
```

### `~/.one/agent/models.json`（推荐，对齐 Pi）

**Pi 风格 `providers` 块：**

```json
{
  "providers": {
    "openai": {
      "baseUrl": "https://api.openai.com/v1",
      "api": "openai-responses",
      "apiKey": "$OPENAI_API_KEY",
      "models": [
        { "id": "gpt-4o", "name": "GPT-4o", "context_window": 128000 },
        { "id": "gpt-4o-mini", "name": "GPT-4o mini" }
      ]
    },
    "ollama": {
      "baseUrl": "http://127.0.0.1:11434/v1",
      "api": "openai-completions",
      "apiKey": "ollama",
      "compat": {
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false
      },
      "models": [
        { "id": "llama3.2" },
        { "id": "qwen2.5-coder:7b" }
      ]
    },
    "my-proxy": {
      "baseUrl": "https://proxy.example.com/v1",
      "api": "openai-completions",
      "providerType": "openai-completions",
      "apiKey": "$MY_PROXY_KEY",
      "compat": {
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false,
        "thinkingFormat": "openrouter",
        "maxTokensField": "max_tokens",
        "openRouterRouting": { "only": ["anthropic"] }
      },
      "models": [
        {
          "id": "gpt-4o",
          "name": "Proxied GPT-4o",
          "reasoning": true,
          "compat": {
            "supportsReasoningEffort": true
          }
        }
      ]
    },
    "claude-proxy": {
      "baseUrl": "https://api.anthropic.com",
      "api": "anthropic-messages",
      "apiKey": "$ANTHROPIC_API_KEY",
      "models": [
        { "id": "claude-sonnet-4-20250514", "name": "Claude Sonnet 4" }
      ]
    },
    "gemini": {
      "baseUrl": "https://generativelanguage.googleapis.com/v1beta",
      "api": "gemini-generate-content",
      "apiKey": "$GEMINI_API_KEY",
      "models": [
        { "id": "gemini-2.5-flash", "name": "Gemini 2.5 Flash" },
        { "id": "gemini-2.5-pro", "name": "Gemini 2.5 Pro" }
      ]
    }
  }
}
```

**扁平 legacy 列表仍然支持：**

```json
{
  "models": [
    {
      "provider": "openai",
      "id": "gpt-4o",
      "api": "openai-responses",
      "baseUrl": "https://api.openai.com/v1"
    }
  ]
}
```

`apiKey` 支持：

- 字面量：`"sk-..."`
- 环境变量：`"$OPENAI_API_KEY"` 或 `"${OPENAI_API_KEY}"`

### `compat`（对齐 Pi models.json）

Provider 级与 model 级均可写 `compat`；**model 覆盖 provider**，未写字段走 **URL/provider 自动探测**（与 Pi `detectCompat` 同思路）。

**OpenAI Chat Completions 常用字段：**

| 字段 | 作用 |
|------|------|
| `supportsDeveloperRole` | system 用 `developer` 还是 `system`（本地 Ollama 常 `false`） |
| `supportsReasoningEffort` | 是否发 `reasoning_effort` |
| `supportsUsageInStreaming` | 是否发 `stream_options.include_usage` |
| `supportsStore` | 是否发 `store: false` |
| `maxTokensField` | `max_completion_tokens` \| `max_tokens` |
| `thinkingFormat` | `openai` / `openrouter` / `deepseek` / `together` / `zai` / `qwen` / `chat-template` / `qwen-chat-template` / `string-thinking` / `ant-ling` |
| `requiresToolResultName` | tool result 是否带 `name` |
| `requiresAssistantAfterToolResult` | tool 后插入假 assistant 再发 user |
| `requiresThinkingAsText` | thinking 折进 text，不发 `reasoning_content` |
| `requiresReasoningContentOnAssistantMessages` | 无 thinking 时仍回放空 `reasoning_content`（DeepSeek） |
| `supportsStrictMode` | tools 是否带 `strict` |
| `openRouterRouting` | 原样写入 body `provider`（OpenRouter 路由） |
| `vercelGatewayRouting` | Vercel AI Gateway `providerOptions.gateway` |
| `chatTemplateKwargs` | `thinkingFormat: chat-template` 时的 kwargs（支持 `{ "$var": "thinking.enabled" }`） |
| `zaiToolStream` | 发 `tool_stream: true` |
| `cacheControlFormat` | `"anthropic"` 时在 system / 末条消息 / 末个 tool 上打 `cache_control`（OpenRouter Claude 等） |
| `supportsLongCacheRetention` | `true` 时 `cache_control.ttl = "1h"`（否则默认 5m ephemeral） |
| `sendSessionAffinityHeaders` | 发送 `x-session-affinity` / `x-session-id` 粘性头（Fireworks 等） |

### Prompt cache 调试（默认开启）

每次 LLM 调用会写入 `~/.one/agent/cache-debug/`（无需环境变量）。

```bash
one --provider anthropic
# 多轮聊两句后：
cat ~/.one/agent/cache-debug/latest.json
tail -n 5 ~/.one/agent/cache-debug/log.jsonl
```

| 文件 | 说明 |
|------|------|
| `latest.json` | **最后一次**请求/响应（分析 + usage + body 摘要） |
| `log.jsonl` | 每次 LLM 调用追加一行 |

关闭：`ONE_DEBUG_CACHE=0`。改目录：`ONE_DEBUG_CACHE_DIR=/tmp/one-cache-debug`。

重点字段：`analysis.breakpoints`、`usage.cache_read_tokens` / `cache_write_tokens`、`hint`。

### 工具输出截断（Pi + Claude 风格）

| 规则 | 默认 |
|------|------|
| 进模型的行数上限 | 2000 行 |
| 进模型的字节上限 | 50KB |
| bash 超大输出（Claude spill） | 超过 **30000 字符**（或 50KB）时，**全文写入** `~/.one/agent/tool-outputs/`，模型只看到 **开头 ~4KB 预览 + 文件路径** |
| 可调 | `BASH_MAX_OUTPUT_LENGTH` 或 `ONE_BASH_MAX_OUTPUT_LENGTH`（1000–150000） |
| read 大文件 | `PARTIAL view` 提示 + `offset`/`limit` 继续读 |

该路径在 `~/.one/agent` 下，默认 **只读** 对模型开放，可用 `read`/`grep` 再取全文。

### 上下文压缩（compaction harness）

| 规则 | 默认 |
|------|------|
| 自动压缩阈值 | **模型 `context_window` 的 70%**（未知窗口时回退 **80 000** tokens） |
| Token 估算 | 优先用上次 API 返回的 prompt size；否则 messages 字符数 / 4 |
| 保留最近消息 | 12 条（不拆断 tool_call / tool_result 对） |
| 手动 | `/compact [instructions]` |
| Overflow 恢复 | API 报 context 过长 → force compact 后重试一次 |

**模型字段 `reasoning: true`**：声明支持 extended thinking；影响 `developer` role 与部分 reasoning 回放逻辑。交互里可用 `/model-add … reasoning=true`。

**Anthropic Messages 字段（同 `compat` 对象）：**

| 字段 | 作用 |
|------|------|
| `forceAdaptiveThinking` | `thinking.type: adaptive` + `output_config.effort` |
| `allowEmptySignature` | 空 signature 仍按 thinking 块回放 |
| `supportsEagerToolInputStreaming` | tools 上 `eager_input_streaming`；`false` 时用 legacy beta header |
| `supportsCacheControlOnTools` | 末个 tool 打 `cache_control`（默认 `true`） |
| `supportsLongCacheRetention` | system/tools/消息 breakpoint 使用 1h TTL |
| `sendSessionAffinityHeaders` | 请求带 `x-session-affinity` |

示例（本地 Ollama）：

```json
{
  "providers": {
    "ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "ollama",
      "compat": {
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false
      },
      "models": [
        {
          "id": "gpt-oss:20b",
          "reasoning": true,
          "thinkingLevelMap": {
            "minimal": null,
            "low": null,
            "medium": null,
            "high": "high",
            "xhigh": null,
            "max": "max"
          }
        }
      ]
    }
  }
}
```

**`thinkingLevelMap`**（模型级）：把 agent 的 off/low/medium/high 映射成厂商字符串；`null` 表示该档不支持（请求侧跳过）。也可用紧凑写法：`high=high,max=max,xhigh=null`。

**Settings UI（Ctrl+G → Providers → 某 provider）**

| 区 | 操作 |
|----|------|
| Connection | protocol / base_url / api_key / default_model |
| **Compat (Pi)** | `thinkingFormat` 选择、`maxTokensField` 选择、各 bool **Enter 循环** auto→true→false、Clear overrides |
| Models → model | `reasoning` 循环、`thinkingLevelMap` 行内编辑、模型级 compat 覆盖 |

CLI / slash 等价：

```text
/provider set ollama supportsDeveloperRole false
/provider set ollama thinking_format openai
/model-set ollama:gpt-oss reasoning true
/model-set ollama:gpt-oss thinking_level_map high=high,max=max,xhigh=null
```

Detect 会根据 provider id / baseUrl 自动推断（ollama、openrouter、deepseek、groq、fireworks、moonshot、lmstudio、vllm、siliconflow、minimax、huggingface…）；显式 `compat` 字段优先。

### 交互 UI 分层（对齐 Claude Code / Codex）

| 层 | 打开方式 | 用途 |
|----|----------|------|
| **输入框上方 `/` 列表** | 输入 `/`（边打边筛） | slash 命令：session / plan / compact…；↑↓ 选、Enter 执行 |
| **输入框上方 Select** | **Ctrl+L**、裸 `/model`；权限 / ask_user | 选模型等 |
| **屏幕中间弹窗** | **Ctrl+G**、裸 `/settings` | Settings 层级配置 |

Settings 层级（无独立 Models 入口；Add model **不离开** Settings）：

```
Settings
├ General (thinking / sandbox / …)
└ Providers
   └ <provider>          ← connection + Models
      └ Models
         ├ + Add model   ← 表单：id* + name / context_window
         └ <model>       ← name / context_window / 删除
```

模型字段（`base_url` / `api` 只在 **Provider** 层配置）：

| 字段 | 必填 | 说明 |
|------|------|------|
| `id` | ✅ | 模型 id |
| `name` | | 显示名（默认 = id） |
| `context_window` | | 上下文窗口 token 数 |

返回上一级：**Esc** / **←** / 空搜索时 **Backspace**。  
切换当前会话模型仍用 **Ctrl+L**（输入框上方 select）。  
配置写入 `~/.one/agent/models.json`（首次保存设 `includeDefaults: false`）。

快捷键：

- **Ctrl+L** — 切换 active model（输入框上方）
- **Ctrl+G** — Settings 居中面板
- **Ctrl+F** — 在 Provider 详情 / Models 列表：调用 OpenAI 兼容 `GET {baseUrl}/models` 拉取后**批量写入** `models.json`（Enter 选「Fetch & import remote models」等价；若终端占用 Ctrl+F，用菜单项）
- **Esc / ←** — Settings 内返回上一级；根级关闭

### 常用环境变量

| 变量 | 作用 |
|------|------|
| `OPENAI_API_KEY` | OpenAI key |
| `OPENAI_BASE_URL` / `OPENAI_API_BASE` | OpenAI base URL |
| `ONE_OPENAI_API` | `openai-responses` / `openai-completions` |
| `ANTHROPIC_API_KEY` | Anthropic key |
| `OPENROUTER_API_KEY` | OpenRouter key |
| `OPENROUTER_BASE_URL` | OpenRouter base（可选） |
| `OLLAMA_HOST` | 如 `http://127.0.0.1:11434` |

## 运行模式

### Interactive

```bash
one
# 或
cargo run -p one-cli --features http-providers -- --provider openai -m gpt-4o
```

快捷键：

- `Enter`：发送消息
- `Ctrl+J` / `Shift+Enter`：多行换行
- `Alt+Enter`：follow-up
- `Ctrl+S`：steer（运行中）
- `Shift+Tab`：切换 **Plan / Build** 模式
- Thinking 深度：`/settings thinking <off|low|medium|high>` 或 `/thinking`（无快捷键）
- `Ctrl+T`：展开/折叠全部 thinking 正文（**默认折叠**为 `▸ thinking · N chars`；流式输出时仍显示末 3 行 tail；点击或 ↵ 可单独展开/折叠一块）
- `↑` / `↓` 或 `Ctrl+P` / `Ctrl+N`：切换之前提交过的提示词（**按项目持久化**，新 session / 重启进程仍可召回；来自 `~/.one/agent/sessions/--cwd--/prompt_history.jsonl`，首次会从历史 session 的用户消息播种）
- `Esc`：输入非空时**立刻**清空草稿并记入 ↑ 历史；输入为空时再按一次 `Esc`（约 0.9s 内）打开 **当前 session** 的 rewind 菜单（conversation-only，不含代码 checkpoint）
- `/`：输入框**上方**命令列表（边打边筛 · ↑↓ 选择 · Enter 执行；同 Claude Code）
- `Alt+H`：打开 Help 目录（同 `/help`；有草稿也能开；`Ctrl+K` / `F1` / `Ctrl+/` 仍兼容）
- `Ctrl+L`：模型 select（输入框上方）
- `Ctrl+G`：Settings 居中面板
- `PageUp` / `PageDown`：滚动对话记录
- `Esc`：中止生成（运行中）；关闭浮层
- `Ctrl+C`：渐进退出（防误触）——浮层打开时先关浮层；输入非空时先清空；其余情况需再按一次才退出（busy 下第二次为强制退出，不会当成“取消生成”）
- `/quit`：强制退出
- `Tab`：路径 / `@file` 补全
- `@path`：发送时注入文件内容

Slash 命令：

| 命令 | 说明 |
|------|------|
| `/help` | 显示帮助 |
| `/session` | 当前 session 路径 / 名称 / 消息数 |
| `/resume [id\|name\|file]` | 列出或打开历史 session（无名称时显示首条用户消息） |
| `/new` | 新建 session |
| `/name <title>` | 设置 session 显示名（优先于首条消息预览） |
| `/tree` / `/tree <id>` | 列出或切换分支 |
| `/rewind` / `/rewind <id>` | 回退到某条用户提示并重新编辑（同 `Esc Esc`） |
| `/model [provider[:model]]` | 切换模型；裸 `/model` 打开**输入框上方** select（同 **Ctrl+L**） |
| `/settings [key value]` | 裸命令打开**居中 Settings**（同 **Ctrl+G**）；带参则写 settings.json |
| `/thinking [off\|low\|medium\|high]` | 设置或循环 thinking |
| `/plan` | 进入 Plan 模式（只读探索 + 写 plan 文件） |
| `/act` / `/build` | 批准计划并切到 Build 模式开始实现 |
| `/compact [instructions]` | 手动压缩上下文（LLM 摘要优先） |
| `/settings [key value]` | 查看或写入统一设置 |
| `/skills [enable\|disable <name>]` | 管理 skills：裸命令打开开关面板；`enable`/`disable` 按名称切换 |
| `/skill:name [args]` | **可选**强制加载 skill（默认由模型 `read` 按需加载） |
| `/export [path]` | 导出 HTML |
| `/reload` | 热重载扩展 / skills / prompts |
| `/clear` | 清空屏幕历史 |
| `/quit` | 退出 |

### Skills（Agent Skills 标准）

遵循 [Agent Skills](https://agentskills.io) progressive disclosure（与 Pi 一致）：

1. **启动**：只把 skill 的 `name` + `description` + `location` 放进 system prompt（XML catalog）
2. **自动激活**：任务匹配时，模型用 **`read` 工具**打开 `SKILL.md`（不要把全文塞进 prompt）
3. **资源**：`scripts/` / `references/` 由模型按相对路径再读
4. **用户强制**：`/skill:name [args]` 可选，注入 skill body + `User: args`
5. **开关管理**（类似 Codex）：`/skills` 面板或 settings 里逐项 enable/disable，无需删除目录

发现路径（[agentskills.io](https://agentskills.io) + Codex 同款；项目优先；同名先发现者胜出）：

| 范围 | 路径 |
|------|------|
| 项目 | `.one/skills/`（客户端）、`.agents/skills/`（跨客户端，含祖先） |
| 用户 | `~/.one/agent/skills/`、**`~/.agents/skills/`**（通用安装位） |
| 兼容 | `~/.claude/skills/`、`~/.codex/skills/`、`~/.grok/skills/`（低优先级） |
| **内置** | 二进制嵌入，落盘到 `~/.one/agent/builtin-skills/`（最低优先级） |

路径沙箱与 skill 对齐：凡进入 catalog 的 skill 目录均可 **`read`**（无需 `--add-dir`）；**不可写** skill 根（改 skill 仍须在 workspace 或 `--add-dir`）。

内置 skills（开箱即用）：

| 名称 | 用途 |
|------|------|
| `create-skill` | 交互式创建新的 `SKILL.md`（项目或用户目录） |

```text
# 自然语言或强制加载
create a skill for reviewing PRs
/skill:create-skill

# 管理（打开/关闭，类似 Codex）
/skills
/skills disable create-skill
/skills enable create-skill
/skills list
```

关闭某个 skill 后：

- 不再出现在 system prompt 的 catalog 中
- `/skill:name` 也无法 force-load，直到重新 enable
- 磁盘上的 `SKILL.md` **不会**被删除

开关状态持久化在 `~/.one/agent/settings.json` 的 `skills_config`（按 `SKILL.md` 绝对路径，语义对齐 Codex 的 `[[skills.config]]`）：

```json
{
  "skills_config": [
    {
      "path": "/home/you/.agents/skills/find-skills/SKILL.md",
      "enabled": false
    }
  ]
}
```

未列出的 skill 默认为 **enabled**。也可在 Settings（Ctrl+G）→ **Skills** 里用 Enter 切换 on/off。

`SKILL.md` 需要 YAML frontmatter：

```markdown
---
name: code-review
description: Structured code review. Use when reviewing PRs or diffs.
---

# Code Review
1. ...
```

Prompt 模板：`/templatename` 展开 `prompts/*.md`

### Print

```bash
one -p "summarize src/"
```

### JSON

```bash
one --mode json -p "hello"
```

### RPC

```bash
one --mode rpc --no-session
```

## 联网搜索

One **内置**（默认 `http-providers` / `network` feature）：

| 工具 | 作用 |
|------|------|
| `web_search` | 网页搜索：有 `BRAVE_API_KEY` 用 Brave API，否则 DuckDuckGo HTML |
| `web_fetch` | 拉取 URL 正文（HTML 粗转文本） |

```bash
export BRAVE_API_KEY=...   # 推荐；https://api-dashboard.search.brave.com/
cargo run -p one-cli -- -p "search for rust async trait best practices"
```

与 **Pi 一致的 skill 路线**（可选，不强制）：

```bash
# 安装 pi-skills 的 brave-search 到标准目录
git clone https://github.com/badlogic/pi-skills /tmp/pi-skills
mkdir -p ~/.agents/skills
ln -s /tmp/pi-skills/brave-search ~/.agents/skills/brave-search
# 在 skill 目录 npm install，并设置 BRAVE_API_KEY
```

模型可对 skill 做 progressive disclosure（catalog → `read` SKILL.md → 跑 scripts），也可用内置 `web_search`。

> 说明：早期沙箱会硬拦 `curl`/`wget`，已去掉；仅拦真正破坏性命令。

## 安全

默认阻断**灾难性** bash（如 `rm -rf /`）。高风险命令需确认，或 `-y` / `ONE_AUTO_APPROVE=1` 自动批准。**不再**默认禁止 `curl`/`wget`（skills 与正常开发需要）。
