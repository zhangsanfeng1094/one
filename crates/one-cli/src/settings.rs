//! Unified settings at `~/.one/agent/settings.json`.
//!
//! Migrates from legacy `preferences.json` (provider + model only).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::preferences;

/// Per-skill enable/disable (Codex `[[skills.config]]` equivalent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillConfigEntry {
    /// Absolute path to `SKILL.md`.
    pub path: String,
    /// When false, skill is hidden from catalog and not force-loadable.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// OpenCode-style tool output caps (settings key `tool_output`).
///
/// Defaults when omitted: 2000 lines / 50 KiB. Over either limit → full spill
/// under `~/.one/agent/tool-outputs/` + preview + path for the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputSettings {
    /// Max lines kept inline before spill (default 2000).
    pub max_lines: Option<usize>,
    /// Max UTF-8 bytes kept inline before spill (default 51200).
    pub max_bytes: Option<usize>,
}

/// User settings — single source for durable interactive preferences.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// off | low | medium | high
    pub thinking: Option<String>,
    /// Skip bash danger prompts.
    pub auto_approve: Option<bool>,
    /// Optional context window override for footer %.
    pub context_window: Option<usize>,
    /// Path sandbox: `workspace-write` (default) | `full-access`.
    pub sandbox: Option<String>,
    /// Extra directories the agent may read/write (same as `--add-dir`).
    pub additional_directories: Option<Vec<String>>,
    /// Fine-grained tool permission rules (Claude-style allow/deny/ask).
    pub permissions: Option<one_tools::PermissionRules>,
    /// Run bash under bubblewrap when sandbox is workspace-write (default true).
    pub bash_sandbox: Option<bool>,
    /// Skills enable/disable list (like Codex `[[skills.config]]`).
    /// Omitted paths default to enabled.
    pub skills_config: Option<Vec<SkillConfigEntry>>,
    /// Runtime feature flags (id → enabled). Omitted ids use registry defaults.
    /// See `runtime/features.rs` (e.g. `subagent` → task tools + prompt section).
    pub features: Option<HashMap<String, bool>>,
    /// Unified tool-output truncation (OpenCode `tool_output`).
    pub tool_output: Option<ToolOutputSettings>,
}

impl Settings {
    /// Apply `tool_output` (+ env overrides) to the process-wide truncate limits.
    pub fn apply_tool_output_limits(&self) {
        let (lines, bytes) = self
            .tool_output
            .as_ref()
            .map(|t| (t.max_lines, t.max_bytes))
            .unwrap_or((None, None));
        let lim = one_tools::ToolOutputLimits::from_env_and_overrides(lines, bytes);
        one_tools::set_tool_output_limits(lim);
    }

    /// Effective feature value (registry default when omitted).
    pub fn feature_enabled(&self, id: &str, default: bool) -> bool {
        self.features
            .as_ref()
            .and_then(|m| m.get(id))
            .copied()
            .unwrap_or(default)
    }

    /// Persist a feature flag (creates the map if needed).
    pub fn set_feature(&mut self, id: &str, enabled: bool) {
        let map = self.features.get_or_insert_with(HashMap::new);
        map.insert(id.to_string(), enabled);
    }

    pub fn skills_config_entries(&self) -> Vec<one_resources::SkillConfigEntry> {
        self.skills_config
            .as_ref()
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| one_resources::SkillConfigEntry {
                        path: e.path.clone(),
                        enabled: e.enabled,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn set_skill_enabled(&mut self, path: &std::path::Path, enabled: bool) {
        let mut entries = self.skills_config.clone().unwrap_or_default();
        let mut rs: Vec<one_resources::SkillConfigEntry> = entries
            .iter()
            .map(|e| one_resources::SkillConfigEntry {
                path: e.path.clone(),
                enabled: e.enabled,
            })
            .collect();
        one_resources::set_skill_enabled(&mut rs, path, enabled);
        entries = rs
            .into_iter()
            .map(|e| SkillConfigEntry {
                path: e.path,
                enabled: e.enabled,
            })
            .collect();
        // Drop entries that are enabled (default) to keep the file tidy —
        // only persist explicit disables (and re-enables that were previously disabled).
        // Keep both true and false so user intent is explicit like Codex.
        self.skills_config = if entries.is_empty() {
            None
        } else {
            Some(entries)
        };
    }
}

fn settings_path() -> PathBuf {
    one_session::agent_dir().join("settings.json")
}

pub fn load() -> Settings {
    let path = settings_path();
    if let Ok(data) = fs::read_to_string(&path) {
        if let Ok(s) = serde_json::from_str::<Settings>(&data) {
            return s;
        }
    }
    // Migrate legacy preferences.json once.
    if let Some(prefs) = preferences::load() {
        let s = Settings {
            provider: Some(prefs.provider),
            model: Some(prefs.model),
            ..Default::default()
        };
        let _ = save(&s);
        return s;
    }
    Settings::default()
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(settings).map_err(|err| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, err)
    })?;
    fs::write(path, data)
}

pub fn path_display() -> String {
    settings_path().display().to_string()
}

/// Apply a single key/value (used by `/settings key value`).
pub fn set_key(settings: &mut Settings, key: &str, value: &str) -> Result<(), String> {
    match key.trim().to_ascii_lowercase().as_str() {
        "provider" => {
            settings.provider = Some(value.trim().to_string());
        }
        "model" => {
            settings.model = Some(value.trim().to_string());
        }
        "thinking" => {
            let v = value.trim().to_ascii_lowercase();
            if !matches!(v.as_str(), "off" | "low" | "medium" | "high") {
                return Err("thinking must be off|low|medium|high".into());
            }
            settings.thinking = Some(v);
        }
        "auto_approve" | "auto-approve" | "yes" => {
            let v = value.trim().to_ascii_lowercase();
            settings.auto_approve = Some(matches!(v.as_str(), "1" | "true" | "yes" | "on"));
        }
        "context_window" | "context-window" | "context" => {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| "context_window must be a number".to_string())?;
            settings.context_window = if n == 0 { None } else { Some(n) };
        }
        "sandbox" => {
            let v = value.trim().to_ascii_lowercase();
            if one_tools::SandboxMode::parse(&v).is_none() {
                return Err(
                    "sandbox must be workspace-write|full-access (aliases: workspace, full)".into(),
                );
            }
            // Normalize to canonical form.
            let mode = one_tools::SandboxMode::parse(&v).expect("checked above");
            settings.sandbox = Some(mode.as_str().to_string());
        }
        "add_dir" | "add-dir" | "additional_directories" => {
            let dirs: Vec<String> = value
                .split([',', ':'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if dirs.is_empty() {
                settings.additional_directories = None;
            } else {
                settings.additional_directories = Some(dirs);
            }
        }
        "bash_sandbox" | "bash-sandbox" => {
            let v = value.trim().to_ascii_lowercase();
            settings.bash_sandbox = Some(matches!(v.as_str(), "1" | "true" | "yes" | "on"));
        }
        "tool_output_max_lines" | "tool-output-max-lines" | "tool_output.max_lines" => {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| "tool_output.max_lines must be a positive number".to_string())?;
            if n < 1 {
                return Err("tool_output.max_lines must be >= 1".into());
            }
            let mut t = settings.tool_output.clone().unwrap_or_default();
            t.max_lines = Some(n);
            settings.tool_output = Some(t);
            settings.apply_tool_output_limits();
        }
        "tool_output_max_bytes" | "tool-output-max-bytes" | "tool_output.max_bytes" => {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| "tool_output.max_bytes must be a positive number".to_string())?;
            if n < 1 {
                return Err("tool_output.max_bytes must be >= 1".into());
            }
            let mut t = settings.tool_output.clone().unwrap_or_default();
            t.max_bytes = Some(n);
            settings.tool_output = Some(t);
            settings.apply_tool_output_limits();
        }
        // Feature flags: /settings feature.subagent off  or  /settings features.subagent on
        key if key.starts_with("feature.") || key.starts_with("features.") => {
            let id = key
                .split_once('.')
                .map(|(_, rest)| rest.trim())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                return Err("feature id required (e.g. feature.subagent)".into());
            }
            // Validate against known registry when available (avoid circular import:
            // accept any non-empty id here; runtime validates known set).
            let current = settings.feature_enabled(&id, true);
            let on = match value.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" | "enable" | "enabled" => true,
                "0" | "false" | "no" | "off" | "disable" | "disabled" => false,
                "toggle" => !current,
                other => {
                    return Err(format!(
                        "feature value must be on|off|toggle (got `{other}`)"
                    ));
                }
            };
            settings.set_feature(&id, on);
        }
        // Append a single rule: /settings allow Bash(cargo *)
        action @ ("allow" | "deny" | "ask") => {
            let rule = value.trim();
            if rule.is_empty() {
                return Err(format!("{action} requires a rule like Bash(git push *)"));
            }
            let rule_action = match action {
                "allow" => one_tools::RuleAction::Allow,
                "deny" => one_tools::RuleAction::Deny,
                _ => one_tools::RuleAction::Ask,
            };
            if one_tools::PermissionRule::parse(rule_action, rule).is_none() {
                return Err(format!("invalid permission rule: {rule}"));
            }
            let mut perms = settings.permissions.clone().unwrap_or_default();
            match action {
                "allow" => perms.allow.push(rule.to_string()),
                "deny" => perms.deny.push(rule.to_string()),
                _ => perms.ask.push(rule.to_string()),
            }
            settings.permissions = Some(perms);
        }
        other => {
            return Err(format!(
                "unknown setting `{other}` · known: provider model thinking auto_approve \
                 context_window sandbox add_dir bash_sandbox tool_output.max_lines \
                 tool_output.max_bytes feature.<id> allow deny ask"
            ));
        }
    }
    Ok(())
}

pub fn rows(settings: &Settings) -> Vec<(String, String)> {
    vec![
        (
            "provider".into(),
            settings
                .provider
                .clone()
                .unwrap_or_else(|| "(unset)".into()),
        ),
        (
            "model".into(),
            settings.model.clone().unwrap_or_else(|| "(unset)".into()),
        ),
        (
            "thinking".into(),
            settings
                .thinking
                .clone()
                .unwrap_or_else(|| "off".into()),
        ),
        (
            "auto_approve".into(),
            settings
                .auto_approve
                .map(|b| if b { "true" } else { "false" })
                .unwrap_or_else(|| "false".into())
                .into(),
        ),
        (
            "context_window".into(),
            settings
                .context_window
                .map(|n| n.to_string())
                .unwrap_or_else(|| "(auto)".into()),
        ),
        (
            "sandbox".into(),
            settings
                .sandbox
                .clone()
                .unwrap_or_else(|| "workspace-write".into()),
        ),
        (
            "add_dir".into(),
            settings
                .additional_directories
                .as_ref()
                .map(|d| d.join(", "))
                .unwrap_or_else(|| "(none)".into()),
        ),
        (
            "bash_sandbox".into(),
            settings
                .bash_sandbox
                .map(|b| if b { "true" } else { "false" })
                .unwrap_or("true")
                .into(),
        ),
        (
            "permissions".into(),
            settings
                .permissions
                .as_ref()
                .map(|p| {
                    format!(
                        "allow={} deny={} ask={}",
                        p.allow.len(),
                        p.deny.len(),
                        p.ask.len()
                    )
                })
                .unwrap_or_else(|| "(none)".into()),
        ),
        (
            "features".into(),
            settings
                .features
                .as_ref()
                .map(|m| {
                    let mut parts: Vec<String> = m
                        .iter()
                        .map(|(k, v)| format!("{k}={}", if *v { "on" } else { "off" }))
                        .collect();
                    parts.sort();
                    if parts.is_empty() {
                        "(defaults)".into()
                    } else {
                        parts.join(" ")
                    }
                })
                .unwrap_or_else(|| "(defaults)".into()),
        ),
        {
            let lim = one_tools::tool_output_limits();
            (
                "tool_output".into(),
                format!(
                    "max_lines={} max_bytes={}",
                    lim.max_lines, lim.max_bytes
                ),
            )
        },
        ("path".into(), path_display()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_key_thinking() {
        let mut s = Settings::default();
        set_key(&mut s, "thinking", "high").unwrap();
        assert_eq!(s.thinking.as_deref(), Some("high"));
        assert!(set_key(&mut s, "thinking", "nope").is_err());
    }

    #[test]
    fn roundtrip_json() {
        let s = Settings {
            provider: Some("deepseek".into()),
            model: Some("deepseek-chat".into()),
            thinking: Some("low".into()),
            auto_approve: Some(true),
            context_window: Some(128_000),
            sandbox: Some("workspace-write".into()),
            additional_directories: Some(vec!["/tmp/extra".into()]),
            permissions: Some(one_tools::PermissionRules {
                allow: vec!["Bash(cargo *)".into()],
                deny: vec!["Bash(git push *)".into()],
                ask: vec![],
            }),
            bash_sandbox: Some(true),
            skills_config: Some(vec![SkillConfigEntry {
                path: "/tmp/s/SKILL.md".into(),
                enabled: false,
            }]),
            features: Some(HashMap::from([("subagent".into(), false)])),
            tool_output: Some(ToolOutputSettings {
                max_lines: Some(5000),
                max_bytes: Some(204_800),
            }),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn tool_output_set_key() {
        let mut s = Settings::default();
        set_key(&mut s, "tool_output.max_lines", "100").unwrap();
        assert_eq!(s.tool_output.as_ref().unwrap().max_lines, Some(100));
        set_key(&mut s, "tool_output.max_bytes", "4096").unwrap();
        assert_eq!(s.tool_output.as_ref().unwrap().max_bytes, Some(4096));
        assert_eq!(one_tools::tool_output_limits().max_lines, 100);
        assert_eq!(one_tools::tool_output_limits().max_bytes, 4096);
        // Restore defaults for other tests in the same process.
        one_tools::set_tool_output_limits(one_tools::ToolOutputLimits::default());
    }

    #[test]
    fn set_key_feature() {
        let mut s = Settings::default();
        set_key(&mut s, "feature.subagent", "off").unwrap();
        assert_eq!(s.feature_enabled("subagent", true), false);
        set_key(&mut s, "features.subagent", "toggle").unwrap();
        assert_eq!(s.feature_enabled("subagent", true), true);
    }

    #[test]
    fn skill_toggle_persists_path() {
        let mut s = Settings::default();
        s.set_skill_enabled(std::path::Path::new("/tmp/x/SKILL.md"), false);
        assert_eq!(s.skills_config.as_ref().unwrap().len(), 1);
        assert!(!s.skills_config.as_ref().unwrap()[0].enabled);
        s.set_skill_enabled(std::path::Path::new("/tmp/x/SKILL.md"), true);
        assert!(s.skills_config.as_ref().unwrap()[0].enabled);
    }
}
