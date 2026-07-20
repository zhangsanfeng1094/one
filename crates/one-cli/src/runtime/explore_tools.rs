//! Explore-mode tool set: **hard whitelist**, not coding-minus-writes.

use std::sync::Arc;

use one_core::tool::Tool;
use one_tools::{
    FindTool, GrepTool, LsTool, PathPolicy, ReadTool,
};

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
    #[allow(unused_mut)]
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::with_policy(policy.clone())),
        Arc::new(GrepTool::with_policy(policy.clone())),
        Arc::new(FindTool::with_policy(policy.clone())),
        Arc::new(LsTool::with_policy(policy)),
    ];
    #[cfg(feature = "network")]
    {
        tools.push(Arc::new(one_tools::WebSearchTool::new()));
        tools.push(Arc::new(one_tools::WebFetchTool::new()));
    }
    tools
}

/// Names actually registered (order may match construction).
pub fn explore_tool_names() -> Vec<String> {
    EXPLORE_TOOL_NAMES.iter().map(|s| (*s).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn explore_has_no_write_or_bash() {
        let tools = explore_tools(PathPolicy::workspace(PathBuf::from("/tmp")));
        let names: Vec<_> = tools.iter().map(|t| t.definition().name).collect();
        for forbidden in ["write", "edit", "bash", "bash_output", "bash_kill", "ask_user", "task"] {
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
