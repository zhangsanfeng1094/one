//! Credential types for API keys and OAuth tokens (Pi `auth.json` shape).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Stored API-key credential.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiKeyCredential {
    #[serde(rename = "type")]
    pub kind: ApiKeyKind,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyKind {
    #[default]
    #[serde(rename = "api_key")]
    ApiKey,
}

/// OAuth token bundle (access + refresh + expiry ms since epoch).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OAuthCredential {
    #[serde(rename = "type")]
    pub kind: OAuthKind,
    pub access: String,
    pub refresh: String,
    /// Absolute expiry time in milliseconds since Unix epoch.
    pub expires: u64,
    /// OpenAI Codex: ChatGPT account id extracted from JWT.
    #[serde(
        default,
        rename = "accountId",
        alias = "account_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    /// Extra provider-specific fields preserved on round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OAuthKind {
    #[default]
    #[serde(rename = "oauth")]
    Oauth,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AuthCredential {
    OAuth(OAuthCredential),
    ApiKey(ApiKeyCredential),
}

impl AuthCredential {
    pub fn api_key(key: impl Into<String>) -> Self {
        Self::ApiKey(ApiKeyCredential {
            kind: ApiKeyKind::ApiKey,
            key: key.into(),
            env: None,
        })
    }

    pub fn oauth(access: impl Into<String>, refresh: impl Into<String>, expires: u64) -> Self {
        Self::OAuth(OAuthCredential {
            kind: OAuthKind::Oauth,
            access: access.into(),
            refresh: refresh.into(),
            expires,
            account_id: None,
            extra: BTreeMap::new(),
        })
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }

    pub fn as_oauth(&self) -> Option<&OAuthCredential> {
        match self {
            Self::OAuth(c) => Some(c),
            _ => None,
        }
    }

    pub fn as_api_key(&self) -> Option<&ApiKeyCredential> {
        match self {
            Self::ApiKey(c) => Some(c),
            _ => None,
        }
    }
}

/// Request-ready auth after resolution / refresh.
#[derive(Debug, Clone, Default)]
pub struct ModelAuth {
    pub api_key: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub base_url: Option<String>,
    /// e.g. "oauth", "auth.json", "ANTHROPIC_API_KEY", "--api-key"
    pub source: Option<String>,
}

/// Non-secret status for UI.
#[derive(Debug, Clone, Default)]
pub struct AuthStatus {
    pub configured: bool,
    pub source: Option<&'static str>,
    pub label: Option<String>,
}

/// Events emitted during interactive login.
#[derive(Debug, Clone)]
pub enum AuthEvent {
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        interval_seconds: Option<u64>,
        expires_in_seconds: Option<u64>,
    },
    Progress {
        message: String,
    },
    Info {
        message: String,
    },
}

/// Prompt kinds for login interaction.
#[derive(Debug, Clone)]
pub enum AuthPrompt {
    Text {
        message: String,
        placeholder: Option<String>,
    },
    ManualCode {
        message: String,
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<SelectOption>,
    },
}

#[derive(Debug, Clone)]
pub struct SelectOption {
    pub id: String,
    pub label: String,
}

/// Callbacks the OAuth flow uses to talk to the UI / CLI.
#[async_trait::async_trait]
pub trait AuthInteraction: Send {
    fn notify(&mut self, event: AuthEvent);
    async fn prompt(&mut self, prompt: AuthPrompt) -> Result<String, String>;
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// Built-in OAuth / subscription provider ids.
pub const PROVIDER_OPENAI_CODEX: &str = "openai-codex";
/// OpenCode Zen (pay-as-you-go gateway).
pub const PROVIDER_OPENCODE: &str = "opencode";
/// OpenCode Go (low-cost subscription catalog).
pub const PROVIDER_OPENCODE_GO: &str = "opencode-go";
/// xAI Grok SuperGrok / X Premium+ OAuth.
pub const PROVIDER_XAI: &str = "xai";

/// Human-readable catalog of OAuth / subscription login targets.
#[derive(Debug, Clone, Copy)]
pub struct OAuthProviderInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

pub fn oauth_provider_catalog() -> &'static [OAuthProviderInfo] {
    &[
        OAuthProviderInfo {
            id: PROVIDER_OPENAI_CODEX,
            name: "OpenAI Codex (ChatGPT Plus/Pro)",
            description: "Browser or device-code OAuth → chatgpt.com backend",
        },
        OAuthProviderInfo {
            id: PROVIDER_XAI,
            name: "xAI Grok (SuperGrok / X Premium+)",
            description: "Browser or device-code OAuth → cli-chat-proxy.grok.com",
        },
        OAuthProviderInfo {
            id: PROVIDER_OPENCODE,
            name: "OpenCode Zen",
            description: "Console sign-in + API key → opencode.ai/zen/v1",
        },
        OAuthProviderInfo {
            id: PROVIDER_OPENCODE_GO,
            name: "OpenCode Go (subscription)",
            description: "Console sign-in + API key → opencode.ai/zen/go/v1",
        },
    ]
}
