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
    };

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

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

    let config = WebServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        open_browser: false,
        info_json: serde_json::json!({
            "name": "one",
            "version": "0.1.0",
        }),
    };

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            tokio::select! {
                _ = start_web_server(config, || {
                    let handler: LocalAcpHandler = Rc::new(|mut incoming, mut outgoing| {
                        Box::pin(async move {
                            use futures::{AsyncBufReadExt, AsyncWriteExt};
                            let mut lines = futures::io::BufReader::new(&mut incoming).lines();
                            while let Some(Ok(line)) = lines.next().await {
                                eprintln!("Handler received line: {}", line);
                                let resp = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"echo\":true}}}}\n");
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
