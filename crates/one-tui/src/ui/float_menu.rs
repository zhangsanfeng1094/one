//! Centered floating menus (Settings, sessions, subagents, …).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::float::{FloatKind, FloatMenu, FloatRenderRow};
use crate::theme::Theme;

use super::text::{pad_or_truncate, scrollbar_thumb_geometry};

/// Centered floating panel — solid backdrop, padded chrome, three-column rows.
///
/// ```text
/// ┌─ title ──────────────────────────────────────────┐
/// │                                                  │  ← top pad
/// │   Filter: query▌                                 │  ← only when typing / edit
/// │   ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  │
/// │   Connection                                     │
/// │     protocol    openai-completions      [select] │  ← key · value · action
/// │   > api_key     set                       [edit] │
/// │                                                  │  ← bottom pad
/// └─ ↑/↓ Navigate · Enter Select · Esc Back ─────────┘
/// ```
///
/// When `edit_mode`, the list is fully dimmed and the `›` marker is hidden so
/// the top field is the sole interaction focus.
pub(super) fn draw_float_menu(frame: &mut Frame<'_>, full: Rect, menu: &FloatMenu) {
    // Keep a little terminal chrome around the modal when possible, while
    // remaining safe on narrow/short terminals. `u16::clamp` panics when its
    // lower bound exceeds the upper bound, which used to make Settings crash
    // below ~48 columns or ~12 rows.
    let is_subagent = matches!(
        menu.kind,
        FloatKind::Subagent | FloatKind::SubagentDetail
    );
    let max_w = full.width.saturating_sub(2);
    // Subagent panels get a touch more width for status + activity meta.
    let min_w = if is_subagent {
        48.min(max_w)
    } else {
        42.min(max_w)
    };
    let width_frac = if is_subagent { 3 } else { 7 }; // 3/4 vs 7/10
    let width_den = if is_subagent { 4 } else { 10 };
    let width = (full.width.saturating_mul(width_frac) / width_den).clamp(min_w, max_w);
    let render_rows = menu.render_rows();
    let show_filter = menu.edit_mode || !menu.search.is_empty();
    // Outer height includes border (2). Inner: top pad + optional filter+rule + list + bottom pad.
    let filter_h: u16 = if show_filter { 2 } else { 0 };
    // Show more rows on tall terminals but never let the float exceed its frame.
    let available_list_rows = full
        .height
        .saturating_sub(if show_filter { 7 } else { 5 })
        .max(1);
    let list_cap = if is_subagent { 28 } else { 24 };
    let list_rows = (render_rows.len() as u16).clamp(1, available_list_rows.min(list_cap));
    let inner_h = 1u16 // top pad
        .saturating_add(filter_h)
        .saturating_add(list_rows)
        .saturating_add(1); // bottom pad
    let max_h = full.height.saturating_sub(2);
    let min_h = 6.min(max_h);
    let height = (inner_h.saturating_add(2)).clamp(min_h, max_h);

    let x = full.x + (full.width.saturating_sub(width)) / 2;
    let y = full.y + (full.height.saturating_sub(height)) / 2;
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    // 1) Clear terminal cells  2) fill solid PANEL  3) border on top.
    // Without the fill, Clear leaves transparent cells and chat bleeds through
    // border / padded regions where no span sets a background.
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Theme::slash_panel()), area);

    let title = format!(" {} ", menu.title);
    let footer = float_footer_text(menu);
    let (border_style, title_style) = if is_subagent {
        (Theme::subagent_border(), Theme::subagent_title())
    } else {
        (Theme::border(), Theme::title())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(border_style)
        .style(Theme::slash_panel())
        .title(Span::styled(title, title_style))
        .title_bottom(Span::styled(footer, Theme::float_footer()));

    let border_inner = block.inner(area);
    frame.render_widget(block, area);

    // Horizontal breathing: 2 cols each side inside the border.
    let h_pad = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(border_inner);

    let v_constraints = if show_filter {
        vec![
            Constraint::Length(1), // top breathe
            Constraint::Length(2), // filter + hairline
            Constraint::Min(3),    // list
            Constraint::Length(1), // bottom breathe
        ]
    } else {
        vec![
            Constraint::Length(1), // top breathe
            Constraint::Min(3),    // list
            Constraint::Length(1), // bottom breathe
        ]
    };
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints(v_constraints)
        .split(h_pad[1]);

    let list_area = if show_filter { parts[2] } else { parts[1] };

    if show_filter {
        let search_area = parts[1];
        let search_line = float_filter_line(menu);
        let rule_w = search_area.width as usize;
        let hairline = "╌".repeat(rule_w.max(1));
        // Paint full row bg so no chat peeks between spans.
        frame.render_widget(
            Paragraph::new(vec![
                search_line,
                Line::from(Span::styled(hairline, Theme::hairline())),
            ])
            .style(Theme::slash_panel()),
            search_area,
        );
    }

    // List with scroll around selected. Detail panels are read-only logs:
    // `selected` is only a scroll anchor — never a focus cursor.
    let readonly_log = matches!(
        menu.kind,
        FloatKind::BackgroundDetail | FloatKind::SubagentDetail
    );
    let max_rows = list_area.height as usize;
    let total_rows = render_rows.len();
    let selected_row = render_rows
        .iter()
        .position(|r| match r {
            FloatRenderRow::Item { entry_index, .. } => *entry_index == menu.selected,
            _ => false,
        })
        .unwrap_or(0);
    // Keep the scroll anchor visible (tail when selected is last line).
    let start = selected_row.saturating_sub(max_rows.saturating_sub(1));
    let end = (start + max_rows).min(total_rows);
    let need_scrollbar = total_rows > max_rows && max_rows > 0;

    // Reserve 1 col on the right for a progress scrollbar when content overflows.
    let sb_w: u16 = if need_scrollbar { 1 } else { 0 };
    let content_area = Rect {
        x: list_area.x,
        y: list_area.y,
        width: list_area.width.saturating_sub(sb_w),
        height: list_area.height,
    };
    let sb_area = if need_scrollbar {
        Some(Rect {
            x: list_area.x + content_area.width,
            y: list_area.y,
            width: 1,
            height: list_area.height,
        })
    } else {
        None
    };

    let editing = menu.edit_mode;
    let col_w = content_area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for row in &render_rows[start..end] {
        match row {
            FloatRenderRow::Header(title) => {
                if is_subagent {
                    lines.push(subagent_header_line(title, col_w));
                } else {
                    lines.push(float_header_line(title, col_w, editing));
                }
            }
            FloatRenderRow::Item {
                entry_index,
                label,
                detail,
                hint,
                style,
            } => {
                if menu.kind == FloatKind::SubagentDetail {
                    lines.push(subagent_log_line(label, detail, col_w));
                } else if menu.kind == FloatKind::Subagent {
                    let active = !editing && *entry_index == menu.selected;
                    lines.push(subagent_item_line(label, detail, hint, col_w, active));
                } else if readonly_log {
                    lines.push(float_log_line(label, detail, col_w));
                } else {
                    let active = !editing && *entry_index == menu.selected;
                    lines.push(float_item_line(
                        label, detail, hint, col_w, active, editing, *style,
                    ));
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            if is_subagent {
                "  (no matches)"
            } else {
                "(no matches)"
            },
            if editing {
                Theme::float_dim()
            } else {
                Theme::slash_desc()
            },
        )));
    }

    // Fill remaining list rows with blank PANEL lines so short menus don't
    // leave uncleared chat cells in the lower half of the float.
    while lines.len() < max_rows {
        lines.push(Line::from(Span::styled(
            " ".repeat(col_w.max(1)),
            Theme::slash_panel(),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines).style(Theme::slash_panel()),
        content_area,
    );

    if let Some(sb) = sb_area {
        draw_float_scrollbar(frame, sb, total_rows, max_rows, start, is_subagent);
    }
}

/// Vertical progress scrollbar for overflow float lists / live logs.
///
/// Track = dim bar; thumb = bright block sized by `viewport / total`.
fn draw_float_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    total: usize,
    viewport: usize,
    offset: usize,
    accent: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let track_h = area.height as usize;
    let (thumb_start, thumb_h) = scrollbar_thumb_geometry(total, viewport, offset, track_h);
    let track_style = if accent {
        Style::default().bg(Theme::PANEL).fg(Theme::BORDER)
    } else {
        Theme::hairline()
    };
    let thumb_style = if accent {
        Style::default()
            .bg(Theme::PANEL)
            .fg(Theme::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(Theme::PANEL)
            .fg(Theme::BORDER_ACTIVE)
            .add_modifier(Modifier::BOLD)
    };

    let mut lines: Vec<Line> = Vec::with_capacity(track_h);
    for i in 0..track_h {
        let in_thumb = i >= thumb_start && i < thumb_start + thumb_h;
        let ch = if in_thumb { "▐" } else { "│" };
        let style = if in_thumb { thumb_style } else { track_style };
        lines.push(Line::from(Span::styled(ch, style)));
    }
    frame.render_widget(Paragraph::new(lines).style(Theme::slash_panel()), area);
}


fn float_footer_text(menu: &FloatMenu) -> String {
    if menu.edit_mode {
        if menu.kind == FloatKind::SettingsDeleteConfirm {
            return " Type exact name  ·  Enter Delete  ·  Esc Cancel ".into();
        }
        return " ←→ Cursor  ·  Enter Save  ·  Esc Cancel ".into();
    }
    let base = match menu.kind {
        FloatKind::Info => " Enter / Esc Close ",
        FloatKind::NewSessionConfirm => " ↑/↓ Choose  ·  Enter Confirm  ·  Esc Cancel ",
        FloatKind::Sessions => " ↑/↓ Navigate  ·  Enter Resume  ·  Esc Back ",
        FloatKind::Tree => " ↑/↓ Navigate  ·  Enter Branch  ·  Esc Back ",
        FloatKind::Rewind => " ↑/↓ Navigate  ·  Enter Edit  ·  Esc Back ",
        FloatKind::Thinking => " ↑/↓ Navigate  ·  Enter Select  ·  Esc Back ",
        FloatKind::Login => " ↑/↓ Navigate  ·  Enter Login  ·  Esc Close ",
        FloatKind::Logout => " ↑/↓ Navigate  ·  Enter Logout  ·  Esc Close ",
        FloatKind::Help => " ↑/↓ Navigate  ·  Enter Open  ·  Esc Back ",
        FloatKind::Models => " ↑/↓ Navigate  ·  Enter Switch  ·  Esc Back ",
        FloatKind::Settings => " ↑/↓ Navigate  ·  Type to filter  ·  Enter Select  ·  Esc Close ",
        FloatKind::SettingsModels => {
            " ↑/↓  ·  Space = show in Ctrl+L  ·  Enter detail  ·  Ctrl+F Import  ·  Esc Back "
        }
        FloatKind::SettingsProviderDetail => {
            " ↑/↓ Navigate  ·  Type to filter  ·  Enter Select  ·  Ctrl+F Import  ·  Esc Back "
        }
        FloatKind::SettingsProviderCompat => {
            " ↑/↓ Navigate  ·  Type to filter  ·  Enter Select  ·  Esc Back "
        }
        FloatKind::SettingsRemoteModels => {
            " ↑/↓ Navigate  ·  Type to filter  ·  Enter Add  ·  Ctrl+F Re-import  ·  Esc Back "
        }
        FloatKind::SettingsToolOutput => " ↑/↓ Navigate  ·  Enter Edit  ·  Esc Back ",
        FloatKind::SettingsCompaction => " ↑/↓ Navigate  ·  Enter Edit/Toggle  ·  Esc Back ",
        FloatKind::SettingsProviders
        | FloatKind::SettingsProviderApi
        | FloatKind::SettingsThinkingFormat
        | FloatKind::SettingsMaxTokensField
        | FloatKind::SettingsModelDetail => " ↑/↓ Navigate  ·  Enter Select  ·  Esc Back ",
        FloatKind::SettingsModelAdd => " ↑/↓ Fields  ·  Enter Edit/Save  ·  Esc Back ",
        FloatKind::SettingsDeleteConfirm => " Type exact name  ·  Enter Delete  ·  Esc Cancel ",
        FloatKind::Skills => " ↑/↓ Navigate  ·  Enter Toggle  ·  Esc Close ",
        FloatKind::Agents => " ↑/↓ Navigate  ·  Enter details/path  ·  Esc Close ",
        FloatKind::Features => " ↑/↓ Navigate  ·  Enter Toggle  ·  Esc Back  ·  ctx needs /new ",
        FloatKind::Mcp => " ↑/↓  ·  Enter  ·  Import  ·  Esc ",
        FloatKind::McpImport => " ↑/↓  ·  Enter import  ·  Esc back ",
        FloatKind::Background => " ↑/↓ wheel  ·  Enter log  ·  x kill  ·  Esc ",
        FloatKind::BackgroundDetail => " ↑/↓ wheel  ·  x kill  ·  Esc list ",
        FloatKind::Subagent => " ↑/↓ wheel  ·  ↵ live log  ·  x kill  ·  Esc ",
        FloatKind::SubagentDetail => " ↑/↓ wheel  ·  PgUp/Dn  ·  x kill  ·  Esc back ",
        FloatKind::Commands | FloatKind::Custom => " ↑/↓ Navigate  ·  Enter Select  ·  Esc Close ",
    };
    base.into()
}

/// Read-only log row for `/ps` task detail — full width, no selection cursor.
fn float_log_line(label: &str, detail: &str, col_w: usize) -> Line<'static> {
    let text = if label.is_empty() {
        detail.to_string()
    } else {
        // stderr / error marker stays as a one-char prefix.
        format!("{label} {detail}")
    };
    let padded = pad_or_truncate(&text, col_w.max(1));
    let style = if label.is_empty() {
        Theme::slash_desc()
    } else {
        Theme::slash_item()
    };
    Line::from(Span::styled(padded, style))
}

/// Section header inside the subagent float (status strip / group title).
fn subagent_header_line(title: &str, col_w: usize) -> Line<'static> {
    let head_style = Style::default()
        .bg(Theme::PANEL)
        .fg(Theme::MUTED)
        .add_modifier(Modifier::ITALIC);
    let rule_style = Theme::hairline();
    let title_w = title.width();
    let rule_len = col_w.saturating_sub(title_w.saturating_add(1)).max(1);
    Line::from(vec![
        Span::styled(format!("{title} "), head_style),
        Span::styled("─".repeat(rule_len), rule_style),
    ])
}

/// Subagent list row: colored status badge · description · muted meta.
///
/// `label` is a status key: `run` / `ok` / `fail` / `stop` (or legacy glyphs).
fn subagent_item_line(
    label: &str,
    detail: &str,
    hint: &str,
    col_w: usize,
    active: bool,
) -> Line<'static> {
    let (glyph, badge, st_style, st_style_sel) = match label {
        "run" | "●" | "● run" | "● agent" => (
            "●",
            " RUN ",
            Theme::subagent_status_run(),
            Theme::subagent_status_run_sel(),
        ),
        "ok" | "✓" | "✓ ok" | "✓ agent" => (
            "✓",
            " OK  ",
            Theme::subagent_status_ok(),
            Theme::subagent_status_ok_sel(),
        ),
        "fail" | "✗" | "✗ fail" | "✗ agent" => (
            "✗",
            " FAIL",
            Theme::subagent_status_fail(),
            Theme::subagent_status_fail_sel(),
        ),
        "stop" | "■" | "■ stop" | "■ agent" => (
            "■",
            " STOP",
            Theme::subagent_status_stop(),
            Theme::subagent_status_stop_sel(),
        ),
        "·" | "—" => (
            "·",
            "  ·  ",
            Theme::subagent_status_stop(),
            Theme::subagent_status_stop_sel(),
        ),
        other => {
            // Fallback: show raw label as badge text.
            let raw = format!(" {other} ");
            return subagent_item_line_raw(raw.as_str(), detail, hint, col_w, active);
        }
    };

    let marker = if active { "› " } else { "  " };
    let badge_style = if active { st_style_sel } else { st_style };
    let name_style = if active {
        Theme::slash_selected()
    } else {
        Theme::slash_item()
    };
    let meta_style = if active {
        Theme::subagent_meta_chip_sel()
    } else {
        Theme::subagent_meta_chip()
    };
    let fill = if active {
        Theme::slash_selected()
    } else {
        Theme::slash_panel()
    };

    // marker(2) + badge(~5) + gap(1) + name + gap(1) + meta
    let badge_w = badge.width().max(5);
    let meta = if hint.is_empty() {
        String::new()
    } else {
        format!(" {hint}")
    };
    let meta_w = meta.width();
    let name_w = col_w
        .saturating_sub(2)
        .saturating_sub(badge_w)
        .saturating_sub(1)
        .saturating_sub(if meta_w == 0 { 0 } else { meta_w });
    let name = if detail.is_empty() {
        " ".repeat(name_w.max(1))
    } else {
        pad_or_truncate(detail, name_w.max(1))
    };
    let pad_w = col_w
        .saturating_sub(2)
        .saturating_sub(badge_w)
        .saturating_sub(1)
        .saturating_sub(name.width())
        .saturating_sub(meta_w);
    let pad = " ".repeat(pad_w);

    // Keep glyph in the badge text for terminals that color poorly; badge already has status word.
    let _ = glyph;
    let mut spans = vec![
        Span::styled(marker.to_string(), fill),
        Span::styled(badge.to_string(), badge_style),
        Span::styled(" ".to_string(), fill),
        Span::styled(name, name_style),
    ];
    if !meta.is_empty() {
        spans.push(Span::styled(pad, fill));
        spans.push(Span::styled(meta, meta_style));
    } else {
        spans.push(Span::styled(pad, fill));
    }
    Line::from(spans)
}

fn subagent_item_line_raw(
    badge: &str,
    detail: &str,
    hint: &str,
    col_w: usize,
    active: bool,
) -> Line<'static> {
    let marker = if active { "› " } else { "  " };
    let fill = if active {
        Theme::slash_selected()
    } else {
        Theme::slash_panel()
    };
    let name_style = if active {
        Theme::slash_selected()
    } else {
        Theme::slash_item()
    };
    let meta_style = if active {
        Theme::subagent_meta_chip_sel()
    } else {
        Theme::subagent_meta_chip()
    };
    let badge_w = badge.width().max(1);
    let meta = if hint.is_empty() {
        String::new()
    } else {
        format!(" {hint}")
    };
    let meta_w = meta.width();
    let name_w = col_w
        .saturating_sub(2)
        .saturating_sub(badge_w)
        .saturating_sub(1)
        .saturating_sub(if meta_w == 0 { 0 } else { meta_w });
    let name = pad_or_truncate(detail, name_w.max(1));
    let pad_w = col_w
        .saturating_sub(2)
        .saturating_sub(badge_w)
        .saturating_sub(1)
        .saturating_sub(name.width())
        .saturating_sub(meta_w);
    Line::from(vec![
        Span::styled(marker.to_string(), fill),
        Span::styled(badge.to_string(), name_style),
        Span::styled(" ".to_string(), fill),
        Span::styled(name, name_style),
        Span::styled(" ".repeat(pad_w), fill),
        Span::styled(meta, meta_style),
    ])
}

/// Colored live-log row for subagent detail (`→` / `✓` / `▸` / …).
fn subagent_log_line(label: &str, detail: &str, col_w: usize) -> Line<'static> {
    let (glyph, g_style, body_style) = match label {
        "→" => ("→", Theme::subagent_log_tool(), Theme::subagent_log_body()),
        "✓" => ("✓", Theme::subagent_log_ok(), Theme::subagent_log_muted()),
        "✗" | "!" => ("✗", Theme::subagent_log_err(), Theme::subagent_log_err()),
        "▸" => ("▸", Theme::subagent_log_meta(), Theme::subagent_log_meta()),
        "◂" => ("◂", Theme::subagent_log_muted(), Theme::subagent_log_muted()),
        "──" => (
            "─",
            Theme::hairline(),
            Theme::subagent_log_muted().add_modifier(Modifier::ITALIC),
        ),
        "·" => ("·", Theme::subagent_log_muted(), Theme::subagent_log_muted()),
        "" => (" ", Theme::slash_panel(), Theme::subagent_log_body()),
        other => {
            // Unknown marker: show as muted prefix.
            let text = if detail.is_empty() {
                other.to_string()
            } else {
                format!("{other} {detail}")
            };
            let padded = pad_or_truncate(&text, col_w.max(1));
            return Line::from(Span::styled(padded, Theme::subagent_log_muted()));
        }
    };

    if label == "──" {
        // Full-width soft divider with a small "summary" caption when detail is set.
        let cap = if detail.is_empty() {
            String::new()
        } else {
            format!(" {detail} ")
        };
        let cap_w = cap.width();
        let side = col_w.saturating_sub(cap_w).saturating_div(2).max(1);
        let left = "─".repeat(side);
        let right = "─".repeat(col_w.saturating_sub(side).saturating_sub(cap_w).max(1));
        return Line::from(vec![
            Span::styled(left, Theme::hairline()),
            Span::styled(cap, body_style),
            Span::styled(right, Theme::hairline()),
        ]);
    }

    // "  glyph  body…" with consistent 2-col gutter.
    let prefix = format!("  {glyph}  ");
    let body_w = col_w.saturating_sub(prefix.width()).max(1);
    let body = if detail.is_empty() {
        " ".repeat(body_w)
    } else {
        pad_or_truncate(detail, body_w)
    };
    let pad = " ".repeat(col_w.saturating_sub(prefix.width()).saturating_sub(body.width()));
    Line::from(vec![
        Span::styled(prefix, g_style),
        Span::styled(body, body_style),
        Span::styled(pad, Theme::slash_panel()),
    ])
}

fn float_filter_line(menu: &FloatMenu) -> Line<'static> {
    let (before, after) = menu.search_split_at_cursor();
    let before = before.to_string();
    let after = after.to_string();
    if menu.edit_mode {
        let prefix = if menu.edit_label.is_empty() {
            "edit: ".to_string()
        } else {
            format!("{}: ", menu.edit_label)
        };
        return Line::from(vec![
            Span::styled(prefix, Theme::float_edit_label()),
            Span::styled(before, Theme::float_edit_text()),
            Span::styled("▌", Theme::input_cursor_on()),
            Span::styled(after, Theme::float_edit_text()),
        ]);
    }
    // Active filter: elevated chip + caret (type-to-filter always available).
    Line::from(vec![
        Span::styled("Filter: ".to_string(), Theme::float_filter_label()),
        Span::styled(before, Theme::float_filter_active()),
        Span::styled("▌", Theme::input_cursor_on()),
        Span::styled(after, Theme::float_filter_active()),
    ])
}

fn float_header_line(title: &str, col_w: usize, editing: bool) -> Line<'static> {
    let head_style = if editing {
        Theme::float_dim()
    } else {
        Style::default()
            .bg(Theme::PANEL)
            .fg(Theme::MUTED)
            .add_modifier(Modifier::BOLD)
    };
    let rule_style = if editing {
        Theme::float_dim_desc()
    } else {
        Theme::hairline()
    };
    let title_w = title.width();
    let rule_len = col_w.saturating_sub(title_w.saturating_add(1)).max(1);
    Line::from(vec![
        Span::styled(format!("{title} "), head_style),
        Span::styled("╌".repeat(rule_len), rule_style),
    ])
}

/// Three-column row: marker+key | value (truncated) | [action]
fn float_item_line(
    label: &str,
    detail: &str,
    hint: &str,
    col_w: usize,
    active: bool,
    editing: bool,
    item_style: crate::float::FloatItemStyle,
) -> Line<'static> {
    use crate::float::FloatItemStyle;
    let is_action = matches!(item_style, FloatItemStyle::Action);
    // Action rows get a diamond marker so they read as CTAs, not data rows.
    let marker = if active {
        if is_action {
            "◆ "
        } else {
            "› "
        }
    } else if is_action {
        "◇ "
    } else {
        "  "
    };
    let action = format_float_action(hint);
    let action_w = action.width();

    // Key column ~25%, action ~15%, value takes the rest (min widths for small panels).
    let key_w = ((col_w * 25) / 100).clamp(12, 22);
    let action_slot = if action.is_empty() {
        0
    } else {
        action_w.saturating_add(1).max(9)
    };
    // marker(2) + key + gap(1) + value + gap(1) + action
    let value_w = col_w
        .saturating_sub(2)
        .saturating_sub(key_w)
        .saturating_sub(1)
        .saturating_sub(if action_slot == 0 { 0 } else { 1 + action_slot });

    let key = pad_or_truncate(label, key_w);
    let value = if detail.is_empty() {
        " ".repeat(value_w)
    } else {
        pad_or_truncate(detail, value_w)
    };

    let (key_style, value_style, action_style, fill_style) = if editing {
        (
            Theme::float_dim(),
            Theme::float_dim_desc(),
            Theme::float_dim_desc(),
            Theme::float_dim(),
        )
    } else if is_action && active {
        (
            Theme::float_cta_selected(),
            Theme::float_cta_selected_desc(),
            Theme::float_cta_chip_selected(),
            Theme::float_cta_selected(),
        )
    } else if is_action {
        (
            Theme::float_cta(),
            Theme::float_cta_desc(),
            Theme::float_cta_chip(),
            Theme::slash_panel(),
        )
    } else if active {
        (
            Theme::slash_selected(),
            Theme::slash_selected(),
            Theme::float_action_selected(),
            Theme::slash_selected(),
        )
    } else {
        (
            Theme::slash_item(),
            Theme::slash_desc(),
            Theme::float_action(),
            Theme::slash_panel(),
        )
    };

    let mut spans = vec![
        Span::styled(marker.to_string(), key_style),
        Span::styled(key, key_style),
        Span::styled(" ".to_string(), key_style),
        Span::styled(value, value_style),
    ];
    if !action.is_empty() {
        let gap = " ".repeat(1);
        // Right-align action inside its slot.
        let pad = action_slot.saturating_sub(action_w);
        spans.push(Span::styled(gap, value_style));
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), value_style));
        }
        spans.push(Span::styled(action, action_style));
    }

    // Pad to full width so selected / panel bg covers the entire row cell.
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    if used < col_w {
        spans.push(Span::styled(" ".repeat(col_w - used), fill_style));
    }

    Line::from(spans)
}

fn format_float_action(hint: &str) -> String {
    let h = hint.trim();
    if h.is_empty() {
        return String::new();
    }
    // Keep pure symbols (→) unbracketed; chip-wrap alphanumeric actions.
    if h.chars().any(|c| c.is_ascii_alphanumeric()) {
        format!("[{h}]")
    } else {
        h.to_string()
    }
}

