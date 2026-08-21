# 快速入门 (Getting Started)

本指南帮助你快速安装、配置并开始使用 One 编程 Agent。

---

## 1. 编译与安装

确保系统已安装 Rust 工具链（1.80+）与基础构建工具：

```bash
# 克隆仓库并编译
git clone https://github.com/one-org/one.git
cd one
cargo build --release -p one-cli

# 二进制文件位于 target/release/one，可将其加入 PATH
cp target/release/one ~/.local/bin/one
```

---

## 2. 账号登录与模型配置

One 支持多种认证方式与主流模型服务商：

### 交互式登录 (`one login` / `/login`)
支持 Codex、xAI Grok、OpenCode Zen/Go 等一键 OAuth 或 API 认证：
```bash
one login
```

### 环境变量配置
你也可以直接导出对应的 API Key：
```bash
# Anthropic
export ANTHROPIC_API_KEY="sk-ant-..."

# OpenAI
export OPENAI_API_KEY="sk-..."

# DeepSeek
export DEEPSEEK_API_KEY="sk-..."

# 本地 Ollama
export OLLAMA_HOST="http://127.0.0.1:11434"
```

---

## 3. 基础使用模式

### 交互式 TUI 模式 (默认)
直接运行 `one` 进入富文本全屏终端：
```bash
one
```

### 单次执行模式 (`-p`)
适合快速执行单次任务或配合脚本流水线使用：
```bash
one -p "检查当前目录下的 git 状态并总结最近的 3 条提交"
```

### 恢复会话 (`--continue` / `--resume`)
```bash
# 继续最近一次对话
one --continue

# 交互式选择历史会话列表恢复
one --resume
```

### 规划模式 (`--plan` / `/plan`)
进入只读探索与规划模式，模型仅进行代码探索并生成实施方案，不会直接修改文件：
```bash
one --plan -p "设计一个重构 auth 模块的实施计划"
```

---

## 4. 常用运行选项

| 参数 | 说明 | 示例 |
| :--- | :--- | :--- |
| `-p, --prompt <PROMPT>` | 单次非交互提示词执行 | `one -p "run tests"` |
| `--provider <PROVIDER>` | 指定模型服务商（anthropic, openai, xai, deepseek, ollama, mock 等） | `one --provider deepseek` |
| `-m, --model <MODEL>` | 指定具体模型 ID | `one -m claude-3-7-sonnet-20250219` |
| `-y, --yes` | 自动批准安全的终端执行与文件编辑 | `one -y` |
| `--full-access` | 禁用工作区路径边界限制（允许访问 cwd 外文件） | `one --full-access` |
| `--no-mcp` | 禁用当前会话的 MCP 扩展连接 | `one --no-mcp` |
| `--no-skills` | 禁用 Agent Skills 技能库注入 | `one --no-skills` |
