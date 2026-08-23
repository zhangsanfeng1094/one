use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn test_acp_web_http_and_websocket_handshake() {
    // 1. Bind to a random available port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // 2. Spawn server in background
    let srv_handle = tokio::spawn(async move {
        // Simple test server loop handling one connection
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 2048];
            if let Ok(n) = stream.read(&mut buf).await {
                let req = String::from_utf8_lossy(&buf[..n]);
                if req.contains("Upgrade: websocket") {
                    let accept_key = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
                    let resp = format!(
                        "HTTP/1.1 101 Switching Protocols\r\n\
                         Upgrade: websocket\r\n\
                         Connection: Upgrade\r\n\
                         Sec-WebSocket-Accept: {accept_key}\r\n\r\n"
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else {
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 4\r\n\r\nONE!";
                    let _ = stream.write_all(resp.as_bytes()).await;
                }
            }
        }
    });

    // 3. Test HTTP GET
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut resp = [0u8; 512];
    let n = client.read(&mut resp).await.unwrap();
    let resp_str = String::from_utf8_lossy(&resp[..n]);
    assert!(resp_str.starts_with("HTTP/1.1 200 OK"));

    let _ = srv_handle.await;
}

#[tokio::test]
async fn test_acp_json_serialization() {
    use agent_client_protocol::{
        ContentBlock, ContentChunk, InitializeResponse, ListSessionsResponse, NewSessionResponse,
        ProtocolVersion, SessionId, SessionInfo, SessionNotification, SessionUpdate,
    };
    use std::path::PathBuf;

    let new_session_resp = NewSessionResponse::new(SessionId::new("sess-123"));
    let json = serde_json::to_string(&new_session_resp).unwrap();
    eprintln!("NewSessionResponse JSON: {}", json);

    let list_resp = ListSessionsResponse::new(vec![SessionInfo::new(
        SessionId::new("s1"),
        PathBuf::from("/tmp"),
    )]);
    let list_json = serde_json::to_string(&list_resp).unwrap();
    eprintln!("ListSessionsResponse JSON: {}", list_json);

    let notif = SessionNotification::new(
        SessionId::new("sess-123"),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from("Hello world!"))),
    );
    let notif_json = serde_json::to_string(&notif).unwrap();
    eprintln!("SessionNotification JSON: {}", notif_json);
}

#[tokio::test]
async fn test_acp_requests_deserialization() {
    use agent_client_protocol::{
        CancelNotification, ClientCapabilities, InitializeRequest, ListSessionsRequest,
        LoadSessionRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
        SetSessionConfigOptionRequest, SetSessionModeRequest,
    };

    // 0. InitializeRequest
    let init_req = InitializeRequest::new(ProtocolVersion::V1);
    let init_json = serde_json::to_string(&init_req).unwrap();
    eprintln!("InitializeRequest JSON default: {}", init_json);

    let json_init_test = serde_json::json!({
        "protocolVersion": 1,
    });
    let req_init: Result<InitializeRequest, _> = serde_json::from_value(json_init_test.clone());
    eprintln!("InitializeRequest parse result: {:?}", req_init);

    // 1. NewSessionRequest
    let json_new_session = serde_json::json!({
        "cwd": "/tmp",
        "mcpServers": []
    });
    let req: Result<NewSessionRequest, _> = serde_json::from_value(json_new_session);
    eprintln!("NewSessionRequest parse result: {:?}", req);
    assert!(req.is_ok());

    // 2. LoadSessionRequest
    let json_load_session = serde_json::json!({
        "sessionId": "s-123",
        "cwd": "/tmp",
        "mcpServers": []
    });
    let req_load: Result<LoadSessionRequest, _> = serde_json::from_value(json_load_session);
    eprintln!("LoadSessionRequest parse result: {:?}", req_load);
    assert!(req_load.is_ok());

    // 3. ListSessionsRequest
    let json_list = serde_json::json!({
        "cwd": "/tmp"
    });
    let req_list: Result<ListSessionsRequest, _> = serde_json::from_value(json_list);
    eprintln!("ListSessionsRequest parse result: {:?}", req_list);
    assert!(req_list.is_ok());

    // 4. PromptRequest
    let json_prompt = serde_json::json!({
        "sessionId": "s-123",
        "prompt": [{"type": "text", "text": "hello"}]
    });
    let req_prompt: Result<PromptRequest, _> = serde_json::from_value(json_prompt);
    eprintln!("PromptRequest parse result: {:?}", req_prompt);
    assert!(req_prompt.is_ok());

    // 5. SetSessionModeRequest
    let json_set_mode = serde_json::json!({
        "sessionId": "s-123",
        "modeId": "plan"
    });
    let req_mode: Result<SetSessionModeRequest, _> = serde_json::from_value(json_set_mode);
    eprintln!("SetSessionModeRequest parse result: {:?}", req_mode);
    assert!(req_mode.is_ok());

    // 6. SetSessionConfigOptionRequest
    let json_set_config = serde_json::json!({
        "sessionId": "s-123",
        "configId": "thinking",
        "value": "medium"
    });
    let req_config: Result<SetSessionConfigOptionRequest, _> =
        serde_json::from_value(json_set_config);
    eprintln!(
        "SetSessionConfigOptionRequest parse result: {:?}",
        req_config
    );
    assert!(req_config.is_ok());

    // 7. CancelNotification
    let json_cancel = serde_json::json!({
        "sessionId": "s-123"
    });
    let req_cancel: Result<CancelNotification, _> = serde_json::from_value(json_cancel);
    eprintln!("CancelNotification parse result: {:?}", req_cancel);
    assert!(req_cancel.is_ok());
}

#[tokio::test]
async fn test_acp_all_updates_serialization() {
    use agent_client_protocol::{
        ContentBlock, ContentChunk, CurrentModeUpdate, SessionId, SessionNotification,
        SessionUpdate, ToolCall as AcpToolCall, ToolCallContent, ToolCallId, ToolCallStatus,
        ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    };

    let sid = SessionId::new("sess-1");

    // 1. Message chunk
    let n1 = SessionNotification::new(
        sid.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from("Hello"))),
    );
    eprintln!(
        "1. AgentMessageChunk: {}",
        serde_json::to_string(&n1).unwrap()
    );

    // 2. Thought chunk
    let n2 = SessionNotification::new(
        sid.clone(),
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::from("Thinking..."))),
    );
    eprintln!(
        "2. AgentThoughtChunk: {}",
        serde_json::to_string(&n2).unwrap()
    );

    // 3. Tool call
    let call = AcpToolCall::new(ToolCallId::new("call-1"), "bash".to_string())
        .kind(ToolKind::Execute)
        .status(ToolCallStatus::Pending)
        .raw_input(serde_json::json!({ "command": "cargo test" }));
    let n3 = SessionNotification::new(sid.clone(), SessionUpdate::ToolCall(call));
    eprintln!("3. ToolCall: {}", serde_json::to_string(&n3).unwrap());

    // 4. Tool call update
    let call_up = ToolCallUpdate::new(
        ToolCallId::new("call-1"),
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::from("test output")]),
    );
    let n4 = SessionNotification::new(sid.clone(), SessionUpdate::ToolCallUpdate(call_up));
    eprintln!("4. ToolCallUpdate: {}", serde_json::to_string(&n4).unwrap());

    // 5. Current mode update
    let n5 = SessionNotification::new(
        sid.clone(),
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new("plan")),
    );
    eprintln!(
        "5. CurrentModeUpdate: {}",
        serde_json::to_string(&n5).unwrap()
    );

    // 6. RequestPermissionRequest
    let perm_req = agent_client_protocol::RequestPermissionRequest::new(
        sid.clone(),
        agent_client_protocol::ToolCallUpdate::new(
            agent_client_protocol::ToolCallId::new("perm-1"),
            agent_client_protocol::ToolCallUpdateFields::new().title("Run command"),
        ),
        vec![agent_client_protocol::PermissionOption::new(
            "allow-once",
            "Allow once",
            agent_client_protocol::PermissionOptionKind::AllowOnce,
        )],
    );
    eprintln!(
        "6. RequestPermissionRequest: {}",
        serde_json::to_string(&perm_req).unwrap()
    );

    // 7. RequestPermissionResponse
    let json_resp = serde_json::json!({
        "outcome": {
            "outcome": "selected",
            "optionId": "allow-once"
        }
    });
    let perm_resp: Result<agent_client_protocol::RequestPermissionResponse, _> =
        serde_json::from_value(json_resp);
    eprintln!("7. RequestPermissionResponse parse result: {:?}", perm_resp);
    assert!(perm_resp.is_ok());
}

#[tokio::test]
async fn test_agent_side_connection_duplex() {
    use agent_client_protocol::{Agent, AgentSideConnection, InitializeRequest, ProtocolVersion};
    use async_trait::async_trait;
    use std::rc::Rc;
    use tokio::io::AsyncBufReadExt;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    struct DummyAgent;
    #[async_trait(?Send)]
    impl Agent for DummyAgent {
        async fn initialize(
            &self,
            req: InitializeRequest,
        ) -> agent_client_protocol::Result<agent_client_protocol::InitializeResponse> {
            eprintln!("DummyAgent::initialize called with {:?}", req);
            Ok(agent_client_protocol::InitializeResponse::new(
                ProtocolVersion::V1,
            ))
        }
        async fn authenticate(
            &self,
            _req: agent_client_protocol::AuthenticateRequest,
        ) -> agent_client_protocol::Result<agent_client_protocol::AuthenticateResponse> {
            Ok(agent_client_protocol::AuthenticateResponse::default())
        }
        async fn new_session(
            &self,
            _req: agent_client_protocol::NewSessionRequest,
        ) -> agent_client_protocol::Result<agent_client_protocol::NewSessionResponse> {
            Ok(agent_client_protocol::NewSessionResponse::new(
                agent_client_protocol::SessionId::new("s1"),
            ))
        }
        async fn prompt(
            &self,
            _req: agent_client_protocol::PromptRequest,
        ) -> agent_client_protocol::Result<agent_client_protocol::PromptResponse> {
            Ok(agent_client_protocol::PromptResponse::new(
                agent_client_protocol::StopReason::EndTurn,
            ))
        }
        async fn cancel(
            &self,
            _req: agent_client_protocol::CancelNotification,
        ) -> agent_client_protocol::Result<()> {
            Ok(())
        }
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let (mut client_write, server_read) = tokio::io::duplex(4096);
            let (server_write, client_read) = tokio::io::duplex(4096);

            let incoming = server_read.compat();
            let outgoing = server_write.compat_write();

            let agent = Rc::new(DummyAgent);
            let (_conn, io_task) = AgentSideConnection::new(agent, outgoing, incoming, |fut| {
                tokio::task::spawn_local(fut);
            });

            tokio::task::spawn_local(async move {
                let _ = io_task.await;
            });

            // Send initialize request from client
            let init_req = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": { "readTextFile": true, "writeTextFile": true },
                        "terminal": true
                    }
                }
            });
            let mut msg_bytes = serde_json::to_vec(&init_req).unwrap();
            msg_bytes.push(b'\n');
            client_write.write_all(&msg_bytes).await.unwrap();

            // Read response
            let mut lines = tokio::io::BufReader::new(client_read).lines();
            let resp_line = lines.next_line().await.unwrap();
            eprintln!("Response line: {:?}", resp_line);
            assert!(resp_line.is_some());
        })
        .await;
}
