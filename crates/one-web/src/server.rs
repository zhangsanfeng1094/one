//! Embedded Web server and WebSocket-to-ACP bridge for One Web UI.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::assets::get_asset;
use crate::ws::{compute_accept_key, read_ws_frame, write_ws_pong, write_ws_text, WsFrame};

pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub type LocalAcpHandler =
    Rc<dyn Fn(Compat<DuplexStream>, Compat<DuplexStream>) -> LocalBoxFuture<'static, ()>>;

pub struct WebServerConfig {
    pub host: String,
    pub port: u16,
    pub open_browser: bool,
    pub info_json: serde_json::Value,
}

/// Run the Web UI and WebSocket server until interrupted.
pub async fn start_web_server(
    config: WebServerConfig,
    acp_handler_factory: impl Fn() -> LocalAcpHandler + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&addr).await?;
    let local_addr = listener.local_addr()?;
    let is_wildcard = config.host == "0.0.0.0" || config.host == "::";
    let browser_url = if is_wildcard {
        format!("http://127.0.0.1:{}/", local_addr.port())
    } else {
        format!("http://{}:{}/", config.host, local_addr.port())
    };

    eprintln!("╔════════════════════════════════════════════════════════════╗");
    eprintln!("║       ⚡ One AI Coding Agent — React Web Interface        ║");
    eprintln!("╠════════════════════════════════════════════════════════════╣");
    if is_wildcard {
        eprintln!("║  Local URL:   {:<44} ║", browser_url);
        eprintln!(
            "║  Network:     All interfaces (0.0.0.0:{})             ║",
            local_addr.port()
        );
    } else {
        eprintln!("║  Web UI URL:  {:<44} ║", browser_url);
    }
    eprintln!("║  Protocol:    Agent Client Protocol v1 over WebSocket      ║");
    eprintln!("║  Frontend:    Vite + React (Embedded SPA)                  ║");
    eprintln!("║  Status:      Listening for connections...                 ║");
    eprintln!("╚════════════════════════════════════════════════════════════╝");

    if config.open_browser {
        open_browser_url(&browser_url);
    }

    let info_json = Arc::new(config.info_json);

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

                let info_clone = Arc::clone(&info_json);
                let acp_handler = acp_handler_factory();
                tokio::task::spawn_local(async move {
                    if let Err(err) = handle_connection(stream, peer, info_clone, acp_handler).await
                    {
                        tracing::debug!(peer = %peer, error = %err, "web connection closed");
                    }
                });
            }
        })
        .await
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
    _peer: SocketAddr,
    info_json: Arc<serde_json::Value>,
    acp_handler: LocalAcpHandler,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);

    let req_line = req.lines().next().unwrap_or_default();
    let mut parts = req_line.split_whitespace();
    let _method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    let is_websocket = req.lines().any(|l| {
        let low = l.to_ascii_lowercase();
        low.starts_with("upgrade:") && low.contains("websocket")
    });

    if is_websocket {
        // Find Sec-WebSocket-Key
        let sec_key = req
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("sec-websocket-key:"))
            .and_then(|line| line.split_once(':'))
            .map(|(_, val)| val.trim())
            .unwrap_or_default();

        if sec_key.is_empty() {
            let resp = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
            stream.write_all(resp.as_bytes()).await?;
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

        // Upgrade complete -> bridge WebSocket stream to ACP handler
        bridge_websocket_to_acp(stream, acp_handler).await?;
    } else if path == "/api/info" {
        let body = info_json.to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Access-Control-Allow-Origin: *\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).await?;
    } else if let Some(asset) = get_asset(path) {
        let body = asset.body();
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Cache-Control: public, max-age=3600\r\n\r\n{}",
            asset.content_type(),
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).await?;
    } else {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
        stream.write_all(resp.as_bytes()).await?;
    }

    Ok(())
}

enum OutgoingWsMsg {
    Text(String),
    Pong(Vec<u8>),
}

async fn bridge_websocket_to_acp(
    stream: TcpStream,
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
