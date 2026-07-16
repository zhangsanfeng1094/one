//! Unified settings at `~/.one/agent/settings.json`.
//!
//! Migrates from legacy `preferences.json` (provider + model only).

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
}

impl Settings {
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
                 context_window sandbox add_dir bash_sandbox allow deny ask"
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
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
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
