use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("mcp config: {0}")]
    Config(String),

    #[error("mcp server `{server}`: {message}")]
    Server { server: String, message: String },

    #[error("mcp io: {0}")]
    Io(#[from] std::io::Error),

    #[error("mcp: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, McpError>;

impl McpError {
    pub fn server(server: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Server {
            server: server.into(),
            message: message.into(),
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}
