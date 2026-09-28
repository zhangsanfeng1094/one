//! Bottom status / footer (1 line: Pi-style keybindings + model / MCP).

use ratatui::layout::Rect;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::Frame;

use crate::app::App;
use crate::message::{MessageRole, ToolStatus};
use crate::theme::Theme;
use crate::tool_view;

use super::prompt::render_split_row;
use super::SPINNER;

pub(super) fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Block::default().style(Theme::footer_bg()), area);
    let (left, right) = status_spans(app);
    render_split_row(frame, area, left, right);
}

fn scroll_position_label(app: &App) -> Option<String> {
    if !app.can_scroll() {
        return None;
    }
    let total = app.chat_total_lines.max(1);
    let view = app.chat_view_height.max(1);
    let start = if app.follow_bottom {
        total.saturating_sub(view)
    } else {
        app.chat_view_start
    };
    let shown = (start + view).min(total);
    let max = total.saturating_sub(view).max(1);
    let pct = if app.follow_bottom {
        100
    } else {
        ((start as f64 / max as f64) * 100.0).round() as u16
    };
    Some(format!("{pct}% · {shown}/{total}"))
}

/// Sparse Pi-style status strip.
///
/// Left:  `esc interrupt  ctrl+c quit  …`
/// Right: model / MCP.
fn status_spans(app: &App) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    fn pair(key: &'static str, label: &'static str) -> [Span<'static>; 2] {
        [
            Span::styled(format!("{key} "), Theme::status_key()),
            Span::styled(format!("{label}  "), Theme::status_faint()),
        ]
    }

    if app.float_open() {
        let mut left = vec![Span::raw(" ")];
        left.extend(pair("↑↓", "nav"));
        left.extend(pair("enter", "select"));
        left.extend(pair("esc", "close"));
        return (left, Vec::new());
    }

    if !app.follow_bottom && app.can_scroll() {
        let mut left = vec![Span::raw(" ")];
        if app.work_summary.visible() {
            left.extend(pair("Alt+T", "work"));
        }
        left.extend(pair("Shift+G", "latest"));
        left.extend(pair("wheel", "scroll"));
        let mut right = status_stats_spans(app);
        if let Some(pos) = scroll_position_label(app) {
            if !right.is_empty() {
                right.insert(0, Span::styled("  ", Theme::footer_bg()));
            }
            right.insert(0, Span::styled(pos, Theme::status_faint()));
        }
        return (left, right);
    }

    if app.busy {
        let compacting = app.busy_activity == "compacting";
        let mut left = vec![Span::raw(" ")];

        let running_tool =
            app.messages.iter().rev().find(|m| {
                m.role == MessageRole::Tool && m.tool_status == Some(ToolStatus::Running)
            });

        if let Some(tool) = running_tool {
            let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
            let label_raw = tool_view::running_tool_label(tool, app.history_cwd.as_deref());
            let label = tool_view::single_line_preview(&label_raw, 36);
            left.push(Span::styled(
                format!("{spinner} {label}  "),
                Theme::tool_name_running(),
            ));
        }

        if !compacting {
            left.extend(pair("esc", "interrupt"));
        }
        if app.work_summary.visible() {
            left.extend(pair("Alt+T", "work"));
        }
        left.extend(pair("ctrl+c", "quit"));
        if !compacting {
            left.extend(pair("ctrl+s", "steer"));
            left.extend(pair("alt+enter", "follow-up"));
            left.extend(pair("ctrl+b", "bg"));
        }

        let mut right = status_stats_spans(app);
        if let Some((retry, max_retries, seconds)) = app.retry_wait_status() {
            let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
            let label = if seconds == 0 {
                format!("{spinner} retry {retry}/{max_retries} · starting…")
            } else {
                format!("{spinner} retry {retry}/{max_retries} · {seconds}s")
            };
            if !right.is_empty() {
                right.insert(0, Span::raw("  "));
            } else {
                right.push(Span::raw(" "));
            }
            right.insert(0, Span::styled(label, Theme::status().fg(Theme::WARNING)));
        } else if compacting {
            let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
            if !right.is_empty() {
                right.insert(0, Span::raw("  "));
            } else {
                right.push(Span::raw(" "));
            }
            right.insert(
                0,
                Span::styled(
                    format!("{spinner} compacting…"),
                    Theme::status().fg(Theme::WARNING),
                ),
            );
        }
        return (left, right);
    }

    let mut left = vec![Span::raw(" ")];
    if app.work_summary.visible() {
        left.extend(pair("Alt+T", "work"));
    }
    if app.transcript_browse_focused() {
        left.extend(pair("j/k", "navigate"));
        left.extend(pair("enter", "expand/collapse"));
        left.extend(pair("esc", "interrupt"));
    } else {
        left.extend(pair("esc", "interrupt"));
        left.extend(pair("ctrl+c", "quit"));
        left.extend(pair("ctrl+n", "new run"));
        left.extend(pair("ctrl+l", "model"));
        left.extend(pair("↑/↓", "navigate"));
        left.extend(pair("enter", "expand/collapse"));
    }

    (left, status_stats_spans(app))
}

fn status_stats_spans(app: &App) -> Vec<Span<'static>> {
    let mut right = Vec::new();

    if !app.mcp_chip_text.is_empty() {
        right.push(Span::styled(
            format!("● {}", app.mcp_chip_text),
            mcp_chip_style(app.mcp_chip_kind),
        ));
    }
    if !right.is_empty() {
        right.push(Span::raw(" "));
    }
    right
}

fn mcp_chip_style(kind: u8) -> ratatui::style::Style {
    match kind {
        1 => Theme::status().fg(Theme::INFO),
        2 => Theme::status().fg(Theme::SUCCESS),
        3 => Theme::status().fg(Theme::WARNING),
        4 => Theme::status().fg(Theme::ERROR),
        _ => Theme::status_faint(),
    }
}

/// Last prompt / estimated context size (not session-cumulative billing).
pub(super) fn format_context_usage(app: &App) -> Option<String> {
    if app.usage_tokens == 0 {
        return None;
    }
    let approx = if app.usage_tokens_estimated { "~" } else { "" };
    let tokens = format_tokens(app.usage_tokens);
    if app.context_window > 0 {
        let pct = (app.usage_tokens * 100) / app.context_window.max(1);
        Some(format!("ctx {approx}{tokens} {pct}%"))
    } else {
        Some(format!("ctx {approx}{tokens}"))
    }
}

pub(super) fn format_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod usage_format_tests {
    use super::*;
    use crate::app::App;

    #[test]
    fn status_shows_context_only_not_session_totals() {
        let mut app = App::new("test");
        app.set_usage_io(714_677, 5_332);
        app.set_usage_cache(599_040, 0);
        app.set_usage_cost_usd(0.42);
        app.set_usage_tokens(44_192);
        app.set_usage_tokens_estimated(false);
        app.set_context_window(1_000_000);

        let ctx = format_context_usage(&app).expect("context");
        assert_eq!(ctx, "ctx 44k 4%");

        let spans = status_stats_spans(&app);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("ctx") && !text.contains("44k"),
            "token fill belongs on the header, not footer: {text}"
        );
        assert!(
            !text.contains("thinking"),
            "think level stays off the footer: {text}"
        );
        assert!(
            !text.contains('↑')
                && !text.contains('↓')
                && !text.contains("cR")
                && !text.contains("session")
                && !text.contains('$'),
            "status must not show session cumulative I/O/cost: {text}"
        );
    }

    #[test]
    fn context_shown_without_window() {
        let mut app = App::new("test");
        app.set_usage_tokens(12_500);
        app.set_usage_tokens_estimated(true);
        assert_eq!(format_context_usage(&app).as_deref(), Some("ctx ~12k"));
    }

    #[test]
    fn busy_status_shows_animated_retry_countdown() {
        let mut app = App::new("test");
        app.begin_busy();
        app.spinner_frame = 3;
        app.begin_retry_wait(2, 10, std::time::Duration::from_secs(5));

        let (_, right) = status_spans(&app);
        let text: String = right.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("retry 2/10"), "status: {text}");
        assert!(text.contains("⠸"), "spinner frame: {text}");
    }

    #[test]
    fn busy_status_omits_wait_park_from_footer() {
        let mut app = App::new("test");
        app.begin_busy();
        app.spinner_frame = 1;
        app.begin_wait_park("all", 2);

        let (_, right) = status_spans(&app);
        let text: String = right.iter().map(|span| span.content.as_ref()).collect();
        assert!(
            !text.contains("Waiting"),
            "wait_park should only render in chat area, not footer: {text}"
        );
    }

    #[test]
    fn status_shows_mcp_without_model() {
        let mut app = App::new("test");
        app.set_current_model("cpa", "gemini-3.8-flash-high");
        app.thinking_level = "medium".into();
        app.set_mcp_chip("MCP 3/3", 2);

        let spans = status_stats_spans(&app);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("gemini-3.8-flash-high"),
            "model moved to header, not in status: {text}"
        );
        assert!(
            !text.contains("medium"),
            "thinking level moved to header, not in status: {text}"
        );
        assert!(text.contains("● MCP 3/3"), "mcp chip in status: {text}");
    }
}
