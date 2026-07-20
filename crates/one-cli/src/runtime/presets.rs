//! Load AgentSpec from preset name, JSON file, or inline value.

use std::path::{Path, PathBuf};

use one_session::agent_dir;

use crate::protocol::{error_code, AgentRef, AgentSpec, ProtocolError};

/// Resolve [`AgentRef`] to a full [`AgentSpec`].
pub fn resolve_agent_ref(r: &AgentRef, cwd: &Path) -> Result<AgentSpec, ProtocolError> {
    match r {
        AgentRef::Preset(name) => load_preset(name, cwd),
        AgentRef::Spec(spec) => Ok(spec.clone()),
    }
}

/// Load by preset / disk agent name.
pub fn load_preset(name: &str, cwd: &Path) -> Result<AgentSpec, ProtocolError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            "empty agent/preset name",
        ));
    }

    // Project then user disk (json or md later; json first).
    for dir in agent_search_dirs(cwd) {
        let json = dir.join(format!("{name}.json"));
        if json.is_file() {
            return load_spec_file(&json);
        }
        // Future: .md frontmatter — not required for MVP CLI.
    }

    // Built-ins
    match name {
        "explore" => Ok(AgentSpec::builtin_explore()),
        "main" | "default" => Ok(AgentSpec::builtin_main()),
        other => Err(ProtocolError::new(
            error_code::UNKNOWN_AGENT,
            format!("unknown preset or agent `{other}`"),
        )),
    }
}

fn agent_search_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    dirs.push(cwd.join(".one").join("agents"));
    dirs.push(agent_dir().join("agents"));
    dirs
}

/// Load a full harness JSON file (`AgentSpec`).
pub fn load_spec_file(path: &Path) -> Result<AgentSpec, ProtocolError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            format!("read {}: {e}", path.display()),
        )
    })?;
    load_spec_json(&raw)
}

pub fn load_spec_json(raw: &str) -> Result<AgentSpec, ProtocolError> {
    serde_json::from_str(raw).map_err(|e| {
        ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            format!("invalid AgentSpec JSON: {e}"),
        )
    })
}

/// Serialize builtin/preset to pretty JSON (for `one agent dump`).
pub fn dump_preset(name: &str, cwd: &Path) -> Result<String, ProtocolError> {
    let spec = load_preset(name, cwd)?;
    serde_json::to_string_pretty(&spec).map_err(|e| {
        ProtocolError::new(error_code::INTERNAL, format!("serialize AgentSpec: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn load_explore_builtin() {
        let s = load_preset("explore", &PathBuf::from("/tmp")).unwrap();
        assert_eq!(s.display_name(), "explore");
        assert!(s.system_prompt.is_some());
    }

    #[test]
    fn unknown_preset_errors() {
        let e = load_preset("no-such-agent-xyz", &PathBuf::from("/tmp")).unwrap_err();
        assert_eq!(e.code, error_code::UNKNOWN_AGENT);
    }

    #[test]
    fn dump_explore_roundtrip() {
        let j = dump_preset("explore", &PathBuf::from("/tmp")).unwrap();
        let s = load_spec_json(&j).unwrap();
        assert_eq!(s.display_name(), "explore");
    }
}
