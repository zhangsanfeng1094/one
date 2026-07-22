//! xAI Grok OAuth (SuperGrok / X Premium+ subscription).
//!
//! Aligns with Grok CLI / Pi community flows:
//! - OIDC discovery at `https://auth.x.ai`
//! - Browser PKCE → `http://127.0.0.1:56121/callback`
//! - Device-code grant (headless)
//! - Refresh token
//!
//! Client id is the public Grok CLI client (xAI has no public registration).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::pkce::{generate_pkce, random_hex};
use super::types::{
    AuthEvent, AuthInteraction, AuthPrompt, OAuthCredential, OAuthKind, SelectOption,
};

/// Subscription chat proxy (SuperGrok quota) — preferred for OAuth tokens.
pub const DEFAULT_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
/// Public API base (API-key / fallback).
pub const PUBLIC_API_BASE_URL: &str = "https://api.x.ai/v1";

const ISSUER: &str = "https://auth.x.ai";
const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";
const CALLBACK_PORT: u16 = 56121;
const DEVICE_CODE_TIMEOUT_SECS: u64 = 15 * 60;
const DEFAULT_POLL_MS: u64 = 5_000;

const BROWSER_METHOD: &str = "browser";
const DEVICE_METHOD: &str = "device_code";

/// Grok CLI identity headers expected by the subscription proxy.
pub fn cli_identity_headers() -> BTreeMap<String, String> {
    let version = std::env::var("ONE_XAI_CLIENT_VERSION")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0.2.101".into());
    let mut h = BTreeMap::new();
    h.insert("X-XAI-Token-Auth".into(), "xai-grok-cli".into());
    h.insert("x-grok-client-version".into(), version.clone());
    h.insert("User-Agent".into(), format!("xai-grok-workspace/{version}"));
    h
}

/// Run full xAI login (method select → browser or device code).
pub async fn login(interaction: &mut dyn AuthInteraction) -> Result<OAuthCredential, String> {
    #[cfg(not(feature = "http-providers"))]
    {
        let _ = interaction;
        return Err("OAuth login requires --features http-providers".into());
    }
    #[cfg(feature = "http-providers")]
    {
        login_inner(interaction).await
    }
}

#[cfg(feature = "http-providers")]
async fn login_inner(interaction: &mut dyn AuthInteraction) -> Result<OAuthCredential, String> {
    let method = interaction
        .prompt(AuthPrompt::Select {
            message: "Select xAI Grok login method:".into(),
            options: vec![
                SelectOption {
                    // Device-code is the reliable SuperGrok path (same as Hermes / Grok CLI headless).
                    id: DEVICE_METHOD.into(),
                    label: "Device code (recommended) — enter code on xAI page".into(),
                },
                SelectOption {
                    id: BROWSER_METHOD.into(),
                    label: "Browser PKCE — needs localhost:56121 callback".into(),
                },
            ],
        })
        .await?;

    match method.as_str() {
        BROWSER_METHOD => login_browser(interaction).await,
        DEVICE_METHOD | "" => login_device_code(interaction).await,
        other => Err(format!("Unknown xAI login method: {other}")),
    }
}

pub async fn login_browser(
    interaction: &mut dyn AuthInteraction,
) -> Result<OAuthCredential, String> {
    #[cfg(not(feature = "http-providers"))]
    {
        let _ = interaction;
        return Err("OAuth login requires --features http-providers".into());
    }
    #[cfg(feature = "http-providers")]
    {
        login_browser_inner(interaction).await
    }
}

pub async fn login_device_code(
    interaction: &mut dyn AuthInteraction,
) -> Result<OAuthCredential, String> {
    #[cfg(not(feature = "http-providers"))]
    {
        let _ = interaction;
        return Err("OAuth login requires --features http-providers".into());
    }
    #[cfg(feature = "http-providers")]
    {
        login_device_code_inner(interaction).await
    }
}

pub async fn refresh(oauth: &OAuthCredential) -> Result<OAuthCredential, String> {
    #[cfg(not(feature = "http-providers"))]
    {
        let _ = oauth;
        return Err("OAuth refresh requires --features http-providers".into());
    }
    #[cfg(feature = "http-providers")]
    {
        refresh_inner(oauth).await
    }
}

#[cfg(feature = "http-providers")]
async fn login_browser_inner(
    interaction: &mut dyn AuthInteraction,
) -> Result<OAuthCredential, String> {
    let discovery = discover().await?;
    let pkce = generate_pkce();
    let state = random_hex(16);
    let url = authorize_url(&discovery.authorization_endpoint, &pkce.challenge, &state);

    let callback_host = std::env::var("ONE_OAUTH_CALLBACK_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "127.0.0.1".into());
    let port = std::env::var("ONE_XAI_OAUTH_CALLBACK_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(CALLBACK_PORT);

    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    let state_for_server = state.clone();
    let host_for_server = callback_host.clone();
    let _server_handle = thread::spawn(move || {
        let _ = start_callback_server(&host_for_server, port, &state_for_server, tx);
    });

    tokio::time::sleep(Duration::from_millis(120)).await;

    if let Ok(early) = rx.try_recv() {
        match early {
            Ok(code) => {
                interaction.notify(AuthEvent::Progress {
                    message: "Exchanging authorization code for tokens...".into(),
                });
                return exchange_code(
                    &discovery.token_endpoint,
                    &code,
                    &pkce.verifier,
                    &redirect_uri(port),
                )
                .await;
            }
            Err(bind_err) => {
                interaction.notify(AuthEvent::Info {
                    message: format!(
                        "Local callback unavailable ({bind_err}). Paste the redirect URL/code instead."
                    ),
                });
                return paste_and_exchange(interaction, &discovery, &pkce.verifier, &state, port)
                    .await;
            }
        }
    }

    interaction.notify(AuthEvent::AuthUrl {
        url: url.clone(),
        instructions: Some(format!(
            "Complete login in the browser.\n\
             · Success looks like a redirect to http://127.0.0.1:{port}/callback\n\
             · If you only see \"Copy the code into Grok Build\", that page is for\n\
               Grok Build's own CLI — cancel and use: one login xai --device-code"
        )),
    });
    let _ = open_browser(&url);

    interaction.notify(AuthEvent::Progress {
        message: format!(
            "Waiting for localhost:{port}/callback (do not type here — finish in the browser)…"
        ),
    });

    // Callback only — do NOT race stdin. A concurrent paste prompt steals keystrokes
    // and, when cancelled by select!, leaves stdin/TUI wedged after resume.
    let code = wait_callback(rx).await?;
    interaction.notify(AuthEvent::Progress {
        message: "Exchanging authorization code for tokens...".into(),
    });
    exchange_code(
        &discovery.token_endpoint,
        &code,
        &pkce.verifier,
        &redirect_uri(port),
    )
    .await
}

#[cfg(feature = "http-providers")]
async fn paste_and_exchange(
    interaction: &mut dyn AuthInteraction,
    discovery: &Discovery,
    verifier: &str,
    expected_state: &str,
    port: u16,
) -> Result<OAuthCredential, String> {
    let input = interaction
        .prompt(AuthPrompt::ManualCode {
            message: "Paste the authorization code or full redirect URL:".into(),
            placeholder: Some(redirect_uri(port)),
        })
        .await?;
    let parsed = parse_authorization_input(&input);
    if let Some(s) = parsed.state.as_deref() {
        if s != expected_state {
            return Err("OAuth state mismatch".into());
        }
    }
    let code = parsed
        .code
        .ok_or_else(|| "Missing authorization code".to_string())?;
    interaction.notify(AuthEvent::Progress {
        message: "Exchanging authorization code for tokens...".into(),
    });
    exchange_code(
        &discovery.token_endpoint,
        &code,
        verifier,
        &redirect_uri(port),
    )
    .await
}

#[cfg(feature = "http-providers")]
async fn wait_callback(rx: mpsc::Receiver<Result<String, String>>) -> Result<String, String> {
    loop {
        match rx.try_recv() {
            Ok(result) => return result,
            Err(mpsc::TryRecvError::Empty) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("OAuth callback server stopped".into());
            }
        }
    }
}

#[cfg(feature = "http-providers")]
async fn login_device_code_inner(
    interaction: &mut dyn AuthInteraction,
) -> Result<OAuthCredential, String> {
    let discovery = discover().await?;
    let client = reqwest::Client::new();
    let device_ep = discovery
        .device_authorization_endpoint
        .as_deref()
        .ok_or("xAI discovery missing device_authorization_endpoint")?;

    let resp = client
        .post(device_ep)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_encode(&[("client_id", CLIENT_ID), ("scope", SCOPE)]))
        .send()
        .await
        .map_err(|e| format!("xAI device authorization failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "xAI device authorization failed ({status}): {body}"
        ));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid device authorization JSON: {e}"))?;
    let device_code = json
        .get("device_code")
        .and_then(|v| v.as_str())
        .ok_or("device_code missing")?
        .to_string();
    let user_code = json
        .get("user_code")
        .and_then(|v| v.as_str())
        .ok_or("user_code missing")?
        .to_string();
    let verification_uri = json
        .get("verification_uri_complete")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("verification_uri").and_then(|v| v.as_str()))
        .ok_or("verification_uri missing")?
        .to_string();
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEVICE_CODE_TIMEOUT_SECS);
    let mut interval_ms = json
        .get("interval")
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        .map(|s| s.saturating_mul(1000))
        .unwrap_or(DEFAULT_POLL_MS)
        .max(DEFAULT_POLL_MS);

    interaction.notify(AuthEvent::DeviceCode {
        user_code: user_code.clone(),
        verification_uri: verification_uri.clone(),
        interval_seconds: Some(interval_ms / 1000),
        expires_in_seconds: Some(expires_in),
    });
    let _ = open_browser(&verification_uri);

    interaction.notify(AuthEvent::Progress {
        message: "Waiting for xAI device authorization...".into(),
    });

    let deadline =
        SystemTime::now() + Duration::from_secs(expires_in.min(DEVICE_CODE_TIMEOUT_SECS));
    // RFC 8628: wait before first poll.
    tokio::time::sleep(Duration::from_millis(interval_ms)).await;

    loop {
        if interaction.is_cancelled() {
            return Err("Login cancelled".into());
        }
        if SystemTime::now() > deadline {
            return Err("xAI device authorization timed out".into());
        }

        let poll = client
            .post(&discovery.token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(form_encode(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &device_code),
                ("client_id", CLIENT_ID),
            ]))
            .send()
            .await
            .map_err(|e| format!("xAI device token poll failed: {e}"))?;

        if poll.status().is_success() {
            let json: Value = poll
                .json()
                .await
                .map_err(|e| format!("invalid token JSON: {e}"))?;
            interaction.notify(AuthEvent::Progress {
                message: "Device authorization complete.".into(),
            });
            return credentials_from_token_json(&json, &discovery.token_endpoint, None);
        }

        let status = poll.status().as_u16();
        let body = poll.text().await.unwrap_or_default();
        let error = serde_json::from_str::<Value>(&body).ok().and_then(|j| {
            j.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
        });

        match error.as_deref() {
            Some("authorization_pending") | None if status == 400 => {
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            }
            Some("slow_down") => {
                interval_ms = interval_ms.saturating_add(5_000).max(DEFAULT_POLL_MS);
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            }
            Some("expired_token") => return Err("xAI device authorization expired".into()),
            Some("access_denied") => return Err("xAI device authorization denied".into()),
            _ => {
                return Err(format!("xAI device token failed ({status}): {body}"));
            }
        }
    }
}

#[cfg(feature = "http-providers")]
async fn refresh_inner(oauth: &OAuthCredential) -> Result<OAuthCredential, String> {
    let token_endpoint = oauth
        .extra
        .get("tokenEndpoint")
        .and_then(|v| v.as_str())
        .unwrap_or("https://auth.x.ai/oauth2/token")
        .to_string();
    let client = reqwest::Client::new();
    let resp = client
        .post(&token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_encode(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", &oauth.refresh),
        ]))
        .send()
        .await
        .map_err(|e| format!("xAI token refresh error: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("xAI token refresh failed ({status}): {text}"));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("refresh invalid JSON: {e}"))?;
    credentials_from_token_json(&json, &token_endpoint, Some(oauth.refresh.clone()))
}

#[cfg(feature = "http-providers")]
async fn exchange_code(
    token_endpoint: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredential, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_encode(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ]))
        .send()
        .await
        .map_err(|e| format!("token exchange failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("xAI token exchange failed ({status}): {text}"));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("token exchange invalid JSON: {e}"))?;
    credentials_from_token_json(&json, token_endpoint, None)
}

fn credentials_from_token_json(
    json: &Value,
    token_endpoint: &str,
    prev_refresh: Option<String>,
) -> Result<OAuthCredential, String> {
    let access = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("access_token missing")?
        .to_string();
    let refresh = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(prev_refresh)
        .ok_or("refresh_token missing")?;
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);
    let mut extra = BTreeMap::new();
    extra.insert(
        "tokenEndpoint".into(),
        Value::String(token_endpoint.to_string()),
    );
    if let Some(tt) = json.get("token_type").and_then(|v| v.as_str()) {
        extra.insert("tokenType".into(), Value::String(tt.into()));
    }
    Ok(OAuthCredential {
        kind: OAuthKind::Oauth,
        access,
        refresh,
        expires: now_ms() + expires_in * 1000,
        account_id: None,
        extra,
    })
}

struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    device_authorization_endpoint: Option<String>,
}

#[cfg(feature = "http-providers")]
async fn discover() -> Result<Discovery, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(DISCOVERY_URL)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("xAI OIDC discovery failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("xAI OIDC discovery returned {}", resp.status()));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("discovery invalid JSON: {e}"))?;
    let issuer = json
        .get("issuer")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_end_matches('/');
    if issuer != ISSUER {
        return Err(format!("xAI discovery issuer mismatch: {issuer}"));
    }
    let authorization_endpoint = validate_xai_endpoint(
        json.get("authorization_endpoint")
            .and_then(|v| v.as_str())
            .ok_or("authorization_endpoint missing")?,
        "authorization_endpoint",
    )?;
    let token_endpoint = validate_xai_endpoint(
        json.get("token_endpoint")
            .and_then(|v| v.as_str())
            .ok_or("token_endpoint missing")?,
        "token_endpoint",
    )?;
    let device_authorization_endpoint = match json
        .get("device_authorization_endpoint")
        .and_then(|v| v.as_str())
    {
        Some(s) => Some(validate_xai_endpoint(s, "device_authorization_endpoint")?),
        None => None,
    };
    Ok(Discovery {
        authorization_endpoint,
        token_endpoint,
        device_authorization_endpoint,
    })
}

fn validate_xai_endpoint(value: &str, field: &str) -> Result<String, String> {
    let value = value.trim();
    let rest = value
        .strip_prefix("https://")
        .ok_or_else(|| format!("invalid {field}: must be https"))?;
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    // strip port if present
    let host = host.split(':').next().unwrap_or(host.as_str());
    if !(host == "x.ai" || host.ends_with(".x.ai")) {
        return Err(format!("Refusing non-xAI OAuth {field}: {value}"));
    }
    Ok(value.to_string())
}

fn authorize_url(authorization_endpoint: &str, challenge: &str, state: &str) -> String {
    let base = authorization_endpoint.trim_end_matches('/');
    let sep = if base.contains('?') { '&' } else { '?' };
    format!(
        "{base}{sep}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        urlencoding_form(CLIENT_ID),
        urlencoding_form(REDIRECT_URI),
        urlencoding_form(SCOPE),
        urlencoding_form(state),
        urlencoding_form(challenge),
    )
}

fn redirect_uri(port: u16) -> String {
    if port == CALLBACK_PORT {
        REDIRECT_URI.to_string()
    } else {
        format!("http://127.0.0.1:{port}/callback")
    }
}

struct ParsedAuth {
    code: Option<String>,
    state: Option<String>,
}

fn parse_authorization_input(input: &str) -> ParsedAuth {
    let value = input.trim();
    if value.is_empty() {
        return ParsedAuth {
            code: None,
            state: None,
        };
    }
    // Full URL or query string containing code=
    let query = if let Some(q) = value.split_once('?').map(|(_, q)| q) {
        q
    } else if value.contains("code=") {
        value.trim_start_matches('?')
    } else {
        return ParsedAuth {
            code: Some(value.to_string()),
            state: None,
        };
    };
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "code" => code = Some(url_decode(v)),
                "state" => state = Some(url_decode(v)),
                _ => {}
            }
        }
    }
    ParsedAuth { code, state }
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding_form(k), urlencoding_form(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn urlencoding_form(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn open_browser(url: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("failed to open browser: {e}")),
    }
}

fn start_callback_server(
    host: &str,
    port: u16,
    expected_state: &str,
    tx: mpsc::Sender<Result<String, String>>,
) -> Result<(), String> {
    let listener = TcpListener::bind((host, port)).map_err(|e| {
        let _ = tx.send(Err(format!(
            "failed to bind OAuth callback on {host}:{port}: {e}"
        )));
        e.to_string()
    })?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;

    let deadline = SystemTime::now() + Duration::from_secs(DEVICE_CODE_TIMEOUT_SECS);
    // Keep accepting until we get a successful auth code.
    // Browsers often hit /favicon.ico or bare / first — those must NOT end the server
    // or the login future returns early and the TUI resumes half-broken.
    loop {
        if SystemTime::now() > deadline {
            let _ = tx.send(Err("OAuth callback timed out".into()));
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => match handle_callback_stream(stream, expected_state) {
                CallbackResult::Success(code) => {
                    let _ = tx.send(Ok(code));
                    return Ok(());
                }
                CallbackResult::Ignore => {
                    // Wrong path / incomplete — keep listening.
                }
                CallbackResult::Fatal(msg) => {
                    let _ = tx.send(Err(msg));
                    return Ok(());
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = tx.send(Err(format!("callback accept error: {e}")));
                return Ok(());
            }
        }
    }
}

enum CallbackResult {
    Success(String),
    /// Non-auth traffic (favicon, wrong path) — keep listening.
    Ignore,
    /// Real OAuth error from the callback query (state mismatch, oauth error param).
    Fatal(String),
}

fn handle_callback_stream(mut stream: TcpStream, expected_state: &str) -> CallbackResult {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 8192];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return CallbackResult::Ignore,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("");
    if !path.starts_with("/callback") {
        let body = oauth_error_html("Callback route not found. Waiting for /callback …");
        let _ = write_html(&mut stream, 404, &body);
        return CallbackResult::Ignore;
    }
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = None;
    let mut oauth_error = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let v = url_decode(v);
            match k {
                "code" => code = Some(v),
                "state" => state = Some(v),
                "error" => oauth_error = Some(v),
                _ => {}
            }
        }
    }
    if let Some(err) = oauth_error {
        let body = oauth_error_html(&format!("Authorization failed: {err}"));
        let _ = write_html(&mut stream, 400, &body);
        return CallbackResult::Fatal(format!("OAuth error: {err}"));
    }
    if state.as_deref() != Some(expected_state) {
        // Wrong or missing state — ignore so a later correct hit can succeed.
        let body = oauth_error_html("State mismatch — still waiting for a valid callback.");
        let _ = write_html(&mut stream, 400, &body);
        return CallbackResult::Ignore;
    }
    let Some(code) = code else {
        let body = oauth_error_html("Missing authorization code — still waiting.");
        let _ = write_html(&mut stream, 400, &body);
        return CallbackResult::Ignore;
    };
    let body = oauth_success_html("xAI Grok authentication completed. You can close this window.");
    let _ = write_html(&mut stream, 200, &body);
    CallbackResult::Success(code)
}

fn write_html(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn oauth_success_html(message: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>Login complete</title>
<style>body{{font-family:system-ui;background:#09090b;color:#fafafa;display:flex;min-height:100vh;align-items:center;justify-content:center;margin:0}}
main{{text-align:center;max-width:28rem;padding:2rem}}h1{{font-size:1.5rem}}p{{color:#a1a1aa}}</style></head>
<body><main><h1>Success</h1><p>{message}</p></main></body></html>"#,
        message = html_escape(message)
    )
}

fn oauth_error_html(message: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>Login error</title>
<style>body{{font-family:system-ui;background:#09090b;color:#fafafa;display:flex;min-height:100vh;align-items:center;justify-content:center;margin:0}}
main{{text-align:center;max-width:28rem;padding:2rem}}h1{{font-size:1.5rem;color:#f87171}}p{{color:#a1a1aa}}</style></head>
<body><main><h1>Error</h1><p>{message}</p></main></body></html>"#,
        message = html_escape(message)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_xai_host() {
        assert!(validate_xai_endpoint("https://auth.x.ai/oauth2/token", "t").is_ok());
        assert!(validate_xai_endpoint("https://evil.com/token", "t").is_err());
        assert!(validate_xai_endpoint("http://auth.x.ai/token", "t").is_err());
    }

    #[test]
    fn parse_redirect() {
        let p = parse_authorization_input("http://127.0.0.1:56121/callback?code=abc&state=xyz");
        assert_eq!(p.code.as_deref(), Some("abc"));
        assert_eq!(p.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn cli_headers_present() {
        let h = cli_identity_headers();
        assert_eq!(
            h.get("X-XAI-Token-Auth").map(|s| s.as_str()),
            Some("xai-grok-cli")
        );
    }
}
