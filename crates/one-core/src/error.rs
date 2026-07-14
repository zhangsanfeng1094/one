use thiserror::Error;

pub type Result<T> = std::result::Result<T, OneError>;

#[derive(Debug, Error)]
pub enum OneError {
    #[error("provider error: {0}")]
    Provider(String),

    #[error("tool `{tool}` failed: {message}")]
    Tool { tool: String, message: String },

    #[error("invalid tool arguments for `{tool}`: {message}")]
    InvalidToolArgs { tool: String, message: String },

    #[error("agent loop exceeded max turns ({max})")]
    MaxTurns { max: usize },

    #[error("agent run aborted")]
    Aborted,

    /// Provider rejected the request because the prompt exceeds the context window.
    #[error("context overflow: {0}")]
    ContextOverflow(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}