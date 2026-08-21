//! Slash-command and HITL select docks above the prompt.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::theme::Theme;

use super::text::truncate_mid;

pub(super) fn draw_slash_dock(frame: &mut Frame<'_>, area: Rect, app: &App) {
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
            PopupRow::Command(_) | PopupRow::Model(_) | PopupRow::Subcommand(_) => {
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
pub(super) fn draw_select_dock(frame: &mut Frame<'_>, area: Rect, app: &App) {
    use crate::select::SelectPhase;

    let Some(prompt) = app.select_prompt() else {
        return;
    };

    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Theme::slash_panel()), area);

    let title = format!(" {} ", prompt.title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(title, Theme::title()))
        .title_bottom(Span::styled(
            format!(" {} ", prompt.footer()),
            Theme::float_footer(),
        ))
        .border_style(Style::default().fg(Theme::PRIMARY).bg(Theme::PANEL))
        .style(Theme::slash_panel());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let max_w = (inner.width as usize).saturating_sub(2);
    let mut lines: Vec<Line> = Vec::new();

    for (i, line) in prompt.body.lines().enumerate() {
        let text = truncate_mid(line, max_w);
        let style = if i == 0 {
            // Headline / question.
            Style::default()
                .fg(Theme::INFO)
                .bg(Theme::PANEL)
                .add_modifier(Modifier::BOLD)
        } else if line.starts_with("Why:") {
            // Escalation justification — make it readable, not dark-gray.
            Style::default().fg(Theme::WARNING).bg(Theme::PANEL)
        } else if line.starts_with("$ ") {
            // Command preview.
            Style::default().fg(Theme::FG).bg(Theme::PANEL)
        } else {
            Style::default().fg(Theme::MUTED).bg(Theme::PANEL)
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    if !prompt.body.is_empty() {
        lines.push(Line::from(Span::styled(
            " ".repeat(max_w.max(1)),
            Theme::slash_panel(),
        )));
    }

    // Scroll window if too many options for the dock height. The footer lives on
    // the bottom border, so the list gets one more visual row than before.
    let opt_n = prompt.option_count();
    let typing_rows = if matches!(prompt.phase, SelectPhase::Typing { .. }) {
        2
    } else {
        0
    };
    let fixed = lines.len() + typing_rows;
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
            Style::default().fg(Theme::MUTED).bg(Theme::PANEL),
        )));
        let input = format!("> {buffer}█");
        lines.push(Line::from(Span::styled(
            truncate_mid(&input, max_w),
            Style::default()
                .fg(Theme::FG)
                .bg(Theme::ELEMENT)
                .add_modifier(Modifier::BOLD),
        )));
    }

    while lines.len() < inner.height as usize {
        lines.push(Line::from(Span::styled(
            " ".repeat(max_w.max(1)),
            Theme::slash_panel(),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines).style(Theme::slash_panel()),
        Rect {
            x: inner.x.saturating_add(1),
            y: inner.y,
            width: inner.width.saturating_sub(2),
            height: inner.height,
        },
    );
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
                    "●"
                } else {
                    "○"
                }
            }
            SelectMode::Multi => {
                if focused {
                    "◉"
                } else {
                    "□"
                }
            }
        };
        (mark, prompt.other_label.as_str(), "")
    } else {
        let opt = &prompt.options[idx];
        let mark = match prompt.mode {
            SelectMode::Single => {
                if focused {
                    "●"
                } else {
                    "○"
                }
            }
            SelectMode::Multi => {
                if prompt.checked.contains(&idx) {
                    "■"
                } else {
                    "□"
                }
            }
        };
        (mark, opt.label.as_str(), opt.description.as_str())
    };

    let row_bg = if focused {
        Theme::ELEMENT
    } else {
        Theme::PANEL
    };
    let marker_style = Style::default()
        .fg(if focused {
            Theme::PRIMARY
        } else {
            Theme::BORDER_ACTIVE
        })
        .bg(row_bg)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default()
        .fg(if focused { Theme::FG } else { Theme::FG })
        .bg(row_bg)
        .add_modifier(if focused {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    let desc_style = Style::default()
        .fg(if focused { Theme::INFO } else { Theme::MUTED })
        .bg(row_bg);
    let num_style = Style::default().fg(Theme::MUTED).bg(row_bg);

    let num = format!(" {:>2} ", idx + 1);
    let prefix_w = UnicodeWidthStr::width(num.as_str()) + UnicodeWidthStr::width(mark) + 1;
    let desc_gap = if desc.is_empty() { 0 } else { 2 };
    let desc_w = if desc.is_empty() {
        0
    } else {
        UnicodeWidthStr::width(desc).min(28)
    };
    let label_budget = max_w.saturating_sub(prefix_w + desc_gap + desc_w).max(1);
    let label = truncate_mid(label, label_budget);
    let used = prefix_w + UnicodeWidthStr::width(label.as_str());
    let desc_budget = max_w.saturating_sub(used + desc_gap);
    let desc = if desc_budget > 2 && !desc.is_empty() {
        format!("  {}", truncate_mid(desc, desc_budget.saturating_sub(2)))
    } else {
        String::new()
    };

    let mut line = Line::from(vec![
        Span::styled(num, num_style),
        Span::styled(mark.to_string(), marker_style),
        Span::styled(" ".to_string(), Style::default().bg(row_bg)),
        Span::styled(label, label_style),
        Span::styled(desc, desc_style),
    ]);
    line.style = Style::default().bg(row_bg);
    line
}
