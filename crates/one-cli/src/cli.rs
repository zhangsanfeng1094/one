use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, ValueEnum)]
pub enum ProviderKind {
    Mock,
    Ollama,
    Anthropic,
    Openai,
    Openrouter,
}

/// OpenAI-compatible wire protocol (Pi-style `api` field).
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum OpenaiApi {
    /// `POST /v1/chat/completions` — widest compatibility.
    #[value(name = "openai-completions", alias = "completions", alias = "chat")]
    Completions,
    /// `POST /v1/responses` — default for first-party OpenAI (like Pi).
    #[default]
    #[value(name = "openai-responses", alias = "responses")]
    Responses,
}

impl From<OpenaiApi> for one_ai::OpenaiWireApi {
    fn from(value: OpenaiApi) -> Self {
        match value {
            OpenaiApi::Completions => one_ai::OpenaiWireApi::Completions,
            OpenaiApi::Responses => one_ai::OpenaiWireApi::Responses,
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

    /// Resume: open the most recent session (same as --continue; TUI also has /resume).
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

    /// OpenAI wire API: `openai-responses` (default) or `openai-completions`.
    /// Also set via env `ONE_OPENAI_API` or `models.json` field `api`.
    #[arg(long = "openai-api", value_enum)]
    pub openai_api: Option<OpenaiApi>,

    /// API base URL override (e.g. `https://api.openai.com/v1`, `http://127.0.0.1:11434/v1`).
    /// Also set via `models.json` `baseUrl` or env `OPENAI_BASE_URL` / `OLLAMA_HOST`.
    #[arg(long = "base-url")]
    pub base_url: Option<String>,

    /// API key override (otherwise env / models.json `apiKey`).
    #[arg(long = "api-key")]
    pub api_key: Option<String>,

    /// Working directory for tools.
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,

    /// Session display name.
    #[arg(short = 'n', long)]
    pub name: Option<String>,

    /// Read-only tools only.
    #[arg(long)]
    pub read_only: bool,

    /// Export current session to HTML file.
    #[arg(long)]
    pub export: Option<PathBuf>,

    /// List available models and exit.
    #[arg(long)]
    pub list_models: bool,

    /// Auto-approve risky bash commands (or set ONE_AUTO_APPROVE=1).
    #[arg(short = 'y', long = "yes")]
    pub auto_approve: bool,

    /// Upload session export to GitHub Gist (requires GITHUB_TOKEN).
    #[arg(long)]
    pub share: bool,
}