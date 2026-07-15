//! OpenCode-faithful chat chrome (dark `opencode` theme).
//!
//! - User: left peach rail + panel fill, no role tag
//! - Assistant: markdown body (headings, lists, code, tables), turn footer
//! - Tool: `⚙ name detail` inline row (running / muted / error)
//! - Prompt: left-border only + agent/model meta strip
//! - one-cli only feeds state; all paint is here

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::float::{FloatKind, FloatMenu, FloatRenderRow};
use crate::markdown;
use crate::message::{AlertLevel, Message, MessageRole, ToolStatus};
use crate::theme::Theme;
use crate::tool_view::{self, DiffLineKind};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
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
            Constraint::Min(3),           // transcript
            Constraint::Length(dock_h),   // select or `/` menu (0 when closed)
            Constraint::Length(prompt_h), // prompt box + agent meta
            Constraint::Length(1),        // footer
        ])
        .split(frame.area());

    app.tick_toast();

    draw_chat(frame, chunks[0], app);
    if select_h > 0 {
        draw_select_dock(frame, chunks[1], app);
    } else if slash_h > 0 {
        draw_slash_dock(frame, chunks[1], app);
    }
    draw_prompt(frame, chunks[2], app);
    draw_status(frame, chunks[3], app);

    // Top-right toast sits above chat (not the footer).
    draw_toast(frame, frame.area(), app);

    // Floating modal on top (Settings, sessions, …) — not used for `/`.
    if let Some(menu) = &app.float {
        draw_float_menu(frame, frame.area(), menu);
    }
}

/// Slash command list above the input (Claude Code / Codex style — see image.png).
///
/// ```text
/// /quit     Quit the application          ← highlighted
/// /help     Browse commands…
/// ─────────────────────────────────────
/// > /█
/// ```
fn draw_slash_dock(frame: &mut Frame<'_>, area: Rect, app: &App) {
    use crate::slash::PopupRow;

    let rows = app.popup_rows();
    if rows.is_empty() || area.height == 0 {
        return;
    }

    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Theme::slash_panel()), area);

    let max_w = area.width as usize;
    let visible = area.height as usize;
    let selected = app.slash_selected.min(rows.len().saturating_sub(1));

    // Scroll window so selection stays visible.
    let start = if rows.len() > visible {
        selected
            .saturating_sub(visible.saturating_sub(1) / 2)
            .min(rows.len().saturating_sub(visible))
    } else {
        0
    };
    let end = (start + visible).min(rows.len());

    let mut lines: Vec<Line> = Vec::new();
    for idx in start..end {
        let row = &rows[idx];
        let focused = idx == selected && row.selectable();
        let name = row.label();
        let desc = row.description();

        match row {
            PopupRow::Header(h) => {
                lines.push(Line::from(Span::styled(
                    truncate_mid(&format!(" {h}"), max_w),
                    Theme::slash_title(),
                )));
            }
            PopupRow::Command(_) | PopupRow::Model(_) => {
                // name left · description right (image layout)
                let name_w = UnicodeWidthStr::width(name.as_str()).clamp(10, 22);
                let name_col = format!(" {:<width$}", name, width = name_w);
                let used = UnicodeWidthStr::width(name_col.as_str());
                let rest = max_w.saturating_sub(used).saturating_sub(1);
                let desc_col = if rest > 2 && !desc.is_empty() {
                    format!(" {}", truncate_mid(&desc, rest.saturating_sub(1)))
                } else {
                    String::new()
                };
                let style = if focused {
                    Theme::slash_selected()
                } else {
                    Theme::slash_item()
                };
                let desc_style = if focused {
                    Theme::slash_selected()
                } else {
                    Theme::slash_desc()
                };
                lines.push(Line::from(vec![
                    Span::styled(name_col, style),
                    Span::styled(desc_col, desc_style),
                ]));
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Codex-style select list docked above the input (model / permission / ask / field edit).
fn draw_select_dock(frame: &mut Frame<'_>, area: Rect, app: &App) {
    use crate::select::SelectPhase;

    let Some(prompt) = app.select_prompt() else {
        return;
    };

    frame.render_widget(Clear, area);
    let title = format!(" {} ", prompt.title);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Theme::bg());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let max_w = (inner.width as usize).saturating_sub(2);
    let mut lines: Vec<Line> = Vec::new();

    for (i, line) in prompt.body.lines().enumerate() {
        let text = truncate_mid(line, max_w);
        if i == 0 {
            lines.push(Line::from(Span::styled(
                text,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    if !prompt.body.is_empty() {
        lines.push(Line::from(""));
    }

    // Scroll window if too many options for the dock height.
    let opt_n = prompt.option_count();
    let typing_rows = if matches!(prompt.phase, SelectPhase::Typing { .. }) {
        2
    } else {
        0
    };
    let fixed = lines.len() + 1 + typing_rows; // + footer
    let avail = (inner.height as usize).saturating_sub(fixed).max(1);
    let start = if opt_n > avail {
        prompt
            .selected
            .saturating_sub(avail.saturating_sub(1) / 2)
            .min(opt_n.saturating_sub(avail))
    } else {
        0
    };
    let end = (start + avail).min(opt_n);
    for idx in start..end {
        lines.push(select_option_line(prompt, idx, max_w));
    }

    if let SelectPhase::Typing { buffer } = &prompt.phase {
        lines.push(Line::from(Span::styled(
            truncate_mid(&prompt.other_label, max_w),
            Style::default().fg(Color::DarkGray),
        )));
        let input = format!("> {buffer}█");
        lines.push(Line::from(Span::styled(
            truncate_mid(&input, max_w),
            Style::default().fg(Color::White),
        )));
    }

    lines.push(Line::from(Span::styled(
        truncate_mid(&prompt.footer(), max_w),
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

fn select_option_line(
    prompt: &crate::select::SelectPrompt,
    idx: usize,
    max_w: usize,
) -> Line<'static> {
    use crate::select::SelectMode;

    let focused = prompt.selected == idx;
    let (mark, label, desc) = if prompt.is_other_row(idx) {
        let mark = match prompt.mode {
            SelectMode::Single => {
                if focused {
                    "(•)"
                } else {
                    "( )"
                }
            }
            SelectMode::Multi => {
                if focused {
                    "[•]"
                } else {
                    "[ ]"
                }
            }
        };
        (mark, prompt.other_label.as_str(), "")
    } else {
        let opt = &prompt.options[idx];
        let mark = match prompt.mode {
            SelectMode::Single => {
                if focused {
                    "(•)"
                } else {
                    "( )"
                }
            }
            SelectMode::Multi => {
                if prompt.checked.contains(&idx) {
                    "[x]"
                } else {
                    "[ ]"
                }
            }
        };
        (mark, opt.label.as_str(), opt.description.as_str())
    };

    let num = idx + 1;
    let main = if desc.is_empty() {
        format!("{num} {mark} {label}")
    } else {
        // Keep description on same line when short; truncate together.
        format!("{num} {mark} {label}")
    };
    let style = if focused {
        Style::default()
            .bg(Color::Rgb(48, 48, 48))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Line::from(Span::styled(truncate_mid(&main, max_w), style))
}

fn truncate_mid(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let w = UnicodeWidthStr::width(s);
    if w <= max {
        return s.to_string();
    }
    if max <= 3 {
        return "…".to_string();
    }
    let keep = max - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + cw > keep {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Ephemeral top-right toast — UI only, never agent context.
fn draw_toast(frame: &mut Frame<'_>, full: Rect, app: &App) {
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

    let (border_fg, title_bg) = match toast.level {
        AlertLevel::Error => (Theme::ERROR, Theme::ERROR),
        AlertLevel::Warn => (Theme::WARNING, Theme::WARNING),
        AlertLevel::Info => (Theme::SECONDARY, Theme::SECONDARY),
    };

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(border_fg))
        .style(Style::default().bg(Theme::PANEL))
        .title(Span::styled(
            " notice ",
            Style::default().fg(Theme::BG).bg(title_bg),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let body: Vec<Line> = lines
        .into_iter()
        .map(|l| {
            Line::from(Span::styled(
                format!(" {l}"),
                Style::default().fg(Theme::FG).bg(Theme::PANEL),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(body).style(Theme::bg()), inner);
}

/// Centered floating panel — dim backdrop + bordered menu with search & groups.
fn draw_float_menu(frame: &mut Frame<'_>, full: Rect, menu: &FloatMenu) {
    // Soft dim: re-paint full area with a translucent-feel dark overlay (solid bg).
    // True alpha isn't available in terminals; use a dark panel wash.
    let dim = Block::default().style(Style::default().bg(Color::Rgb(0x0a, 0x0a, 0x0a)));
    // Only darken by drawing a semi-empty overlay is hard; skip full dim, just center card.

    let width = full.width.saturating_mul(7) / 10; // ~70%
    let width = width.clamp(40, full.width.saturating_sub(4));
    let render_rows = menu.render_rows();
    // title+search+sep + rows + footer  ≈ content height
    let content_h = (render_rows.len() as u16).min(16).saturating_add(5);
    let height = content_h.clamp(10, full.height.saturating_sub(2));

    let x = full.x + (full.width.saturating_sub(width)) / 2;
    let y = full.y + (full.height.saturating_sub(height)) / 2;
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    // Clear under the float so chat doesn't bleed through.
    frame.render_widget(Clear, area);

    let title = format!(" {} ", menu.title);
    let footer = if menu.edit_mode {
        " type value  ·  Enter save  ·  Esc cancel "
    } else {
        match menu.kind {
            FloatKind::Info => " Enter / Esc close ",
            FloatKind::Sessions => " ↑/↓  ·  Enter resume  ·  Esc  ·  type to filter ",
            FloatKind::Tree => " ↑/↓  ·  Enter branch  ·  Esc  ·  type to filter ",
            FloatKind::Rewind => " ↑/↓  ·  Enter edit prompt  ·  Esc  ·  type to filter ",
            FloatKind::Thinking => " ↑/↓  ·  Enter set level  ·  Esc ",
            FloatKind::Help => " ↑/↓  ·  Enter open  ·  Esc  ·  type to search ",
            FloatKind::Models => " ↑/↓  ·  Enter switch  ·  Esc  ·  type to search ",
            FloatKind::Settings => " ↑/↓  ·  Enter  ·  Esc close  ·  type to filter ",
            FloatKind::SettingsModels => {
                " ↑/↓  ·  Enter  ·  Ctrl+F fetch  ·  Esc/← back  ·  type to filter "
            }
            FloatKind::SettingsProviders
            | FloatKind::SettingsProviderDetail
            | FloatKind::SettingsProviderApi
            | FloatKind::SettingsRemoteModels
            | FloatKind::SettingsModelDetail => " ↑/↓  ·  Enter  ·  Esc/← back  ·  type to filter ",
            FloatKind::SettingsModelAdd => {
                " ↑/↓ fields  ·  Enter edit/save  ·  Esc/← back  ·  type in search "
            }
            FloatKind::Commands | FloatKind::Custom => {
                " ↑/↓  ·  Enter select  ·  Esc close  ·  type to search "
            }
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Theme::border())
        .style(Theme::slash_panel())
        .title(Span::styled(title, Theme::title()))
        .title_bottom(Span::styled(footer, Theme::slash_title()));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Layout inside: search (1) + list (rest)
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(3)])
        .split(inner);

    // Search / in-float edit row
    let search_prefix = if menu.edit_mode {
        if menu.edit_label.is_empty() {
            "  edit: ".to_string()
        } else {
            format!("  {}: ", menu.edit_label)
        }
    } else {
        "  search: ".to_string()
    };
    let search_line = if menu.search.is_empty() {
        Line::from(vec![
            Span::styled(search_prefix, Theme::slash_desc()),
            Span::styled(if menu.edit_mode { "▌" } else { "…" }, Theme::slash_desc()),
        ])
    } else {
        Line::from(vec![
            Span::styled(search_prefix, Theme::slash_desc()),
            Span::styled(menu.search.as_str(), Theme::slash_item()),
            Span::styled("▌", Theme::input_cursor_on()),
        ])
    };
    frame.render_widget(
        Paragraph::new(vec![
            search_line,
            Line::from(Span::styled(
                "  ".to_string() + &"─".repeat(parts[0].width.saturating_sub(2) as usize),
                Theme::slash_desc(),
            )),
        ])
        .style(Theme::slash_panel()),
        parts[0],
    );

    // List with scroll around selected
    let list_area = parts[1];
    let max_rows = list_area.height as usize;
    // Map selected entry_index → row index for scroll
    let selected_row = render_rows
        .iter()
        .position(|r| match r {
            FloatRenderRow::Item { entry_index, .. } => *entry_index == menu.selected,
            _ => false,
        })
        .unwrap_or(0);
    let start = selected_row.saturating_sub(max_rows.saturating_sub(1));
    let end = (start + max_rows).min(render_rows.len());

    let mut lines: Vec<Line> = Vec::new();
    for row in &render_rows[start..end] {
        match row {
            FloatRenderRow::Header(title) => {
                lines.push(Line::from(vec![
                    Span::styled("  ", Theme::slash_panel()),
                    Span::styled(
                        format!("{title} "),
                        Style::default()
                            .bg(Theme::PANEL)
                            .fg(Theme::MUTED)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "─".repeat(
                            list_area
                                .width
                                .saturating_sub(4 + title.len() as u16)
                                .max(1) as usize,
                        ),
                        Theme::slash_desc(),
                    ),
                ]));
            }
            FloatRenderRow::Item {
                entry_index,
                label,
                detail,
                hint,
            } => {
                let active = *entry_index == menu.selected;
                let marker = if active { "› " } else { "  " };
                let name_style = if active {
                    Theme::slash_selected()
                } else {
                    Theme::slash_item()
                };
                let desc_style = if active {
                    Theme::slash_selected()
                } else {
                    Theme::slash_desc()
                };
                // label left, detail mid, hint right (best-effort in one line)
                let mut spans = vec![
                    Span::styled(marker, name_style),
                    Span::styled(format!("{label:<20}"), name_style),
                ];
                if !detail.is_empty() {
                    spans.push(Span::styled(format!("  {detail}"), desc_style));
                }
                // pad then hint
                let used = 2
                    + 20
                    + if detail.is_empty() {
                        0
                    } else {
                        2 + detail.chars().count()
                    };
                let pad = list_area
                    .width
                    .saturating_sub(used as u16)
                    .saturating_sub(hint.chars().count() as u16 + 2);
                if !hint.is_empty() {
                    spans.push(Span::styled(
                        format!("{:>width$}", hint, width = pad as usize + hint.len()),
                        desc_style,
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Theme::slash_desc(),
        )));
    }

    frame.render_widget(Paragraph::new(lines).style(Theme::slash_panel()), list_area);

    let _ = dim; // reserved if we add backdrop later
}

fn draw_chat(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    // Outer padding like OpenCode session (paddingLeft/Right ~2).
    let pad = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let area = pad[1];
    let wrap_width = area.width.max(16) as usize;
    let view_h = area.height as usize;

    // Flatten every message into display lines, then window by **line** offset.
    // Full history stays in `app.messages`; we only paint a viewport slice.
    let (all_lines, owners) = build_chat_lines(app, wrap_width);
    let total = all_lines.len();
    let max_from_bottom = total.saturating_sub(view_h);

    // Publish metrics so PgUp/Home know page size / max offset.
    app.chat_view_height = view_h;
    app.chat_total_lines = total;
    app.chat_line_owners = owners;
    app.chat_line_text = all_lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();

    let empty_welcome = app.messages.is_empty() && !app.busy;

    if empty_welcome && app.follow_bottom {
        // Fresh session / after `/clear`: pin welcome to the top (title first).
        // Keep follow_bottom false so later PgDn can reveal lower tips.
        app.follow_bottom = false;
        app.chat_scroll = max_from_bottom;
    } else if app.follow_bottom {
        app.chat_scroll = 0;
    } else {
        // Keep offset in range so PageDown can re-stick without a huge backlog.
        app.chat_scroll = app.chat_scroll.min(max_from_bottom);
        if app.chat_scroll == 0 && !empty_welcome {
            app.follow_bottom = true;
        }
    }

    let from_bottom = if app.follow_bottom || view_h == 0 {
        0
    } else {
        app.chat_scroll
    };
    // start = first visible line index from the top of the transcript.
    let start = max_from_bottom.saturating_sub(from_bottom);
    let end = (start + view_h).min(total);
    app.chat_view_start = start;
    // New / short chats start at the top of the pane (0,0) — not pinned to the prompt.
    app.chat_top_pad = 0;
    let sel = app.selection_range();
    let window: Vec<Line<'static>> = if start < end {
        all_lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let abs = start + i;
                if sel.is_some_and(|(lo, hi)| abs >= lo && abs <= hi) {
                    highlight_line(line)
                } else {
                    line.clone()
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    frame.render_widget(Paragraph::new(window).style(Theme::bg()), area);
}

/// Paint a line with selection background (keeps glyph content).
fn highlight_line(line: &Line<'static>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|s| {
            let mut style = s.style;
            // Force readable selection colors; keep bold/italic if present.
            style = style.patch(Theme::selection());
            Span::styled(s.content.clone(), style)
        })
        .collect();
    if spans.is_empty() {
        Line::from(Span::styled(" ", Theme::selection()))
    } else {
        Line::from(spans)
    }
}

/// Build the full chat transcript as terminal lines (wrap-aware).
/// Also returns per-line message ownership for click targeting.
fn build_chat_lines(app: &App, wrap_width: usize) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut owners: Vec<Option<usize>> = Vec::new();

    let push_owned = |lines: &mut Vec<Line<'static>>,
                      owners: &mut Vec<Option<usize>>,
                      chunk: Vec<Line<'static>>,
                      owner: Option<usize>| {
        for line in chunk {
            lines.push(line);
            owners.push(owner);
        }
    };

    // Fresh session: fill the empty pane with a soft welcome + tips.
    if app.messages.is_empty() && !app.busy {
        push_owned(
            &mut lines,
            &mut owners,
            empty_state_lines(app, wrap_width),
            None,
        );
        return (lines, owners);
    }

    let mut i = 0;
    while i < app.messages.len() {
        let msg = &app.messages[i];
        if msg.role == MessageRole::Tool {
            let streak = tool_view::tool_streak_len(&app.messages, i);
            if tool_view::streak_can_collapse(&app.messages, i, streak) {
                // Blank before group (unless at very top).
                if !lines.is_empty() {
                    lines.push(Line::from(Span::styled("", Theme::bg())));
                    owners.push(None);
                }
                let group_lines = render_tool_group(&app.messages[i..i + streak], wrap_width);
                // Entire chip belongs to first tool index for click expand.
                push_owned(&mut lines, &mut owners, group_lines, Some(i));
                i += streak;
                continue;
            }
            // Tight stack: no blank between consecutive tools.
            for (k, tmsg) in app.messages[i..i + streak].iter().enumerate() {
                if k == 0 && !lines.is_empty() {
                    // Blank before the stack starts (separate from prior user/assistant).
                    let prev_was_tool = i > 0 && app.messages[i - 1].role == MessageRole::Tool;
                    if !prev_was_tool {
                        lines.push(Line::from(Span::styled("", Theme::bg())));
                        owners.push(None);
                    }
                }
                let chunk = message_lines(tmsg, app, wrap_width);
                push_owned(&mut lines, &mut owners, chunk, Some(i + k));
            }
            i += streak;
            continue;
        }

        if !lines.is_empty() {
            lines.push(Line::from(Span::styled("", Theme::bg())));
            owners.push(None);
        }
        let chunk = message_lines(msg, app, wrap_width);
        let owner = if matches!(
            msg.role,
            MessageRole::Alert | MessageRole::Thinking | MessageRole::Tool
        ) {
            Some(i)
        } else {
            None
        };
        push_owned(&mut lines, &mut owners, chunk, owner);
        i += 1;
    }

    // Spinner while waiting for first token, or while only thinking has started.
    if app.busy && app.stream_buffer.is_empty() && app.thinking_buffer.is_empty() {
        let show = app.messages.last().map(|m| !m.streaming).unwrap_or(true);
        if show {
            if !lines.is_empty() {
                lines.push(Line::from(""));
                owners.push(None);
            }
            let spin = SPINNER[app.spinner_frame % SPINNER.len()];
            let label = if app.thinking_level != "off" {
                "Thinking…"
            } else {
                "Working…"
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{spin} "), Theme::prompt_bar()),
                Span::styled(label, Theme::busy()),
            ]));
            owners.push(None);
        }
    }

    debug_assert_eq!(lines.len(), owners.len());
    (lines, owners)
}

/// Empty-session welcome: title, context, tips — kept short so it fits a
/// typical chat pane without scrolling away the title.
fn empty_state_lines(app: &App, wrap_width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let blank = || Line::from(Span::styled("", Theme::bg()));
    let muted = |s: String| Line::from(vec![Span::raw("  "), Span::styled(s, Theme::meta())]);
    let tip_row = |parts: &[(&str, &str)]| {
        let mut spans = vec![Span::raw("  ")];
        for (i, (key, desc)) in parts.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("   ", Theme::meta()));
            }
            spans.push(Span::styled(format!("{key} "), Theme::status_key()));
            spans.push(Span::styled((*desc).to_string(), Theme::meta()));
        }
        Line::from(spans)
    };

    lines.push(blank());
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "one",
            Style::default()
                .fg(Theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  coding agent", Theme::meta()),
    ]));

    let agent = if app.agent_label.is_empty() {
        "Build"
    } else {
        app.agent_label.as_str()
    };
    let mut ctx = format!("{agent}");
    if !app.current_model.is_empty() {
        ctx.push_str("  ·  ");
        ctx.push_str(&app.current_model);
    } else if !app.mode_label.is_empty() {
        ctx.push_str("  ·  ");
        ctx.push_str(&app.mode_label);
    }
    if !app.current_provider.is_empty() {
        ctx.push_str("  ·  ");
        ctx.push_str(&app.current_provider);
    }
    lines.push(muted(ctx));
    lines.push(blank());

    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Describe a task to get started — tools run when needed.",
            Theme::assistant_body(),
        ),
    ]));
    lines.push(blank());

    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "tips",
            Style::default()
                .fg(Theme::MUTED)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(tip_row(&[
        ("/", "commands"),
        ("Space", "Plan ↔ Build"),
        ("Ctrl+L", "model"),
    ]));
    lines.push(tip_row(&[
        ("Ctrl+J", "newline"),
        ("Esc Esc", "rewind"),
        ("↑↓", "history"),
    ]));
    lines.push(tip_row(&[
        ("/help", "more"),
        ("/model", "switch"),
        ("/resume", "sessions"),
    ]));
    lines.push(blank());

    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "try",
            Style::default()
                .fg(Theme::MUTED)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    for (i, example) in [
        "list files in this directory",
        "explain how the agent loop works",
        "fix the failing tests",
    ]
    .into_iter()
    .enumerate()
    {
        // Peach number chip — solid bg, no emoji/circled-digit tofu.
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!(" {} ", i + 1), Theme::badge_primary()),
            Span::styled(format!("  \"{example}\""), Theme::meta()),
        ]));
    }
    lines.push(blank());

    let footer = "paste image/text · drag to copy · /quit to exit";
    if wrap_width > 0 && display_width(footer) + 2 > wrap_width {
        for part in wrap_str(footer, wrap_width.saturating_sub(2)) {
            lines.push(muted(part));
        }
    } else {
        lines.push(muted(footer.into()));
    }
    lines
}

/// Collapsed multi-tool chip (soft chip, not a raw text dump).
///
/// ```text
///   ▸  3 tools   read · bash · edit
/// ```
fn render_tool_group(tools: &[Message], wrap_width: usize) -> Vec<Line<'static>> {
    let n = tools.len();
    let mut labels: Vec<String> = tools
        .iter()
        .map(|t| t.tool_name.clone().unwrap_or_else(|| "tool".into()))
        .collect();
    let budget = wrap_width.saturating_sub(16).max(12);
    let mut joined = labels.join("  ·  ");
    if display_width(&joined) > budget {
        while labels.len() > 1 && display_width(&joined) > budget {
            labels.pop();
            joined = format!("{}  ·  +{}", labels.join("  ·  "), n - labels.len());
        }
        if display_width(&joined) > budget {
            joined = truncate_display(&joined, budget);
        }
    }
    vec![Line::from(vec![
        Span::raw("  "),
        Span::styled("▸", Theme::tool_icon_done()),
        Span::styled(format!("  {n} tools  "), Theme::tool_group_title()),
        Span::styled(joined, Theme::tool_group()),
        Span::styled("   ↵", Theme::meta()),
    ])]
}

fn message_lines(message: &Message, app: &App, wrap_width: usize) -> Vec<Line<'static>> {
    match message.role {
        MessageRole::User => render_user(&message.content, wrap_width),
        MessageRole::Alert => render_alert(message, wrap_width),
        MessageRole::Thinking => render_thinking(message, app, wrap_width),
        MessageRole::Assistant => {
            let mut lines = render_assistant(
                &message.content,
                message.streaming,
                app.cursor_on,
                wrap_width,
            );
            if !message.streaming {
                if let Some(footer) = &message.footer {
                    // Soft turn meta: muted hairline + peach mode glyph.
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("╰ ", Theme::meta()),
                        Span::styled(footer.clone(), Theme::meta()),
                    ]));
                }
            }
            lines
        }
        MessageRole::System => render_system(&message.content, wrap_width),
        MessageRole::Tool => render_tool(message, app, wrap_width),
    }
}

/// While thinking is streaming, only keep the rolling tail so long chains
/// don't flood the transcript (last N wrapped lines).
const THINKING_STREAM_TAIL_LINES: usize = 3;

/// Thinking / reasoning block — collapsible, muted.
///
/// ```text
///   ▸ thinking · 128 chars   ↵/click   (finished, default collapsed)
///   ▾ thinking …                       (streaming: last 3 wrapped lines)
///     …
///   ▾ thinking · 128 chars             (expanded full body)
///     …
/// ```
fn render_thinking(message: &Message, app: &App, wrap_width: usize) -> Vec<Line<'static>> {
    let chars = message.content.chars().count();
    // Live stream always shows a short tail; finished blocks honor per-message
    // expand (click/↵) or the global Ctrl+T default (`show_thinking`).
    let expanded = message.streaming || message.thinking_expanded;
    let chevron = if expanded { "▾" } else { "▸" };
    let mut lines = Vec::new();

    if expanded {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(chevron, Theme::thinking_chevron()),
            Span::styled(" thinking", Theme::thinking_title()),
            if message.streaming {
                Span::styled(" …", Theme::thinking_meta())
            } else {
                Span::styled(format!(" · {chars} chars"), Theme::thinking_meta())
            },
            // Expanded finished blocks: hint how to collapse again.
            if !message.streaming {
                Span::styled("   ↵/click", Theme::meta())
            } else {
                Span::raw("")
            },
        ]));
        let budget = wrap_width.saturating_sub(4).max(8);
        let mut body = wrap_paragraphs(&message.content, budget);
        // Live stream: rolling window of the last few lines only.
        if message.streaming && body.len() > THINKING_STREAM_TAIL_LINES {
            body = body[body.len() - THINKING_STREAM_TAIL_LINES..].to_vec();
        }
        let last = body.len().saturating_sub(1);
        if body.is_empty() {
            // Empty so far — reserve one row with a stable-width caret (no vertical jump).
            if message.streaming {
                let caret = if app.cursor_on {
                    Span::styled("▌", Theme::cursor())
                } else {
                    Span::raw(" ")
                };
                lines.push(Line::from(vec![Span::raw("    "), caret]));
            }
        } else {
            for (i, line) in body.into_iter().enumerate() {
                let mut spans = vec![
                    Span::raw("    "),
                    Span::styled(line, Theme::thinking_body()),
                ];
                // Inline caret on the last line (same pattern as assistant stream).
                // Always reserve width so blink does not shift layout.
                if message.streaming && i == last {
                    if app.cursor_on {
                        spans.push(Span::styled(" ▌", Theme::cursor()));
                    } else {
                        spans.push(Span::raw("  "));
                    }
                }
                lines.push(Line::from(spans));
            }
        }
    } else {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(chevron, Theme::thinking_chevron()),
            Span::styled(" thinking", Theme::thinking_title()),
            Span::styled(format!(" · {chars} chars"), Theme::thinking_meta()),
            Span::styled("   ↵/click", Theme::meta()),
        ]));
    }
    lines
}

/// User: peach left rail + soft panel fill (tight, no empty pad rows).
fn render_user(content: &str, wrap_width: usize) -> Vec<Line<'static>> {
    let budget = wrap_width.saturating_sub(3).max(8);
    let wrapped = wrap_paragraphs(content, budget);
    let mut out = Vec::with_capacity(wrapped.len().max(1));

    if wrapped.is_empty() {
        out.push(Line::from(vec![
            Span::styled("▌", Theme::user_bar()),
            Span::styled(" ", Theme::user_pad()),
            Span::styled(" ".repeat(budget), Theme::user_pad()),
        ]));
        return out;
    }

    for line in &wrapped {
        let pad_len = budget.saturating_sub(display_width(line));
        out.push(Line::from(vec![
            Span::styled("▌", Theme::user_bar()),
            Span::styled(" ", Theme::user_pad()),
            Span::styled(line.clone(), Theme::user_body()),
            Span::styled(" ".repeat(pad_len), Theme::user_pad()),
        ]));
    }

    out
}

/// Assistant: soft indent + full markdown (tables, code, lists, …) + streaming caret.
fn render_assistant(
    content: &str,
    streaming: bool,
    cursor_on: bool,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    // 2-space indent leaves room without crowding the user bubble.
    let budget = wrap_width.saturating_sub(2).max(8);
    let mut out = Vec::new();

    if content.trim().is_empty() {
        if streaming {
            let caret = if cursor_on {
                Span::styled("▌", Theme::cursor())
            } else {
                Span::raw(" ")
            };
            out.push(Line::from(vec![Span::raw("  "), caret]));
        }
        return out;
    }

    let md_lines = markdown::render(content, budget);
    // Drop a single trailing blank line so the turn footer sits tighter.
    let mut md_lines = md_lines;
    while md_lines
        .last()
        .is_some_and(|l| l.spans.is_empty() || l.spans.iter().all(|s| s.content.trim().is_empty()))
    {
        md_lines.pop();
    }

    let last = md_lines.len().saturating_sub(1);
    for (i, line) in md_lines.into_iter().enumerate() {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(line.spans);
        if streaming && i == last {
            if cursor_on {
                spans.push(Span::styled(" ▌", Theme::cursor()));
            } else {
                spans.push(Span::raw("  "));
            }
        }
        out.push(Line::from(spans));
    }
    out
}

fn render_system(content: &str, wrap_width: usize) -> Vec<Line<'static>> {
    // Compaction / meta style: subtle top rule for multi-word notices, else faint line.
    let budget = wrap_width.saturating_sub(4).max(8);
    let mut out = Vec::new();

    if content.eq_ignore_ascii_case("compaction") || content.starts_with("──") {
        out.push(Line::from(Span::styled(
            format!(" {}", "─".repeat(wrap_width.saturating_sub(2).min(40))),
            Theme::meta(),
        )));
        return out;
    }

    for (i, line) in wrap_paragraphs(content, budget).into_iter().enumerate() {
        let lead = if i == 0 { "   " } else { "   " };
        out.push(Line::from(vec![
            Span::raw(lead),
            Span::styled(line, Theme::system_body()),
        ]));
    }
    out
}

/// Tool row — OpenCode-ish hierarchy with clear tree + status color.
///
/// ```text
///   ✓ bash  cargo test
///     └ exit 0 · 12 lines
///   ✗ bash  false
///     ├ exit 1
///     └ boom
/// ```
fn render_tool(message: &Message, app: &App, wrap_width: usize) -> Vec<Line<'static>> {
    let name = message.tool_name.clone().unwrap_or_else(|| "tool".into());
    let detail = message.content.trim();
    let status = message.tool_status.unwrap_or(ToolStatus::Done);

    let (icon, icon_style) = match status {
        ToolStatus::Running => {
            let spin = SPINNER[app.spinner_frame % SPINNER.len()];
            (spin.to_string(), Theme::tool_icon_running())
        }
        ToolStatus::Done => ("✓".into(), Theme::tool_icon_done()),
        ToolStatus::Error => ("✗".into(), Theme::tool_icon_error()),
    };

    // Kind-colored name when done; peach when running; red when error.
    let name_style = match status {
        ToolStatus::Running => Theme::tool_name_running(),
        ToolStatus::Error => Theme::tool_name_error(),
        ToolStatus::Done => Theme::tool_kind(&name),
    };
    let detail_style = match status {
        ToolStatus::Running => Theme::tool_detail_running(),
        ToolStatus::Error => Theme::tool_text_error(),
        ToolStatus::Done => Theme::tool_detail_done(),
    };

    let name_w = display_width(&name).max(4).min(10);
    let budget = wrap_width.saturating_sub(4 + name_w + 2).max(8);
    let pretty = if detail.is_empty() {
        String::new()
    } else {
        truncate_display(&pretty_tool_args(detail), budget)
    };

    let mut lines = Vec::new();
    // Header:  `  ✓ bash  cargo test`
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(format!("{icon} "), icon_style),
        Span::styled(format!("{name:<name_w$}"), name_style),
    ];
    if !pretty.is_empty() {
        spans.push(Span::styled(format!("  {pretty}"), detail_style));
    }
    lines.push(Line::from(spans));

    let show_summary = message
        .tool_summary
        .as_ref()
        .is_some_and(|_s| !message.tool_expanded || message.tool_output.is_none());
    if show_summary {
        if let Some(summary) = message.tool_summary.as_deref() {
            let sum_style = if status == ToolStatus::Error {
                Theme::tool_summary_err()
            } else if summary.starts_with("exit 0") || summary.starts_with("ok") {
                Theme::tool_summary_ok()
            } else {
                Theme::tool_detail_done()
            };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled("└ ", Theme::tool_tree()),
                Span::styled(
                    truncate_display(summary, wrap_width.saturating_sub(8)),
                    sum_style,
                ),
            ]));
        }
    }

    // Expanded body with proper tree rails (├ / └), not a floating │ dump.
    // Caps are generous: the main chat is line-scrolled (full viewport), so long
    // tool output should participate in that scroll instead of feeling "clipped".
    if message.tool_expanded {
        if let Some(output) = message.tool_output.as_deref() {
            let body_budget = wrap_width.saturating_sub(8).max(12);
            let is_diff = tool_view::looks_like_diff(output);
            let max_lines = if status == ToolStatus::Error {
                40
            } else if is_diff {
                48
            } else {
                60
            };
            let default_style = if status == ToolStatus::Error {
                Theme::error_body()
            } else {
                Theme::tool_detail_done()
            };
            let rail_style = if status == ToolStatus::Error {
                Theme::error_bar()
            } else {
                Theme::tool_tree()
            };

            // Flatten wrapped lines first so tree tips land on the true last visual row.
            let mut visual: Vec<(String, Style)> = Vec::new();
            let raw_lines: Vec<&str> = output.lines().collect();
            let total_raw = raw_lines.len();
            for line in raw_lines.iter().take(max_lines) {
                let style = if is_diff {
                    match tool_view::classify_diff_line(line) {
                        DiffLineKind::Add => Theme::diff_add(),
                        DiffLineKind::Del => Theme::diff_del(),
                        DiffLineKind::Meta => Theme::diff_meta(),
                        DiffLineKind::Context | DiffLineKind::Plain => default_style,
                    }
                } else if status == ToolStatus::Error {
                    Theme::error_body()
                } else if line.starts_with("exit 0") {
                    Theme::tool_summary_ok()
                } else if line.starts_with("exit ") {
                    Theme::tool_summary_err()
                } else {
                    default_style
                };
                for wrapped in wrap_str(line, body_budget) {
                    visual.push((wrapped, style));
                }
            }
            if total_raw > max_lines {
                visual.push((format!("… +{} lines", total_raw - max_lines), Theme::meta()));
            }

            let last = visual.len().saturating_sub(1);
            for (i, (text, style)) in visual.into_iter().enumerate() {
                let branch = if i == last { "└ " } else { "│ " };
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(branch, rail_style),
                    Span::styled(text, style),
                ]));
            }
        }
    }

    lines
}

/// UI-only alert card mid-transcript (not LLM context).
fn render_alert(message: &Message, wrap_width: usize) -> Vec<Line<'static>> {
    let level = message.alert_level.unwrap_or(AlertLevel::Info);
    let (tag, bar, body, tag_bg) = match level {
        AlertLevel::Error => (
            " error ",
            Theme::error_bar(),
            Theme::error_body(),
            Theme::ERROR,
        ),
        AlertLevel::Warn => (
            " warn  ",
            Style::default().fg(Theme::WARNING),
            Style::default().fg(Theme::WARNING).bg(Theme::PANEL),
            Theme::WARNING,
        ),
        AlertLevel::Info => (
            " info  ",
            Theme::meta(),
            Theme::system_body(),
            Theme::BORDER_ACTIVE,
        ),
    };
    let budget = wrap_width.saturating_sub(6).max(12);
    let mut out = Vec::new();
    out.push(Line::from(vec![
        Span::styled("  ", Theme::bg()),
        Span::styled(tag, Style::default().fg(Theme::BG).bg(tag_bg)),
    ]));
    for line in wrap_paragraphs(&message.content, budget) {
        out.push(Line::from(vec![
            Span::styled("  ", Theme::bg()),
            Span::styled("┃ ", bar),
            Span::styled(line, body),
        ]));
    }
    out
}

/// Truncate by **display width** (CJK-safe), append … if needed.
fn truncate_display(s: &str, max_cols: usize) -> String {
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    let limit = max_cols.saturating_sub(1);
    for ch in s.chars() {
        let cw = char_width(ch);
        if w + cw > limit {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Soften `{"path":"foo"}` → `foo` for common tools.
fn pretty_tool_args(s: &str) -> String {
    let t = s.trim();
    if t.starts_with('{') && t.ends_with('}') {
        // Try to pull "path" / "command" / "pattern" / "file_path" values.
        for key in ["path", "file_path", "command", "pattern", "query", "url"] {
            let needle = format!("\"{key}\"");
            if let Some(idx) = t.find(&needle) {
                let after = &t[idx + needle.len()..];
                if let Some(colon) = after.find(':') {
                    let rest = after[colon + 1..].trim();
                    if let Some(val) = json_string_value(rest) {
                        return val;
                    }
                }
            }
        }
    }
    t.to_string()
}

fn json_string_value(s: &str) -> Option<String> {
    let s = s.trim();
    if !s.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = s[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '"' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

fn wrap_paragraphs(content: &str, width: usize) -> Vec<String> {
    if content.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for (pi, para) in content.split('\n').enumerate() {
        if pi > 0 && para.is_empty() {
            out.push(String::new());
            continue;
        }
        let wrapped = wrap_str(para, width);
        if wrapped.is_empty() {
            out.push(String::new());
        } else {
            out.extend(wrapped);
        }
    }
    out
}

/// Soft-wrap by **terminal columns** (CJK = 2). Never split mid-grapheme.
fn wrap_str(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    if text.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut cur_w = 0usize;

    // Prefer breaking on spaces when possible.
    for word in text.split_inclusive(' ') {
        let ww = display_width(word);
        if cur_w > 0 && cur_w + ww > width {
            out.push(std::mem::take(&mut current));
            cur_w = 0;
        }
        if ww > width {
            // Hard-split overlong token by columns.
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
                cur_w = 0;
            }
            for ch in word.chars() {
                let cw = char_width(ch);
                if cur_w > 0 && cur_w + cw > width {
                    out.push(std::mem::take(&mut current));
                    cur_w = 0;
                }
                current.push(ch);
                cur_w += cw;
            }
        } else {
            current.push_str(word);
            cur_w += ww;
        }
    }
    if !current.is_empty() {
        // Trim trailing spaces from visual lines for cleaner look.
        out.push(current.trim_end().to_string());
    }
    out
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn char_width(ch: char) -> usize {
    UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4])).max(1)
}

/// Prompt — soft left bar + multi-line input + software typewriter caret.
///
/// ```text
/// │              ← top padding (not input)
/// │  hello ▌     ← indent + text + blinking software caret
/// │              ← bottom padding
///   Build  deepseek-v4-flash  opencode
/// ```
///
/// Caret sits on a **dedicated 1-column slot** after the text (or before the
/// placeholder when empty). Hardware cursor stays hidden.
fn draw_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let input_lines_n = app.input_line_count() as u16;
    let box_h = (input_lines_n + 2).clamp(3, 8); // top pad + lines + bottom pad
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(box_h), Constraint::Min(1)])
        .split(area);
    let box_area = rows[0];
    let meta_area = rows[1];

    let bar_style = if app.busy {
        Theme::prompt_bar_busy()
    } else {
        Theme::prompt_bar()
    };

    let placeholder = if app.busy {
        "steer or follow-up…"
    } else {
        "Message…  / commands  ^J newline  Ctrl+L model  Ctrl+G settings"
    };

    const INDENT: &str = "  ";

    // Software caret (▌) so the typewriter is visible even when the hardware
    // I-beam is hidden by the emulator / tmux / mouse reporting.
    let caret = if app.cursor_on {
        Span::styled("▌", Theme::input_cursor_on())
    } else {
        Span::styled(" ", Theme::input_cursor_off())
    };

    // Multi-line input: one Line per input row; caret on last line.
    // Image attachments appear as `[图片.img]` tokens inside the text (deletable).
    let mut content: Vec<Line> = vec![Line::from("")]; // top padding
    if app.input.is_empty() {
        content.push(Line::from(vec![
            Span::raw(INDENT),
            caret.clone(),
            Span::styled(placeholder, Theme::input_placeholder()),
        ]));
    } else {
        let lines: Vec<&str> = app.input.split('\n').collect();
        let last = lines.len().saturating_sub(1);
        for (i, line) in lines.iter().enumerate() {
            if i == last {
                content.push(Line::from(vec![
                    Span::raw(INDENT),
                    Span::styled(*line, Theme::input_text()),
                    caret.clone(),
                ]));
            } else {
                content.push(Line::from(vec![
                    Span::raw(INDENT),
                    Span::styled(*line, Theme::input_text()),
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
    // Hardware cursor stays hidden — software ▌ above is the typewriter caret.

    // Prompt meta: agent (accent)  model  provider — no api/host noise.
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

    let mut meta_spans = vec![
        Span::styled("  ", Theme::bg()),
        Span::styled(agent, Theme::prompt_bar()),
    ];
    if !model.is_empty() {
        meta_spans.push(Span::styled("  ", Theme::bg()));
        meta_spans.push(Span::styled(model, Theme::meta()));
    }
    if !provider.is_empty() {
        meta_spans.push(Span::styled("  ", Theme::bg()));
        meta_spans.push(Span::styled(provider, Theme::status_faint()));
    }
    if app.thinking_level != "off" {
        meta_spans.push(Span::styled("  ", Theme::bg()));
        meta_spans.push(Span::styled(
            format!("think:{}", app.thinking_level),
            Theme::status_faint(),
        ));
    }
    if app.usage_tokens > 0 {
        meta_spans.push(Span::styled("  ", Theme::bg()));
        meta_spans.push(Span::styled(
            format!("~{}", format_tokens(app.usage_tokens)),
            Theme::status_faint(),
        ));
    }
    if app.busy {
        meta_spans.push(Span::styled("  running", Theme::status_faint()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(meta_spans)).style(Theme::bg()),
        meta_area,
    );
}

/// Terminal columns occupied by `s` (fullwidth / CJK = 2) as u16 for layout.
fn display_cols(s: &str) -> u16 {
    display_width(s).min(u16::MAX as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn typed_input_is_visible_in_buffer() {
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("test");
        app.input = "hello-world".into();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let flat: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            flat.contains("hello-world"),
            "typed input must appear in the frame buffer, got:\n{flat}"
        );
    }

    #[test]
    fn tall_assistant_message_shows_bottom_when_following() {
        // Regression: Ratatui List drops items taller than the viewport → blank chat.
        let backend = TestBackend::new(40, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("test");
        // Many lines so content exceeds chat area (layout: Min(3)+4+1 on height 14 → ~9 rows).
        let body: String = (0..40)
            .map(|i| format!("line-{i:02} unique-tail-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push_assistant(&body);
        app.follow_bottom = true;

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let flat: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            flat.contains("line-39") || flat.contains("unique-tail-39"),
            "follow-bottom must show the end of a multi-page reply, got:\n{flat}"
        );
        assert!(
            !flat.contains("line-00 unique"),
            "top of a tall reply should scroll off when following bottom"
        );
    }

    #[test]
    fn page_up_reveals_older_messages() {
        let backend = TestBackend::new(40, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("test");
        app.push_user("first-user-turn-marker");
        app.push_assistant("first-assistant-reply-marker");
        // Pad with enough lines so early messages leave the first viewport.
        let body: String = (0..50)
            .map(|i| format!("pad-line-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push_user("latest-user");
        app.push_assistant(&body);

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let bottom: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            !bottom.contains("first-user-turn-marker"),
            "older turns should be off-screen at bottom stick"
        );

        // Scroll all the way to the top of the transcript.
        app.scroll_to_top();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let top: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            top.contains("first-user-turn-marker"),
            "scroll-to-top must show early history, got:\n{top}"
        );
        assert!(!app.follow_bottom);
    }

    #[test]
    fn placeholder_shown_when_empty() {
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("test");

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let flat: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            flat.contains("Message"),
            "placeholder must appear when input is empty, got:\n{flat}"
        );
    }

    #[test]
    fn empty_session_shows_welcome_tips() {
        // Tall enough for chat pane to show welcome title + a tip row.
        let backend = TestBackend::new(72, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("one");
        app.set_agent_label("Build");
        app.set_current_model("mock", "mock-model");

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let flat: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            flat.contains("coding agent"),
            "empty session should show welcome title, got:\n{flat}"
        );
        assert!(
            flat.contains("tips") || flat.contains("commands"),
            "empty session should show guidance, got:\n{flat}"
        );
        assert!(
            flat.contains("mock-model"),
            "empty session should surface current model, got:\n{flat}"
        );

        // Once a message exists, welcome leaves the transcript.
        app.push_user("hello there unique-marker");
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let after: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            after.contains("hello there unique-marker"),
            "user message must paint, got:\n{after}"
        );
        assert!(
            !after.contains("coding agent"),
            "welcome must hide after first message, got:\n{after}"
        );
    }

    #[test]
    fn caret_sits_on_reserved_slot_not_mid_text() {
        // empty: indent(2) + slot(1) → caret col = border(1)+2 = 3 from box.x
        // typed "ab": indent(2)+width(2)+slot → caret after "ab"
        assert_eq!(display_cols("ab"), 2);
        assert_eq!(display_cols("你好"), 4); // fullwidth
        assert_eq!(display_cols(""), 0);
    }

    #[test]
    fn streaming_thinking_shows_only_last_three_lines() {
        let backend = TestBackend::new(48, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("test");
        app.busy = true;
        // Distinct markers per line so we can assert the rolling tail.
        let body = (0..8)
            .map(|i| format!("think-line-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.messages
            .push(crate::message::Message::streaming_thinking(body));
        app.cursor_on = true;

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let flat: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();

        assert!(
            flat.contains("think-line-05")
                && flat.contains("think-line-06")
                && flat.contains("think-line-07"),
            "streaming thinking must keep the last 3 lines, got:\n{flat}"
        );
        assert!(
            !flat.contains("think-line-00") && !flat.contains("think-line-04"),
            "older thinking lines must scroll off the rolling window, got:\n{flat}"
        );
    }

    #[test]
    fn streaming_thinking_caret_does_not_add_extra_row() {
        // Blink off must not drop a full line (that was the vertical jump).
        let body = "alpha\nbeta\ngamma\ndelta";
        let msg = crate::message::Message::streaming_thinking(body);
        let mut app = App::new("test");
        app.cursor_on = true;
        let on = render_thinking(&msg, &app, 40);
        app.cursor_on = false;
        let off = render_thinking(&msg, &app, 40);
        assert_eq!(
            on.len(),
            off.len(),
            "caret blink must keep the same row count (no vertical jump)"
        );
        // Tail window: header + last 3 body lines (delta/beta/gamma/delta → last 3).
        assert_eq!(on.len(), 1 + THINKING_STREAM_TAIL_LINES);
    }

    #[test]
    fn assistant_renders_markdown_table() {
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("test");
        app.push_assistant(
            "## Specs\n\n| Field | Value |\n|-------|-------|\n| RAM   | 16 GB |\n| Disk  | 1 TB  |\n",
        );

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let flat: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();

        assert!(flat.contains("Specs"), "heading: {flat}");
        assert!(
            flat.contains("Field") && flat.contains("Value"),
            "header: {flat}"
        );
        assert!(
            flat.contains("RAM") && flat.contains("16 GB"),
            "body: {flat}"
        );
        assert!(
            flat.contains('┌') || flat.contains('│'),
            "table borders: {flat}"
        );
    }

    #[test]
    fn status_and_meta_are_sparse() {
        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("test");
        app.set_agent_label("Build");
        app.set_mode_label("deepseek-v4-flash");
        app.set_current_model("opencode", "deepseek-v4-flash");
        for i in 0..30 {
            app.push_assistant(&format!("line-{i:02}"));
        }

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        let h = buf.area.height as usize;
        let cells: Vec<String> = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        // Last two rows: prompt meta + status strip.
        let meta_row: String = cells[(h - 2) * w..(h - 1) * w].concat();
        let status_row: String = cells[(h - 1) * w..h * w].concat();

        // Meta: agent + model + provider only.
        assert!(meta_row.contains("Build"), "agent on meta: {meta_row}");
        assert!(
            meta_row.contains("deepseek-v4-flash"),
            "model on meta: {meta_row}"
        );
        assert!(
            meta_row.contains("opencode"),
            "provider on meta: {meta_row}"
        );
        assert!(
            !meta_row.contains("completions") && !meta_row.contains(" · "),
            "meta must not dump api/host or middle-dot soup: {meta_row}"
        );

        // Status: short key+label pairs.
        assert!(status_row.contains("enter"), "send key: {status_row}");
        assert!(
            status_row.contains("cmd") || status_row.contains("/"),
            "commands: {status_row}"
        );
        assert!(
            !status_row.contains("ctrl+c")
                && !status_row.contains("ctrl+p")
                && !status_row.contains("ctrl+l")
                && !status_row.contains(" · "),
            "idle status should stay sparse: {status_row}"
        );
    }
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (left, right) = status_spans(app);

    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(8), Constraint::Length(12)])
        .split(area);

    frame.render_widget(Paragraph::new(Line::from(left)).style(Theme::bg()), row[0]);
    frame.render_widget(
        Paragraph::new(Line::from(right))
            .alignment(Alignment::Right)
            .style(Theme::bg()),
        row[1],
    );
}

/// Sparse, context-aware status strip.
/// Keys slightly brighter than labels; only show what matters in the current mode.
fn status_spans(app: &App) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    // Key + label pairs, joined with double-space (no middle-dot soup).
    fn pair(key: &'static str, label: &'static str) -> [Span<'static>; 2] {
        [
            Span::styled(key, Theme::status_key()),
            Span::styled(label, Theme::status()),
        ]
    }

    // Notices are top-right toasts now — footer stays for keybindings only.

    if app.float_open() {
        let mut left = vec![Span::raw("  ")];
        left.extend(pair("↑↓", " nav  "));
        left.extend(pair("enter", " select  "));
        left.extend(pair("esc", " close"));
        return (left, Vec::new());
    }

    if !app.follow_bottom && app.can_scroll() {
        let max = app.max_scroll().max(1);
        let pct = ((app.chat_scroll as f64 / max as f64) * 100.0).round() as u16;
        let mut left = vec![Span::raw("  ")];
        left.extend(pair("end", " latest"));
        let right = vec![Span::styled(format!("↑{pct}%  "), Theme::status_faint())];
        return (left, right);
    }

    if app.busy {
        let mut left = vec![Span::raw("  ")];
        // Soft cancel vs hard exit — Ctrl+C must never be "just stop".
        left.extend(pair("q/esc", " stop  "));
        left.extend(pair("^c", " quit  "));
        left.extend(pair("ctrl+s", " steer"));
        let right = vec![Span::styled("working  ", Theme::status_faint())];
        return (left, right);
    }

    // Idle: common actions + usage / thinking on the right.
    let mut left = vec![Span::raw("  ")];
    left.extend(pair("enter", " send  "));
    left.extend(pair("/", " cmd  "));
    left.extend(pair("^j", " nl  "));
    left.extend(pair("↑↓", " hist  "));
    left.extend(pair("esc", " clear"));
    if app.can_scroll() {
        left.push(Span::raw("  "));
        left.extend(pair("pgup", " scroll"));
    }
    // Mouse on: drag selects in-app → OSC 52 clipboard (not terminal native select).
    if app.mouse_capture {
        left.push(Span::raw("  "));
        left.extend(pair("drag", " copy"));
    }

    let mut right = Vec::new();
    if app.thinking_level != "off" {
        // Default is collapsed; only badge when the user opted into full bodies.
        let vis = if app.show_thinking { "·full" } else { "" };
        right.push(Span::styled(
            format!("think:{}{vis}  ", app.thinking_level),
            Theme::status_faint(),
        ));
    }
    // Prefer precise provider I/O tokens when available.
    if app.usage_input > 0 || app.usage_output > 0 {
        let mut usage = format!(
            "↑{} ↓{}  ",
            format_tokens(app.usage_input as usize),
            format_tokens(app.usage_output as usize)
        );
        if app.usage_cost_usd > 0.0 {
            if app.usage_cost_usd < 0.01 {
                usage.push_str(&format!("${:.4}  ", app.usage_cost_usd));
            } else {
                usage.push_str(&format!("${:.3}  ", app.usage_cost_usd));
            }
        }
        if app.context_window > 0 && app.usage_tokens > 0 {
            let pct = (app.usage_tokens * 100) / app.context_window.max(1);
            usage.push_str(&format!("ctx {pct}%  "));
        }
        right.push(Span::styled(usage, Theme::status_faint()));
    } else if app.usage_tokens > 0 {
        let usage = if app.context_window > 0 {
            let pct = (app.usage_tokens * 100) / app.context_window.max(1);
            format!("~{} tok {}%  ", format_tokens(app.usage_tokens), pct)
        } else {
            format!("~{} tok  ", format_tokens(app.usage_tokens))
        };
        right.push(Span::styled(usage, Theme::status_faint()));
    }
    (left, right)
}

fn format_tokens(n: usize) -> String {
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
