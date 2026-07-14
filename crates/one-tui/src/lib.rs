//! Interactive terminal UI for One, built on [Ratatui](https://ratatui.rs/).
//!
//! Visual language follows OpenCode-style soft chrome:
//! user turns get a left accent rail (no `you>` tags), assistant is plain body text,
//! tools render as compact `◇ name · args` rows.
//!
//! Architecture:
//! - **Persistent** [`TerminalSession`]: alternate screen + mouse capture (hides shell scrollback)
//! - **Structured** [`Message`] roles with soft, role-specific paint (not label soup)
//! - **Streaming** via [`TerminalSession::run_busy`] (token paint + blink caret)
//! - **Scroll**: line-window transcript + PgUp/PgDn / ↑↓ / Home/End / mouse wheel
//!   (Shift+drag still selects text in most emulators)
//! - **Tools**: collapsed multi-tool groups, edit/write diffs, click or Ctrl+O to expand
//!   (TUI previews only — full results stay in agent context)
//! - **one-cli** only feeds agent events into [`App`]; all drawing is in this crate

pub mod app;
pub mod error;
pub mod float;
pub mod markdown;
pub mod message;
pub mod slash;
pub mod terminal;
pub mod theme;
pub mod tool_view;
pub mod ui;

pub use app::{App, InteractiveApp, RunOutcome, Toast};
pub use error::Result;
pub use float::{FloatItem, FloatKind, FloatMenu, FloatSection};
pub use message::{AlertLevel, Message, MessageRole, ToolStatus};
pub use slash::{ModelChoice, PopupKind, PopupRow, SlashCommand, SLASH_COMMANDS};
pub use terminal::TerminalSession;

pub use crossterm;
pub use ratatui;
