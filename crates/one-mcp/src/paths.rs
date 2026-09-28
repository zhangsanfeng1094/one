//! Agent-home path resolution for MCP config.
//!
//! Mirrors `one-session::paths::agent_dir` so `mcp.json` lands where the rest of
//! One looks for it. Without the `ONE_AGENT_DIR` / `ONE_DATA_DIR` checks below,
//! MCP would silently read a different file than the path policy and the TUI
//! report, which makes "which file is actually in effect?" unanswerable.

use std::path::PathBuf;

/// `~/.one/agent`, honouring `ONE_AGENT_DIR` / `ONE_DATA_DIR`.
pub fn agent_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ONE_AGENT_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir.trim());
        }
    }
    if let Ok(dir) = std::env::var("ONE_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir.trim()).join("agent");
        }
    }
    dirs_home().join(".one").join("agent")
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honours_agent_dir_env_override() {
        let previous = std::env::var("ONE_AGENT_DIR").ok();
        std::env::set_var("ONE_AGENT_DIR", "/tmp/one-mcp-paths-test");
        assert_eq!(agent_dir(), PathBuf::from("/tmp/one-mcp-paths-test"));
        match previous {
            Some(v) => std::env::set_var("ONE_AGENT_DIR", v),
            None => std::env::remove_var("ONE_AGENT_DIR"),
        }
    }

    #[test]
    fn defaults_to_home_dot_one_agent() {
        let previous = std::env::var("ONE_AGENT_DIR").ok();
        std::env::remove_var("ONE_AGENT_DIR");
        let expected = dirs_home().join(".one").join("agent");
        assert_eq!(agent_dir(), expected);
        if let Some(v) = previous {
            std::env::set_var("ONE_AGENT_DIR", v);
        }
    }
}
