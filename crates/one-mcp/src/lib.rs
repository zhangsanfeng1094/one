//! # one-mcp
//!
//! Platform foundation MCP client for One.
//!
//! - **Config**: One owns `~/.one/agent/mcp.json` + `.one/mcp.json` only
//! - **Import**: scan Claude / Codex / Cursor explicitly (TUI or `one mcp import`)
//! - **Connect**: async background handshakes; tools appear as servers ready
//! - **Sessions**: connection pool survives `/new` (only messages reset)

pub mod config;
pub mod error;
pub mod manager;
pub mod naming;
mod paths;
pub mod tool;

pub use config::{
    expand_env, import_servers_to_user, load_effective, load_merged, load_one_only,
    load_user_or_empty, parse_config_json, project_mcp_path, save_user_config,
    scan_import_candidates, set_server_disabled_persistent, user_mcp_path, ConfigSourceKind,
    ConfigSourceReport, ImportCandidate, ImportReport, LoadedMcpConfig, McpConfig, McpServerConfig,
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_STARTUP_TIMEOUT_SEC, DEFAULT_TOOL_TIMEOUT_SEC,
};
pub use error::{McpError, Result};
pub use manager::{
    probe_server, McpChip, McpChipKind, McpLoadStatus, McpManager, McpProgressHandle, McpServerRow,
    ServerHealth,
};
pub use naming::public_tool_name;
