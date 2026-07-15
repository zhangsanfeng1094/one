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
```

## 权限与路径沙箱

默认 **`workspace-write`**：`read` / `write` / `edit` / `grep` / `find` / `ls` 只能访问：

| 范围 | 权限 |
|------|------|
| `--cwd`（工作区根） | 读 + 写 |
| `--add-dir` / settings `additional_directories` | 读 + 写 |
| `~/.one/agent`（skills / plans） | **仅读** |
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
- 始终可走 **Other** 键入自由文本。
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
- `Space`（输入为空时）：切换 **Plan / Build** 模式
- Thinking 深度：`/settings thinking <off|low|medium|high>` 或 `/thinking`（无快捷键）
- `Ctrl+T`：展开/折叠全部 thinking 正文（**默认折叠**为 `▸ thinking · N chars`；流式输出时仍显示末 3 行 tail；点击或 ↵ 可单独展开/折叠一块）
- `↑` / `↓` 或 `Ctrl+P` / `Ctrl+N`：切换之前提交过的提示词（**按项目持久化**，新 session / 重启进程仍可召回；来自 `~/.one/agent/sessions/--cwd--/prompt_history.jsonl`，首次会从历史 session 的用户消息播种）
- `Esc`：输入非空时**立刻**清空草稿并记入 ↑ 历史；输入为空时再按一次 `Esc`（约 0.9s 内）打开 **当前 session** 的 rewind 菜单（conversation-only，不含代码 checkpoint）
- `/`：输入框**上方**命令列表（边打边筛 · ↑↓ 选择 · Enter 执行；同 Claude Code）
- `Ctrl+L`：模型 select（输入框上方）
- `Ctrl+G`：Settings 居中面板
- `PageUp` / `PageDown`：滚动对话记录
- `q` / `Esc`：中止生成（运行中；`q` 仅在输入为空时）
- `Esc`：关闭浮层
- `Ctrl+C` / `/quit`：强制退出（含卡死时；busy 下不会当成“取消”）
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
