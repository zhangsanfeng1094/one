//! Runtime feature flags (settings-driven capability bundles).
//!
//! Features gate tools + system-prompt sections together. Flags that change
//! model context (`affects_context`) apply on cold start or `/new`, not mid-chat.

use std::collections::BTreeMap;

use crate::settings::Settings;

/// Feature id for the subagent / `task` tool package.
pub const FEATURE_SUBAGENT: &str = "subagent";

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

/// Built-in feature registry (V1: subagent only; extend here later).
pub const FEATURE_REGISTRY: &[FeatureDef] = &[FeatureDef {
    id: FEATURE_SUBAGENT,
    label: "Subagent (task)",
    description: "task / job_output / wait_tasks / job_kill + prompt policy",
    default_enabled: true,
    affects_context: true,
    tool_names: &["task", "job_output", "wait_tasks", "job_kill"],
}];

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
                .and_then(|m| m.get(def.id))
                .copied()
                .unwrap_or(def.default_enabled);
            enabled.insert(def.id.to_string(), on);
        }
        Self { enabled }
    }

    /// Apply process-level kill-switches after settings (CLI / env).
    pub fn with_process_overrides(mut self, no_subagent: bool) -> Self {
        if no_subagent {
            self.enabled.insert(FEATURE_SUBAGENT.to_string(), false);
        }
        self
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.enabled.get(id).copied().unwrap_or_else(|| {
            FEATURE_REGISTRY
                .iter()
                .find(|d| d.id == id)
                .map(|d| d.default_enabled)
                .unwrap_or(false)
        })
    }

    pub fn set(&mut self, id: &str, on: bool) {
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
}

/// Look up a registry definition.
pub fn feature_def(id: &str) -> Option<&'static FeatureDef> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn default_subagent_on() {
        let s = FeatureState::default();
        assert!(s.subagent_enabled());
        assert_eq!(s.fingerprint(), "subagent=1");
    }

    #[test]
    fn settings_override_off() {
        let mut settings = Settings::default();
        let mut m = HashMap::new();
        m.insert("subagent".into(), false);
        settings.features = Some(m);
        let s = FeatureState::from_settings(&settings);
        assert!(!s.subagent_enabled());
        assert_eq!(s.fingerprint(), "subagent=0");
    }

    #[test]
    fn process_override_forces_off() {
        let s = FeatureState::default().with_process_overrides(true);
        assert!(!s.subagent_enabled());
    }

    #[test]
    fn parse_bool_toggle() {
        assert_eq!(parse_bool_token("toggle", true).unwrap(), false);
        assert_eq!(parse_bool_token("on", false).unwrap(), true);
        assert!(parse_bool_token("maybe", true).is_err());
    }
}
