use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_sessions: usize,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub feishu: FeishuConfig,
    #[serde(default)]
    pub slack: SlackConfig,
    #[serde(default)]
    pub connectors: Vec<ConnectorConfig>,
    #[serde(default)]
    pub connectors_dir: Option<PathBuf>,
}

fn default_model() -> String {
    "grok-beta".to_string()
}

fn default_max_concurrent() -> usize {
    50
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            default_model: default_model(),
            max_concurrent_sessions: default_max_concurrent(),
            security: SecurityConfig::default(),
            telegram: TelegramConfig::default(),
            discord: DiscordConfig::default(),
            feishu: FeishuConfig::default(),
            slack: SlackConfig::default(),
            connectors: Vec::new(),
            connectors_dir: None,
        }
    }
}

impl BotConfig {
    /// Load config from file (TOML or JSON).
    pub fn load_from_path(
        path: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)?;
        Self::load_from_str(
            &content,
            path.extension().and_then(|e| e.to_str()).unwrap_or(""),
        )
    }

    /// Load config from string content given format hint.
    pub fn load_from_str(
        content: &str,
        ext_hint: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if ext_hint.eq_ignore_ascii_case("json") || content.trim_start().starts_with('{') {
            let cfg: BotConfig = serde_json::from_str(content)?;
            Ok(cfg)
        } else {
            let cfg: BotConfig = toml::from_str(content)?;
            Ok(cfg)
        }
    }

    /// Load config from default locations (`bot.toml`, `~/.one/bot.toml`) merged with env vars.
    pub fn load_default() -> Self {
        let mut cfg = Self::from_env();

        // 1. Check local bot.toml / bot.json
        for candidate in &["bot.toml", "one-bot.toml", "bot.json", ".one/bot.toml"] {
            let p = Path::new(candidate);
            if p.exists() {
                if let Ok(loaded) = Self::load_from_path(p) {
                    cfg.merge(loaded);
                    return cfg;
                }
            }
        }

        // 2. Check ~/.one/bot.toml
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let global_toml = home.join(".one").join("bot.toml");
            if global_toml.exists() {
                if let Ok(loaded) = Self::load_from_path(&global_toml) {
                    cfg.merge(loaded);
                    return cfg;
                }
            }
        }

        cfg
    }

    /// Merge loaded config with self, giving priority to non-default values in `other`.
    pub fn merge(&mut self, other: Self) {
        if other.default_model != default_model() {
            self.default_model = other.default_model;
        }
        if other.max_concurrent_sessions != default_max_concurrent() {
            self.max_concurrent_sessions = other.max_concurrent_sessions;
        }
        if other.telegram.enabled {
            self.telegram = other.telegram;
        }
        if other.discord.enabled {
            self.discord = other.discord;
        }
        if other.feishu.enabled {
            self.feishu = other.feishu;
        }
        if other.slack.enabled {
            self.slack = other.slack;
        }
        if !other.connectors.is_empty() {
            self.connectors.extend(other.connectors);
        }
        if other.connectors_dir.is_some() {
            self.connectors_dir = other.connectors_dir;
        }
        if !other.security.admin_users.is_empty() {
            self.security.admin_users = other.security.admin_users;
        }
        if !other.security.allowed_channels.is_empty() {
            self.security.allowed_channels = other.security.allowed_channels;
        }
    }

    /// Load config from environment variables (e.g. `TELEGRAM_BOT_TOKEN`, `ONE_BOT_ADMINS`, etc.).
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(token) = std::env::var("TELEGRAM_BOT_TOKEN") {
            if !token.is_empty() {
                config.telegram.enabled = true;
                config.telegram.bot_token = token;
            }
        }
        if let Ok(token) = std::env::var("DISCORD_BOT_TOKEN") {
            if !token.is_empty() {
                config.discord.enabled = true;
                config.discord.bot_token = token;
            }
        }
        if let Ok(app_id) = std::env::var("FEISHU_APP_ID") {
            let app_secret = std::env::var("FEISHU_APP_SECRET").unwrap_or_default();
            if !app_id.is_empty() && !app_secret.is_empty() {
                config.feishu.enabled = true;
                config.feishu.app_id = app_id;
                config.feishu.app_secret = app_secret;
            }
        }
        if let Ok(admins) = std::env::var("ONE_BOT_ADMINS") {
            config.security.admin_users = admins
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(channels) = std::env::var("ONE_BOT_CHANNELS") {
            config.security.allowed_channels = channels
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        config
    }
}

/// External connector subprocess configuration (Hermes-aligned).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorConfig {
    #[serde(default)]
    pub name: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    #[serde(default)]
    pub admin_users: Vec<String>,
    #[serde(default)]
    pub allowed_channels: Vec<String>,
    #[serde(default = "default_true")]
    pub require_approval_for_bash: bool,
    #[serde(default = "default_workspace_root")]
    pub workspace_root: PathBuf,
}

fn default_true() -> bool {
    true
}

fn default_workspace_root() -> PathBuf {
    PathBuf::from(".")
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            admin_users: Vec::new(),
            allowed_channels: Vec::new(),
            require_approval_for_bash: true,
            workspace_root: default_workspace_root(),
        }
    }
}

impl SecurityConfig {
    pub fn is_user_admin(&self, user_id: &str) -> bool {
        if self.admin_users.is_empty() {
            return true; // No admins configured: open mode
        }
        self.admin_users.iter().any(|u| u == user_id)
    }

    pub fn is_channel_allowed(&self, channel_id: &str) -> bool {
        if self.allowed_channels.is_empty() {
            return true; // No channel restriction
        }
        self.allowed_channels.iter().any(|c| c == channel_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bot_token: String,
    #[serde(default = "default_tg_api_base")]
    pub api_base: String,
    #[serde(default = "default_stream_edit_interval")]
    pub stream_edit_interval_ms: u64,
}

fn default_tg_api_base() -> String {
    "https://api.telegram.org".to_string()
}

fn default_stream_edit_interval() -> u64 {
    1000
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            api_base: default_tg_api_base(),
            stream_edit_interval_ms: default_stream_edit_interval(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscordConfig {
    pub enabled: bool,
    pub bot_token: String,
    pub application_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeishuConfig {
    pub enabled: bool,
    pub app_id: String,
    pub app_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlackConfig {
    pub enabled: bool,
    pub bot_token: String,
    pub app_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bot_toml() {
        let toml_str = r#"
default_model = "grok-4"
max_concurrent_sessions = 20

[security]
admin_users = ["user_1", "user_2"]
allowed_channels = ["chan_dev"]
require_approval_for_bash = true

[telegram]
enabled = true
bot_token = "123456:ABC-DEF"
stream_edit_interval_ms = 800

[feishu]
enabled = true
app_id = "cli_a1b2c3d4"
app_secret = "secret_xyz"

[[connectors]]
name = "wecom_bot"
command = "./connectors/wecom"
args = ["--port", "8080"]
enabled = true
"#;

        let cfg = BotConfig::load_from_str(toml_str, "toml").unwrap();
        assert_eq!(cfg.default_model, "grok-4");
        assert_eq!(cfg.max_concurrent_sessions, 20);
        assert!(cfg.telegram.enabled);
        assert_eq!(cfg.telegram.bot_token, "123456:ABC-DEF");
        assert_eq!(cfg.telegram.stream_edit_interval_ms, 800);
        assert!(cfg.feishu.enabled);
        assert_eq!(cfg.feishu.app_id, "cli_a1b2c3d4");
        assert_eq!(cfg.connectors.len(), 1);
        assert_eq!(cfg.connectors[0].name.as_deref(), Some("wecom_bot"));
        assert_eq!(cfg.connectors[0].command, "./connectors/wecom");
        assert_eq!(cfg.connectors[0].args, vec!["--port", "8080"]);
        assert!(cfg.security.is_user_admin("user_1"));
        assert!(!cfg.security.is_user_admin("user_other"));
    }

    #[test]
    fn test_parse_bot_json() {
        let json_str = r#"{
            "default_model": "gpt-4o",
            "telegram": {
                "enabled": true,
                "bot_token": "tg_token_xyz"
            },
            "connectors": [
                {
                    "name": "slack_custom",
                    "command": "python3",
                    "args": ["slack.py"],
                    "enabled": true
                }
            ]
        }"#;

        let cfg = BotConfig::load_from_str(json_str, "json").unwrap();
        assert_eq!(cfg.default_model, "gpt-4o");
        assert!(cfg.telegram.enabled);
        assert_eq!(cfg.connectors.len(), 1);
        assert_eq!(cfg.connectors[0].command, "python3");
    }
}
