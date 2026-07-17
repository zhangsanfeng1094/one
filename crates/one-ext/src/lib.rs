//! One Rust-native extension runtime (Codex-inspired).
//!
//! # Layers
//!
//! - **Extension trait** — tools, context, lifecycle, Pre/Post tool intercept
//! - **Registry** — install-time collection of extensions
//! - **Runtime** — dispatch + `ExtensionData` + external script hooks
//! - **Plugins** — `plugin.json` discovery (skills / hooks / MCP snippets)
//! - **Hooks** — subprocess PreToolUse / PostToolUse (JSON stdin/stdout)
//!
//! Core stays free of this crate; `one-cli` bridges via
//! [`ExtensionRuntime::agent_hooks`] and [`ExtensionRuntime::tool_gate`].

pub mod builtin;
pub mod data;
pub mod error;
pub mod events;
pub mod hooks;
pub mod loader;
pub mod plugin;
pub mod registry;
pub mod runtime;
pub mod traits;

#[cfg(feature = "dylib")]
pub mod dylib;

pub use data::ExtensionData;
pub use error::{ExtError, Result};
pub use events::{
    ExtensionCommand, ExtensionContext, ExtensionEvent, PreToolDecision, PromptFragment,
};
pub use hooks::{HookHandler, HooksConfig};
pub use loader::{discover_all, discover_extensions, DiscoveryResult};
pub use plugin::{discover_plugins, DiscoveredPlugin, PluginManifest};
pub use registry::{ExtensionRegistry, ExtensionRegistryBuilder};
pub use runtime::ExtensionRuntime;
pub use traits::Extension;
