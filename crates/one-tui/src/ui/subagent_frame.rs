//! TV4 — Grok-style fullscreen framed child transcript.
//!
//! Replaces the parent chat/prompt while a subagent is open. Observational:
//! scroll and kill only; no child prompt. Close with `q` / Esc.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::float::{FloatKind, FloatMenu, FloatRenderRow};
use crate::theme::Theme;

use super::text::{pad_or_truncate, scrollbar_thumb_geometry};
use super::SPINNER;

pub(super) fn draw_subagent_frame(frame: &mut Frame<'_>, full: Rect, app: &App, menu: &FloatMenu) {
    debug_assert_eq!(menu.kind, FloatKind::SubagentDetail);

    frame.render_widget(Clear, full);
    frame.render_widget(Block::default().style(Theme::bg()), full);

    // Outer margin so the frame reads as a session overlay, not the whole TTY.
    let inset = Rect {
        x: full.x.saturating_add(1),
        y: full.y.saturating_add(0),
        width: full.width.saturating_sub(2),
        height: full.height.saturating_sub(1).max(1),
    };

    frame.render_widget(Block::default().style(Theme::slash_panel()), inset);

    let (status_icon, status_style) = frame_status_icon(menu, app.spinner_frame);
    let title_spans = frame_title_spans(menu, status_icon, status_style);
    let footer = " observational  ·  ↑/↓ scroll  ·  x kill  ·  q/Esc back ";

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Double)
        .border_style(Theme::subagent_frame_border())
        .style(Theme::slash_panel())
        .title(title_spans)
        .title_bottom(Span::styled(footer, Theme::float_footer()));

    let inner = block.inner(inset);
    frame.render_widget(block, inset);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status strip
            Constraint::Min(1),    // transcript
        ])
        .split(inner);

    let section = menu
        .sections
        .first()
        .map(|s| s.title.as_str())
        .unwrap_or("");
    let strip = pad_or_truncate(&format!(" {section}"), parts[0].width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            strip,
            Style::default()
                .bg(Theme::PANEL)
                .fg(Theme::MUTED)
                .add_modifier(Modifier::ITALIC),
        ))),
        parts[0],
    );

    draw_frame_log(frame, parts[1], menu);
}

fn frame_status_icon(menu: &FloatMenu, spinner_frame: usize) -> (&'static str, Style) {
    let section = menu
        .sections
        .first()
        .map(|s| s.title.to_ascii_lowercase())
        .unwrap_or_default();
    if section.contains("queued") {
        ("…", Theme::subagent_status_stop())
    } else if section.contains("running") {
        (
            SPINNER[spinner_frame % SPINNER.len()],
            Theme::subagent_status_run(),
        )
    } else if section.contains("timeout") {
        ("⏱", Theme::subagent_status_fail())
    } else if section.contains("fail") {
        ("✗", Theme::subagent_status_fail())
    } else if section.contains("abort") {
        ("■", Theme::subagent_status_stop())
    } else if section.contains("done") {
        ("✓", Theme::subagent_status_ok())
    } else {
        ("▸", Theme::subagent_title())
    }
}

fn frame_title_spans<'a>(menu: &'a FloatMenu, icon: &'a str, icon_style: Style) -> Line<'a> {
    let desc = menu.title.trim().trim_start_matches(['▸', '◆', ' ']);
    Line::from(vec![
        Span::styled(format!(" {icon} "), icon_style),
        Span::styled(
            format!("{desc} "),
            Theme::subagent_title().add_modifier(Modifier::BOLD),
        ),
        Span::styled("[q] ", Theme::float_footer()),
    ])
}

fn draw_frame_log(frame: &mut Frame<'_>, area: Rect, menu: &FloatMenu) {
    let render_rows = menu.render_rows();
    let max_rows = area.height as usize;
    if max_rows == 0 {
        return;
    }
    let total_rows = render_rows.len();
    let selected_row = render_rows
        .iter()
        .position(|r| match r {
            FloatRenderRow::Item { entry_index, .. } => *entry_index == menu.selected,
            _ => false,
        })
        .unwrap_or(0);
    let start = selected_row.saturating_sub(max_rows.saturating_sub(1));
    let end = (start + max_rows).min(total_rows);
    let need_scrollbar = total_rows > max_rows;

    let sb_w: u16 = if need_scrollbar { 1 } else { 0 };
    let content_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width.saturating_sub(sb_w),
        height: area.height,
    };

    let col_w = content_area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for row in &render_rows[start..end] {
        match row {
            FloatRenderRow::Header(title) => {
                let w = title.width();
                let rule = "─".repeat(col_w.saturating_sub(w.saturating_add(1)).max(1));
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{title} "),
                        Style::default()
                            .bg(Theme::PANEL)
                            .fg(Theme::MUTED)
                            .add_modifier(Modifier::ITALIC),
                    ),
                    Span::styled(rule, Theme::hairline()),
                ]));
            }
            FloatRenderRow::Item { label, detail, .. } => {
                lines.push(frame_log_line(label, detail, col_w));
            }
        }
    }
    frame.render_widget(
        Paragraph::new(lines).style(Theme::slash_panel()),
        content_area,
    );

    if need_scrollbar {
        let sb_area = Rect {
            x: area.x + content_area.width,
            y: area.y,
            width: 1,
            height: area.height,
        };
        let (thumb_start, thumb_h) =
            scrollbar_thumb_geometry(total_rows, max_rows, start, sb_area.height as usize);
        let mut sb_lines = Vec::with_capacity(sb_area.height as usize);
        for i in 0..sb_area.height as usize {
            let ch = if i >= thumb_start && i < thumb_start + thumb_h {
                "█"
            } else {
                "│"
            };
            sb_lines.push(Line::from(Span::styled(ch, Theme::hairline())));
        }
        frame.render_widget(Paragraph::new(sb_lines), sb_area);
    }
}

fn frame_log_line(label: &str, detail: &str, col_w: usize) -> Line<'static> {
    let style = match label {
        "→" => Theme::subagent_log_tool(),
        "✓" => Theme::subagent_log_ok(),
        "✗" | "!" => Theme::subagent_log_err(),
        "▸" | "◂" => Theme::subagent_title(),
        "──" => Style::default()
            .bg(Theme::PANEL)
            .fg(Theme::MUTED)
            .add_modifier(Modifier::ITALIC),
        _ => Theme::slash_desc(),
    };
    let text = if label.is_empty() {
        detail.to_string()
    } else {
        format!("{label} {detail}")
    };
    Line::from(Span::styled(pad_or_truncate(&text, col_w.max(1)), style))
}
