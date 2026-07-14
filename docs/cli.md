# CLI 参考

## 基本用法

```bash
one                          # 交互模式（自动创建 session）
one -p "explain this repo"   # print 模式
one --continue / -c          # 继续最近 session
one --resume / -r            # 同 --continue（交互内另有 /resume）
one --session PATH           # 打开指定 session 文件
one --no-session             # 不持久化
one --read-only              # 只启用 read/grep/find/ls
one --cwd /path/to/project   # 指定工作目录
one -n "refactor auth"        # 设置 session 名称
one -y                       # 自动批准高风险 bash 命令
one --export out.html        # 导出 session 为 HTML
one --share                  # 上传 session 到 GitHub Gist（需 GITHUB_TOKEN）
one --list-models            # 列出可用模型
```

## Provider 与模型配置

```bash
one --provider mock          # 默认，本地测试
one --provider ollama        # 本地 Ollama（需 network feature）
one --provider anthropic     # 需 ANTHROPIC_API_KEY + http-providers
one --provider openai        # 需 OPENAI_API_KEY + http-providers
one --provider openrouter    # 需 OPENROUTER_API_KEY + http-providers

one --model gpt-4o           # 模型 id（-m）
one --base-url https://api.openai.com/v1
one --api-key sk-...
one --openai-api openai-responses   # 或 openai-completions
```

### 配置优先级（高 → 低）

| 字段 | CLI | models.json | 环境变量 | 默认 |
|------|-----|-------------|----------|------|
| provider | `--provider` | — | — | `mock` |
| model | `--model` / `-m` | provider 下第一个 model / entry | `OPENROUTER_MODEL` 等 | 见下表 |
| baseUrl | `--base-url` | `providers.*.baseUrl` 或 model `baseUrl` | `OPENAI_BASE_URL` / `OLLAMA_HOST` / … | 官方 URL |
| api | `--openai-api` | `api` 字段 | `ONE_OPENAI_API` | openai→responses，其它→completions |
| apiKey | `--api-key` | `apiKey`（支持 `$ENV`） | `OPENAI_API_KEY` 等 | — |

默认 model：

| provider | default model |
|----------|----------------|
| mock | `mock-v1` |
| openai | `gpt-4o` |
| anthropic | `claude-sonnet-4-20250514` |
| ollama | `llama3.2` |
| openrouter | `anthropic/claude-sonnet-4` |

### OpenAI wire API

| 值 | Endpoint | 说明 |
|----|----------|------|
| `openai-responses`（OpenAI 默认） | `POST {baseUrl}/responses` | 对齐 Pi 官方 OpenAI |
| `openai-completions` | `POST {baseUrl}/chat/completions` | 兼容 Ollama / 代理 / OpenRouter |

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
      "models": [
        { "id": "llama3.2" },
        { "id": "qwen2.5-coder:7b" }
      ]
    },
    "my-proxy": {
      "baseUrl": "https://proxy.example.com/v1",
      "api": "openai-completions",
      "apiKey": "$MY_PROXY_KEY",
      "models": [
        { "id": "gpt-4o", "name": "Proxied GPT-4o" }
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
- `Shift+Tab`：循环 thinking 级别（off → low → medium → high）
- `Ctrl+L`：模型选择器 · `Ctrl+P`：命令面板
- `Esc`：中止生成 / 关闭浮层
- `Ctrl+C` / `/quit`：退出

Slash 命令：

| 命令 | 说明 |
|------|------|
| `/help` | 显示帮助 |
| `/session` | 当前 session 路径 / 名称 / 消息数 |
| `/resume [id\|name\|file]` | 列出或打开历史 session |
| `/new` | 新建 session |
| `/name <title>` | 设置 session 显示名 |
| `/tree` / `/tree <id>` | 列出或切换分支 |
| `/model <provider>[:model]` | 切换 provider / 模型 |
| `/thinking [off\|low\|medium\|high]` | 设置或循环 thinking |
| `/compact [instructions]` | 手动压缩上下文（LLM 摘要优先） |
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

发现路径（项目优先；同名先发现者胜出）：

| 范围 | 路径 |
|------|------|
| 项目 | `.one/skills/`、`.agents/skills/`（含祖先） |
| 用户 | `~/.one/agent/skills/`、`~/.agents/skills/` |
| 兼容 | `~/.claude/skills/`、`~/.codex/skills/`、`~/.grok/skills/` |
| **内置** | 二进制嵌入，落盘到 `~/.one/agent/builtin-skills/`（最低优先级） |

内置 skills（开箱即用）：

| 名称 | 用途 |
|------|------|
| `create-skill` | 交互式创建新的 `SKILL.md`（项目或用户目录） |

```text
# 自然语言或强制加载
create a skill for reviewing PRs
/skill:create-skill
```

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
