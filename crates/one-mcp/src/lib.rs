//! # one-mcp
//!
//! Platform foundation MCP client for One (Grok-aligned loading).
//!
//! - **Config**: One native `mcp.json` only; also scans Claude / Cursor / `.mcp.json`
//! - **Connect**: async background handshakes; tools appear as servers ready
//! - **Sessions**: connection pool survives `/new` (only messages reset)

pub mod config;
pub mod error;
pub mod manager;
pub mod naming;
mod paths;
pub mod tool;

pub use config::{
    expand_env, load_effective, load_merged, load_user_or_empty, parse_config_json,
    project_mcp_path, save_user_config, set_server_disabled_persistent, user_mcp_path,
    ConfigSourceKind, ConfigSourceReport, LoadedMcpConfig, McpConfig, McpServerConfig,
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_STARTUP_TIMEOUT_SEC, DEFAULT_TOOL_TIMEOUT_SEC,
};
pub use error::{McpError, Result};
pub use manager::{
    probe_server, McpChip, McpChipKind, McpLoadStatus, McpManager, McpProgressHandle, McpServerRow,
    ServerHealth,
};
pub use naming::public_tool_name;
