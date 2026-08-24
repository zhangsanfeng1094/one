//! OpenCode-faithful chat chrome (dark `opencode` theme).
//!
//! - User: peach rail + warm elevated bubble (bold) — strongest content signal
//! - Chat focus (j/k): blue rail + neutral wash — separate from user peach
//! - Assistant: markdown body (headings, lists, code, tables), turn footer
//! - Tool: `⚙ name detail` inline row (running / muted / error)
//! - Prompt: left-border only + agent/model meta strip
//! - one-cli only feeds state; all paint is here
//!
//! Split by paint surface: [`chat`], [`prompt`], [`status`], [`dock`],
//! [`float_menu`], [`toast`], plus shared [`text`] helpers.

mod chat;
mod dock;
mod float_menu;
mod header;
mod prompt;
mod status;
mod subagent_frame;
pub(crate) mod text;
mod toast;

#[cfg(test)]
mod tests;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::Block;
use ratatui::Frame;

use crate::app::App;
use crate::float::FloatKind;
use crate::theme::Theme;

use chat::draw_chat;
use dock::{draw_select_dock, draw_slash_dock};
use float_menu::draw_float_menu;
use header::draw_header;
use prompt::draw_prompt;
use status::draw_status;
use subagent_frame::draw_subagent_frame;
use toast::draw_toast;

pub(crate) const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    app.tick_toast();
    // TV4: a framed child transcript replaces the parent view (Grok overlay).
    if app
        .float
        .as_ref()
        .is_some_and(|m| m.kind == FloatKind::SubagentDetail)
    {
        if let Some(menu) = app.float.as_ref() {
            draw_subagent_frame(frame, frame.area(), app, menu);
        }
        draw_toast(frame, frame.area(), app);
        return;
    }

    // Clear to OpenCode near-black.
    frame.render_widget(Block::default().style(Theme::bg()), frame.area());

    // Dock above the prompt (priority: HITL select > `/` command menu).
    // Centered float remains for Settings (Ctrl+G) and sessions/tree/etc.
    let input_lines = app.input_line_count() as u16;
    let prompt_h = (input_lines + 2).clamp(3, 8).saturating_add(1); // box + meta
    let select_h = app.select_dock_height();
    let slash_h = if select_h == 0 {
        app.slash_dock_height()
    } else {
        0
    };
    let dock_h = select_h.max(slash_h);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),        // grok-build top header (path + context)
            Constraint::Min(3),           // transcript
            Constraint::Length(dock_h),   // select or `/` menu (0 when closed)
            Constraint::Length(prompt_h), // prompt box + agent meta
            Constraint::Length(1),        // footer
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], app);
    draw_chat(frame, chunks[1], app);
    if select_h > 0 {
        draw_select_dock(frame, chunks[2], app);
    } else if slash_h > 0 {
        draw_slash_dock(frame, chunks[2], app);
    }
    draw_prompt(frame, chunks[3], app);
    draw_status(frame, chunks[4], app);

    // Top-right toast sits above chat (not the footer).
    draw_toast(frame, frame.area(), app);

    // Floating modal on top (Settings, sessions, …) — not used for `/`.
    if let Some(menu) = &app.float {
        draw_float_menu(frame, frame.area(), menu);
    }
}
