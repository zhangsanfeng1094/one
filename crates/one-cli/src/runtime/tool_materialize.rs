//! Materialize [`crate::protocol::ToolsSpec`] via [`one_tools::ToolRegistry`].
//!
//! Single path for harness + (optionally) parent assembly: resolve names → factories.

use std::sync::Arc;

use one_core::tool::Tool;
use one_tools::{
    resolve_tool_names, BuiltinToolProfile, ToolBuildContext, ToolRegistry, UnknownToolError,
};

use crate::protocol::{error_code, ProtocolError, ToolProfile, ToolsSpec};

/// Map protocol profile → builtin catalog.
pub fn to_builtin_profile(profile: &ToolProfile) -> BuiltinToolProfile {
    match profile {
        ToolProfile::Coding => BuiltinToolProfile::Coding,
        ToolProfile::ReadOnly => BuiltinToolProfile::ReadOnly,
        ToolProfile::Plan => BuiltinToolProfile::Plan,
        ToolProfile::None => BuiltinToolProfile::None,
    }
}

/// Explore hard-whitelist profile used when name is explore / default research face.
pub fn explore_profile() -> BuiltinToolProfile {
    BuiltinToolProfile::Explore
}

/// Resolve final tool names for a ToolsSpec.
///
/// Special case: `read_only` with empty allow and empty extra uses **Explore**
/// catalog (no ask_user) when `prefer_explore_for_read_only` is true — harness
/// subagents default. Main session can pass false to keep ask_user.
pub fn resolve_names(spec: &ToolsSpec, prefer_explore_for_read_only: bool) -> Vec<String> {
    let profile = if prefer_explore_for_read_only
        && matches!(spec.profile, ToolProfile::ReadOnly)
        && spec.allow.is_empty()
        && spec.extra.is_empty()
    {
        // deny still applied (e.g. deny ask_user is no-op on explore)
        BuiltinToolProfile::Explore
    } else {
        to_builtin_profile(&spec.profile)
    };
    resolve_tool_names(profile, &spec.allow, &spec.deny, &spec.extra)
}

/// Build tools from ToolsSpec + registry + context.
pub fn materialize_tools(
    spec: &ToolsSpec,
    registry: &ToolRegistry,
    ctx: &ToolBuildContext,
    prefer_explore_for_read_only: bool,
) -> Result<Vec<Arc<dyn Tool>>, ProtocolError> {
    let mut names = resolve_names(spec, prefer_explore_for_read_only);
    if spec.mcp {
        // Append MCP tools already registered on the registry (dynamic instances).
        for known in registry.known_names() {
            if is_mcp_tool_name(&known) && !names.iter().any(|n| n == &known) {
                names.push(known);
            }
        }
        names = filter_mcp_allow(names, &spec.mcp_allow);
    } else {
        names.retain(|n| !is_mcp_tool_name(n));
    }

    // Plan tools (plan / exit_plan_mode) are only available when registered as
    // instances; skip missing plan names rather than hard-failing profile lists.
    let names: Vec<String> = names
        .into_iter()
        .filter(|n| {
            if n == "plan" || n == "exit_plan_mode" {
                registry.contains(n)
            } else {
                true
            }
        })
        .collect();

    registry
        .materialize(&names, ctx)
        .map_err(|e| ProtocolError::new(error_code::INVALID_AGENT_SPEC, format_unknown_tool(&e)))
}

fn format_unknown_tool(e: &UnknownToolError) -> String {
    format!(
        "tool `{}` not in registry (add a builtin factory, MCP tool, or extension). known: {}",
        e.name,
        e.known.join(", ")
    )
}

fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__") || name.contains("__")
}

fn filter_mcp_allow(names: Vec<String>, mcp_allow: &[String]) -> Vec<String> {
    if mcp_allow.is_empty() {
        return names;
    }
    names
        .into_iter()
        .filter(|n| {
            if !is_mcp_tool_name(n) {
                return true;
            }
            mcp_allow.iter().any(|pat| mcp_name_matches(n, pat))
        })
        .collect()
}

fn mcp_name_matches(name: &str, pattern: &str) -> bool {
    if pattern == "*" || pattern == name {
        return true;
    }
    // server name only: match server__*
    if !pattern.contains("__") {
        return name.starts_with(&format!("{pattern}__"))
            || name.starts_with(&format!("mcp__{pattern}__"));
    }
    name == pattern || name.ends_with(pattern)
}

/// Default registry for harness (builtins only; caller may overlay MCP/extra).
pub fn harness_registry() -> ToolRegistry {
    ToolRegistry::with_builtins()
}

/// Build context for a harness / non-interactive run.
pub fn harness_build_context(
    policy: one_tools::PathPolicy,
    auto_approve: bool,
) -> ToolBuildContext {
    ToolBuildContext {
        policy,
        auto_approve,
        bg_registry: Arc::new(one_tools::BackgroundTaskRegistry::new()),
        ask_user: Some(Arc::new(one_tools::FailClosedAskUser)),
        tool_gate: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ToolsSpec;
    use one_tools::PathPolicy;
    use std::path::PathBuf;

    #[test]
    fn explore_default_excludes_ask_user() {
        let spec = ToolsSpec::read_only();
        let names = resolve_names(&spec, true);
        assert!(!names.iter().any(|n| n == "ask_user"));
        assert!(names.iter().any(|n| n == "read"));
    }

    #[test]
    fn coding_allow_write() {
        let mut reg = harness_registry();
        let _ = &mut reg;
        let spec = ToolsSpec {
            profile: ToolProfile::Coding,
            allow: vec!["read".into(), "write".into(), "edit".into()],
            ..Default::default()
        };
        let ctx = harness_build_context(PathPolicy::workspace(PathBuf::from("/tmp")), true);
        let tools = materialize_tools(&spec, &harness_registry(), &ctx, true).unwrap();
        let names: Vec<_> = tools.iter().map(|t| t.definition().name).collect();
        assert_eq!(names, vec!["read", "write", "edit"]);
    }

    #[test]
    fn unknown_extra_fails() {
        let spec = ToolsSpec {
            profile: ToolProfile::None,
            allow: vec!["not_a_real_tool".into()],
            ..Default::default()
        };
        let ctx = harness_build_context(PathPolicy::workspace(PathBuf::from("/tmp")), true);
        match materialize_tools(&spec, &harness_registry(), &ctx, true) {
            Ok(_) => panic!("expected unknown tool"),
            Err(err) => {
                assert_eq!(err.code, error_code::INVALID_AGENT_SPEC);
                assert!(err.message.contains("not_a_real_tool"));
            }
        }
    }
}
