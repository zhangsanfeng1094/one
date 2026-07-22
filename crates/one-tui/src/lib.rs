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
//! - **Scroll + copy**: mouse wheel scrolls chat (capture on). Drag selects
//!   text (character-level) in-app and copies via **OSC 52** (lazygit/Claude Code pattern).
//!   `Ctrl+Shift+C` / `y` also copy; `Ctrl+Shift+M` toggles mouse;
//!   `ONE_MOUSE=0` starts with mouse off.
//! - **Tools**: collapsed multi-tool groups, edit/write diffs, click or Ctrl+O to expand
//!   (TUI previews only — full results stay in agent context)
//! - **one-cli** only feeds agent events into [`App`]; all drawing is in this crate

pub mod app;
pub mod clipboard;
pub mod error;
pub mod float;
pub mod markdown;
pub mod message;
pub mod select;
pub mod settings;
pub mod slash;
pub mod state;
pub mod terminal;
pub mod theme;
pub mod tool_view;
pub mod ui;

pub use app::{expand_at_files, App, InteractiveApp};
pub use crate::state::{
    ApprovalAnswer, ApprovalPrompt, ConfigOp, ModelDraft, PendingImage, PendingText, RunOutcome,
    SelectKind, SelectPos, Toast,
};
pub use error::Result;
pub use float::{FloatItem, FloatKind, FloatMenu, FloatSection};
pub use message::{AlertLevel, Message, MessageRole, ToolStatus};
pub use select::{SelectMode, SelectOption, SelectPhase, SelectPrompt, SelectResult};
pub use slash::{ModelChoice, PopupKind, PopupRow, SlashCommand, SLASH_COMMANDS};
pub use terminal::{ForceQuit, TerminalSession};

pub use crossterm;
pub use ratatui;
