//! One Web UI and WebSocket server.
//!
//! Provides an embedded HTTP static server for the Vite + React Single Page Application
//! and bridges WebSocket connections directly into Agent Client Protocol (ACP).

pub mod assets;
pub mod http;
pub mod server;
pub mod ws;

pub use http::{HttpRequest, HttpResponse};
pub use server::{
    start_web_server, ApiFuture, ApiHandler, LocalAcpHandler, LocalBoxFuture, RequestGuard,
    WebServerConfig,
};
