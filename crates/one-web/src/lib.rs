//! One Web UI and WebSocket server.
//!
//! Provides an embedded HTTP static server for the Vite + React Single Page Application
//! and bridges WebSocket connections directly into Agent Client Protocol (ACP).

pub mod assets;
pub mod server;
pub mod sha1;
pub mod ws;

pub use server::{start_web_server, LocalAcpHandler, LocalBoxFuture, WebServerConfig};
