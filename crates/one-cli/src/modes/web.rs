//! Web UI Mode for One (ACP over WebSocket with Vite+React frontend).

use std::rc::Rc;

use agent_client_protocol::AgentSideConnection;
use one_web::{start_web_server, LocalAcpHandler, WebServerConfig};

use crate::cli::Cli;
use crate::modes::acp::OneAcpAgent;

/// Run the Web UI and WebSocket server.
pub async fn run_web_server(
    cli: Cli,
    host: &str,
    port: u16,
    open_browser: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd_str = cli.cwd.to_string_lossy().to_string();
    let info_json = serde_json::json!({
        "name": "one",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "ACP v1",
        "cwd": cwd_str,
    });

    let config = WebServerConfig {
        host: host.to_string(),
        port,
        open_browser,
        info_json,
    };

    let cli_for_factory = cli.clone();
    start_web_server(config, move || {
        let cli_inner = cli_for_factory.clone();
        let handler: LocalAcpHandler = Rc::new(move |incoming, outgoing| {
            let cli_clone = cli_inner.clone();
            Box::pin(async move {
                let agent = Rc::new(OneAcpAgent::new(cli_clone));
                let (conn, io_task) =
                    AgentSideConnection::new(agent.clone(), outgoing, incoming, |fut| {
                        tokio::task::spawn_local(fut);
                    });
                agent.set_client(Rc::new(conn));

                if let Err(err) = io_task.await {
                    tracing::debug!(error = %err, "acp io task ended");
                }
                agent.shutdown().await;
            })
        });
        handler
    })
    .await
}
