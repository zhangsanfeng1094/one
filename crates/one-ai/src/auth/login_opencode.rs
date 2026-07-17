//! OpenCode Zen / Go subscription login.
//!
//! Official OpenCode path: sign in at <https://opencode.ai/auth>, create an API
//! key, paste it into the client. There is no public third-party OAuth client for
//! minting Zen tokens yet, so this mirrors Pi's `/login` for `opencode` /
//! `opencode-go`: browser to the console + store the API key in `auth.json`.
//!
//! Optional: import key from OpenCode CLI (`~/.local/share/opencode/auth.json`).

use std::fs;
use std::path::PathBuf;

use super::types::{
    AuthCredential, AuthEvent, AuthInteraction, AuthPrompt, SelectOption, PROVIDER_OPENCODE,
    PROVIDER_OPENCODE_GO,
};
use super::AuthStorage;

pub const OPENCODE_CONSOLE_URL: &str = "https://opencode.ai/auth";
pub const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";
pub const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

const METHOD_PASTE: &str = "paste";
const METHOD_IMPORT: &str = "import_opencode_cli";

/// Interactive login: pick method → store API key for `opencode` or `opencode-go`.
pub async fn login(
    storage: &AuthStorage,
    provider_id: &str,
    interaction: &mut dyn AuthInteraction,
) -> Result<AuthCredential, String> {
    let provider = normalize_provider_id(provider_id)?;
    let label = display_name(provider);

    let method = interaction
        .prompt(AuthPrompt::Select {
            message: format!("Select {label} login method:"),
            options: vec![
                SelectOption {
                    id: METHOD_PASTE.into(),
                    label: "Paste API key (recommended)".into(),
                },
                SelectOption {
                    id: METHOD_IMPORT.into(),
                    label: "Import from OpenCode CLI auth.json".into(),
                },
            ],
        })
        .await?;

    let key = match method.as_str() {
        METHOD_IMPORT => import_from_opencode_cli(provider, interaction)?,
        METHOD_PASTE | "" => paste_api_key(provider, interaction).await?,
        other => return Err(format!("Unknown OpenCode login method: {other}")),
    };

    let key = key.trim().to_string();
    if key.is_empty() {
        return Err("API key is empty".into());
    }
    if key.len() < 8 {
        return Err("API key looks too short".into());
    }

    interaction.notify(AuthEvent::Progress {
        message: format!("Validating key against {} …", base_url_for(provider)),
    });
    validate_api_key(provider, &key).await?;

    let cred = AuthCredential::api_key(key);
    storage
        .set(provider, cred.clone())
        .map_err(|e| e.to_string())?;

    // Shared OPENCODE_API_KEY covers both Zen and Go catalogs — mirror into the
    // sibling provider when it has no credential yet.
    let sibling = sibling_provider(provider);
    if !storage.has(sibling) {
        let _ = storage.set(sibling, cred.clone());
        interaction.notify(AuthEvent::Info {
            message: format!(
                "Also stored key for `{sibling}` (shared OpenCode subscription credential)."
            ),
        });
    }

    interaction.notify(AuthEvent::Info {
        message: format!("✓ {label} API key saved to {}", storage.path().display()),
    });
    Ok(cred)
}

fn normalize_provider_id(raw: &str) -> Result<&'static str, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        PROVIDER_OPENCODE | "zen" | "opencode-zen" | "opencode_zen" => Ok(PROVIDER_OPENCODE),
        PROVIDER_OPENCODE_GO | "go" | "opencode_go" => Ok(PROVIDER_OPENCODE_GO),
        other => Err(format!(
            "unknown OpenCode provider `{other}` · use `{PROVIDER_OPENCODE}` or `{PROVIDER_OPENCODE_GO}`"
        )),
    }
}

fn display_name(provider: &str) -> &'static str {
    match provider {
        PROVIDER_OPENCODE_GO => "OpenCode Go",
        _ => "OpenCode Zen",
    }
}

pub fn base_url_for(provider: &str) -> &'static str {
    match provider {
        PROVIDER_OPENCODE_GO => OPENCODE_GO_BASE_URL,
        _ => OPENCODE_ZEN_BASE_URL,
    }
}

fn sibling_provider(provider: &str) -> &'static str {
    if provider == PROVIDER_OPENCODE_GO {
        PROVIDER_OPENCODE
    } else {
        PROVIDER_OPENCODE_GO
    }
}

async fn paste_api_key(
    provider: &str,
    interaction: &mut dyn AuthInteraction,
) -> Result<String, String> {
    let label = display_name(provider);
    interaction.notify(AuthEvent::AuthUrl {
        url: OPENCODE_CONSOLE_URL.into(),
        instructions: Some(format!(
            "1. Sign in (GitHub/Google)\n\
             2. Subscribe / open billing if needed ({label})\n\
             3. Create an API key and paste it below"
        )),
    });
    let _ = open_browser(OPENCODE_CONSOLE_URL);

    interaction
        .prompt(AuthPrompt::Text {
            message: format!("Paste your OpenCode API key for {label}:"),
            placeholder: Some("sk-…".into()),
        })
        .await
}

fn import_from_opencode_cli(
    provider: &str,
    interaction: &mut dyn AuthInteraction,
) -> Result<String, String> {
    let path = opencode_cli_auth_path();
    interaction.notify(AuthEvent::Progress {
        message: format!("Reading {} …", path.display()),
    });
    if !path.exists() {
        return Err(format!(
            "OpenCode CLI auth not found at {} · run `opencode auth login` or paste a key",
            path.display()
        ));
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;

    // Prefer exact provider, then sibling, then any opencode* entry.
    let candidates = [
        provider,
        sibling_provider(provider),
        PROVIDER_OPENCODE,
        PROVIDER_OPENCODE_GO,
    ];
    for id in candidates {
        if let Some(key) = extract_key(&value, id) {
            interaction.notify(AuthEvent::Info {
                message: format!("Imported `{id}` key from OpenCode CLI"),
            });
            return Ok(key);
        }
    }

    // First api-type entry under any key containing "opencode".
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            if k.contains("opencode") {
                if let Some(key) = v.get("key").and_then(|x| x.as_str()) {
                    if !key.is_empty() {
                        interaction.notify(AuthEvent::Info {
                            message: format!("Imported `{k}` key from OpenCode CLI"),
                        });
                        return Ok(key.to_string());
                    }
                }
            }
        }
    }

    Err(format!(
        "no opencode / opencode-go key in {}",
        path.display()
    ))
}

fn extract_key(value: &serde_json::Value, provider: &str) -> Option<String> {
    let entry = value.get(provider)?;
    entry
        .get("key")
        .and_then(|k| k.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn opencode_cli_auth_path() -> PathBuf {
    if let Ok(p) = std::env::var("OPENCODE_AUTH_PATH") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("opencode/auth.json");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/opencode/auth.json")
}

async fn validate_api_key(provider: &str, key: &str) -> Result<(), String> {
    #[cfg(not(feature = "http-providers"))]
    {
        let _ = (provider, key);
        return Ok(());
    }
    #[cfg(feature = "http-providers")]
    {
        let base = base_url_for(provider);
        let url = format!("{}/models", base.trim_end_matches('/'));
        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .bearer_auth(key)
            .header("x-opencode-client", "one")
            .send()
            .await
            .map_err(|e| format!("validate key: network error: {e}"))?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let head: String = text.chars().take(240).collect();
        Err(format!(
            "OpenCode rejected API key ({status}): {head}"
        ))
    }
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = url;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_aliases() {
        assert_eq!(normalize_provider_id("zen").unwrap(), PROVIDER_OPENCODE);
        assert_eq!(
            normalize_provider_id("opencode-go").unwrap(),
            PROVIDER_OPENCODE_GO
        );
        assert_eq!(normalize_provider_id("go").unwrap(), PROVIDER_OPENCODE_GO);
        assert!(normalize_provider_id("openai").is_err());
    }

    #[test]
    fn extract_key_from_cli_shape() {
        let v = serde_json::json!({
            "opencode-go": { "type": "api", "key": "sk-test-key" }
        });
        assert_eq!(
            extract_key(&v, PROVIDER_OPENCODE_GO).as_deref(),
            Some("sk-test-key")
        );
        assert!(extract_key(&v, PROVIDER_OPENCODE).is_none());
    }
}
