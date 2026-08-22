//! Enterprise governance and requirements.toml enforcement.
//!
//! Checks system and user policy locks (such as disabling always-approve / YOLO mode).

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RequirementsFile {
    pub ui: Option<RequirementsUi>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RequirementsUi {
    pub disable_bypass_permissions_mode: Option<bool>,
    /// Legacy alias in requirements.toml: `yolo = false`
    pub yolo: Option<bool>,
}

/// Returns Ok if always-approve (bypassPermissions / YOLO) is allowed by policy,
/// or Err with an explanation if disabled.
pub fn check_bypass_permissions_allowed() -> Result<(), String> {
    if is_bypass_permissions_disabled() {
        Err("Always-approve mode is disabled by policy (requirements.toml)".to_string())
    } else {
        Ok(())
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Checks whether always-approve is disabled by any system or user `requirements.toml`.
pub fn is_bypass_permissions_disabled() -> bool {
    let mut candidate_paths = vec![
        PathBuf::from("/etc/grok/requirements.toml"),
        PathBuf::from("/etc/one/requirements.toml"),
    ];

    if let Some(home) = dirs_home() {
        candidate_paths.push(home.join(".grok/requirements.toml"));
        candidate_paths.push(home.join(".one/requirements.toml"));
    }

    for path in candidate_paths {
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(req) = toml::from_str::<RequirementsFile>(&content) {
                    if let Some(ui) = req.ui {
                        if ui.disable_bypass_permissions_mode == Some(true) {
                            return true;
                        }
                        if ui.yolo == Some(false) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_requirements_file() {
        let toml_str = r#"
        [ui]
        disable_bypass_permissions_mode = true
        "#;
        let parsed: RequirementsFile = toml::from_str(toml_str).unwrap();
        assert_eq!(
            parsed.ui.unwrap().disable_bypass_permissions_mode,
            Some(true)
        );

        let legacy_str = r#"
        [ui]
        yolo = false
        "#;
        let parsed_legacy: RequirementsFile = toml::from_str(legacy_str).unwrap();
        assert_eq!(parsed_legacy.ui.unwrap().yolo, Some(false));
    }
}
