//! OpenAI Codex (ChatGPT Plus/Pro) OAuth — aligned with Pi `openai-codex` flow.
//!
//! - Browser PKCE + localhost:1455/auth/callback
//! - Device-code (headless) via auth.openai.com deviceauth APIs
//! - Token refresh + accountId extraction from JWT

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use serde_json::Value;

use super::pkce::{generate_pkce, random_hex};
use super::types::{
    AuthEvent, AuthInteraction, AuthPrompt, OAuthCredential, OAuthKind, SelectOption,
};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_CODE_TIMEOUT_SECS: u64 = 15 * 60;
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const ORIGINATOR: &str = "one";

const BROWSER_METHOD: &str = "browser";
const DEVICE_METHOD: &str = "device_code";

/// Run full Codex login (method select → browser or device code).
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
            message: "Select OpenAI Codex login method:".into(),
            options: vec![
                SelectOption {
                    id: BROWSER_METHOD.into(),
                    label: "Browser login (default)".into(),
                },
                SelectOption {
                    id: DEVICE_METHOD.into(),
                    label: "Device code login (headless)".into(),
                },
            ],
        })
        .await?;

    match method.as_str() {
        DEVICE_METHOD => login_device_code(interaction).await,
        BROWSER_METHOD | "" => login_browser(interaction).await,
        other => Err(format!("Unknown OpenAI Codex login method: {other}")),
    }
}

/// Browser-only login (skips method select) — useful for CLI flags.
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

/// Device-code-only login.
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
    let pkce = generate_pkce();
    let state = random_hex(16);
    let url = authorize_url(&pkce.challenge, &state);

    let callback_host = std::env::var("ONE_OAUTH_CALLBACK_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "127.0.0.1".into());

    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    let state_for_server = state.clone();
    let host_for_server = callback_host.clone();
    let _server_handle = thread::spawn(move || {
        let _ = start_callback_server(&host_for_server, 1455, &state_for_server, tx);
    });

    // Give the listener a moment to bind (or fail).
    tokio::time::sleep(Duration::from_millis(120)).await;

    // If bind already failed, fall back to paste-only (no hanging race with stdin).
    if let Ok(early) = rx.try_recv() {
        match early {
            Ok(code) => {
                // Extremely unlikely this fast, but handle it.
                interaction.notify(AuthEvent::Progress {
                    message: "Exchanging authorization code for tokens...".into(),
                });
                return exchange_code(&code, &pkce.verifier, REDIRECT_URI).await;
            }
            Err(bind_err) => {
                interaction.notify(AuthEvent::Info {
                    message: format!(
                        "Local callback unavailable ({bind_err}). Paste the redirect URL/code instead."
                    ),
                });
                let input = interaction
                    .prompt(AuthPrompt::ManualCode {
                        message: "Paste the authorization code or full redirect URL:".into(),
                        placeholder: Some(REDIRECT_URI.into()),
                    })
                    .await?;
                let parsed = parse_authorization_input(&input);
                if let Some(s) = parsed.state.as_deref() {
                    if s != state {
                        return Err("OAuth state mismatch".into());
                    }
                }
                let code = parsed
                    .code
                    .ok_or_else(|| "Missing authorization code".to_string())?;
                interaction.notify(AuthEvent::Progress {
                    message: "Exchanging authorization code for tokens...".into(),
                });
                return exchange_code(&code, &pkce.verifier, REDIRECT_URI).await;
            }
        }
    }

    interaction.notify(AuthEvent::AuthUrl {
        url: url.clone(),
        instructions: Some(
            "Complete login in your browser. Waiting for localhost:1455 callback…\n\
             (If nothing happens, cancel and use: one login --device-code)"
                .into(),
        ),
    });
    let _ = open_browser(&url);

    interaction.notify(AuthEvent::Progress {
        message: "Waiting for browser callback (do not type here — finish login in the browser)…"
            .into(),
    });

    // Wait for callback only — do NOT race stdin. A racing stdin read_line would
    // keep a background thread stealing keystrokes after TUI resumes.
    let code = wait_callback(rx).await?;

    interaction.notify(AuthEvent::Progress {
        message: "Exchanging authorization code for tokens...".into(),
    });

    exchange_code(&code, &pkce.verifier, REDIRECT_URI).await
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
    let client = reqwest::Client::new();
    let resp = client
        .post(DEVICE_USER_CODE_URL)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;

    if resp.status().as_u16() == 404 {
        return Err(
            "OpenAI Codex device code login is not enabled for this server. Use browser login."
                .into(),
        );
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "OpenAI Codex device code request failed with status {status}: {body}"
        ));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid device code JSON: {e}"))?;
    let device_auth_id = json
        .get("device_auth_id")
        .and_then(|v| v.as_str())
        .ok_or("device_auth_id missing")?
        .to_string();
    let user_code = json
        .get("user_code")
        .and_then(|v| v.as_str())
        .ok_or("user_code missing")?
        .to_string();
    let interval = json
        .get("interval")
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        })
        .unwrap_or(5.0);
    let interval_secs = if interval < 1.0 { 1 } else { interval as u64 };

    interaction.notify(AuthEvent::DeviceCode {
        user_code: user_code.clone(),
        verification_uri: DEVICE_VERIFICATION_URI.into(),
        interval_seconds: Some(interval_secs),
        expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECS),
    });
    let _ = open_browser(DEVICE_VERIFICATION_URI);

    let deadline = SystemTime::now() + Duration::from_secs(DEVICE_CODE_TIMEOUT_SECS);
    let mut sleep_ms = interval_secs.saturating_mul(1000).max(1000);

    // RFC 8628: wait before first poll.
    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;

    loop {
        if interaction.is_cancelled() {
            return Err("Login cancelled".into());
        }
        if SystemTime::now() > deadline {
            return Err("Device flow timed out".into());
        }

        let poll = client
            .post(DEVICE_TOKEN_URL)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            }))
            .send()
            .await
            .map_err(|e| format!("device auth poll failed: {e}"))?;

        if poll.status().is_success() {
            let json: Value = poll
                .json()
                .await
                .map_err(|e| format!("invalid device token JSON: {e}"))?;
            let code = json
                .get("authorization_code")
                .and_then(|v| v.as_str())
                .ok_or("authorization_code missing")?
                .to_string();
            let verifier = json
                .get("code_verifier")
                .and_then(|v| v.as_str())
                .ok_or("code_verifier missing")?
                .to_string();
            interaction.notify(AuthEvent::Progress {
                message: "Exchanging device authorization for tokens...".into(),
            });
            return exchange_code(&code, &verifier, DEVICE_REDIRECT_URI).await;
        }

        let status = poll.status().as_u16();
        let body = poll.text().await.unwrap_or_default();
        if status == 403 || status == 404 {
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            continue;
        }

        let error_code = serde_json::from_str::<Value>(&body).ok().and_then(|j| {
            let err = j.get("error")?;
            if let Some(s) = err.as_str() {
                return Some(s.to_string());
            }
            err.get("code")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
        });

        match error_code.as_deref() {
            Some("deviceauth_authorization_pending") | None if status == 400 => {
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                continue;
            }
            Some("slow_down") => {
                sleep_ms = sleep_ms.saturating_add(5000).max(1000);
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                continue;
            }
            _ => {
                return Err(format!(
                    "OpenAI Codex device auth failed with status {status}: {body}"
                ));
            }
        }
    }
}

#[cfg(feature = "http-providers")]
async fn refresh_inner(oauth: &OAuthCredential) -> Result<OAuthCredential, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_encode(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &oauth.refresh),
            ("client_id", CLIENT_ID),
        ]))
        .send()
        .await
        .map_err(|e| format!("OpenAI Codex token refresh error: {e}"))?;
    let token = read_token_response(resp, "refresh").await?;
    credentials_from_token(token)
}

#[cfg(feature = "http-providers")]
async fn exchange_code(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredential, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
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
    let token = read_token_response(resp, "exchange").await?;
    credentials_from_token(token)
}

struct TokenBundle {
    access: String,
    refresh: String,
    expires: u64,
}

#[cfg(feature = "http-providers")]
async fn read_token_response(
    resp: reqwest::Response,
    operation: &str,
) -> Result<TokenBundle, String> {
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "OpenAI Codex token {operation} failed ({status}): {text}"
        ));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("token {operation} invalid JSON: {e}"))?;
    let access = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("access_token missing")?
        .to_string();
    let refresh = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or("refresh_token missing")?
        .to_string();
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .ok_or("expires_in missing")?;
    Ok(TokenBundle {
        access,
        refresh,
        expires: now_ms() + expires_in * 1000,
    })
}

fn credentials_from_token(token: TokenBundle) -> Result<OAuthCredential, String> {
    let account_id = extract_account_id(&token.access)
        .ok_or_else(|| "Failed to extract accountId from token".to_string())?;
    Ok(OAuthCredential {
        kind: OAuthKind::Oauth,
        access: token.access,
        refresh: token.refresh,
        expires: token.expires,
        account_id: Some(account_id),
        extra: BTreeMap::new(),
    })
}

pub fn extract_account_id(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token)?;
    payload
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn decode_jwt_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| STANDARD.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn authorize_url(challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?{}",
        form_encode(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", ORIGINATOR),
        ])
    )
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
    if value.starts_with("http://") || value.starts_with("https://") {
        let query = value
            .split_once('?')
            .map(|(_, q)| q.split('#').next().unwrap_or(q))
            .unwrap_or("");
        let mut code = None;
        let mut state = None;
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                let v = url_decode(v);
                match k {
                    "code" => code = Some(v),
                    "state" => state = Some(v),
                    _ => {}
                }
            }
        }
        return ParsedAuth { code, state };
    }
    // Manual paste of "code#state"
    if let Some((code, state)) = value.split_once('#') {
        return ParsedAuth {
            code: Some(code.to_string()),
            state: Some(state.to_string()),
        };
    }
    if value.contains("code=") {
        let mut code = None;
        let mut state = None;
        for pair in value.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k.ends_with("code") {
                    code = Some(v.to_string());
                } else if k.ends_with("state") {
                    state = Some(v.to_string());
                }
            }
        }
        return ParsedAuth { code, state };
    }
    ParsedAuth {
        code: Some(value.to_string()),
        state: None,
    }
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
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

/// Fire-and-forget browser open. **Must not wait** on the child — on many Linux
/// setups `xdg-open` blocks until the browser process exits, which freezes CLI/TUI.
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
        Ok(_child) => Ok(()),
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
    loop {
        if SystemTime::now() > deadline {
            let _ = tx.send(Err("OAuth callback timed out".into()));
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = handle_callback_stream(stream, expected_state, &tx) {
                    let _ = tx.send(Err(e));
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Manual paste path drops the receiver; accept loop exits on next send failure
                // or when the process ends. Sleep and retry.
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = tx.send(Err(format!("callback accept error: {e}")));
                return Ok(());
            }
        }
    }
}

fn handle_callback_stream(
    mut stream: TcpStream,
    expected_state: &str,
    tx: &mpsc::Sender<Result<String, String>>,
) -> Result<(), String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    // GET /auth/callback?code=...&state=... HTTP/1.1
    let path = first_line.split_whitespace().nth(1).unwrap_or("");
    if !path.starts_with("/auth/callback") {
        let body = oauth_error_html("Callback route not found.");
        let _ = write_html(&mut stream, 404, &body);
        return Ok(());
    }
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let v = url_decode(v);
            match k {
                "code" => code = Some(v),
                "state" => state = Some(v),
                _ => {}
            }
        }
    }
    if state.as_deref() != Some(expected_state) {
        let body = oauth_error_html("State mismatch.");
        let _ = write_html(&mut stream, 400, &body);
        let _ = tx.send(Err("OAuth state mismatch".into()));
        return Ok(());
    }
    let Some(code) = code else {
        let body = oauth_error_html("Missing authorization code.");
        let _ = write_html(&mut stream, 400, &body);
        let _ = tx.send(Err("Missing authorization code".into()));
        return Ok(());
    };
    let body = oauth_success_html("OpenAI authentication completed. You can close this window.");
    let _ = write_html(&mut stream, 200, &body);
    let _ = tx.send(Ok(code));
    Ok(())
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

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
