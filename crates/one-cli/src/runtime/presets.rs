//! Load AgentSpec from preset name, JSON file, Markdown frontmatter, or inline value.

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

    // Project then user disk: .json preferred over .md for same stem.
    for dir in agent_search_dirs(cwd) {
        let json = dir.join(format!("{name}.json"));
        if json.is_file() {
            return load_spec_file(&json);
        }
        let md = dir.join(format!("{name}.md"));
        if md.is_file() {
            return load_spec_file(&md);
        }
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

/// Load a harness agent file (`.json` full AgentSpec or `.md` frontmatter + body).
pub fn load_spec_file(path: &Path) -> Result<AgentSpec, ProtocolError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            format!("read {}: {e}", path.display()),
        )
    })?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" => load_spec_markdown(&raw, path),
        _ => load_spec_json(&raw),
    }
}

pub fn load_spec_json(raw: &str) -> Result<AgentSpec, ProtocolError> {
    serde_json::from_str(raw).map_err(|e| {
        ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            format!("invalid AgentSpec JSON: {e}"),
        )
    })
}

/// Markdown agent: YAML frontmatter → AgentSpec fields; body → `system_prompt` if unset.
///
/// ```markdown
/// ---
/// name: implementer
/// description: Writes code
/// tools:
///   profile: none
///   allow: [read, write, edit, bash]
/// max_turns: 24
/// permission_mode: dont_ask
/// ---
/// You implement changes. Do not ask the user.
/// ```
pub fn load_spec_markdown(raw: &str, path: &Path) -> Result<AgentSpec, ProtocolError> {
    let (fm, body) = split_frontmatter(raw);
    if fm.trim().is_empty() {
        return Err(ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            format!(
                "{}: agent markdown requires YAML frontmatter between --- lines",
                path.display()
            ),
        ));
    }
    let mut value: serde_json::Value = serde_yaml::from_str(&fm).map_err(|e| {
        ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            format!("{}: invalid agent frontmatter YAML: {e}", path.display()),
        )
    })?;
    let obj = value.as_object_mut().ok_or_else(|| {
        ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            format!("{}: frontmatter must be a YAML mapping", path.display()),
        )
    })?;
    let body_trim = body.trim();
    if !body_trim.is_empty() {
        let needs_prompt = match obj.get("system_prompt") {
            None => true,
            Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::String(s)) if s.is_empty() => true,
            _ => false,
        };
        if needs_prompt {
            obj.insert(
                "system_prompt".into(),
                serde_json::Value::String(body_trim.to_string()),
            );
        } else if !obj.contains_key("append_system_prompt") {
            // Keep explicit system_prompt; append body as extra guidance.
            obj.insert(
                "append_system_prompt".into(),
                serde_json::Value::String(body_trim.to_string()),
            );
        }
    }
    if !obj.contains_key("name") {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            obj.insert("name".into(), serde_json::Value::String(stem.to_string()));
        }
    }
    serde_json::from_value(value).map_err(|e| {
        ProtocolError::new(
            error_code::INVALID_AGENT_SPEC,
            format!(
                "{}: frontmatter is not a valid AgentSpec: {e}",
                path.display()
            ),
        )
    })
}

fn split_frontmatter(content: &str) -> (String, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (String::new(), content.to_string());
    }
    let after = &trimmed[3..];
    let after = after
        .strip_prefix('\n')
        .or_else(|| after.strip_prefix("\r\n"))
        .unwrap_or(after);
    if let Some(end) = after.find("\n---") {
        let fm = after[..end].to_string();
        let rest = &after[end + 4..];
        let body = rest
            .strip_prefix('\n')
            .or_else(|| rest.strip_prefix("\r\n"))
            .unwrap_or(rest)
            .to_string();
        return (fm, body);
    }
    (String::new(), content.to_string())
}

/// Serialize builtin/preset to pretty JSON (for `one agent dump`).
pub fn dump_preset(name: &str, cwd: &Path) -> Result<String, ProtocolError> {
    let spec = load_preset(name, cwd)?;
    serde_json::to_string_pretty(&spec)
        .map_err(|e| ProtocolError::new(error_code::INTERNAL, format!("serialize AgentSpec: {e}")))
}

/// Where an agent definition was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    Project,
    User,
    Builtin,
}

impl AgentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::Builtin => "builtin",
        }
    }
}

/// One row for TUI /agents or CLI listing.
#[derive(Debug, Clone)]
pub struct AgentCatalogEntry {
    pub name: String,
    pub source: AgentSource,
    /// Absolute path when on disk; `None` for pure builtins.
    pub path: Option<PathBuf>,
    pub description: String,
    pub tools_preview: String,
    pub max_turns: Option<usize>,
    pub isolation: String,
    pub spawn_allow: Vec<String>,
}

/// Project agents dir: `<cwd>/.one/agents`.
pub fn project_agents_dir(cwd: &Path) -> PathBuf {
    cwd.join(".one").join("agents")
}

/// User agents dir: `~/.one/agent/agents`.
pub fn user_agents_dir() -> PathBuf {
    agent_dir().join("agents")
}

/// List all agents: disk (project + user) then builtins not already shadowed.
pub fn list_agents(cwd: &Path) -> Vec<AgentCatalogEntry> {
    use crate::runtime::harness::preview_tool_names;
    let mut by_name: std::collections::BTreeMap<String, AgentCatalogEntry> =
        std::collections::BTreeMap::new();

    // User first, project overwrites (same as load priority for display).
    let user_dir = user_agents_dir();
    scan_agent_dir(&user_dir, AgentSource::User, &mut by_name);
    let project_dir = project_agents_dir(cwd);
    scan_agent_dir(&project_dir, AgentSource::Project, &mut by_name);

    // Builtins if not already present as disk files.
    for (name, spec) in [
        ("explore", AgentSpec::builtin_explore()),
        ("main", AgentSpec::builtin_main()),
    ] {
        if by_name.contains_key(name) {
            continue;
        }
        by_name.insert(
            name.into(),
            catalog_from_spec(name, AgentSource::Builtin, None, &spec),
        );
    }

    // Stable sort: project, user, builtin; then name.
    let mut out: Vec<_> = by_name.into_values().collect();
    out.sort_by(|a, b| {
        let rank = |s: AgentSource| match s {
            AgentSource::Project => 0,
            AgentSource::User => 1,
            AgentSource::Builtin => 2,
        };
        rank(a.source)
            .cmp(&rank(b.source))
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

fn scan_agent_dir(
    dir: &Path,
    source: AgentSource,
    out: &mut std::collections::BTreeMap<String, AgentCatalogEntry>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut by_stem: std::collections::BTreeMap<String, PathBuf> =
        std::collections::BTreeMap::new();
    for ent in rd.flatten() {
        let path = ent.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "json" && ext != "md" && ext != "markdown" {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match by_stem.get(stem) {
            Some(existing) if existing.extension().and_then(|e| e.to_str()) == Some("json") => {}
            _ => {
                if ext == "json" || !by_stem.contains_key(stem) {
                    by_stem.insert(stem.to_string(), path);
                }
            }
        }
    }
    for (stem, path) in by_stem {
        if let Ok(spec) = load_spec_file(&path) {
            let name = spec.name.clone().unwrap_or_else(|| stem.clone());
            let abs = path.canonicalize().unwrap_or(path);
            out.insert(
                name.clone(),
                catalog_from_spec(&name, source, Some(abs), &spec),
            );
        }
    }
}

fn catalog_from_spec(
    name: &str,
    source: AgentSource,
    path: Option<PathBuf>,
    spec: &AgentSpec,
) -> AgentCatalogEntry {
    use crate::runtime::harness::preview_tool_names;
    let tools = preview_tool_names(spec);
    let tools_preview = if tools.is_empty() {
        "(no tools)".into()
    } else if tools.len() <= 6 {
        tools.join(", ")
    } else {
        format!("{}, … (+{})", tools[..5].join(", "), tools.len() - 5)
    };
    let mut desc = spec.description.clone().unwrap_or_default();
    if desc.chars().count() > 100 {
        desc = desc.chars().take(97).collect::<String>() + "…";
    }
    AgentCatalogEntry {
        name: name.to_string(),
        source,
        path,
        description: desc,
        tools_preview,
        max_turns: spec.max_turns,
        isolation: spec.isolation.as_str().to_string(),
        spawn_allow: spec.spawn_policy.allow.clone(),
    }
}

/// Absolute path for a named agent if it exists on disk (project then user).
pub fn resolve_agent_path(name: &str, cwd: &Path) -> Option<PathBuf> {
    let name = name.trim();
    for dir in agent_search_dirs(cwd) {
        for ext in ["json", "md"] {
            let p = dir.join(format!("{name}.{ext}"));
            if p.is_file() {
                return Some(p.canonicalize().unwrap_or(p));
            }
        }
    }
    None
}

/// Discover `*.json` / `*.md` agent files (project then user) and merge into parent.
///
/// - Skips `main` / `default` (those are the parent itself).
/// - Inserts into `agents` table when missing.
/// - Appends name to `spawn_policy.allow` when not already listed (unless allow
///   contains `"*"`).
/// - Ensures `max_depth >= 1` when any workers are present.
/// - Same stem: `.json` wins over `.md` (project over user via scan order).
pub fn merge_discovered_agents(parent: &mut AgentSpec, cwd: &Path) {
    let mut found: std::collections::BTreeMap<String, AgentSpec> =
        std::collections::BTreeMap::new();
    // User first, then project overwrites (project wins).
    for dir in agent_search_dirs(cwd).into_iter().rev() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        // Collect by stem; prefer json when both exist in one dir.
        let mut by_stem: std::collections::BTreeMap<String, PathBuf> =
            std::collections::BTreeMap::new();
        for ent in rd.flatten() {
            let path = ent.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "json" && ext != "md" && ext != "markdown" {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem == "main" || stem == "default" {
                continue;
            }
            match by_stem.get(stem) {
                Some(existing) if existing.extension().and_then(|e| e.to_str()) == Some("json") => {
                    // keep json
                }
                _ => {
                    if ext == "json" || !by_stem.contains_key(stem) {
                        by_stem.insert(stem.to_string(), path);
                    }
                }
            }
        }
        for (stem, path) in by_stem {
            if let Ok(mut child) = load_spec_file(&path) {
                if child.name.is_none() {
                    child.name = Some(stem.clone());
                }
                if child.spawn_policy.allow.is_empty() {
                    child.spawn_policy = crate::protocol::SpawnPolicy::none();
                }
                found.insert(stem, child);
            }
        }
    }
    if found.is_empty() {
        return;
    }
    let star = parent.spawn_policy.allow.iter().any(|a| a == "*");
    for (name, child) in found {
        parent.agents.entry(name.clone()).or_insert(child);
        if !star && !parent.spawn_policy.allow.iter().any(|a| a == &name) {
            parent.spawn_policy.allow.push(name);
        }
    }
    if !parent.spawn_policy.allow.is_empty() && parent.spawn_policy.max_depth == 0 {
        parent.spawn_policy.max_depth = 1;
    }
    if parent.spawn_policy.max_concurrent == 0 && parent.can_spawn() {
        parent.spawn_policy.max_concurrent = 4;
    }
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

    #[test]
    fn merge_discovered_agents_from_disk() {
        let dir = std::env::temp_dir().join(format!("one-agents-{}", std::process::id()));
        let agents = dir.join(".one").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let worker = r#"{
            "name": "implementer",
            "description": "Writes code",
            "system_prompt": "You implement.",
            "tools": { "profile": "none", "allow": ["read", "write", "edit"] },
            "max_turns": 8,
            "spawn_policy": { "allow": [], "max_depth": 0, "max_concurrent": 0 }
        }"#;
        std::fs::write(agents.join("implementer.json"), worker).unwrap();

        let mut main = AgentSpec::builtin_main();
        merge_discovered_agents(&mut main, &dir);
        assert!(main.agents.contains_key("implementer"));
        assert!(main.spawn_policy.allow.iter().any(|a| a == "implementer"));
        assert!(main.spawn_policy.max_depth >= 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_agents_includes_disk_and_builtin() {
        let dir = std::env::temp_dir().join(format!("one-agents-list-{}", std::process::id()));
        let agents = dir.join(".one").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("research.json"),
            r#"{"name":"research","tools":{"profile":"none","allow":["read"]},"system_prompt":"x"}"#,
        )
        .unwrap();
        let list = list_agents(&dir);
        assert!(list
            .iter()
            .any(|e| e.name == "research" && e.path.is_some()));
        assert!(list.iter().any(|e| e.name == "explore"));
        let research = list.iter().find(|e| e.name == "research").unwrap();
        assert!(research
            .path
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .contains("research.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_markdown_agent() {
        let dir = std::env::temp_dir().join(format!("one-agents-md-{}", std::process::id()));
        let agents = dir.join(".one").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let md = r#"---
name: reviewer
description: Reviews diffs
tools:
  profile: none
  allow:
    - read
    - grep
max_turns: 10
permission_mode: dont_ask
---
You review code. Be concise.
"#;
        let path = agents.join("reviewer.md");
        std::fs::write(&path, md).unwrap();
        let spec = load_spec_file(&path).unwrap();
        assert_eq!(spec.display_name(), "reviewer");
        assert_eq!(spec.tools.allow, vec!["read", "grep"]);
        assert!(spec
            .system_prompt
            .as_deref()
            .unwrap_or("")
            .contains("review code"));

        let mut main = AgentSpec::builtin_main();
        merge_discovered_agents(&mut main, &dir);
        assert!(main.agents.contains_key("reviewer"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
