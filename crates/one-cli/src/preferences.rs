use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreferences {
    pub provider: String,
    pub model: String,
}

fn preferences_path() -> PathBuf {
    one_session::agent_dir().join("preferences.json")
}

pub fn load() -> Option<ModelPreferences> {
    let path = preferences_path();
    let data = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save(provider: &str, model: &str) -> std::io::Result<()> {
    let path = preferences_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let prefs = ModelPreferences {
        provider: provider.to_string(),
        model: model.to_string(),
    };
    let data = serde_json::to_string_pretty(&prefs).map_err(|err| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, err)
    })?;
    fs::write(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_json() {
        let prefs = ModelPreferences {
            provider: "openai".into(),
            model: "gpt-4o".into(),
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let back: ModelPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(prefs, back);
    }
}