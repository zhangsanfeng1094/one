use thiserror::Error;

pub type Result<T> = std::result::Result<T, ExtError>;

#[derive(Debug, Error)]
pub enum ExtError {
    #[error("extension `{name}`: {message}")]
    Extension { name: String, message: String },

    #[error("extension load failed: {0}")]
    Load(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}