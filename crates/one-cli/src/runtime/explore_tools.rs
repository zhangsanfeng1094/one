//! Explore-mode tool set: **hard whitelist**, not coding-minus-writes.
//!
//! Implemented via [`one_tools::ToolRegistry`] (Explore profile).

use std::sync::Arc;

use one_core::tool::Tool;
use one_tools::{materialize_explore, PathPolicy, ToolBuildContext};

/// Canonical explore tool names (docs + tests must stay aligned).
pub const EXPLORE_TOOL_NAMES: &[&str] = &[
    "read",
    "grep",
    "find",
    "ls",
    #[cfg(feature = "network")]
    "web_search",
    #[cfg(feature = "network")]
    "web_fetch",
];

/// Build explore tools only. Never includes write/edit/bash/ask_user/task/MCP.
pub fn explore_tools(policy: PathPolicy) -> Vec<Arc<dyn Tool>> {
    let ctx = ToolBuildContext::workspace(policy.cwd().to_path_buf()).with_policy(policy);
    materialize_explore(&ctx)
}

/// Names actually registered (order may match construction).
pub fn explore_tool_names() -> Vec<String> {
    EXPLORE_TOOL_NAMES
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn explore_has_no_write_or_bash() {
        let tools = explore_tools(PathPolicy::workspace(PathBuf::from("/tmp")));
        let names: Vec<_> = tools.iter().map(|t| t.definition().name).collect();
        for forbidden in [
            "write",
            "edit",
            "bash",
            "bash_output",
            "bash_kill",
            "ask_user",
            "task",
        ] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "explore must not include {forbidden}, got {names:?}"
            );
        }
        for required in ["read", "grep", "find", "ls"] {
            assert!(
                names.iter().any(|n| n == required),
                "explore must include {required}, got {names:?}"
            );
        }
    }
}
