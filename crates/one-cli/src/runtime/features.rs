//! Runtime feature flags (settings-driven capability bundles).
//!
//! Features gate tools + system-prompt sections together. Flags that change
//! model context (`affects_context`) apply on cold start or `/new`, not mid-chat.

use std::collections::BTreeMap;

use one_resources::MemoryLoadOptions;

use crate::settings::Settings;

/// Feature id for the subagent / `task` tool package.
pub const FEATURE_SUBAGENT: &str = "subagent";
/// Feature id for provider-native Web/X search.
pub const FEATURE_SERVER_SEARCH: &str = "server_search";
/// Feature id for the whole cross-session memory package (L2 + tools + write path).
pub const FEATURE_MEMORY: &str = "memory";
/// Legacy feature id accepted when reading settings (maps to [`FEATURE_MEMORY`]).
pub const FEATURE_MEMORY_LEGACY: &str = "memory_write";

/// Static definition of a product feature.
#[derive(Debug, Clone, Copy)]
pub struct FeatureDef {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub default_enabled: bool,
    /// When true, toggling requires a new conversation to apply.
    pub affects_context: bool,
    /// Tool names registered only when this feature is on (documentation + filters).
    pub tool_names: &'static [&'static str],
}

pub const FEATURE_REGISTRY: &[FeatureDef] = &[
    FeatureDef {
        id: FEATURE_SUBAGENT,
        label: "Subagent (task)",
        description: "task / job_output / wait_tasks / job_kill + prompt policy",
        default_enabled: true,
        affects_context: true,
        tool_names: &["task", "job_output", "wait_tasks", "job_kill"],
    },
    FeatureDef {
        id: FEATURE_SERVER_SEARCH,
        label: "Server search",
        // Request-side only: whether we declare hosted `{type:web_search}` (+ x_search).
        // Response parsing (web_search_call / citations) is always on — proxies may inject.
        description: "Inject hosted web_search on main request when model supports it (else local Brave/DDG). Does not gate response handling",
        // Match pi-xai agentic default-on for Responses-capable models.
        default_enabled: true,
        // Hosted inject changes the tools array the model sees (local function dropped).
        affects_context: true,
        // Local function `web_search` is independent (present when not injecting).
        tool_names: &[],
    },
    FeatureDef {
        id: FEATURE_MEMORY,
        label: "Memory",
        description: "Cross-session memory package: L2 catalog, memory_search, memory_write, path roots, compact→L4",
        default_enabled: true,
        // Prompt section + tools + path policy are model-visible.
        affects_context: true,
        tool_names: &["memory_search", "memory_write"],
    },
];

/// Effective feature enable map (defaults filled in for known ids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureState {
    /// id → enabled (only known registry ids).
    enabled: BTreeMap<String, bool>,
}

impl Default for FeatureState {
    fn default() -> Self {
        Self::from_settings(&Settings::default())
    }
}

impl FeatureState {
    /// Resolve effective flags from settings (omit → registry default).
    pub fn from_settings(settings: &Settings) -> Self {
        let mut enabled = BTreeMap::new();
        for def in FEATURE_REGISTRY {
            let on = settings
                .features
                .as_ref()
                .and_then(|m| {
                    m.get(def.id).copied().or_else(|| {
                        // Accept legacy `memory_write` feature key as `memory`.
                        if def.id == FEATURE_MEMORY {
                            m.get(FEATURE_MEMORY_LEGACY).copied()
                        } else {
                            None
                        }
                    })
                })
                .unwrap_or(def.default_enabled);
            enabled.insert(def.id.to_string(), on);
        }
        Self { enabled }
    }

    /// Apply process-level kill-switches after settings (CLI / env).
    pub fn with_process_overrides(mut self, no_subagent: bool, no_memory: bool) -> Self {
        if no_subagent {
            self.enabled.insert(FEATURE_SUBAGENT.to_string(), false);
        }
        if no_memory {
            self.enabled.insert(FEATURE_MEMORY.to_string(), false);
        }
        self
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        // Legacy alias for UI / /settings that still say memory_write.
        let id = if id == FEATURE_MEMORY_LEGACY {
            FEATURE_MEMORY
        } else {
            id
        };
        self.enabled.get(id).copied().unwrap_or_else(|| {
            FEATURE_REGISTRY
                .iter()
                .find(|d| d.id == id)
                .map(|d| d.default_enabled)
                .unwrap_or(false)
        })
    }

    pub fn set(&mut self, id: &str, on: bool) {
        let id = if id == FEATURE_MEMORY_LEGACY {
            FEATURE_MEMORY
        } else {
            id
        };
        if FEATURE_REGISTRY.iter().any(|d| d.id == id) {
            self.enabled.insert(id.to_string(), on);
        }
    }

    /// Stable fingerprint for pending vs applied comparison.
    pub fn fingerprint(&self) -> String {
        self.enabled
            .iter()
            .map(|(k, v)| format!("{k}={}", if *v { "1" } else { "0" }))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Rows for TUI / status: (id, label, detail, enabled, affects_context).
    pub fn rows(&self) -> Vec<(String, String, String, bool, bool)> {
        FEATURE_REGISTRY
            .iter()
            .map(|d| {
                let on = self.is_enabled(d.id);
                (
                    d.id.to_string(),
                    d.label.to_string(),
                    d.description.to_string(),
                    on,
                    d.affects_context,
                )
            })
            .collect()
    }

    pub fn subagent_enabled(&self) -> bool {
        self.is_enabled(FEATURE_SUBAGENT)
    }

    pub fn server_search_enabled(&self) -> bool {
        self.is_enabled(FEATURE_SERVER_SEARCH)
    }

    /// Whole memory package (L2 + tools + write roots + archive).
    pub fn memory_enabled(&self) -> bool {
        self.is_enabled(FEATURE_MEMORY)
    }
}

/// Look up a registry definition.
pub fn feature_def(id: &str) -> Option<&'static FeatureDef> {
    let id = if id == FEATURE_MEMORY_LEGACY {
        FEATURE_MEMORY
    } else {
        id
    };
    FEATURE_REGISTRY.iter().find(|d| d.id == id)
}

/// Whether toggling this feature changes model context (prompt/tools).
pub fn feature_affects_context(id: &str) -> bool {
    feature_def(id).map(|d| d.affects_context).unwrap_or(true)
}

/// Parse on/off/toggle tokens for `/settings feature …`.
pub fn parse_bool_token(value: &str, current: bool) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enable" | "enabled" => Ok(true),
        "0" | "false" | "no" | "off" | "disable" | "disabled" => Ok(false),
        "toggle" => Ok(!current),
        other => Err(format!(
            "feature value must be on|off|toggle (got `{other}`)"
        )),
    }
}

/// Env kill-switch: `ONE_DISABLE_SUBAGENT=1` (same spirit as skills/mcp).
pub fn env_no_subagent() -> bool {
    std::env::var_os("ONE_DISABLE_SUBAGENT").is_some_and(|v| v != "0" && v != "false")
}

/// Env kill-switch for the whole memory package (`ONE_NO_MEMORY=1` / `ONE_MEMORY=0`).
pub fn env_no_memory() -> bool {
    if std::env::var_os("ONE_NO_MEMORY").is_some_and(|v| v != "0" && v != "false") {
        return true;
    }
    matches!(
        std::env::var("ONE_MEMORY").ok().as_deref(),
        Some("0") | Some("false") | Some("off") | Some("no")
    )
}

/// Effective memory load options: **feature `memory` is the package master switch**.
///
/// When the feature is off (or env/CLI force-off), `enabled` and `write_enabled`
/// are both false — no L2, no tools, no memory path grants, no compact archive.
pub fn effective_memory_options(
    features: &FeatureState,
    settings: &Settings,
) -> MemoryLoadOptions {
    let mut opts = settings.memory_load_options();
    if !features.memory_enabled() {
        opts.enabled = false;
        opts.write_enabled = false;
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn default_subagent_and_server_search_on() {
        let s = FeatureState::default();
        assert!(s.subagent_enabled());
        assert!(s.server_search_enabled());
        assert!(s.memory_enabled());
        assert_eq!(s.fingerprint(), "memory=1,server_search=1,subagent=1");
    }

    #[test]
    fn settings_override_off() {
        let mut settings = Settings::default();
        let mut m = HashMap::new();
        m.insert("subagent".into(), false);
        m.insert("server_search".into(), false);
        m.insert("memory".into(), false);
        settings.features = Some(m);
        let s = FeatureState::from_settings(&settings);
        assert!(!s.subagent_enabled());
        assert!(!s.server_search_enabled());
        assert!(!s.memory_enabled());
        assert_eq!(s.fingerprint(), "memory=0,server_search=0,subagent=0");
    }

    #[test]
    fn legacy_memory_write_feature_key_maps_to_memory() {
        let mut settings = Settings::default();
        let mut m = HashMap::new();
        m.insert("memory_write".into(), false);
        settings.features = Some(m);
        let s = FeatureState::from_settings(&settings);
        assert!(!s.memory_enabled());
        assert!(!s.is_enabled("memory_write"));
    }

    #[test]
    fn memory_feature_registered() {
        let def = feature_def(FEATURE_MEMORY).expect("memory registered");
        assert!(def.default_enabled);
        assert!(def.affects_context);
        assert_eq!(def.tool_names, &["memory_search", "memory_write"]);
    }

    #[test]
    fn process_override_forces_off() {
        let s = FeatureState::default().with_process_overrides(true, true);
        assert!(!s.subagent_enabled());
        assert!(!s.memory_enabled());
    }

    #[test]
    fn effective_memory_off_when_feature_off() {
        let mut settings = Settings::default();
        settings.memory = Some(crate::settings::MemorySettings {
            enabled: Some(true),
            write: Some(true),
            ..Default::default()
        });
        let mut m = HashMap::new();
        m.insert(FEATURE_MEMORY.into(), false);
        settings.features = Some(m);
        let features = FeatureState::from_settings(&settings);
        let opts = effective_memory_options(&features, &settings);
        assert!(!opts.enabled);
        assert!(!opts.write_enabled);
    }

    #[test]
    fn parse_bool_toggle() {
        assert_eq!(parse_bool_token("toggle", true).unwrap(), false);
        assert_eq!(parse_bool_token("on", false).unwrap(), true);
        assert!(parse_bool_token("maybe", true).is_err());
    }

    #[test]
    fn server_search_is_inject_only_default_on() {
        let def = feature_def(FEATURE_SERVER_SEARCH).expect("server_search registered");
        assert!(def.default_enabled);
        assert!(def.affects_context);
        // Does not gate the local function tool — only request inject of hosted tools.
        assert!(def.tool_names.is_empty());
        let desc = def.description.to_ascii_lowercase();
        assert!(
            desc.contains("inject") && desc.contains("response"),
            "description should say inject + response ungated: {}",
            def.description
        );

        let mut settings = Settings::default();
        settings.set_feature(FEATURE_SERVER_SEARCH, false);
        let state = FeatureState::from_settings(&settings);
        assert!(!state.server_search_enabled());
    }
}
