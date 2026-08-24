//! Grok-build style Top Header Strip with elevated chrome and visual polish.
//!
//! - Left:  live status dot (`●`), workspace folder icon (`📁`), path with bold repo name
//! - Right: elevated context pill (`⚡ 32k / 128k ▪▪▫▫▫▫ 25%`) with dynamic watermark coloring

use std::path::{Path, PathBuf};

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;
use crate::ui::text::display_width;

use super::status::format_tokens;

/// Draw the top header bar across the full terminal width with elevated background.
pub(super) fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Fill the background of the entire header area with elevated PANEL style.
    frame.render_widget(Block::default().style(Theme::top_bar_bg()), area);

    let left = header_left_spans(app);
    let right = header_right_spans(app, area.width);

    render_header_split(frame, area, left, right);
}

/// Render header row with left-aligned workspace info and right-aligned context pill.
fn render_header_split(
    frame: &mut Frame<'_>,
    area: Rect,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
) {
    if right.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(left)).style(Theme::top_bar_bg()),
            area,
        );
        return;
    }

    let right_text: String = right.iter().map(|s| s.content.as_ref()).collect();
    let right_cols = display_width(&right_text) as u16;
    let max_right = area.width.saturating_sub(14);
    let right_w = right_cols.min(max_right).max(1);

    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(8), Constraint::Length(right_w)])
        .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(left)).style(Theme::top_bar_bg()),
        row[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(right))
            .alignment(Alignment::Right)
            .style(Theme::top_bar_bg()),
        row[1],
    );
}

/// Format the left side: status dot + project icon + parent path (muted) + project name (bold).
pub(super) fn header_left_spans(app: &App) -> Vec<Span<'static>> {
    let cwd_path = app
        .history_cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let cwd_str = cwd_path.to_string_lossy().to_string();
    let home = std::env::var("HOME").unwrap_or_default();

    let normalized = if !home.is_empty() && cwd_str.starts_with(&home) {
        let rest = &cwd_str[home.len()..];
        if rest.starts_with('/') {
            format!("~{rest}")
        } else if rest.is_empty() {
            "~".to_string()
        } else {
            format!("~/{rest}")
        }
    } else {
        cwd_str
    };

    let mut spans = vec![Span::raw(" ")];

    // Status indicator dot
    spans.push(Span::styled("● ", Theme::top_bar_status(app.busy)));

    // Project folder icon
    spans.push(Span::styled("📁 ", Theme::top_bar()));

    let path_obj = Path::new(&normalized);
    let file_name = path_obj
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| {
            if normalized == "/" {
                "/"
            } else if normalized == "~" {
                "~"
            } else {
                normalized.as_str()
            }
        });

    let parent = path_obj.parent().and_then(|p| p.to_str()).unwrap_or("");

    if !parent.is_empty() && parent != "/" && parent != "~" {
        spans.push(Span::styled(format!("{parent}/"), Theme::top_bar()));
    } else if parent == "/" {
        spans.push(Span::styled("/", Theme::top_bar_sep()));
    } else if parent == "~" {
        spans.push(Span::styled("~/", Theme::top_bar_sep()));
    }

    spans.push(Span::styled(file_name.to_string(), Theme::top_bar_folder()));

    spans
}

/// Format the right side: elevated context pill with mini-meter bar and percentage.
pub(super) fn header_right_spans(app: &App, term_width: u16) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if app.usage_tokens == 0 {
        return spans;
    }

    let approx = if app.usage_tokens_estimated { "~" } else { "" };
    let tokens_str = format_tokens(app.usage_tokens);

    // Pill start padding
    spans.push(Span::styled(" ", Theme::top_bar_pill()));
    spans.push(Span::styled("⚡ ", Theme::top_bar_pill_muted()));
    spans.push(Span::styled(
        format!("{approx}{tokens_str}"),
        Theme::top_bar_pill(),
    ));

    if app.context_window > 0 {
        let pct = ((app.usage_tokens * 100) / app.context_window.max(1)).min(100);
        let win_str = format_tokens(app.context_window);
        let usage_style = Theme::context_usage_style(pct);

        spans.push(Span::styled(" / ", Theme::top_bar_pill_muted()));
        spans.push(Span::styled(win_str, Theme::top_bar_pill_muted()));

        // On wider terminals (width >= 70), add mini progress bar inside the pill
        if term_width >= 70 {
            spans.push(Span::raw(" "));
            let meter = mini_meter_spans(pct);
            spans.extend(meter);
        }

        spans.push(Span::styled(format!(" {pct}%"), usage_style));
    }

    spans.push(Span::styled(" ", Theme::top_bar_pill()));
    spans.push(Span::raw(" ")); // trailing margin from screen edge

    spans
}

/// Generate a 6-block discrete mini meter for context fill.
fn mini_meter_spans(pct: usize) -> Vec<Span<'static>> {
    const TOTAL_CELLS: usize = 6;
    let filled = (pct * TOTAL_CELLS + 50) / 100;
    let filled = filled.min(TOTAL_CELLS);
    let empty = TOTAL_CELLS.saturating_sub(filled);

    let fill_style = Theme::context_usage_style(pct);
    let empty_style = Theme::top_bar_pill_muted();

    vec![
        Span::styled("▪".repeat(filled), fill_style),
        Span::styled("▫".repeat(empty), empty_style),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    #[test]
    fn header_renders_project_path_and_context() {
        let mut app = App::new("test");
        app.history_cwd = Some(PathBuf::from("/home/user/myproject"));
        app.set_usage_tokens(45_000);
        app.set_usage_tokens_estimated(false);
        app.set_context_window(200_000);

        let left = header_left_spans(&app);
        let left_text: String = left.iter().map(|s| s.content.as_ref()).collect();
        assert!(left_text.contains("myproject"), "left: {left_text}");
        assert!(left_text.contains("●"), "status dot: {left_text}");

        let right = header_right_spans(&app, 80);
        let right_text: String = right.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            right_text.contains("45k") && right_text.contains("200k") && right_text.contains("22%"),
            "right: {right_text}"
        );
        assert!(right_text.contains("▪"), "meter: {right_text}");
    }

    #[test]
    fn header_without_window_shows_tokens_only() {
        let mut app = App::new("test");
        app.history_cwd = Some(PathBuf::from("/tmp/workspace"));
        app.set_usage_tokens(15_000);
        app.set_usage_tokens_estimated(true);

        let right = header_right_spans(&app, 80);
        let right_text: String = right.iter().map(|s| s.content.as_ref()).collect();
        assert!(right_text.contains("~15k"), "right: {right_text}");
    }

    #[test]
    fn mini_meter_calculates_correctly() {
        let spans_low = mini_meter_spans(15);
        let text_low: String = spans_low.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text_low, "▪▫▫▫▫▫");

        let spans_high = mini_meter_spans(85);
        let text_high: String = spans_high.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text_high, "▪▪▪▪▪▫");
    }
}
