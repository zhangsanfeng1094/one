//! Discovery: extensions.json + plugins + hooks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::builtin::{builtin_by_name, StatusExtension};
use crate::hooks::{load_hooks, HooksConfig};
use crate::plugin::{apply_plugin_enablement, discover_plugins, DiscoveredPlugin};
use crate::registry::ExtensionRegistryBuilder;
use crate::runtime::ExtensionRuntime;
use crate::traits::Extension;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionsManifest {
    /// Flat list (legacy): array of entries.
    #[serde(default)]
    #[serde(alias = "extensions")]
    entries: Option<Vec<ExtensionManifestEntry>>,
    #[serde(default)]
    disabled_plugins: Vec<String>,
    /// If set, only these plugins are enabled.
    #[serde(default)]
    enabled_plugins: Option<Vec<String>>,
    /// Disable the default `status` builtin when no extensions listed.
    #[serde(default)]
    no_default_status: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ManifestRoot {
    /// Legacy: bare JSON array of entries.
    Array(Vec<ExtensionManifestEntry>),
    Object(ExtensionsManifest),
}

#[derive(Debug, Deserialize)]
struct ExtensionManifestEntry {
    name: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    builtin: bool,
    #[serde(default)]
    enabled: Option<bool>,
}

/// Result of full discovery (runtime + plugin metadata for skills/MCP overlay).
pub struct DiscoveryResult {
    pub runtime: ExtensionRuntime,
    pub plugins: Vec<DiscoveredPlugin>,
    pub skill_dirs: Vec<PathBuf>,
    pub prompt_dirs: Vec<PathBuf>,
    pub system_overlays: Vec<String>,
    /// Raw MCP server JSON objects contributed by plugins (name → value).
    pub plugin_mcp_servers: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Discover extensions from `agent_dir/extensions.json`, plugins, and hooks.
pub async fn discover_extensions(agent_dir: &Path) -> crate::Result<ExtensionRuntime> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Ok(discover_all(&cwd, agent_dir).await?.runtime)
}

/// Full discovery with plugin resource paths (preferred entry for CLI).
pub async fn discover_all(cwd: &Path, agent_dir: &Path) -> crate::Result<DiscoveryResult> {
    let mut builder = ExtensionRegistryBuilder::new();
    let mut no_default_status = false;
    let mut disabled_plugins = Vec::new();
    let mut enabled_plugins: Option<Vec<String>> = None;

    let manifest_path = agent_dir.join("extensions.json");
    if manifest_path.exists() {
        let raw = tokio::fs::read_to_string(&manifest_path).await?;
        match serde_json::from_str::<ManifestRoot>(&raw)? {
            ManifestRoot::Array(entries) => {
                load_entries(&mut builder, agent_dir, &entries);
            }
            ManifestRoot::Object(m) => {
                no_default_status = m.no_default_status;
                disabled_plugins = m.disabled_plugins;
                enabled_plugins = m.enabled_plugins;
                if let Some(entries) = m.entries {
                    load_entries(&mut builder, agent_dir, &entries);
                }
            }
        }
    }

    let mut plugins = discover_plugins(cwd, agent_dir).await?;
    apply_plugin_enablement(&mut plugins, &disabled_plugins, enabled_plugins.as_deref());

    let mut skill_dirs = Vec::new();
    let mut prompt_dirs = Vec::new();
    let mut system_overlays = Vec::new();
    let mut plugin_mcp_servers = std::collections::BTreeMap::new();
    let mut hook_files = Vec::new();

    for plugin in plugins.iter().filter(|p| p.enabled) {
        skill_dirs.extend(plugin.skill_dirs());
        prompt_dirs.extend(plugin.prompt_dirs());
        if let Some(p) = plugin.system_overlay_path() {
            if let Ok(text) = tokio::fs::read_to_string(&p).await {
                system_overlays.push(text);
            }
        }
        if let Some(h) = plugin.hooks_path() {
            hook_files.push(h);
        }
        for (name, val) in &plugin.manifest.mcp_servers {
            plugin_mcp_servers.insert(name.clone(), val.clone());
        }
        for ext_ref in &plugin.manifest.extensions {
            if let Some(ext) = resolve_extension_ref(agent_dir, &plugin.root, ext_ref) {
                builder.install(ext);
            }
        }
    }

    let mut registry = builder.build();
    if registry.is_empty() && !no_default_status {
        let mut b = ExtensionRegistryBuilder::new();
        b.install(Arc::new(StatusExtension::new()));
        registry = b.build();
    }

    let hooks: HooksConfig = load_hooks(agent_dir, &hook_files);
    let runtime = ExtensionRuntime::from_registry(registry, hooks, cwd.to_path_buf());

    Ok(DiscoveryResult {
        runtime,
        plugins,
        skill_dirs,
        prompt_dirs,
        system_overlays,
        plugin_mcp_servers,
    })
}

fn load_entries(
    builder: &mut ExtensionRegistryBuilder,
    agent_dir: &Path,
    entries: &[ExtensionManifestEntry],
) {
    for entry in entries {
        if entry.enabled == Some(false) {
            continue;
        }
        if entry.builtin {
            if let Some(ext) = builtin_by_name(&entry.name) {
                builder.install(ext);
            } else {
                tracing::warn!(name = %entry.name, "unknown builtin extension");
            }
        } else if let Some(path) = &entry.path {
            let full = agent_dir.join(path);
            if let Some(ext) = load_path_extension(&full) {
                builder.install(ext);
            }
        } else if let Some(ext) = builtin_by_name(&entry.name) {
            // Bare name defaults to builtin.
            builder.install(ext);
        }
    }
}

fn resolve_extension_ref(
    agent_dir: &Path,
    plugin_root: &Path,
    ext_ref: &str,
) -> Option<Arc<dyn Extension>> {
    if let Some(ext) = builtin_by_name(ext_ref) {
        return Some(ext);
    }
    let candidates = [
        plugin_root.join(ext_ref),
        agent_dir.join(ext_ref),
        PathBuf::from(ext_ref),
    ];
    for path in candidates {
        if path.is_file() {
            if let Some(ext) = load_path_extension(&path) {
                return Some(ext);
            }
        }
    }
    tracing::warn!(ext_ref, "could not resolve extension reference");
    None
}

fn load_path_extension(path: &Path) -> Option<Arc<dyn Extension>> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if matches!(ext, "so" | "dll" | "dylib") {
        #[cfg(feature = "dylib")]
        {
            return crate::dylib::load(path).ok();
        }
        #[cfg(not(feature = "dylib"))]
        {
            tracing::warn!(
                path = %path.display(),
                "dylib loading requires one-ext feature `dylib`"
            );
            return None;
        }
    }
    None
}
