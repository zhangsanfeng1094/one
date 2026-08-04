//! Ephemeral top-right toast overlay.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::App;
use crate::message::AlertLevel;
use crate::theme::Theme;

use super::text::{display_width, wrap_str};

/// Ephemeral top-right toast — UI only, never agent context.
pub(super) fn draw_toast(frame: &mut Frame<'_>, full: Rect, app: &App) {
    let Some(toast) = app.toast_active() else {
        return;
    };
    let text = toast.text.trim();
    if text.is_empty() {
        return;
    }

    let max_w = (full.width.saturating_mul(2) / 5).clamp(24, 56);
    let content_w = display_width(text).min(max_w as usize - 4).max(8);
    let width = (content_w + 4).min(full.width.saturating_sub(2) as usize) as u16;
    // Wrap to at most 3 lines.
    let wrapped = wrap_str(text, content_w);
    let lines: Vec<String> = wrapped.into_iter().take(3).collect();
    let height = (lines.len() as u16).saturating_add(2).min(5);

    let x = full.x + full.width.saturating_sub(width).saturating_sub(1);
    let y = full.y.saturating_add(1);
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    // Info stays quiet (dim label) — errors/warns keep a strong chip.
    // Low-frequency mouse/copy tips must not outshine the prompt.
    let (border_fg, title_span) = match toast.level {
        AlertLevel::Error => (
            Theme::ERROR,
            Span::styled(" error ", Style::default().fg(Theme::BG).bg(Theme::ERROR)),
        ),
        AlertLevel::Warn => (
            Theme::WARNING,
            Span::styled(" warn ", Style::default().fg(Theme::BG).bg(Theme::WARNING)),
        ),
        AlertLevel::Info => (
            Theme::BORDER,
            Span::styled(" i ", Style::default().fg(Theme::MUTED).bg(Theme::PANEL)),
        ),
    };

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(border_fg).bg(Theme::PANEL))
        .style(Style::default().bg(Theme::PANEL))
        .title(title_span);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let body_fg = match toast.level {
        AlertLevel::Info => Theme::MUTED,
        _ => Theme::FG,
    };
    let body: Vec<Line> = lines
        .into_iter()
        .map(|l| {
            Line::from(Span::styled(
                format!(" {l}"),
                Style::default().fg(body_fg).bg(Theme::PANEL),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(body).style(Theme::bg()), inner);
}
