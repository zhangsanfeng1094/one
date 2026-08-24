//! Input box and agent/model meta strip under the chat.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

use super::text::display_width;

/// Soft left bar + multi-line input + software typewriter caret.
///
/// **Layout contract (each fact once):**
/// - Meta left  → session identity (agent / model / provider)
/// - Meta right → live ops chips only (MCP / bg / running)
/// - Status left → contextual keybindings
/// - Status right → session stats (think level / token usage)
///
/// Caret sits on a **dedicated 1-column slot** after the text (or before the
/// placeholder when empty). Hardware cursor stays hidden.
pub(super) fn draw_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let input_lines_n = app.input_line_count() as u16;
    let box_h = (input_lines_n + 2).clamp(3, 8); // top pad + lines + bottom pad
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(box_h), Constraint::Min(1)])
        .split(area);
    let box_area = rows[0];
    let meta_area = rows[1];

    // Left rail + caret track real interaction focus (not just "no modal").
    // Busy always keeps the busy rail; otherwise dim when float/select/j/k browse
    // owns focus so a blinking peach caret cannot fake "prompt focused".
    let prompt_focused = app.prompt_focused();
    let bar_style = if app.busy {
        Theme::prompt_bar_busy()
    } else if prompt_focused {
        Theme::prompt_bar()
    } else {
        Theme::prompt_bar_unfocused()
    };

    // Keep placeholder quiet — keybindings live on the sparse status strip / Alt+H help.
    // Busy: light steer hint only; Esc/Ctrl+C live on the status row (avoid wall-of-text).
    let placeholder = if app.busy {
        "steer or follow-up…"
    } else if app.transcript_browse_focused() {
        // Short: keys live on the status strip; avoid a long dual-hint soup.
        "type to edit…"
    } else {
        "Message…"
    };

    const INDENT: &str = "  ";

    // Software caret (█) so the typewriter is visible even when the hardware
    // I-beam is hidden by the emulator / tmux / mouse reporting.
    // Hidden while float / select / empty-prompt transcript browse owns focus
    // (Grok: inactive pane hides caret so focus is unambiguous).
    let caret = if prompt_focused && app.cursor_on {
        Span::styled("█", Theme::input_cursor_on())
    } else if prompt_focused {
        Span::styled(" ", Theme::input_cursor_off())
    } else {
        // Unfocused: no caret slot (empty input still shows placeholder only).
        Span::raw("")
    };

    // Multi-line input: one Line per input row; caret at `input_cursor`.
    // Image attachments appear as `[图片.img]` tokens inside the text (deletable).
    let mut content: Vec<Line> = vec![Line::from("")]; // top padding
    let mut cursor_pos: Option<(u16, u16)> = None;
    if app.input.is_empty() {
        content.push(Line::from(vec![
            Span::raw(INDENT),
            caret.clone(),
            Span::styled(placeholder, Theme::input_placeholder()),
        ]));
        if prompt_focused {
            cursor_pos = Some((box_area.x + 3, box_area.y + 1));
        }
    } else {
        // Place caret using char offset (matches App::input_cursor).
        let mut remaining = app.input_cursor.min(app.input.chars().count());
        let lines: Vec<&str> = app.input.split('\n').collect();
        let last = lines.len().saturating_sub(1);
        let mut caret_line = last;
        let mut caret_col = lines[last].chars().count();
        for (i, line) in lines.iter().enumerate() {
            let line_len = line.chars().count();
            if remaining <= line_len {
                caret_line = i;
                caret_col = remaining;
                break;
            }
            remaining -= line_len;
            if i < last {
                // Consume the `\n` between lines.
                if remaining == 0 {
                    // Caret sits at end of this line (before newline).
                    caret_line = i;
                    caret_col = line_len;
                    break;
                }
                remaining -= 1;
            }
        }
        for (i, line) in lines.iter().enumerate() {
            if i == caret_line {
                let col = caret_col.min(line.chars().count());
                let byte = line
                    .chars()
                    .take(col)
                    .map(|c| c.len_utf8())
                    .sum::<usize>()
                    .min(line.len());
                let before = &line[..byte];
                let after = &line[byte..];
                content.push(Line::from(vec![
                    Span::raw(INDENT),
                    Span::styled(before.to_string(), Theme::input_text()),
                    caret.clone(),
                    Span::styled(after.to_string(), Theme::input_text()),
                ]));
                if prompt_focused {
                    let before_w = display_width(before) as u16;
                    cursor_pos = Some((box_area.x + 3 + before_w, box_area.y + 1 + i as u16));
                }
            } else {
                content.push(Line::from(vec![
                    Span::raw(INDENT),
                    Span::styled((*line).to_string(), Theme::input_text()),
                ]));
            }
        }
    }
    content.push(Line::from("")); // bottom padding

    let paragraph = Paragraph::new(content).style(Theme::input()).block(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(bar_style)
            .style(Style::default().bg(Theme::ELEMENT)),
    );

    frame.render_widget(paragraph, box_area);
    if let Some((cx, cy)) = cursor_pos {
        if cx < box_area.right() && cy < box_area.bottom() {
            frame.set_cursor_position((cx, cy));
        }
    }

    // Prompt meta — identity left, live ops chips right. Stats live on status.
    draw_prompt_meta(frame, meta_area, app);
}

/// Prompt meta strip under the input box.
///
/// Left:  `Build · grok-4.5 · ziyong`  (mode · model · provider, separated)
/// Right: `MCP 3/3  bg:1 · top…  running`  (ops only — never think/tokens)
fn draw_prompt_meta(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let agent = if app.agent_label.is_empty() {
        "Build".to_string()
    } else {
        app.agent_label.clone()
    };
    let model = if !app.current_model.is_empty() {
        app.current_model.clone()
    } else if !app.mode_label.is_empty() {
        app.mode_label.clone()
    } else {
        String::new()
    };
    let provider = app.current_provider.clone();

    // —— left: session identity with clear separators ——
    let sep = || Span::styled(" · ", Theme::status_faint());
    let mut left = vec![
        Span::styled("  ", Theme::bg()),
        // Copper identity tag — PRIMARY reserved for caret / list selection.
        Span::styled(agent, Theme::mode_label()),
    ];
    if !model.is_empty() {
        left.push(sep());
        left.push(Span::styled(model, Theme::meta()));
    }
    if !provider.is_empty() {
        left.push(sep());
        left.push(Span::styled(provider, Theme::status_faint()));
    }

    // —— right: live ops chips only ——
    let mut right: Vec<Span<'static>> = Vec::new();
    // MCP progress (e.g. MCP 3/3) — coarse, no connection noise. Status does NOT mirror.
    if !app.mcp_chip_text.is_empty() {
        right.push(Span::styled(
            app.mcp_chip_text.clone(),
            mcp_chip_style(app.mcp_chip_kind),
        ));
    }
    // Background bash only (e.g. bg:1 · cargo…) — open /ps.
    if !app.bg_chip_text.is_empty() {
        if !right.is_empty() {
            right.push(Span::styled("  ", Theme::bg()));
        }
        right.push(Span::styled(
            app.bg_chip_text.clone(),
            bg_chip_style(app.bg_chip_kind),
        ));
    }
    // Subagents (e.g. task:1 · explore…) — open /tasks, not /ps.
    if !app.task_chip_text.is_empty() {
        if !right.is_empty() {
            right.push(Span::styled("  ", Theme::bg()));
        }
        right.push(Span::styled(
            app.task_chip_text.clone(),
            bg_chip_style(app.task_chip_kind),
        ));
    }
    if app.busy {
        if !right.is_empty() {
            right.push(Span::styled("  ", Theme::bg()));
        }
        right.push(Span::styled("running", Theme::status_faint()));
    }
    if !right.is_empty() {
        right.push(Span::raw("  ")); // trailing pad so right edge breathes
    }

    render_split_row(frame, area, left, right);
}

/// Paint a single-row strip with left content + right-aligned trailing content.
/// Right width is measured from content (not a fixed column count) so chips
/// never collide with identity/key labels.
pub(super) fn render_split_row(
    frame: &mut Frame<'_>,
    area: Rect,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
) {
    if right.is_empty() {
        frame.render_widget(Paragraph::new(Line::from(left)).style(Theme::bg()), area);
        return;
    }

    let right_text: String = right.iter().map(|s| s.content.as_ref()).collect();
    let right_cols = display_width(&right_text) as u16;
    // Leave at least ~12 cols for left identity/keys; clamp right if terminal is tight.
    let max_right = area.width.saturating_sub(12);
    let right_w = right_cols.min(max_right).max(1);

    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(8), Constraint::Length(right_w)])
        .split(area);

    frame.render_widget(Paragraph::new(Line::from(left)).style(Theme::bg()), row[0]);
    frame.render_widget(
        Paragraph::new(Line::from(right))
            .alignment(Alignment::Right)
            .style(Theme::bg()),
        row[1],
    );
}

fn mcp_chip_style(kind: u8) -> Style {
    match kind {
        1 => Theme::status().fg(Theme::INFO),    // loading
        2 => Theme::status_faint(),              // all ok — quiet
        3 => Theme::status().fg(Theme::WARNING), // partial
        4 => Theme::status().fg(Theme::ERROR),   // error
        _ => Theme::status_faint(),
    }
}

fn bg_chip_style(kind: u8) -> Style {
    match kind {
        1 => Theme::status().fg(Theme::INFO),    // running
        2 => Theme::status_faint(),              // recent done
        3 => Theme::status().fg(Theme::WARNING), // mixed
        4 => Theme::status().fg(Theme::ERROR),   // failed
        _ => Theme::status_faint(),
    }
}
