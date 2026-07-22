//! Provider credential storage and OAuth / subscription login (Pi-compatible `auth.json`).

mod login_opencode;
mod oauth_codex;
mod oauth_xai;
mod pkce;
mod storage;
mod types;

pub use login_opencode::{
    base_url_for as opencode_base_url, login as login_opencode, OPENCODE_CONSOLE_URL,
    OPENCODE_GO_BASE_URL, OPENCODE_ZEN_BASE_URL,
};
pub use oauth_codex::{
    extract_account_id, login as login_openai_codex, login_browser as login_openai_codex_browser,
    login_device_code as login_openai_codex_device, refresh as refresh_openai_codex,
    DEFAULT_BASE_URL as CODEX_BASE_URL,
};
pub use oauth_xai::{
    cli_identity_headers as xai_cli_headers, login as login_xai,
    login_browser as login_xai_browser, login_device_code as login_xai_device,
    refresh as refresh_xai, DEFAULT_BASE_URL as XAI_BASE_URL,
    PUBLIC_API_BASE_URL as XAI_PUBLIC_API_BASE_URL,
};
pub use storage::{AuthError, AuthResult, AuthStorage};
pub use types::{
    oauth_provider_catalog, ApiKeyCredential, AuthCredential, AuthEvent, AuthInteraction,
    AuthPrompt, AuthStatus, ModelAuth, OAuthCredential, OAuthProviderInfo, SelectOption,
    PROVIDER_OPENAI_CODEX, PROVIDER_OPENCODE, PROVIDER_OPENCODE_GO, PROVIDER_XAI,
};

/// Run login for a known provider id and persist to storage.
///
/// - `openai-codex`: true OAuth (browser PKCE / device code)
/// - `xai` / `grok`: xAI SuperGrok OAuth
/// - `opencode` / `opencode-go`: subscription console + API key
pub async fn login_provider(
    storage: &AuthStorage,
    provider_id: &str,
    interaction: &mut dyn AuthInteraction,
) -> Result<AuthCredential, String> {
    match provider_id {
        PROVIDER_OPENAI_CODEX | "codex" | "chatgpt" => {
            let cred = login_openai_codex(interaction).await?;
            storage
                .set(PROVIDER_OPENAI_CODEX, AuthCredential::OAuth(cred.clone()))
                .map_err(|e| e.to_string())?;
            Ok(AuthCredential::OAuth(cred))
        }
        PROVIDER_XAI | "grok" | "xai-oauth" | "supergrok" => {
            let cred = login_xai(interaction).await?;
            storage
                .set(PROVIDER_XAI, AuthCredential::OAuth(cred.clone()))
                .map_err(|e| e.to_string())?;
            Ok(AuthCredential::OAuth(cred))
        }
        PROVIDER_OPENCODE | "zen" | "opencode-zen" | "opencode_zen" | PROVIDER_OPENCODE_GO
        | "go" | "opencode_go" => login_opencode(storage, provider_id, interaction).await,
        other => Err(format!(
            "unknown login provider `{other}` · available: {}",
            oauth_provider_catalog()
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Convenience: browser-only Codex login (skips method select).
pub async fn login_openai_codex_browser_persist(
    storage: &AuthStorage,
    interaction: &mut dyn AuthInteraction,
) -> Result<OAuthCredential, String> {
    let cred = login_openai_codex_browser(interaction).await?;
    storage
        .set(PROVIDER_OPENAI_CODEX, AuthCredential::OAuth(cred.clone()))
        .map_err(|e| e.to_string())?;
    Ok(cred)
}

/// Convenience: device-code Codex login.
pub async fn login_openai_codex_device_persist(
    storage: &AuthStorage,
    interaction: &mut dyn AuthInteraction,
) -> Result<OAuthCredential, String> {
    let cred = login_openai_codex_device(interaction).await?;
    storage
        .set(PROVIDER_OPENAI_CODEX, AuthCredential::OAuth(cred.clone()))
        .map_err(|e| e.to_string())?;
    Ok(cred)
}

/// Convenience: browser-only xAI login.
pub async fn login_xai_browser_persist(
    storage: &AuthStorage,
    interaction: &mut dyn AuthInteraction,
) -> Result<OAuthCredential, String> {
    let cred = login_xai_browser(interaction).await?;
    storage
        .set(PROVIDER_XAI, AuthCredential::OAuth(cred.clone()))
        .map_err(|e| e.to_string())?;
    Ok(cred)
}

/// Convenience: device-code xAI login.
pub async fn login_xai_device_persist(
    storage: &AuthStorage,
    interaction: &mut dyn AuthInteraction,
) -> Result<OAuthCredential, String> {
    let cred = login_xai_device(interaction).await?;
    storage
        .set(PROVIDER_XAI, AuthCredential::OAuth(cred.clone()))
        .map_err(|e| e.to_string())?;
    Ok(cred)
}
