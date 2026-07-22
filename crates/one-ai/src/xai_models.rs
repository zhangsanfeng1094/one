//! Built-in xAI Grok (SuperGrok OAuth) model catalog + `models.json` seeding.

use std::path::Path;

use crate::auth::{PROVIDER_XAI, XAI_BASE_URL};
use crate::models_file::{load_models_file, save_models_file, ModelsConfig};
use crate::openai::ProviderApi;
use crate::registry::{ModelEntry, ProviderConfig};

#[derive(Debug, Clone, Copy)]
pub struct XaiModelDef {
    pub id: &'static str,
    pub name: &'static str,
    pub context_window: u32,
    pub reasoning: bool,
}

/// Curated SuperGrok / Grok Build models (subscription proxy catalog).
pub const XAI_BUILTIN_MODELS: &[XaiModelDef] = &[
    XaiModelDef {
        id: "grok-4.5",
        name: "Grok 4.5",
        context_window: 500_000,
        reasoning: true,
    },
    XaiModelDef {
        id: "grok-build",
        name: "Grok Build",
        context_window: 256_000,
        reasoning: true,
    },
    XaiModelDef {
        id: "grok-composer-2.5-fast",
        name: "Grok Composer 2.5 Fast",
        context_window: 200_000,
        reasoning: false,
    },
    XaiModelDef {
        id: "grok-4.20-0309-reasoning",
        name: "Grok 4.20 Reasoning",
        context_window: 256_000,
        reasoning: true,
    },
    XaiModelDef {
        id: "grok-4.20-0309-non-reasoning",
        name: "Grok 4.20 Non-Reasoning",
        context_window: 256_000,
        reasoning: false,
    },
];

pub const XAI_DEFAULT_MODEL: &str = "grok-4.5";

#[derive(Debug, Clone)]
pub struct XaiSeedReport {
    pub path: std::path::PathBuf,
    pub added: usize,
    pub updated: usize,
    pub total: usize,
    pub default_model: String,
}

pub fn xai_model_entries() -> Vec<ModelEntry> {
    XAI_BUILTIN_MODELS
        .iter()
        .map(|m| ModelEntry {
            provider: PROVIDER_XAI.into(),
            id: m.id.into(),
            name: m.name.into(),
            context_window: Some(m.context_window),
            // Subscription proxy speaks Responses API (Grok CLI).
            api: Some("openai-responses".into()),
            base_url: Some(XAI_BASE_URL.into()),
            api_key: None,
            reasoning: Some(m.reasoning),
            thinking_level_map: None,
            compat: None,
        })
        .collect()
}

pub fn seed_xai_models(path: &Path) -> Result<XaiSeedReport, String> {
    let mut cfg = if path.exists() {
        crate::models_file::try_load_models_file(path)?
    } else {
        ModelsConfig::with_defaults()
    };

    let mut added = 0usize;
    let mut updated = 0usize;
    for entry in xai_model_entries() {
        let existed = cfg.find_model(PROVIDER_XAI, &entry.id).is_some();
        cfg.upsert_model(entry);
        if existed {
            updated += 1;
        } else {
            added += 1;
        }
    }

    cfg.ensure_provider(PROVIDER_XAI);
    if let Some(p) = cfg.provider_mut(PROVIDER_XAI) {
        p.base_url = Some(XAI_BASE_URL.into());
        p.api = Some(ProviderApi::OpenaiResponses);
        p.provider_type = Some("openai-responses".into());
        p.default_model = Some(XAI_DEFAULT_MODEL.into());
    } else {
        cfg.upsert_provider(ProviderConfig {
            id: PROVIDER_XAI.into(),
            provider_type: Some("openai-responses".into()),
            base_url: Some(XAI_BASE_URL.into()),
            api: Some(ProviderApi::OpenaiResponses),
            api_key: None,
            api_key_raw: None,
            default_model: Some(XAI_DEFAULT_MODEL.into()),
            compat: None,
        });
    }

    save_models_file(path, &cfg)?;
    let total = cfg.registry.list_by_provider(PROVIDER_XAI).len();
    Ok(XaiSeedReport {
        path: path.to_path_buf(),
        added,
        updated,
        total,
        default_model: XAI_DEFAULT_MODEL.into(),
    })
}

pub fn seed_xai_models_default() -> Result<XaiSeedReport, String> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    seed_xai_models(&home.join(".one/agent/models.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn seed_writes_models() {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("one-xai-seed-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        std::fs::write(
            &path,
            "{\n  \"includeDefaults\": false,\n  \"providers\": {}\n}\n",
        )
        .unwrap();
        let report = seed_xai_models(&path).unwrap();
        assert_eq!(report.added, XAI_BUILTIN_MODELS.len());
        let cfg = load_models_file(&path);
        assert!(cfg.find_model(PROVIDER_XAI, "grok-4.5").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
