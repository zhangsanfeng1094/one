//! Plugin discovery (Codex `plugin.json` analogue for One).
//!
//! A plugin is a directory with `.one-plugin/plugin.json` (or `plugin.json` at root)
//! that can contribute skills paths, MCP server snippets, hook configs, and
//! extension references (builtin names or dylib paths).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Parsed plugin manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Relative skill roots under the plugin directory.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Relative prompt roots.
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Builtin extension names or relative paths to dylibs.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Optional MCP servers contributed by this plugin (same shape as mcp.json entries).
    #[serde(default, alias = "mcp_servers")]
    pub mcp_servers: BTreeMap<String, serde_json::Value>,
    /// Relative path to hooks.toml / hooks.json, or inline hook table path.
    #[serde(default)]
    pub hooks: Option<String>,
    /// Optional system overlay markdown (relative path).
    #[serde(default)]
    pub system_overlay: Option<String>,
}

/// A discovered plugin on disk.
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    pub root: PathBuf,
    pub manifest: PluginManifest,
    pub enabled: bool,
}

impl DiscoveredPlugin {
    pub fn skill_dirs(&self) -> Vec<PathBuf> {
        self.manifest
            .skills
            .iter()
            .map(|s| self.root.join(s))
            .filter(|p| p.is_dir())
            .collect()
    }

    pub fn prompt_dirs(&self) -> Vec<PathBuf> {
        self.manifest
            .prompts
            .iter()
            .map(|s| self.root.join(s))
            .filter(|p| p.is_dir())
            .collect()
    }

    pub fn system_overlay_path(&self) -> Option<PathBuf> {
        self.manifest
            .system_overlay
            .as_ref()
            .map(|p| self.root.join(p))
            .filter(|p| p.is_file())
    }

    pub fn hooks_path(&self) -> Option<PathBuf> {
        self.manifest
            .hooks
            .as_ref()
            .map(|p| self.root.join(p))
            .filter(|p| p.is_file())
    }
}

/// Find plugin.json under a plugin root.
pub fn find_plugin_manifest_path(root: &Path) -> Option<PathBuf> {
    let candidates = [
        root.join(".one-plugin").join("plugin.json"),
        root.join(".codex-plugin").join("plugin.json"),
        root.join("plugin.json"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

pub fn load_plugin_manifest(path: &Path) -> crate::Result<PluginManifest> {
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

/// Scan `dir` for plugin subdirectories (one level).
pub async fn discover_plugins_in(dir: &Path) -> crate::Result<Vec<DiscoveredPlugin>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(manifest_path) = find_plugin_manifest_path(&path) else {
            continue;
        };
        match load_plugin_manifest(&manifest_path) {
            Ok(manifest) => {
                out.push(DiscoveredPlugin {
                    root: path,
                    manifest,
                    enabled: true,
                });
            }
            Err(e) => {
                tracing::warn!(
                    path = %manifest_path.display(),
                    error = %e,
                    "skip invalid plugin manifest"
                );
            }
        }
    }
    out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    Ok(out)
}

/// Default discovery roots (project → user).
pub async fn discover_plugins(cwd: &Path, agent_dir: &Path) -> crate::Result<Vec<DiscoveredPlugin>> {
    let mut all = Vec::new();
    let roots = [
        cwd.join(".one").join("plugins"),
        agent_dir.join("plugins"),
    ];
    for root in roots {
        all.extend(discover_plugins_in(&root).await?);
    }
    // Dedup by name (project first).
    let mut seen = std::collections::HashSet::new();
    all.retain(|p| seen.insert(p.manifest.name.clone()));
    Ok(all)
}

/// Filter by optional enable list from extensions.json / settings.
pub fn apply_plugin_enablement(
    plugins: &mut [DiscoveredPlugin],
    disabled: &[String],
    enabled_only: Option<&[String]>,
) {
    for p in plugins.iter_mut() {
        if disabled
            .iter()
            .any(|d| d.eq_ignore_ascii_case(&p.manifest.name))
        {
            p.enabled = false;
            continue;
        }
        if let Some(allow) = enabled_only {
            p.enabled = allow
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&p.manifest.name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_minimal_manifest() {
        let raw = r#"{"name":"demo","skills":["skills"],"extensions":["status"]}"#;
        let m: PluginManifest = serde_json::from_str(raw).unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.skills, vec!["skills"]);
    }

    #[tokio::test]
    async fn discover_plugin_dir() {
        let dir = tempfile_dir();
        let plugin = dir.join("demo");
        std::fs::create_dir_all(plugin.join(".one-plugin")).unwrap();
        let mut f = std::fs::File::create(plugin.join(".one-plugin/plugin.json")).unwrap();
        write!(f, r#"{{"name":"demo","version":"0.1.0"}}"#).unwrap();
        let found = discover_plugins_in(&dir).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest.name, "demo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("one-ext-plugin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
