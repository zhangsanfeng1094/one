use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use crate::builtin::StatusExtension;
use crate::runtime::ExtensionRuntime;
use crate::traits::Extension;

#[derive(Debug, Deserialize)]
struct ExtensionManifestEntry {
    name: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    builtin: bool,
}

pub async fn discover_extensions(agent_dir: &Path) -> crate::Result<ExtensionRuntime> {
    let mut extensions: Vec<Arc<dyn Extension>> = Vec::new();
    let manifest_path = agent_dir.join("extensions.json");

    if manifest_path.exists() {
        let raw = tokio::fs::read_to_string(&manifest_path).await?;
        let entries: Vec<ExtensionManifestEntry> = serde_json::from_str(&raw)?;
        for entry in entries {
            if entry.builtin {
                if let Some(ext) = builtin_by_name(&entry.name) {
                    extensions.push(ext);
                }
            } else if let Some(path) = entry.path {
                let full = agent_dir.join(path);
                #[cfg(feature = "dylib")]
                if full.extension().and_then(|e| e.to_str()) == Some("so")
                    || full.extension().and_then(|e| e.to_str()) == Some("dll")
                {
                    if let Ok(ext) = crate::dylib::load(&full) {
                        extensions.push(ext);
                    }
                }
                let _ = full;
            }
        }
    }

    if extensions.is_empty() {
        extensions.push(Arc::new(StatusExtension::new()));
    }

    Ok(ExtensionRuntime::new(extensions))
}

fn builtin_by_name(name: &str) -> Option<Arc<dyn Extension>> {
    match name {
        "status" => Some(Arc::new(StatusExtension::new())),
        _ => None,
    }
}