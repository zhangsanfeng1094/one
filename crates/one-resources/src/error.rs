use thiserror::Error;

pub type Result<T> = std::result::Result<T, ResourceError>;

#[derive(Debug, Error)]
pub enum ResourceError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}