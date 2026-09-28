//! Pi-style top header: workspace path on the left, context fill / model on the right.

use std::path::{Path, PathBuf};

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;
use crate::ui::text::display_width;

use super::status::format_tokens;

/// Draw the top header bar across the full terminal width.
pub(super) fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Block::default().style(Theme::top_bar_bg()), area);

    let left = header_left_spans(app);
    let right = header_right_spans(app, area.width);

    render_header_split(frame, area, left, right);
}

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

/// Left: folder glyph + home-shortened workspace path.
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

    vec![
        Span::raw(" "),
        Span::styled("> ", Theme::top_bar_folder()),
        Span::styled(
            if parent.is_empty() {
                file_name.to_string()
            } else if parent == "/" {
                format!("/{file_name}")
            } else {
                format!("{parent}/{file_name}")
            },
            Theme::top_bar_folder(),
        ),
    ]
}

/// Right: context fill · model & thinking level.
pub(super) fn header_right_spans(app: &App, _term_width: u16) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut first = true;
    let mut push_sep = |spans: &mut Vec<Span<'static>>| {
        if !first {
            spans.push(Span::styled("  |  ", Theme::top_bar_sep()));
        }
        first = false;
    };

    if app.usage_tokens > 0 {
        push_sep(&mut spans);
        spans.extend(context_pill_spans(app));
    }

    let model = if !app.current_model.is_empty() {
        app.current_model.as_str()
    } else if !app.mode_label.is_empty() {
        app.mode_label.as_str()
    } else {
        ""
    };
    let has_thinking = app.thinking_level != "off" && !app.thinking_level.is_empty();

    if !model.is_empty() || has_thinking {
        push_sep(&mut spans);
        if !model.is_empty() {
            spans.push(Span::styled(model.to_string(), Theme::top_bar_model()));
        }
        if has_thinking {
            if !model.is_empty() {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                app.thinking_level.clone(),
                Theme::status_faint(),
            ));
        }
    }

    if !spans.is_empty() {
        spans.push(Span::raw(" "));
    }
    spans
}

/// Context pill: `32k / 128k (25%)`.
fn context_pill_spans(app: &App) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let approx = if app.usage_tokens_estimated { "~" } else { "" };
    let tokens_str = format_tokens(app.usage_tokens);

    spans.push(Span::styled(" ", Theme::top_bar_pill()));
    if app.context_window > 0 {
        let pct = ((app.usage_tokens * 100) / app.context_window.max(1)).min(100);
        let win_str = format_tokens(app.context_window);
        let usage_style = Theme::context_usage_style(pct);

        spans.push(Span::styled(
            format!("{approx}{tokens_str}"),
            if pct < 8 {
                Theme::top_bar_pill_muted()
            } else {
                Theme::top_bar_pill()
            },
        ));
        spans.push(Span::styled(" / ", Theme::top_bar_pill_muted()));
        spans.push(Span::styled(win_str, Theme::top_bar_pill_muted()));
        spans.push(Span::styled(format!(" ({pct}%)"), usage_style));
    } else {
        spans.push(Span::styled(
            format!("{approx}{tokens_str}"),
            Theme::top_bar_pill(),
        ));
    }
    spans.push(Span::styled(" ", Theme::top_bar_pill()));
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    #[test]
    fn header_renders_project_path_context_and_model() {
        let mut app = App::new("test");
        app.history_cwd = Some(PathBuf::from("/home/user/myproject"));
        app.set_usage_tokens(45_000);
        app.set_usage_tokens_estimated(false);
        app.set_context_window(200_000);
        app.current_model = "gemini-3.8-flash-high".into();
        app.thinking_level = "medium".into();

        let left = header_left_spans(&app);
        let left_text: String = left.iter().map(|s| s.content.as_ref()).collect();
        assert!(left_text.contains("myproject"), "left: {left_text}");
        assert!(left_text.contains('>'), "path prefix: {left_text}");
        assert!(
            !left_text.contains("●"),
            "status dot must not paint: {left_text}"
        );

        let right = header_right_spans(&app, 80);
        let right_text: String = right.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            right_text.contains("gemini-3.8-flash-high"),
            "model must be on header: {right_text}"
        );
        assert!(
            right_text.contains("medium"),
            "thinking level must be on header: {right_text}"
        );
        assert!(
            !right_text.contains(':'),
            "clock must not be on header: {right_text}"
        );
        assert!(
            right_text.contains("45k")
                && right_text.contains("200k")
                && (right_text.contains("22%") || right_text.contains("(22%)")),
            "context fill: {right_text}"
        );

        let wide_right = header_right_spans(&app, 120);
        let wide_text: String = wide_right.iter().map(|s| s.content.as_ref()).collect();
        assert!(wide_text.contains("45k / 200k"), "context: {wide_text}");
    }

    #[test]
    fn header_without_window_shows_tokens_and_model() {
        let mut app = App::new("test");
        app.history_cwd = Some(PathBuf::from("/tmp/workspace"));
        app.set_usage_tokens(15_000);
        app.set_usage_tokens_estimated(true);
        app.current_model = "grok-4.5".into();

        let right = header_right_spans(&app, 80);
        let right_text: String = right.iter().map(|s| s.content.as_ref()).collect();
        assert!(right_text.contains("grok-4.5"), "right: {right_text}");
        assert!(right_text.contains("~15k"), "tokens: {right_text}");
    }
}
