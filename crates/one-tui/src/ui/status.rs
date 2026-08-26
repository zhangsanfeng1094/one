//! Bottom status / footer (2 lines: identity + key badges).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

use super::prompt::{identity_spans, ops_spans, render_split_row};
use super::SPINNER;

pub(super) fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Block::default().style(Theme::footer_bg()), area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let ident_right = {
        let mut right = ops_spans(app);
        if let Some(pos) = scroll_position_label(app) {
            if !right.is_empty() {
                right.insert(0, Span::styled("  ", Theme::footer_bg()));
            }
            right.insert(0, Span::styled(pos, Theme::status_faint().bg(Theme::PANEL)));
        }
        right
    };
    render_split_row(frame, rows[0], identity_spans(app), ident_right);

    let (left, right) = status_spans(app);
    render_split_row(frame, rows[1], left, right);
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

/// Sparse, context-aware status strip.
///
/// Left:  keybindings only (mode-aware).
/// Right: session stats only (think level / live context fill).
/// Never MCP, bg, or session-cumulative ↑↓ — those live elsewhere / not in chrome.
fn status_spans(app: &App) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    // Key + label pairs, joined with double-space (no middle-dot soup).
    fn pair(key: &'static str, label: &'static str) -> [Span<'static>; 2] {
        [
            Span::styled(format!(" {key} "), Theme::key_badge()),
            Span::styled(format!(" {label}  "), Theme::key_badge_label()),
        ]
    }

    // Notices are top-right toasts now — footer stays for keybindings only.

    if app.float_open() {
        let mut left = vec![Span::raw("  ")];
        left.extend(pair("↑↓", " nav  "));
        left.extend(pair("enter", " select  "));
        left.extend(pair("esc", " close  "));
        left.extend(pair("Ctrl+C", " close"));
        return (left, Vec::new());
    }

    if !app.follow_bottom && app.can_scroll() {
        let mut left = vec![Span::raw(" ")];
        left.extend(pair("Shift+G", "latest"));
        left.extend(pair("wheel", "scroll"));
        return (left, Vec::new());
    }

    if app.busy {
        let compacting = app.busy_activity == "compacting";
        let mut left = vec![Span::raw("  ")];
        // Soft cancel vs hard exit — single Ctrl+C never exits (double-tap quit).
        if !compacting {
            left.extend(pair("esc", " stop  "));
        }
        left.extend(pair("Ctrl+C", "×2 quit  "));
        if !compacting {
            left.extend(pair("Ctrl+S", " steer"));
        }
        // Ops chips (MCP/bg) stay on meta; status only shows activity + stats.
        let mut right = status_stats_spans(app);
        if let Some((retry, max_retries, seconds)) = app.retry_wait_status() {
            let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
            let label = if seconds == 0 {
                format!("{spinner} retry {retry}/{max_retries} · starting…")
            } else {
                format!("{spinner} retry {retry}/{max_retries} · {seconds}s")
            };
            right.insert(0, Span::raw("  "));
            right.insert(0, Span::styled(label, Theme::status().fg(Theme::WARNING)));
        } else if compacting {
            let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
            right.insert(0, Span::raw("  "));
            right.insert(
                0,
                Span::styled(
                    format!("{spinner} compacting…"),
                    Theme::status().fg(Theme::WARNING),
                ),
            );
        }
        if right.is_empty() {
            right.push(Span::styled("working", Theme::status_faint()));
            right.push(Span::raw("  "));
        }
        return (left, right);
    }

    // Idle: core chrome only — full catalog is Alt+H help float.
    // When chat focus is active (empty prompt browse), surface expand/nav keys.
    let mut left = vec![Span::raw("  ")];
    if app.transcript_browse_focused() {
        left.extend(pair("j/k", " nav  "));
        left.extend(pair("↵", " expand  "));
        left.extend(pair("Alt+Z", " fold"));
    } else {
        left.extend(pair("Ctrl+G", " settings"));
        left.push(Span::styled("  │  ", Theme::status_faint()));
        left.extend(pair("Ctrl+L", " model"));
        left.push(Span::styled("  │  ", Theme::status_faint()));
        left.extend(pair("Alt+H", " help"));
        left.push(Span::styled("  │  ", Theme::status_faint()));
        left.extend(pair("click", " expand"));
    }

    (left, status_stats_spans(app))
}

/// Right-side session stats for the status strip: think level + live context.
/// Kept separate so busy/idle modes share one formatter (no MCP/bg).
///
/// Session-cumulative ↑↓ / cache / cost are **not** shown here — they inflate
/// into hundreds of k and get misread as context fill. Status only surfaces
/// current prompt size (`ctx`).
fn status_stats_spans(app: &App) -> Vec<Span<'static>> {
    let mut right = Vec::new();
    if app.thinking_level != "off" {
        let vis = if app.show_thinking { "·full" } else { "" };
        right.push(Span::styled(
            format!("think:{}{vis}", app.thinking_level),
            Theme::status_faint().bg(Theme::PANEL),
        ));
        right.push(Span::raw("  "));
    }
    right
}

/// Last prompt / estimated context size (not session-cumulative billing).
/// Token fill itself is painted on the header bar, not the footer.
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
        // Cumulative bill is huge — must not appear on the status strip.
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
            "token fill belongs on the header, not footer stats: {text}"
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
        assert_eq!(
            format_context_usage(&app).as_deref(),
            Some("ctx ~12k") // ≥10k uses integer k (format_tokens)
        );
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
}
