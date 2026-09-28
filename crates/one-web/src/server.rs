//! Embedded Web server and WebSocket-to-ACP bridge for One Web UI.
//!
//! The server is intentionally framework-free: a released `one` binary must run
//! with no Node.js or other runtime. Routing is a small dispatch chain —
//! request guard → pluggable API handler → built-in endpoints → embedded SPA.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::assets::get_asset;
use crate::http::{read_request, HttpRequest, HttpResponse, DEFAULT_MAX_BODY, DEFAULT_MAX_HEAD};
use crate::ws::{compute_accept_key, read_ws_frame, write_ws_pong, write_ws_text, WsFrame};

pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub type LocalAcpHandler =
    Rc<dyn Fn(Compat<DuplexStream>, Compat<DuplexStream>) -> LocalBoxFuture<'static, ()>>;

/// Future returned by a pluggable API handler.
pub type ApiFuture = Pin<Box<dyn Future<Output = Option<HttpResponse>> + 'static>>;

/// Pluggable API handler: returns `None` to fall through to the built-in routes.
pub type ApiHandler = Rc<dyn Fn(HttpRequest) -> ApiFuture>;

/// Pre-dispatch guard (auth, Host/Origin checks). Returning `Err` short-circuits
/// the request with that response.
pub type RequestGuard = Rc<dyn Fn(&HttpRequest) -> Result<(), HttpResponse>>;

/// Configuration for [`start_web_server`].
pub struct WebServerConfig {
    /// Bind address, e.g. `127.0.0.1`.
    pub host: String,
    /// Bind port; `0` selects an ephemeral port.
    pub port: u16,
    /// Open the default browser once the listener is bound.
    pub open_browser: bool,
    /// Payload served by the built-in `/api/info` endpoint.
    pub info_json: serde_json::Value,
    /// Replaces the startup banner when set (used by the config studio to print
    /// a tokenized URL).
    pub banner: Option<String>,
    /// Value for `Access-Control-Allow-Origin`. `None` disables CORS entirely,
    /// which is required for same-origin-only servers such as the config studio.
    pub cors_allow_origin: Option<String>,
    /// Runs before any routing decision.
    pub guard: Option<RequestGuard>,
    /// Handles `/api/*` routes; `None` responses fall through.
    pub api: Option<ApiHandler>,
}

impl Default for WebServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 3000,
            open_browser: false,
            info_json: serde_json::Value::Null,
            banner: None,
            cors_allow_origin: Some("*".to_string()),
            guard: None,
            api: None,
        }
    }
}

/// Run the Web UI and WebSocket server until interrupted.
pub async fn start_web_server(
    config: WebServerConfig,
    acp_handler_factory: impl Fn() -> LocalAcpHandler + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&addr).await.map_err(|e| {
        // A bare `AddrInUse` is unhelpful: the config studio requires a fixed
        // port (the token lives in the URL), so the actionable fix is "pick
        // another port" rather than a raw OS error.
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!("无法监听 {addr}：端口已被占用，请用 --port 指定另一个端口（{e}）")
        } else {
            format!("无法监听 {addr}：{e}")
        }
    })?;
    let local_addr = listener.local_addr()?;
    let is_wildcard = config.host == "0.0.0.0" || config.host == "::";
    let browser_url = if is_wildcard {
        format!("http://127.0.0.1:{}/", local_addr.port())
    } else {
        format!("http://{}:{}/", config.host, local_addr.port())
    };

    match &config.banner {
        Some(text) => eprintln!("{text}"),
        None => print_default_banner(&browser_url, local_addr.port(), is_wildcard),
    }

    if config.open_browser {
        open_browser_url(&browser_url);
    }

    let shared = Arc::new(SharedState {
        info_json: config.info_json,
        cors_allow_origin: config.cors_allow_origin,
        guard: config.guard,
        api: config.api,
    });

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!(error = %e, "accept failed");
                        continue;
                    }
                };

                let state = Arc::clone(&shared);
                let acp_handler = acp_handler_factory();
                tokio::task::spawn_local(async move {
                    if let Err(err) = handle_connection(stream, state, acp_handler).await {
                        tracing::debug!(peer = %peer, error = %err, "web connection closed");
                    }
                });
            }
        })
        .await
}

/// State shared by every connection.
struct SharedState {
    info_json: serde_json::Value,
    cors_allow_origin: Option<String>,
    guard: Option<RequestGuard>,
    api: Option<ApiHandler>,
}

fn print_default_banner(browser_url: &str, port: u16, is_wildcard: bool) {
    eprintln!("╔════════════════════════════════════════════════════════════╗");
    eprintln!("║       ⚡ One AI Coding Agent — React Web Interface        ║");
    eprintln!("╠════════════════════════════════════════════════════════════╣");
    if is_wildcard {
        eprintln!("║  Local URL:   {browser_url:<44} ║");
        eprintln!("║  Network:     All interfaces (0.0.0.0:{port})             ║");
    } else {
        eprintln!("║  Web UI URL:  {browser_url:<44} ║");
    }
    eprintln!("║  Protocol:    Agent Client Protocol v1 over WebSocket      ║");
    eprintln!("║  Frontend:    Vite + React (Embedded SPA)                  ║");
    eprintln!("║  Status:      Listening for connections...                 ║");
    eprintln!("╚════════════════════════════════════════════════════════════╝");
}

fn open_browser_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", url])
        .spawn();
}

async fn handle_connection(
    mut stream: TcpStream,
    state: Arc<SharedState>,
    acp_handler: LocalAcpHandler,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = match read_request(&mut stream, DEFAULT_MAX_HEAD, DEFAULT_MAX_BODY).await {
        Ok(Some(req)) => req,
        Ok(None) => return Ok(()),
        Err(err) => {
            let resp = HttpResponse::error(400, format!("malformed HTTP request: {err}"));
            let _ = stream.write_all(&resp.to_bytes()).await;
            return Ok(());
        }
    };

    // Guards run first so an unauthorized request can never reach the ACP bridge.
    if let Some(guard) = &state.guard {
        if let Err(resp) = guard(&request) {
            stream.write_all(&resp.to_bytes()).await?;
            stream.flush().await?;
            return Ok(());
        }
    }

    let is_websocket = request
        .header("upgrade")
        .map(|v| v.to_ascii_lowercase().contains("websocket"))
        .unwrap_or(false);

    if is_websocket {
        return upgrade_websocket(stream, &request, acp_handler).await;
    }

    let response = route(&request, &state).await;
    stream.write_all(&response.to_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Dispatch a non-upgrade request through the API handler, built-ins, and assets.
async fn route(request: &HttpRequest, state: &SharedState) -> HttpResponse {
    if let Some(api) = &state.api {
        if let Some(resp) = api(request.clone()).await {
            return resp;
        }
    }

    if request.path == "/api/info" {
        let mut resp = HttpResponse::json(200, &state.info_json);
        if let Some(origin) = &state.cors_allow_origin {
            resp = resp.with_header("Access-Control-Allow-Origin", origin.clone());
        }
        return resp;
    }

    if let Some(asset) = get_asset(&request.path) {
        let body = asset.body();
        return HttpResponse::new(200, asset.content_type(), body.as_bytes().to_vec())
            .with_header("Cache-Control", "public, max-age=3600");
    }

    HttpResponse::text(404, "Not Found")
}

async fn upgrade_websocket(
    mut stream: TcpStream,
    request: &HttpRequest,
    acp_handler: LocalAcpHandler,
) -> Result<(), Box<dyn std::error::Error>> {
    let sec_key = request.header("sec-websocket-key").unwrap_or_default();
    if sec_key.is_empty() {
        let resp = HttpResponse::error(400, "missing Sec-WebSocket-Key");
        stream.write_all(&resp.to_bytes()).await?;
        return Ok(());
    }

    let accept_key = compute_accept_key(sec_key);
    let handshake_resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept_key}\r\n\r\n"
    );
    stream.write_all(handshake_resp.as_bytes()).await?;
    stream.flush().await?;

    // Any bytes parsed past the head belong to the WebSocket stream.
    bridge_websocket_to_acp(stream, request.body.clone(), acp_handler).await
}

enum OutgoingWsMsg {
    Text(String),
    Pong(Vec<u8>),
}

async fn bridge_websocket_to_acp(
    stream: TcpStream,
    initial: Vec<u8>,
    acp_handler: LocalAcpHandler,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut ws_rx, mut ws_tx) = tokio::io::split(stream);

    let (ws_out_tx, mut ws_out_rx) = mpsc::unbounded_channel::<OutgoingWsMsg>();

    // Duplex pipe 1: Web Client (ws_rx) -> Agent incoming
    let (mut agent_in_tx, agent_in_rx) = tokio::io::duplex(64 * 1024);
    // Duplex pipe 2: Agent outgoing -> Web Client (ws_tx)
    let (agent_out_tx, agent_out_rx) = tokio::io::duplex(64 * 1024);

    let incoming = agent_in_rx.compat();
    let outgoing = agent_out_tx.compat_write();

    // Spawn the ACP connection task
    let acp_fut = (acp_handler)(incoming, outgoing);
    let acp_task = tokio::task::spawn_local(acp_fut);

    // Task 1: Dedicated WebSocket Writer task
    let ws_writer_task = tokio::task::spawn_local(async move {
        while let Some(msg) = ws_out_rx.recv().await {
            match msg {
                OutgoingWsMsg::Text(text) => {
                    if write_ws_text(&mut ws_tx, &text).await.is_err() {
                        break;
                    }
                }
                OutgoingWsMsg::Pong(data) => {
                    if write_ws_pong(&mut ws_tx, &data).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Task 2: Agent output pipe -> WebSocket writer channel
    let ws_out_tx_clone = ws_out_tx.clone();
    let agent_out_task = tokio::task::spawn_local(async move {
        let mut lines = tokio::io::BufReader::new(agent_out_rx).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if ws_out_tx_clone
                .send(OutgoingWsMsg::Text(trimmed.to_string()))
                .is_err()
            {
                break;
            }
        }
    });

    // Task 3: WebSocket frames -> Agent input pipe & pongs
    let in_task = tokio::task::spawn_local(async move {
        if !initial.is_empty() && agent_in_tx.write_all(&initial).await.is_err() {
            return;
        }
        loop {
            match read_ws_frame(&mut ws_rx).await {
                Ok(WsFrame::Text(text)) => {
                    let mut payload = text.into_bytes();
                    payload.push(b'\n');
                    if agent_in_tx.write_all(&payload).await.is_err() {
                        break;
                    }
                }
                Ok(WsFrame::Ping(data)) => {
                    let _ = ws_out_tx.send(OutgoingWsMsg::Pong(data));
                }
                Ok(WsFrame::Close(_)) | Err(_) => {
                    break;
                }
                _ => {}
            }
        }
    });

    // Wait until connection closes
    tokio::select! {
        _ = acp_task => {},
        _ = ws_writer_task => {},
        _ = agent_out_task => {},
        _ = in_task => {},
    }

    Ok(())
}
