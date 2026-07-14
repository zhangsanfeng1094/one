use thiserror::Error;

pub type Result<T> = std::result::Result<T, SessionError>;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("entry not found: {0}")]
    EntryNotFound(String),

    #[error("invalid session file: {0}")]
    InvalidFormat(String),

    #[error("no sessions found for cwd")]
    NoSessions,

    #[error("share failed: {0}")]
    Share(String),
}