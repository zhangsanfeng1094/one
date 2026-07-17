use thiserror::Error;

pub type Result<T> = std::result::Result<T, ExtError>;

#[derive(Debug, Error)]
pub enum ExtError {
    #[error("extension `{name}`: {message}")]
    Extension { name: String, message: String },

    #[error("extension load failed: {0}")]
    Load(String),

    #[error("hook `{name}` failed: {message}")]
    Hook { name: String, message: String },

    #[error("plugin `{name}`: {message}")]
    Plugin { name: String, message: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml error: {0}")]
    Toml(String),
}
