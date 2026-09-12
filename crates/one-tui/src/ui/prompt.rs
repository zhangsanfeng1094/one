//! Input box and agent/model meta strip under the chat.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

use super::text::{display_width, take_prefix_cols, tokenize_input_chips, InputChipKind};

const INDENT: &str = "  ";

#[derive(Clone, Debug)]
struct PromptVisualRow {
    char_start: usize,
    char_len: usize,
    spans: Vec<Span<'static>>,
}

impl PromptVisualRow {
    fn to_line(&self) -> Line<'static> {
        let mut spans = vec![Span::raw(INDENT)];
        spans.extend(self.spans.clone());
        Line::from(spans)
    }

    fn width_at_char_offset(&self, char_offset: usize) -> usize {
        let mut w = 0usize;
        let mut remaining = char_offset.min(self.char_len);
        for span in &self.spans {
            if remaining == 0 {
                break;
            }
            let count = span.content.chars().count();
            if remaining >= count {
                w += display_width(&span.content);
                remaining -= count;
            } else {
                let byte_idx = span
                    .content
                    .chars()
                    .take(remaining)
                    .map(|c| c.len_utf8())
                    .sum::<usize>();
                w += display_width(&span.content[..byte_idx]);
                break;
            }
        }
        w
    }
}

fn wrap_prompt_line(
    line: &str,
    line_start_char: usize,
    selection: Option<(usize, usize)>,
    usable_width: usize,
) -> Vec<PromptVisualRow> {
    let usable_w = usable_width.max(1);
    if line.is_empty() {
        return vec![PromptVisualRow {
            char_start: line_start_char,
            char_len: 0,
            spans: Vec::new(),
        }];
    }

    let tokens = tokenize_input_chips(line);
    let line_len = line.chars().count();
    let (selected_start, selected_end) = selection.unwrap_or((usize::MAX, usize::MAX));
    let selection_start = selected_start.saturating_sub(line_start_char).min(line_len);
    let selection_end = selected_end.saturating_sub(line_start_char).min(line_len);

    let mut styled_tokens: Vec<(String, Style, Option<InputChipKind>)> = Vec::new();
    let mut char_offset = 0usize;
    for (text, kind) in tokens {
        let base_style = match kind {
            Some(InputChipKind::Text) => Theme::input_text_chip(),
            Some(InputChipKind::Image) => Theme::input_image_chip(),
            None => Theme::input_text(),
        };
        let run_len = text.chars().count();
        let run_start = char_offset;
        let run_end = run_start + run_len;
        let overlap_start = selection_start.max(run_start).min(run_end);
        let overlap_end = selection_end.max(run_start).min(run_end);
        if overlap_start < overlap_end {
            let split_at =
                |index: usize| text.chars().take(index).map(char::len_utf8).sum::<usize>();
            let before = split_at(overlap_start - run_start);
            let selected = split_at(overlap_end - run_start);
            if before > 0 {
                styled_tokens.push((text[..before].to_string(), base_style, kind));
            }
            styled_tokens.push((
                text[before..selected].to_string(),
                Theme::input_selection(),
                kind,
            ));
            if selected < text.len() {
                styled_tokens.push((text[selected..].to_string(), base_style, kind));
            }
        } else if !text.is_empty() {
            styled_tokens.push((text, base_style, kind));
        }
        char_offset = run_end;
    }

    let mut rows: Vec<PromptVisualRow> = Vec::new();
    let mut cur_spans: Vec<Span<'static>> = Vec::new();
    let mut cur_char_start = line_start_char;
    let mut cur_char_len = 0usize;
    let mut cur_col = 0usize;

    for (text, style, kind) in styled_tokens {
        let token_w = display_width(&text);
        if kind.is_some() && cur_col > 0 && cur_col + token_w > usable_w && token_w <= usable_w {
            rows.push(PromptVisualRow {
                char_start: cur_char_start,
                char_len: cur_char_len,
                spans: std::mem::take(&mut cur_spans),
            });
            cur_char_start += cur_char_len;
            cur_char_len = 0;
            cur_col = 0;
        }

        let mut rest = text.as_str();
        while !rest.is_empty() {
            if cur_col >= usable_w {
                rows.push(PromptVisualRow {
                    char_start: cur_char_start,
                    char_len: cur_char_len,
                    spans: std::mem::take(&mut cur_spans),
                });
                cur_char_start += cur_char_len;
                cur_char_len = 0;
                cur_col = 0;
            }
            let room = usable_w.saturating_sub(cur_col).max(1);
            let (take, advance) = take_prefix_cols(rest, room);
            if take.is_empty() {
                rows.push(PromptVisualRow {
                    char_start: cur_char_start,
                    char_len: cur_char_len,
                    spans: std::mem::take(&mut cur_spans),
                });
                cur_char_start += cur_char_len;
                cur_char_len = 0;
                cur_col = 0;
                continue;
            }
            let take_chars = take.chars().count();
            cur_char_len += take_chars;
            cur_col += advance;
            if let Some(last) = cur_spans.last_mut() {
                if last.style == style {
                    let mut s = last.content.to_string();
                    s.push_str(take);
                    *last = Span::styled(s, style);
                } else {
                    cur_spans.push(Span::styled(take.to_string(), style));
                }
            } else {
                cur_spans.push(Span::styled(take.to_string(), style));
            }
            rest = &rest[take.len()..];
            if cur_col >= usable_w && !rest.is_empty() {
                rows.push(PromptVisualRow {
                    char_start: cur_char_start,
                    char_len: cur_char_len,
                    spans: std::mem::take(&mut cur_spans),
                });
                cur_char_start += cur_char_len;
                cur_char_len = 0;
                cur_col = 0;
            }
        }
    }

    if !cur_spans.is_empty() || rows.is_empty() {
        rows.push(PromptVisualRow {
            char_start: cur_char_start,
            char_len: cur_char_len,
            spans: cur_spans,
        });
    }

    rows
}

/// Soft left bar + multi-line input backed by the terminal's native caret.
///
/// **Layout contract (each fact once):**
/// - Meta left  → session identity (agent / model / provider)
/// - Meta right → live ops chips only (MCP / bg / running)
/// - Status left → contextual keybindings
/// - Status right → session stats (think level / token usage)
///
/// The terminal owns the caret shape and blink phase. The prompt only reports
/// its desired screen position to Ratatui while it has interaction focus; the
/// user's terminal cursor preference supplies the visual style.
pub(super) fn draw_prompt(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let box_area = area;

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
    let placeholder = if app.busy && app.busy_activity == "compacting" {
        "compacting context…"
    } else if app.busy {
        "steer or follow-up…"
    } else if app.transcript_browse_focused() {
        // Short: keys live on the status strip; avoid a long dual-hint soup.
        "type to edit…"
    } else {
        "Message…"
    };

    let usable_width = (box_area.width.saturating_sub(4) as usize).max(1);

    // Multi-line input: visual lines wrapped to terminal width; the native caret follows
    // `input_cursor`. Long paste / images render as solid chips (`[文本 · 12
    // lines · 3KB]` / `[图片.img]`), not as the raw body fanned across the
    // composer.
    let mut content: Vec<Line> = vec![Line::from("")]; // top padding
    let mut cursor_pos: Option<(u16, u16)> = None;
    app.prompt_content_x = box_area.x + 3;
    app.prompt_content_y = box_area.y + 1;
    app.prompt_visible_line_starts.clear();
    if app.input.is_empty() {
        app.prompt_visible_line_starts.push(0);
        content.push(Line::from(vec![
            Span::raw(INDENT),
            Span::styled(placeholder, Theme::input_placeholder()),
        ]));
        if prompt_focused {
            cursor_pos = Some((box_area.x + 3, box_area.y + 1));
        }
    } else {
        let lines: Vec<&str> = app.input.split('\n').collect();
        let caret_target = app.input_cursor.min(app.input.chars().count());
        let mut all_visual_rows: Vec<PromptVisualRow> = Vec::new();
        let mut caret_pos: Option<(usize, u16)> = None;
        let mut logical_char_offset = 0usize;

        for (line_idx, line) in lines.iter().enumerate() {
            let is_last_logical = line_idx + 1 == lines.len();
            let logical_len = line.chars().count();
            let logical_end = logical_char_offset + logical_len;

            let rows = wrap_prompt_line(
                line,
                logical_char_offset,
                app.input_selection_range(),
                usable_width,
            );
            let row_count = rows.len();

            for (row_idx, row) in rows.into_iter().enumerate() {
                let vis_idx = all_visual_rows.len();
                let is_last_row_of_logical = row_idx + 1 == row_count;

                if caret_pos.is_none() {
                    let row_end = row.char_start + row.char_len;
                    if caret_target >= row.char_start && caret_target < row_end {
                        let offset_in_row = caret_target - row.char_start;
                        let col_w = row.width_at_char_offset(offset_in_row) as u16;
                        caret_pos = Some((vis_idx, col_w));
                    } else if caret_target == row_end
                        && is_last_row_of_logical
                        && (is_last_logical || caret_target == logical_end)
                    {
                        let col_w = row.width_at_char_offset(row.char_len) as u16;
                        caret_pos = Some((vis_idx, col_w));
                    }
                }

                all_visual_rows.push(row);
            }

            logical_char_offset = logical_end + 1;
        }

        if caret_pos.is_none() && !all_visual_rows.is_empty() {
            let last_idx = all_visual_rows.len() - 1;
            let col_w = all_visual_rows[last_idx]
                .width_at_char_offset(all_visual_rows[last_idx].char_len)
                as u16;
            caret_pos = Some((last_idx, col_w));
        }

        let max_visible = box_area.height.saturating_sub(2) as usize;
        let max_visible = max_visible.max(1);
        let total_rows = all_visual_rows.len();
        let (caret_row_idx, caret_col_w) = caret_pos.unwrap_or((0, 0));

        let start = if total_rows <= max_visible {
            0
        } else {
            let max_start = total_rows - max_visible;
            caret_row_idx.saturating_sub(max_visible - 1).min(max_start)
        };
        let end = (start + max_visible).min(total_rows);

        for (vis_i, row) in all_visual_rows[start..end].iter().enumerate() {
            let abs_i = start + vis_i;
            let caret_here = abs_i == caret_row_idx;
            app.prompt_visible_line_starts.push(row.char_start);
            content.push(row.to_line());
            if caret_here && prompt_focused {
                cursor_pos = Some((box_area.x + 3 + caret_col_w, box_area.y + 1 + vis_i as u16));
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
}

pub(super) fn identity_spans(app: &App) -> Vec<Span<'static>> {
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

    let sep = || Span::styled(" · ", Theme::status_faint().bg(Theme::PANEL));
    let mut left = vec![
        Span::styled("  ", Theme::footer_bg()),
        Span::styled(agent, Theme::mode_label().bg(Theme::PANEL)),
    ];
    if !model.is_empty() {
        left.push(sep());
        left.push(Span::styled(model, Theme::meta().bg(Theme::PANEL)));
    }
    if !provider.is_empty() {
        left.push(sep());
        left.push(Span::styled(
            provider,
            Theme::status_faint().bg(Theme::PANEL),
        ));
    }
    left
}

pub(super) fn ops_spans(app: &App) -> Vec<Span<'static>> {
    let mut right: Vec<Span<'static>> = Vec::new();
    if !app.mcp_chip_text.is_empty() {
        right.push(Span::styled(
            app.mcp_chip_text.clone(),
            mcp_chip_style(app.mcp_chip_kind).bg(Theme::PANEL),
        ));
    }
    if !app.bg_chip_text.is_empty() {
        if !right.is_empty() {
            right.push(Span::styled("  ", Theme::footer_bg()));
        }
        right.push(Span::styled(
            app.bg_chip_text.clone(),
            bg_chip_style(app.bg_chip_kind).bg(Theme::PANEL),
        ));
    }
    if !app.task_chip_text.is_empty() {
        if !right.is_empty() {
            right.push(Span::styled("  ", Theme::footer_bg()));
        }
        right.push(Span::styled(
            app.task_chip_text.clone(),
            bg_chip_style(app.task_chip_kind).bg(Theme::PANEL),
        ));
    }
    if app.busy {
        if !right.is_empty() {
            right.push(Span::styled("  ", Theme::footer_bg()));
        }
        right.push(Span::styled(
            "running",
            Theme::status_faint().bg(Theme::PANEL),
        ));
    }
    if !right.is_empty() {
        right.push(Span::raw("  "));
    }
    right
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
        frame.render_widget(
            Paragraph::new(Line::from(left)).style(Theme::footer_bg()),
            area,
        );
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

    frame.render_widget(
        Paragraph::new(Line::from(left)).style(Theme::footer_bg()),
        row[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(right))
            .alignment(Alignment::Right)
            .style(Theme::footer_bg()),
        row[1],
    );
}

fn mcp_chip_style(kind: u8) -> Style {
    match kind {
        1 => Theme::status().fg(Theme::INFO),
        2 => Style::default()
            .fg(Theme::SUCCESS)
            .add_modifier(ratatui::style::Modifier::DIM),
        3 => Theme::status().fg(Theme::WARNING),
        4 => Theme::status().fg(Theme::ERROR),
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
