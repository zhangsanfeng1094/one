//! Built-in OpenAI Codex (ChatGPT OAuth) model catalog.
//!
//! Source of truth aligned with Pi `@earendil-works/pi-ai`
//! `OPENAI_CODEX_MODELS` (package 0.80.x, auto-generated catalog).
//! Not discovered via API — ChatGPT backend has no public OpenAI-style
//! `GET /models` for this wire.

use std::path::Path;

use crate::auth::{CODEX_BASE_URL, PROVIDER_OPENAI_CODEX};
use crate::compat::ThinkingLevelMap;
use crate::models_file::{load_models_file, save_models_file, ModelsConfig};
use crate::openai::ProviderApi;
use crate::registry::{ModelEntry, ProviderConfig};

/// One built-in Codex model definition.
#[derive(Debug, Clone, Copy)]
pub struct CodexModelDef {
    pub id: &'static str,
    pub name: &'static str,
    pub context_window: u32,
    /// Pi `thinkingLevelMap` as `high=high,xhigh=xhigh,minimal=low` style pairs
    /// (only non-identity / non-default mappings we care about for agent levels).
    pub thinking_level_map: Option<&'static str>,
}

/// Full Pi catalog for `openai-codex` (as of pi-ai 0.80.x).
pub const OPENAI_CODEX_BUILTIN_MODELS: &[CodexModelDef] = &[
    CodexModelDef {
        id: "gpt-5.3-codex-spark",
        name: "GPT-5.3 Codex Spark",
        context_window: 128_000,
        thinking_level_map: Some("xhigh=xhigh,minimal=low"),
    },
    CodexModelDef {
        id: "gpt-5.4",
        name: "GPT-5.4",
        context_window: 272_000,
        thinking_level_map: Some("xhigh=xhigh,minimal=low"),
    },
    CodexModelDef {
        id: "gpt-5.4-mini",
        name: "GPT-5.4 mini",
        context_window: 272_000,
        thinking_level_map: Some("xhigh=xhigh,minimal=low"),
    },
    CodexModelDef {
        id: "gpt-5.5",
        name: "GPT-5.5",
        context_window: 272_000,
        thinking_level_map: Some("xhigh=xhigh,minimal=low"),
    },
    CodexModelDef {
        id: "gpt-5.6-luna",
        name: "GPT-5.6 Luna",
        context_window: 372_000,
        thinking_level_map: Some("xhigh=xhigh,max=max,minimal=low"),
    },
    CodexModelDef {
        id: "gpt-5.6-sol",
        name: "GPT-5.6 Sol",
        context_window: 372_000,
        thinking_level_map: Some("xhigh=xhigh,max=max,minimal=low"),
    },
    CodexModelDef {
        id: "gpt-5.6-terra",
        name: "GPT-5.6 Terra",
        context_window: 372_000,
        thinking_level_map: Some("xhigh=xhigh,max=max,minimal=low"),
    },
];

/// Default model id after login / first use.
pub const OPENAI_CODEX_DEFAULT_MODEL: &str = "gpt-5.4";

fn thinking_map(raw: Option<&str>) -> Option<ThinkingLevelMap> {
    let raw = raw?;
    crate::compat::parse_thinking_level_map(raw).ok()
}

/// Registry entries for all built-in Codex models.
pub fn openai_codex_model_entries() -> Vec<ModelEntry> {
    OPENAI_CODEX_BUILTIN_MODELS
        .iter()
        .map(|m| ModelEntry {
            provider: PROVIDER_OPENAI_CODEX.into(),
            id: m.id.into(),
            name: m.name.into(),
            context_window: Some(m.context_window),
            api: Some("openai-codex-responses".into()),
            base_url: Some(CODEX_BASE_URL.into()),
            api_key: None,
            reasoning: Some(true),
            thinking_level_map: thinking_map(m.thinking_level_map),
            compat: None,
        })
        .collect()
}

/// Result of seeding `models.json`.
#[derive(Debug, Clone)]
pub struct CodexSeedReport {
    pub path: std::path::PathBuf,
    pub added: usize,
    pub updated: usize,
    pub total: usize,
    pub default_model: String,
}

/// Upsert every built-in Codex model + provider block into `models.json`.
///
/// - Creates the file from defaults when missing.
/// - Does not remove user-added custom models under other providers.
/// - Sets provider `default_model` to [`OPENAI_CODEX_DEFAULT_MODEL`] when unset
///   or when it pointed at a removed/unknown id.
pub fn seed_openai_codex_models(path: &Path) -> Result<CodexSeedReport, String> {
    let mut cfg = if path.exists() {
        crate::models_file::try_load_models_file(path)?
    } else {
        ModelsConfig::with_defaults()
    };

    let mut added = 0usize;
    let mut updated = 0usize;

    for entry in openai_codex_model_entries() {
        let existed = cfg.find_model(PROVIDER_OPENAI_CODEX, &entry.id).is_some();
        cfg.upsert_model(entry);
        if existed {
            updated += 1;
        } else {
            added += 1;
        }
    }

    // Provider-level defaults for wire + base. Prefer gpt-5.4 as default after login.
    cfg.ensure_provider(PROVIDER_OPENAI_CODEX);
    if let Some(p) = cfg.provider_mut(PROVIDER_OPENAI_CODEX) {
        p.base_url = Some(CODEX_BASE_URL.into());
        p.api = Some(ProviderApi::OpenaiResponses);
        p.provider_type = Some("openai-codex-responses".into());
        p.default_model = Some(OPENAI_CODEX_DEFAULT_MODEL.into());
    } else {
        cfg.upsert_provider(ProviderConfig {
            id: PROVIDER_OPENAI_CODEX.into(),
            provider_type: Some("openai-codex-responses".into()),
            base_url: Some(CODEX_BASE_URL.into()),
            api: Some(ProviderApi::OpenaiResponses),
            api_key: None,
            api_key_raw: None,
            default_model: Some(OPENAI_CODEX_DEFAULT_MODEL.into()),
            compat: None,
        });
    }

    // Drop obsolete stub ids we used to ship (not in Pi catalog).
    for stale in ["gpt-5.3-codex"] {
        let _ = cfg.remove_model(PROVIDER_OPENAI_CODEX, stale);
    }

    save_models_file(path, &cfg)?;

    let total = cfg.registry.list_by_provider(PROVIDER_OPENAI_CODEX).len();
    let default_model = cfg
        .provider(PROVIDER_OPENAI_CODEX)
        .and_then(|p| p.default_model.clone())
        .unwrap_or_else(|| OPENAI_CODEX_DEFAULT_MODEL.into());

    Ok(CodexSeedReport {
        path: path.to_path_buf(),
        added,
        updated,
        total,
        default_model,
    })
}

/// Seed into the default `~/.one/agent/models.json`.
pub fn seed_openai_codex_models_default() -> Result<CodexSeedReport, String> {
    let path = default_models_json_path();
    seed_openai_codex_models(&path)
}

fn default_models_json_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".one/agent/models.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn catalog_has_pi_ids() {
        let ids: Vec<_> = OPENAI_CODEX_BUILTIN_MODELS.iter().map(|m| m.id).collect();
        assert!(ids.contains(&"gpt-5.4"));
        assert!(ids.contains(&"gpt-5.6-luna"));
        assert_eq!(ids.len(), 7);
    }

    #[test]
    fn seed_writes_all_models() {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("one-codex-seed-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        // No built-in merge — only what we seed.
        std::fs::write(
            &path,
            "{\n  \"includeDefaults\": false,\n  \"providers\": {}\n}\n",
        )
        .unwrap();

        let report = seed_openai_codex_models(&path).unwrap();
        assert_eq!(report.total, 7);
        assert_eq!(report.added, 7);
        assert_eq!(report.default_model, "gpt-5.4");

        let cfg = load_models_file(&path);
        assert!(cfg.find_model(PROVIDER_OPENAI_CODEX, "gpt-5.5").is_some());
        assert!(cfg
            .find_model(PROVIDER_OPENAI_CODEX, "gpt-5.6-sol")
            .is_some());
        assert!(cfg
            .find_model(PROVIDER_OPENAI_CODEX, "gpt-5.3-codex")
            .is_none());

        // Second seed: all updates, no new.
        let report2 = seed_openai_codex_models(&path).unwrap();
        assert_eq!(report2.added, 0);
        assert_eq!(report2.updated, 7);

        let _ = std::fs::remove_dir_all(dir);
    }
}
