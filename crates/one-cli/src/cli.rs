use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::mcp_cmd::McpCli;

#[derive(Debug, Clone, ValueEnum)]
pub enum ProviderKind {
    Mock,
    Ollama,
    Anthropic,
    Openai,
    /// ChatGPT Plus/Pro subscription via OAuth (`/login`).
    #[value(name = "openai-codex", alias = "codex", alias = "chatgpt")]
    OpenaiCodex,
    Openrouter,
    Deepseek,
    Gemini,
}

/// Wire protocol for request/response encoding (Pi-style `api` field).
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum OpenaiApi {
    /// `POST {base}/chat/completions` — widest OpenAI-compatible surface.
    #[value(
        name = "openai-completions",
        alias = "completions",
        alias = "chat",
        alias = "openai-compatible"
    )]
    Completions,
    /// `POST {base}/responses` — default for first-party OpenAI (like Pi).
    #[default]
    #[value(name = "openai-responses", alias = "responses")]
    Responses,
    /// `POST {base}/v1/messages` — Anthropic Messages API.
    #[value(name = "anthropic-messages", alias = "anthropic", alias = "messages")]
    AnthropicMessages,
    /// `POST {base}/models/{model}:generateContent` — Gemini native.
    #[value(
        name = "gemini-generate-content",
        alias = "gemini",
        alias = "generate-content"
    )]
    GeminiGenerateContent,
}

impl From<OpenaiApi> for one_ai::ProviderApi {
    fn from(value: OpenaiApi) -> Self {
        match value {
            OpenaiApi::Completions => one_ai::ProviderApi::OpenaiCompletions,
            OpenaiApi::Responses => one_ai::ProviderApi::OpenaiResponses,
            OpenaiApi::AnthropicMessages => one_ai::ProviderApi::AnthropicMessages,
            OpenaiApi::GeminiGenerateContent => one_ai::ProviderApi::GeminiGenerateContent,
        }
    }
}

#[derive(Debug, Clone, ValueEnum, Default)]
pub enum RunMode {
    #[default]
    Interactive,
    Print,
    Json,
    Rpc,
    /// Agent Client Protocol over stdio (IDE / external clients).
    /// Prefer the `one acp` subcommand for editor configs.
    Acp,
    /// Web UI server (ACP over WebSocket).
    /// Accessible via browser at `http://127.0.0.1:3000/`.
    Web,
}

#[derive(Parser, Debug, Clone)]
#[command(name = "one", about = "One coding agent", version)]
pub struct Cli {
    /// Prompt text.
    ///
    /// - Bare `-p "…"` (no `--mode`) → **print** mode (scripts / CI).
    /// - `--mode interactive -p "…"` or `--tui -p "…"` → **TUI**, first user turn.
    #[arg(short = 'p', long = "print")]
    pub print: Option<String>,

    /// Run mode.
    ///
    /// With an explicit `--mode interactive` plus `-p`, the prompt is the first
    /// TUI turn (not print mode). Bare `-p` without `--mode` still selects print
    /// for backward compatibility.
    #[arg(long, value_enum, default_value_t = RunMode::Interactive)]
    pub mode: RunMode,

    /// Force interactive TUI even when `-p` is set (seed the first user message).
    ///
    /// Equivalent to `--mode interactive -p "…"` for the common supervise/eval path.
    #[arg(long = "tui")]
    pub tui: bool,

    /// Continue the most recent session for this cwd.
    #[arg(short = 'c', long = "continue")]
    pub r#continue: bool,

    /// Resume: open interactive session picker in TUI (print/json: most recent).
    #[arg(short = 'r', long = "resume")]
    pub resume: bool,

    /// Open a specific session file.
    #[arg(long)]
    pub session: Option<PathBuf>,

    /// Do not persist a session file.
    #[arg(long, global = true)]
    pub no_session: bool,

    /// Provider to use (defaults to last selection, or mock).
    #[arg(long, value_enum, global = true)]
    pub provider: Option<ProviderKind>,

    /// Model id (overrides provider default / models.json).
    #[arg(long, short = 'm', global = true)]
    pub model: Option<String>,

    /// Wire protocol: `openai-responses` | `openai-completions` | `anthropic-messages` | `gemini-generate-content`.
    /// Also set via env `ONE_OPENAI_API` or `models.json` `api` / `providerType`.
    #[arg(long = "openai-api", value_enum)]
    pub openai_api: Option<OpenaiApi>,

    /// API base URL override (e.g. `https://api.openai.com/v1`, `http://127.0.0.1:11434/v1`).
    /// Also set via `models.json` `baseUrl` or env `OPENAI_BASE_URL` / `OLLAMA_HOST`.
    #[arg(long = "base-url", global = true)]
    pub base_url: Option<String>,

    /// API key override (otherwise env / models.json `apiKey`).
    #[arg(long = "api-key", global = true)]
    pub api_key: Option<String>,

    /// Working directory for tools (workspace root).
    #[arg(long, default_value = ".", global = true)]
    pub cwd: PathBuf,

    /// Extra directories the agent may read/write (repeatable).
    /// Paths outside cwd + these roots are denied unless `--full-access`.
    #[arg(long = "add-dir", value_name = "DIR", global = true)]
    pub add_dir: Vec<PathBuf>,

    /// Disable workspace path boundary (file tools may touch any path).
    /// Prefer containers/VMs; also set via settings `sandbox=full-access`.
    #[arg(long = "full-access", global = true)]
    pub full_access: bool,

    /// Session display name.
    #[arg(short = 'n', long)]
    pub name: Option<String>,

    /// Read-only tools only.
    #[arg(long, global = true)]
    pub read_only: bool,

    /// Start in plan mode (explore + write plan; no code edits until /act).
    #[arg(long, global = true)]
    pub plan: bool,

    /// Export current session to HTML file.
    #[arg(long)]
    pub export: Option<PathBuf>,

    /// List available models and exit.
    #[arg(long)]
    pub list_models: bool,

    /// List built-in + configured providers and exit.
    #[arg(long)]
    pub list_providers: bool,

    /// Auto-approve risky bash commands (or set ONE_AUTO_APPROVE=1).
    /// Does not disable the workspace path boundary — use `--full-access` for that.
    #[arg(short = 'y', long = "yes", global = true)]
    pub auto_approve: bool,

    /// Auto-approve all tool executions without interactive permission prompts (alias: `--yolo`).
    #[arg(long = "always-approve", alias = "yolo", global = true)]
    pub always_approve: bool,

    /// Permission mode: `default` (ask) | `acceptEdits` | `auto` | `dontAsk` | `bypassPermissions` (always-approve).
    #[arg(long = "permission-mode", value_name = "MODE", global = true)]
    pub permission_mode: Option<String>,

    /// Upload session export to GitHub Gist (requires GITHUB_TOKEN).
    #[arg(long)]
    pub share: bool,

    /// Do not connect MCP servers for this session.
    #[arg(long = "no-mcp", global = true)]
    pub no_mcp: bool,

    /// Do not inject the skills catalog into the system prompt (or load skill roots).
    /// Also set via env `ONE_DISABLE_SKILLS=1`. Useful for isolated harness evals.
    #[arg(long = "no-skills", global = true)]
    pub no_skills: bool,

    /// Disable the subagent feature for this process (`task` / job tools + prompt section).
    /// Also set via env `ONE_DISABLE_SUBAGENT=1`. Overrides settings.features.subagent.
    #[arg(long = "no-subagent", global = true)]
    pub no_subagent: bool,

    /// Do not inject the cross-session memory L2 catalog into the system prompt.
    /// Also set via env `ONE_NO_MEMORY=1` / `ONE_MEMORY=0`.
    #[arg(long = "no-memory", global = true)]
    pub no_memory: bool,

    /// Export execution trace to Langfuse (turns / LLM / tools / usage / scores).
    ///
    /// Requires `LANGFUSE_PUBLIC_KEY` + `LANGFUSE_SECRET_KEY`.
    /// Optional: `LANGFUSE_BASE_URL` (default `https://cloud.langfuse.com`).
    /// Also enabled by `ONE_TRACE=1`. See `docs/harness-eval.md`.
    #[arg(long = "trace", alias = "langfuse")]
    pub trace: bool,

    /// Include larger LLM / tool I/O previews in the Langfuse trace
    /// (default: short tool-arg preview + lengths only).
    #[arg(long = "trace-full")]
    pub trace_full: bool,

    /// Max agent turns per user prompt (tool-call loops). Default 64.
    #[arg(long = "max-turns", default_value_t = 64)]
    pub max_turns: usize,

    /// Machine-readable result: `text` | `json` (RunResult envelope). See docs/protocol.md.
    #[arg(long = "output-format", value_name = "FMT")]
    pub output_format: Option<String>,

    /// Optional subcommands (`one mcp …` / `one bench` / `one agent` / `one run` / `one resume`).
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Commands {
    /// Manage MCP servers (list / add / remove / doctor)
    Mcp(McpCli),
    /// Subscription / OAuth login (Codex, xAI Grok, OpenCode Zen/Go, …)
    Login(LoginCli),
    /// Clear stored OAuth / API credentials
    Logout(LogoutCli),
    /// Run harness capability tasks and score them
    Bench(BenchCli),
    /// Run / dump / inspect agent presets (CLI harness; same as subagent)
    Agent(AgentCli),
    /// Run harness with --preset or --spec (full AgentSpec JSON)
    Run(crate::agent_cmd::RunCli),
    /// Resume a session after quitting (`one resume` / `one resume <id|name|path>`)
    Resume(ResumeCli),
    /// Manually teach, list, or reset intent graph rules (`one learn`)
    Learn(LearnCli),
    /// Speak Agent Client Protocol (ACP) over stdio for IDE embedding
    ///
    /// JSON-RPC 2.0 on stdin/stdout. Configure editors as:
    /// `one acp --cwd /path/to/project --provider …`
    Acp(AcpCli),
    /// Web UI server (ACP over WebSocket).
    ///
    /// Open the web interface to interact with One via browser.
    Web(WebCli),
}

/// CLI: `one web` — Web UI server (ACP over WebSocket).
#[derive(Debug, Clone, clap::Args)]
pub struct WebCli {
    /// Port to listen on (default: 3000).
    #[arg(long, default_value_t = 3000)]
    pub port: u16,

    /// Host address to bind to (default: 127.0.0.1).
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Automatically open the Web UI in the default browser.
    #[arg(long)]
    pub open: bool,
}

/// CLI: `one learn [RULE]` — manually teach or manage intent graph rules.
///
/// ```text
/// one learn "当用户询问架构时，建议优先使用 find 和 deepwiki"
/// one learn --list
/// one learn --status
/// one learn --reset
/// one learn --test "帮我查一下 reqwest 的用法"
/// ```
#[derive(Debug, Clone, clap::Args)]
pub struct LearnCli {
    /// Rule text to learn (natural language or structured format `意图: ... | 触发: ... | 提醒: ...`).
    #[arg(value_name = "RULE")]
    pub rule: Option<String>,

    /// List all custom learned rules.
    #[arg(long = "list", short = 'l')]
    pub list: bool,

    /// Show intent graph statistics.
    #[arg(long = "status", short = 's')]
    pub status: bool,

    /// Reset all custom rules and revert to built-in graph.
    #[arg(long = "reset")]
    pub reset: bool,

    /// Dry-run inference against QUERY (prints matched intents / reminders / tools).
    #[arg(long = "test", value_name = "QUERY")]
    pub test: Option<String>,
}

/// CLI: `one acp` — Agent Client Protocol server (stdio).
#[derive(Debug, Clone, clap::Args, Default)]
pub struct AcpCli {}

/// CLI: `one resume [SPEC]` — re-open a project session from the shell.
///
/// After you exit the TUI, sessions stay under `~/.one/agent/sessions/…`.
/// This is the out-of-process counterpart of in-TUI `/resume`.
///
/// ```text
/// one resume                 # interactive picker (same as `one -r`)
/// one resume bench:task      # by /name, id prefix, preview substring, or path
/// one resume --list          # print recent sessions for --cwd and exit
/// one resume abc123 -p "…"   # headless continue on a matched session
/// ```
#[derive(Debug, Clone, clap::Args)]
pub struct ResumeCli {
    /// Session id (prefix ok), `/name` title, first-prompt substring, or session file path.
    /// Omit to open the interactive picker (`one -r`).
    #[arg(value_name = "SPEC")]
    pub spec: Option<String>,

    /// List recent sessions for this project (`--cwd`) and exit.
    #[arg(long = "list", short = 'l')]
    pub list: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct AgentCli {
    #[command(subcommand)]
    pub command: crate::agent_cmd::AgentCommands,
}

#[derive(Debug, Clone, clap::Args)]
pub struct BenchCli {
    /// Task pack root (default: `./benches/tasks` or next to the workspace).
    #[arg(long, value_name = "DIR")]
    pub tasks_dir: Option<PathBuf>,

    /// Only run this task id (directory name under tasks_dir).
    #[arg(long)]
    pub task: Option<String>,

    /// Suite filter: `smoke` (mock-friendly) | `all` (default: smoke).
    #[arg(long, default_value = "smoke")]
    pub suite: String,

    /// Output directory for traces + summary (default: `./benches/out/<timestamp>`).
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Max turns override for bench runs.
    #[arg(long, default_value_t = 16)]
    pub max_turns: usize,

    /// Keep temp workspaces after the run (for debugging).
    #[arg(long)]
    pub keep: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct LoginCli {
    /// Provider id: `openai-codex` | `xai` | `opencode` | `opencode-go`
    /// (aliases: codex, chatgpt, grok, zen, go).
    /// Omit to pick interactively from the catalog.
    #[arg(value_name = "PROVIDER")]
    pub provider: Option<String>,

    /// Codex / xAI: device-code flow (headless / remote).
    #[arg(long = "device-code")]
    pub device_code: bool,

    /// Codex / xAI: force browser PKCE flow (skip method prompt).
    #[arg(long = "browser")]
    pub browser: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct LogoutCli {
    /// Provider id to clear. Default: openai-codex.
    #[arg(default_value = "openai-codex")]
    pub provider: String,

    /// Remove all stored credentials.
    #[arg(long)]
    pub all: bool,
}
