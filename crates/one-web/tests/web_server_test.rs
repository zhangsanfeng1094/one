use futures::StreamExt;
use one_web::{start_web_server, LocalAcpHandler, WebServerConfig};
use std::rc::Rc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn test_web_server_assets_and_api() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // `WebServerConfig` holds non-`Send` handlers, so it is built on the
        // server's own thread rather than moved across the boundary.
        let config = WebServerConfig {
            host: "127.0.0.1".to_string(),
            port,
            open_browser: false,
            info_json: serde_json::json!({
                "name": "one",
                "version": "0.1.0",
                "protocol": "ACP v1",
                "cwd": "/test/cwd",
            }),
            ..Default::default()
        };

        rt.block_on(async move {
            tokio::select! {
                _ = start_web_server(config, || {
                    let handler: LocalAcpHandler = Rc::new(|_in, _out| {
                        Box::pin(async move {})
                    });
                    handler
                }) => {},
                _ = &mut shutdown_rx => {},
            }
        });
    });

    // Wait a brief moment for server to bind
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    // 1. Test GET /
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    let resp = String::from_utf8_lossy(&buf[..n]);
    assert!(resp.starts_with("HTTP/1.1 200 OK"));
    assert!(resp.contains("Content-Type: text/html"));
    assert!(resp.contains("<div id=\"root\"></div>"));

    // 2. Test GET /assets/app.js
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    stream
        .write_all(b"GET /assets/app.js HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    let resp = String::from_utf8_lossy(&buf[..n]);
    assert!(resp.starts_with("HTTP/1.1 200 OK"));
    assert!(resp.contains("Content-Type: application/javascript"));

    // 3. Test GET /assets/index.css
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    stream
        .write_all(b"GET /assets/index.css HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    let resp = String::from_utf8_lossy(&buf[..n]);
    assert!(resp.starts_with("HTTP/1.1 200 OK"));
    assert!(resp.contains("Content-Type: text/css"));

    // 4. Test GET /api/info
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    stream
        .write_all(b"GET /api/info HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    let resp = String::from_utf8_lossy(&buf[..n]);
    assert!(resp.starts_with("HTTP/1.1 200 OK"));
    assert!(resp.contains("application/json"));
    assert!(resp.contains("\"protocol\":\"ACP v1\""));

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn test_web_server_websocket_rpc_end_to_end() {
    use one_web::ws::{read_ws_frame, write_ws_text, WsFrame};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let config = WebServerConfig {
            host: "127.0.0.1".to_string(),
            port,
            open_browser: false,
            info_json: serde_json::json!({
                "name": "one",
                "version": "0.1.0",
            }),
            ..Default::default()
        };

        rt.block_on(async move {
            tokio::select! {
                _ = start_web_server(config, || {
                    let handler: LocalAcpHandler = Rc::new(|mut incoming, mut outgoing| {
                        Box::pin(async move {
                            use futures::{AsyncBufReadExt, AsyncWriteExt};
                            let mut lines = futures::io::BufReader::new(&mut incoming).lines();
                            while let Some(Ok(line)) = lines.next().await {
                                eprintln!("Handler received line: {}", line);
                                let resp = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"echo\":true}}\n";
                                let _ = outgoing.write_all(resp.as_bytes()).await;
                                let _ = outgoing.flush().await;
                            }
                        })
                    });
                    handler
                }) => {},
                _ = &mut shutdown_rx => {},
            }
        });
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    // Connect and upgrade to WebSocket
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let upgrade_req = format!(
        "GET /ws HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(upgrade_req.as_bytes()).await.unwrap();

    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    let resp = String::from_utf8_lossy(&buf[..n]);
    assert!(resp.starts_with("HTTP/1.1 101 Switching Protocols"));
    assert!(resp.contains("Upgrade: websocket"));

    // Send RPC message via WebSocket frame
    let client_msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "test",
        "params": {}
    });
    write_ws_text(&mut stream, &client_msg.to_string())
        .await
        .unwrap();

    // Read response WebSocket frame
    let frame = read_ws_frame(&mut stream).await.unwrap();
    match frame {
        WsFrame::Text(text) => {
            eprintln!("Received WS text frame: {}", text);
            assert!(text.contains("\"echo\":true"));
        }
        other => panic!("Unexpected frame: {:?}", other),
    }

    let _ = shutdown_tx.send(());
}

/// The Config Studio depends on three server-level behaviours: a pluggable API
/// handler that sees request bodies and can short-circuit, a guard that runs
/// before routing, and the SPA fallback that serves `/config`.
#[tokio::test]
async fn test_pluggable_api_guard_and_spa_fallback() {
    use one_web::{HttpRequest, HttpResponse};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let config = WebServerConfig {
            host: "127.0.0.1".to_string(),
            port,
            open_browser: false,
            info_json: serde_json::json!({
                "name": "one",
                "version": "0.1.0",
                "mode": "config",
            }),
            // Same-origin only, like the studio.
            cors_allow_origin: None,
            guard: Some(Rc::new(|req: &HttpRequest| {
                if req.header("x-one-config-token") != Some("secret") {
                    return Err(HttpResponse::error(401, "missing token"));
                }
                Ok(())
            })),
            api: Some(Rc::new(|req: HttpRequest| {
                Box::pin(async move {
                    match req.path.as_str() {
                        // Proves the body was read for a POST.
                        "/api/config/echo" => Some(HttpResponse::json(
                            200,
                            &serde_json::json!({ "body": String::from_utf8_lossy(&req.body) }),
                        )),
                        // Short-circuits the built-in `/api/info`.
                        "/api/info" => Some(HttpResponse::json(
                            200,
                            &serde_json::json!({ "from": "handler" }),
                        )),
                        _ => None,
                    }
                })
            })),
            ..Default::default()
        };

        rt.block_on(async move {
            tokio::select! {
                _ = start_web_server(config, || {
                    let handler: LocalAcpHandler =
                        Rc::new(|_in, _out| Box::pin(async move {}));
                    handler
                }) => {},
                _ = &mut shutdown_rx => {},
            }
        });
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    async fn send(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    // 1. The guard rejects an unauthenticated request before routing.
    let resp = send(port, "GET /api/info HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
    assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
    assert!(resp.contains("missing token"), "{resp}");

    // 2. The API handler wins over the built-in `/api/info`.
    let resp = send(
        port,
        "GET /api/info HTTP/1.1\r\nHost: 127.0.0.1\r\nX-One-Config-Token: secret\r\n\r\n",
    )
    .await;
    assert!(resp.contains("\"from\":\"handler\""), "{resp}");

    // 3. POST bodies reach the handler intact.
    let body = r#"{"draft":"{\"a\":1}"}"#;
    let resp = send(
        port,
        &format!(
            "POST /api/config/echo HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             X-One-Config-Token: secret\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{}",
            body.len(),
            body
        ),
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("draft"), "{resp}");

    // 4. The SPA fallback serves the shell for `/config` (client-side routing).
    //    This test guard protects every path; the studio's own guard exempts
    //    static assets, which is covered by its unit tests.
    let resp = send(
        port,
        "GET /config HTTP/1.1\r\nHost: 127.0.0.1\r\nX-One-Config-Token: secret\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("text/html"), "{resp}");
    assert!(resp.contains("<div id=\"root\"></div>"), "{resp}");

    // 5. CORS stays off: the config API must not be reachable cross-origin.
    assert!(!resp.contains("Access-Control-Allow-Origin"), "{resp}");

    // 6. Static assets are not swallowed by the API handler.
    let resp = send(
        port,
        "GET /assets/app.js HTTP/1.1\r\nHost: 127.0.0.1\r\nX-One-Config-Token: secret\r\n\r\n",
    )
    .await;
    assert!(resp.contains("application/javascript"), "{resp}");

    let _ = shutdown_tx.send(());
}
