# Provider Compat 机制

> 源码主入口：`crates/one-ai/src/compat.rs`  
> 装配入口：`crates/one-cli/src/provider.rs`（`resolve_*` → `build_provider`）  
> 对齐目标：Pi `@earendil-works/pi-ai` 的 `OpenAICompletionsCompat` + 精简版 `AnthropicMessagesCompat`

---

## 1. 要解决什么问题

各种「OpenAI 兼容」端点其实并不兼容：

| 差异点 | 例子 |
|--------|------|
| system 角色名 | 官方 reasoning 模型要 `developer`，本地/代理多数只认 `system` |
| max tokens 字段 | `max_completion_tokens` vs `max_tokens` |
| thinking / reasoning 编码 | `reasoning_effort` / `reasoning:{effort}` / `thinking:{type}` / `enable_thinking` … |
| tool 消息形状 | Mistral/Groq 要 tool result 带 `name` |
| assistant 多轮 | DeepSeek 要回放 `reasoning_content` |
| cache / session | OpenRouter Anthropic 路由要 `cache_control`；Fireworks 要 affinity header |

`compat` 把这些差异收成**可配置、可自动探测的一组开关**，在拼 HTTP body 时分支。

---

## 2. 总览：数据流

```text
models.json
  providers.<id>.compat     ──┐
  providers.<id>.models[].compat ──┤  merge（model 覆盖 provider）
  models[].thinkingLevelMap ───┤
                               ▼
                    CompatConfig (partial, 全 Option)
                               │
          ┌────────────────────┼────────────────────┐
          ▼                    ▼                    ▼
   detect(provider,      partial.openai.      partial.anthropic()
    base_url, model)      resolve(...)         .resolve()
          │                    │                    │
          └──── merge_override ─┘                    │
                    ▼                                ▼
           ResolvedOpenAiCompat           ResolvedAnthropicCompat
                    │                                │
                    ▼                                ▼
        OpenAiProvider / OpenRouter          AnthropicProvider
        (chat/completions body)              (messages body)
```

**原则**：

1. 磁盘上的 `compat` 字段都是 **tri-state**（`true` / `false` / 省略=auto）
2. 未写的字段走 **`OpenAiCompletionsCompat::detect(provider, base_url, model_id)`**
3. 用户显式值永远覆盖 detect
4. **model-level 覆盖 provider-level**（`merge_override`）

---

## 3. 两套协议，两套 Resolved

磁盘统一用 **`CompatConfig`**（OpenAI + Anthropic 字段可共存），运行时按 wire 拆开：

| 类型 | 用途 | 默认 resolve |
|------|------|--------------|
| `ResolvedOpenAiCompat` | Chat Completions（含 OpenRouter、DeepSeek、自定义 proxy） | detect + override |
| `ResolvedAnthropicCompat` | Anthropic Messages API | 固定默认，**无 detect** |

Gemini / Ollama 原生路径 **不消费** OpenAI compat（见 §6）。

`CompatConfig` 共享字段名（如 `supportsLongCacheRetention`）只挂在 OpenAI 侧，Anthropic 通过 `anthropic()` 回读，避免 serde flatten 冲突。

---

## 4. OpenAI Completions compat 字段

### 4.1 行为开关（bool）

| 字段 (camelCase) | 作用 | 在 body / 消息里怎么用 |
|------------------|------|------------------------|
| `supportsDeveloperRole` | reasoning 模型 system → `developer` | `system_role()` |
| `supportsReasoningEffort` | 是否发 `reasoning_effort` 等 effort 字段 | `apply_thinking` |
| `supportsUsageInStreaming` | `stream_options.include_usage` | chat body |
| `supportsStore` | 发 `store: false` | chat body |
| `supportsStrictMode` | tools.function 带 `strict` | chat body |
| `requiresToolResultName` | tool 消息带 `name` | map tool result |
| `requiresAssistantAfterToolResult` | tool 后插空 assistant；user 前也插 | 消息序列 |
| `requiresThinkingAsText` | thinking 折叠进 content 文本 | map assistant |
| `requiresReasoningContentOnAssistantMessages` | 无 thinking 也补空 `reasoning_content` | map assistant（DeepSeek） |
| `zaiToolStream` | body `tool_stream: true` | Z.ai |
| `sendSessionAffinityHeaders` | 请求头 x-session-* | headers |
| `supportsLongCacheRetention` | Anthropic 风格 cache TTL 选择 | cache_control |

### 4.2 枚举 / 结构化

| 字段 | 取值 | 含义 |
|------|------|------|
| `maxTokensField` | `max_completion_tokens`（默认）/ `max_tokens` | 上限字段名 |
| `thinkingFormat` | 见下表 | thinking 编码形态 |
| `thinkingLevelMap` | 模型级 map（非 compat 内） | `off/low/medium/high` → 提供商字符串或 null |
| `chatTemplateKwargs` | 自由 JSON + `$var` | `chat-template` 格式用 |
| `openRouterRouting` | JSON | 写入 body `provider` |
| `vercelGatewayRouting` | JSON | Vercel gateway → `providerOptions.gateway` |
| `cacheControlFormat` | 仅认 `"anthropic"` | OpenRouter/Vercel 上 Anthropic 缓存 |
| `sessionAffinityFormat` | `openai` / `openai-nosession` / `openrouter` | header 名 |

### 4.3 ThinkingFormat → 实际 JSON

| format | 开启 thinking 时大致形状 |
|--------|--------------------------|
| `openai` | `reasoning_effort: "high"` |
| `openrouter` | `reasoning: { effort }` + `include_reasoning: true`；关则 effort=`none`（可 map） |
| `deepseek` | `thinking: { type: enabled\|disabled }` + 可选 `reasoning_effort` |
| `together` | `reasoning: { enabled: bool }` + 可选 effort |
| `zai` | `thinking: { type: enabled\|disabled }` + 可选 effort |
| `qwen` | `enable_thinking: bool` |
| `qwen-chat-template` | `chat_template_kwargs: { enable_thinking, preserve_thinking }` |
| `chat-template` | 用 `chatTemplateKwargs` 模板（`$var: thinking.enabled / thinking.effort`） |
| `string-thinking` | `thinking: "high"` 字符串 |
| `ant-ling` | 仅在有 effort 时 `reasoning: { effort }` |

`thinkingLevelMap` 语义：

- 缺 key → 用 agent 默认 effort 字符串（`low`/`medium`/`high`）
- `"high": "max"` → 发 `max`
- `"low": null` → 该 level 不支持，跳过 thinking 参数

---

## 5. detect：按 provider / URL / model 自动推断

实现：`OpenAiCompletionsCompat::detect(provider, base_url, model_id)`  
识别靠 **provider id 小写** 或 **base_url 子串**。

### 5.1 识别到的「厂商 / 部署」类别

| 类别 | 触发条件（节选） |
|------|------------------|
| OpenRouter | id=`openrouter` 或 url 含 `openrouter.ai` |
| DeepSeek | id / `deepseek.com` |
| Z.ai / 智谱 | `zai` / `zhipu` / `api.z.ai` / `open.bigmodel.cn` |
| Together | `together` / together.ai\|xyz |
| Moonshot / Kimi | moonshot / kimi 相关 id 或域名 |
| Grok / xAI | `xai`/`grok` / `api.x.ai` |
| Ollama / LM Studio / vLLM / SGLang | 本地端口或 id（统称 **localish**） |
| Groq / Fireworks / Cerebras / Mistral / HF / SiliconFlow / MiniMax | 对应 id 或域名 |
| NVIDIA NIM | nvidia / nim / integrate.api.nvidia.com |
| Cloudflare Workers / AI Gateway | 对应 path |
| Vercel AI Gateway | `ai-gateway.vercel.sh` |
| Ant-Ling | ant-ling |
| GitHub Copilot | github-copilot / copilot |
| Chutes | chutes.ai |

### 5.2 关键默认策略（逻辑摘要）

```text
is_non_standard = 上述大多数第三方 / 本地
use_max_tokens  = chutes, moonshot, CF gateway, together, nvidia, ant-ling,
                  localish, groq, fireworks, mistral, HF, siliconflow, minimax
```

| 决策 | 默认逻辑 |
|------|----------|
| `maxTokensField` | `use_max_tokens` → `max_tokens`，否则 `max_completion_tokens` |
| `thinkingFormat` | deepseek→Deepseek；zai→Zai；together→Together；ant-ling→AntLing；openrouter/vercel→Openrouter；localish 且 model 含 qwen→QwenChatTemplate；其余 Openai |
| `supportsReasoningEffort` | **false** if grok/zai/moonshot/together/CF-gw/nvidia/ant-ling/localish/groq/mistral/minimax |
| `supportsDeveloperRole` | openrouter 且 model 前缀 `anthropic/` 或 `openai/` → true；否则仅 **非 non_standard 且非 openrouter/vercel** |
| `supportsStore` | `!is_non_standard` |
| `supportsUsageInStreaming` | `!is_localish` |
| `requiresToolResultName` | mistral \|\| groq |
| `requiresReasoningContentOnAssistantMessages` | deepseek |
| `zaiToolStream` | zai |
| `supportsStrictMode` | 排除 moonshot/together/CF-gw/nvidia/localish/mistral/groq |
| `cacheControlFormat` | openrouter + `anthropic/*` 或 vercel + model 含 anthropic → `"anthropic"` |
| `sendSessionAffinityHeaders` | fireworks |
| `sessionAffinityFormat` | openrouter → Openrouter |
| `supportsLongCacheRetention` | 排除 together/CF/nvidia/ant-ling/localish/groq |

### 5.3 常见 provider 的 detect 画像

| Provider | thinkingFormat | max_tokens 字段 | developer 角色 | 其它要点 |
|----------|----------------|-----------------|----------------|----------|
| **openai**（官方） | openai | max_completion_tokens | ✅（非 non_standard） | store、usage、strict 全开 |
| **openrouter** | openrouter | max_completion_tokens | 仅 anthropic/ 与 openai/ 模型 | Anthropic 路由 cache_control；session format=openrouter |
| **deepseek** | deepseek | max_completion_tokens | ❌ | 回放要 `reasoning_content` |
| **ollama** / 本地 | openai（qwen 模型→qwen-chat-template） | max_tokens | ❌ | 无 effort、无 store、无 usage stream |
| **groq** | openai | max_tokens | ❌ | tool result 要 `name`；无 long cache |
| **zai** | zai | max_completion_tokens | ❌ | `tool_stream`；无 effort |
| **together** | together | max_tokens | ❌ | 无 long cache |
| **fireworks** | openai | max_tokens | ❌ | 默认发 session affinity |
| **mistral** | openai | max_tokens | ❌ | tool result `name`；无 strict |

自定义 `providers`（任意 id + baseUrl）会按 **id 名 + URL 形态** 命中同一套规则；完全陌生则接近「标准 OpenAI」。

---

## 6. 各 Provider 实现怎么接 compat

装配在 `one-cli/src/provider.rs` 的 `build_provider`：

| 入口 provider | Wire API | 实际实现 | compat 用法 |
|---------------|----------|----------|-------------|
| `mock` | — | MockProvider | 无 |
| `ollama` | 原生 `/api/chat` 等 | **OllamaProvider** | **不走 OpenAI compat**（单独实现） |
| `openrouter` | chat/completions | OpenRouterProvider → 内嵌 OpenAiProvider | `ResolvedOpenAiCompat`（detect 固定 provider=`openrouter`） |
| `anthropic` 或 `api=anthropic-messages` | Messages | AnthropicProvider | `ResolvedAnthropicCompat` |
| `gemini` 或 `api=gemini-generate-content` | generateContent | GeminiProvider | **无 compat**；若用户显式设 `openai-completions` 则 fallback 到 OpenAI 路径 |
| 其余（openai / deepseek / 自定义） | completions 或 responses | OpenAiProvider | Completions 用 full compat；**Responses 基本不用 OpenAI compat**（thinking 走 `thinking::apply_responses_thinking`） |

### 6.1 OpenAiProvider（Chat Completions）

`build_chat_body` 消费 `ResolvedOpenAiCompat`：

1. system 角色：`system_role(reasoning_model)`
2. 可选 Anthropic 风格 `cache_control`（`cacheControlFormat=anthropic`）
3. tool 后插 assistant / tool `name` / thinking 折叠 / `reasoning_content`
4. `stream_options` / `store` / tool `strict`
5. `apply_thinking` + `apply_routing_and_extras`
6. 请求头：session affinity

`with_thinking_wire` 仍可覆盖 `thinking_format`（legacy 桥接）。

### 6.2 OpenRouterProvider

薄封装：固定 Completions + `provider_id=openrouter` + 默认 `reasoning_model=true`，compat 可被 models.json 覆盖。

### 6.3 AnthropicProvider

`ResolvedAnthropicCompat` 字段：

| 字段 | 默认 | 作用 |
|------|------|------|
| `supportsEagerToolInputStreaming` | true | tool 上 `eager_input_streaming`；false 时加 beta header 细粒度 tool stream |
| `supportsLongCacheRetention` | true | cache_control TTL |
| `supportsCacheControlOnTools` | true | tools 上挂 cache_control |
| `sendSessionAffinityHeaders` | false | `x-session-affinity` |
| `supportsTemperature` | true | （解析侧；body 侧温度映射另议） |
| `forceAdaptiveThinking` | false | `thinking: {type:adaptive}` + `output_config.effort` |
| `allowEmptySignature` | false | 空 signature 仍回放 thinking 块（兼容代理） |

**无 detect**：不写 models.json 就用上表默认。共享 long-cache / session 字段从 OpenAI 侧 compat 回填。

### 6.4 GeminiProvider

原生 `generateContent` / `streamGenerateContent`。compat 体系不参与。若走 OpenAI 兼容基址，代码会剥掉 URL 里的 `/openai` 再打原生路径；若配置成 `openai-completions`，则整条链路变成 OpenAiProvider + OpenAI compat。

### 6.5 OllamaProvider

独立 HTTP 协议，**不读** `ResolvedOpenAiCompat`。若用 OpenAI 兼容的 Ollama（`/v1/chat/completions`），应配置为通用 OpenAI provider（id 含 ollama 或 baseUrl 含 11434），此时走 detect 的 localish 规则。

### 6.6 OpenAI Responses API

`build_responses_body` **几乎不读** OpenAI compat（固定 `store:false`，thinking 走 Responses 专用 helper）。compat 主要服务 **Chat Completions 兼容面**。

---

## 7. models.json 配置形状

Pi 兼容 camelCase：

```json
{
  "providers": {
    "my-proxy": {
      "baseUrl": "https://proxy.example/v1",
      "api": "openai-completions",
      "apiKey": "…",
      "compat": {
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": true,
        "thinkingFormat": "openrouter",
        "maxTokensField": "max_tokens",
        "openRouterRouting": { "only": ["anthropic"] }
      },
      "models": [
        {
          "id": "claude-sonnet",
          "reasoning": true,
          "thinkingLevelMap": { "high": "max", "low": null },
          "compat": {
            "supportsDeveloperRole": true
          }
        }
      ]
    },
    "anthropic": {
      "compat": {
        "forceAdaptiveThinking": true,
        "allowEmptySignature": true,
        "supportsLongCacheRetention": true
      },
      "models": [{ "id": "claude-sonnet-4-20250514" }]
    }
  }
}
```

合并：`provider.compat` ←override— `model.compat` → 再 `detect` 填洞。  
`thinkingLevelMap` 在 model 上，resolve 后 `with_thinking_level_map`。

CLI / Settings 可改：`compat.*`、`thinkingFormat`、`maxTokensField`，以及 `COMPAT_BOOL_FIELDS` 列表中的 tri-state 开关（auto→true→false→auto）。

---

## 8. 源码地图

| 文件 | 职责 |
|------|------|
| `one-ai/src/compat.rs` | 类型、detect、resolve、apply_thinking / max_tokens / routing |
| `one-ai/src/openai.rs` | Chat Completions 消费 `ResolvedOpenAiCompat` |
| `one-ai/src/openrouter.rs` | OpenRouter 包装 + detect 默认 |
| `one-ai/src/anthropic.rs` | Messages 消费 `ResolvedAnthropicCompat` |
| `one-ai/src/gemini.rs` | 原生 Gemini，无 OpenAI compat |
| `one-ai/src/models_file.rs` | 读写 models.json 中的 compat |
| `one-ai/src/thinking.rs` | 旧 ThinkingWire + Anthropic/Responses thinking 辅助 |
| `one-ai/src/cache.rs` | cache_control / session id |
| `one-cli/src/provider.rs` | merge + resolve + `build_provider` 注入 |

---

## 9. 设计边界（读代码时注意）

1. **compat 主要是 Chat Completions 的方言层**；Anthropic 是另一套小表；Gemini 原生不在内。
2. **detect 是启发式**，不保证覆盖所有代理；异常端点用 models.json 显式 override。
3. **OpenAI Responses** 路径未接入完整 compat 矩阵。
4. Ollama **原生** provider 与「Ollama 的 OpenAI 兼容端口」是两条路。
5. 与 Pi 对齐意图明确，但 Anthropic 侧是 **subset**，不是完整 Pi 镜像。

---

## 10. 快速排查

请求被拒时，按序看：

1. 当前 wire：`openai-completions` / `openai-responses` / `anthropic-messages` / `gemini-…`
2. `provider_compat_summary` / 日志里的 `cache_control_format`、`thinking_format`
3. 是否误用了 `developer` 角色 → `supportsDeveloperRole: false`
4. max tokens 字段名是否反了 → `maxTokensField`
5. DeepSeek 多轮丢 reasoning → 确认 `requiresReasoningContentOnAssistantMessages`（detect 已开）
6. OpenRouter 走 Claude → 是否需要 `cacheControlFormat: anthropic`（detect 在 `anthropic/*` 模型上已开）
