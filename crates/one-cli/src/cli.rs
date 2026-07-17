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
}

#[derive(Parser, Debug, Clone)]
#[command(name = "one", about = "One coding agent", version)]
pub struct Cli {
    /// Prompt for print/json mode, or initial message for interactive.
    #[arg(short = 'p', long = "print")]
    pub print: Option<String>,

    /// Run mode.
    #[arg(long, value_enum, default_value_t = RunMode::Interactive)]
    pub mode: RunMode,

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
    #[arg(long)]
    pub no_session: bool,

    /// Provider to use (defaults to last selection, or mock).
    #[arg(long, value_enum)]
    pub provider: Option<ProviderKind>,

    /// Model id (overrides provider default / models.json).
    #[arg(long, short = 'm')]
    pub model: Option<String>,

    /// Wire protocol: `openai-responses` | `openai-completions` | `anthropic-messages` | `gemini-generate-content`.
    /// Also set via env `ONE_OPENAI_API` or `models.json` `api` / `providerType`.
    #[arg(long = "openai-api", value_enum)]
    pub openai_api: Option<OpenaiApi>,

    /// API base URL override (e.g. `https://api.openai.com/v1`, `http://127.0.0.1:11434/v1`).
    /// Also set via `models.json` `baseUrl` or env `OPENAI_BASE_URL` / `OLLAMA_HOST`.
    #[arg(long = "base-url")]
    pub base_url: Option<String>,

    /// API key override (otherwise env / models.json `apiKey`).
    #[arg(long = "api-key")]
    pub api_key: Option<String>,

    /// Working directory for tools (workspace root).
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,

    /// Extra directories the agent may read/write (repeatable).
    /// Paths outside cwd + these roots are denied unless `--full-access`.
    #[arg(long = "add-dir", value_name = "DIR")]
    pub add_dir: Vec<PathBuf>,

    /// Disable workspace path boundary (file tools may touch any path).
    /// Prefer containers/VMs; also set via settings `sandbox=full-access`.
    #[arg(long = "full-access")]
    pub full_access: bool,

    /// Session display name.
    #[arg(short = 'n', long)]
    pub name: Option<String>,

    /// Read-only tools only.
    #[arg(long)]
    pub read_only: bool,

    /// Start in plan mode (explore + write plan; no code edits until /act).
    #[arg(long)]
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
    #[arg(short = 'y', long = "yes")]
    pub auto_approve: bool,

    /// Upload session export to GitHub Gist (requires GITHUB_TOKEN).
    #[arg(long)]
    pub share: bool,

    /// Do not connect MCP servers for this session.
    #[arg(long = "no-mcp")]
    pub no_mcp: bool,

    /// Do not inject the skills catalog into the system prompt (or load skill roots).
    /// Also set via env `ONE_DISABLE_SKILLS=1`. Useful for isolated harness evals.
    #[arg(long = "no-skills")]
    pub no_skills: bool,

    /// Write agent execution trace as JSONL (turns / LLM latency / tools / usage).
    /// Use `-` for a default path under `~/.one/agent/traces/`.
    /// For harness eval and comparison — see `docs/harness-eval.md`.
    #[arg(long = "trace", value_name = "PATH", num_args = 0..=1, default_missing_value = "-")]
    pub trace: Option<PathBuf>,

    /// Include full tool argument JSON in the trace (default: truncated preview only).
    #[arg(long = "trace-full")]
    pub trace_full: bool,

    /// Optional subcommands (`one mcp …` / `one trace-stats` / `one bench`).
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Commands {
    /// Manage MCP servers (list / add / remove / doctor)
    Mcp(McpCli),
    /// Subscription / OAuth login (Codex, OpenCode Zen/Go, …)
    Login(LoginCli),
    /// Clear stored OAuth / API credentials
    Logout(LogoutCli),
    /// Summarize a JSONL execution trace (`--trace` output)
    #[command(name = "trace-stats")]
    TraceStats(TraceStatsCli),
    /// Run harness capability tasks and score them
    Bench(BenchCli),
}

#[derive(Debug, Clone, clap::Args)]
pub struct TraceStatsCli {
    /// Path to a JSONL trace file written by `--trace`.
    pub path: PathBuf,

    /// Emit machine-readable JSON instead of a text report.
    #[arg(long)]
    pub json: bool,
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
    /// Provider id: `openai-codex` | `opencode` | `opencode-go`
    /// (aliases: codex, chatgpt, zen, go).
    /// Omit to pick interactively from the catalog.
    #[arg(value_name = "PROVIDER")]
    pub provider: Option<String>,

    /// Codex only: device-code flow (headless / remote).
    #[arg(long = "device-code")]
    pub device_code: bool,

    /// Codex only: force browser PKCE flow (skip method prompt).
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