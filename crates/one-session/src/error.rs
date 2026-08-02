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

    #[error("{0}")]
    NotFound(String),

    /// Multiple sessions matched a fuzzy `resume` / `/resume` query.
    ///
    /// `candidates` are short labels (name · id prefix · path) for CLI printing.
    #[error("ambiguous session `{spec}` ({n} matches)", n = candidates.len())]
    Ambiguous {
        spec: String,
        candidates: Vec<String>,
    },

    #[error("share failed: {0}")]
    Share(String),
}
