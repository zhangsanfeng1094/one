//! Chat transcript paint: messages, tools, thinking, selection highlight.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;
use crate::markdown;
use crate::message::{AlertLevel, ChatLineTarget, Message, MessageRole, ToolStatus};
use crate::theme::Theme;
use crate::tool_view::{self, DiffLineKind};

use super::text::{
    display_width, scrollbar_thumb_geometry, truncate_display, truncate_display_middle,
    wrap_paragraphs, wrap_str, wrap_styled_segments,
};
use super::SPINNER;

pub(super) fn draw_chat(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    // Outer padding: left gutter · content · right gutter (also scrollbar track).
    let pad = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let content = pad[1];
    let sb_col = pad[2];
    let wrap_width = content.width.max(16) as usize;
    let view_h = content.height as usize;

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
        app.chat_scroll = 0;
    } else if app.follow_bottom {
        app.chat_scroll = 0;
    } else {
        // A history scroll is top-relative, so new output below the viewport
        // cannot move what the reader is currently inspecting.
        app.chat_scroll = app.chat_scroll.min(max_from_bottom);
    }

    let start = if app.follow_bottom || view_h == 0 {
        max_from_bottom
    } else {
        app.chat_scroll
    };
    let end = (start + view_h).min(total);
    app.chat_view_start = start;
    // New / short chats start at the top of the pane (0,0) — not pinned to the prompt.
    app.chat_top_pad = 0;
    // Content origin for mouse → caret mapping (matches horizontal pad above).
    app.chat_content_x = content.x;
    let sel = app.selection_span();
    let window: Vec<Line<'static>> = if start < end {
        all_lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let abs = start + i;
                match sel {
                    Some((lo, hi)) if abs >= lo.line && abs <= hi.line => {
                        let (col_lo, col_hi) = if lo.line == hi.line {
                            (lo.col, hi.col)
                        } else if abs == lo.line {
                            (lo.col, usize::MAX)
                        } else if abs == hi.line {
                            (0, hi.col)
                        } else {
                            (0, usize::MAX)
                        };
                        highlight_line_range(line, col_lo, col_hi)
                    }
                    _ => line.clone(),
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    frame.render_widget(Paragraph::new(window).style(Theme::bg()), content);

    // Right-edge progress scrollbar when the transcript is taller than the viewport.
    if total > view_h && view_h > 0 && sb_col.width > 0 && sb_col.height > 0 {
        draw_chat_scrollbar(frame, sb_col, total, view_h, start);
    }
}

/// Main transcript scrollbar (right gutter). Thumb tracks the visible window.
fn draw_chat_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    total: usize,
    viewport: usize,
    offset: usize,
) {
    let track_h = area.height as usize;
    if track_h == 0 {
        return;
    }
    let (thumb_start, thumb_h) = scrollbar_thumb_geometry(total, viewport, offset, track_h);
    let track_style = Style::default().bg(Theme::BG).fg(Theme::ELEMENT);
    let thumb_style = Style::default()
        .bg(Theme::BG)
        .fg(Theme::BORDER_ACTIVE)
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line> = Vec::with_capacity(track_h);
    for i in 0..track_h {
        let in_thumb = i >= thumb_start && i < thumb_start + thumb_h;
        // Slightly softer than float (▐) so chat chrome stays quiet.
        let ch = if in_thumb { "▌" } else { " " };
        let style = if in_thumb { thumb_style } else { track_style };
        lines.push(Line::from(Span::styled(ch, style)));
    }
    frame.render_widget(Paragraph::new(lines).style(Theme::bg()), area);
}

/// Paint `[char_lo, char_hi)` of a line with selection background.
///
/// `char_hi == usize::MAX` means through the end of the line.
fn highlight_line_range(line: &Line<'static>, char_lo: usize, char_hi: usize) -> Line<'static> {
    if char_lo == 0 && char_hi == usize::MAX {
        return highlight_line_full(line);
    }
    let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let n = plain.chars().count();
    let lo = char_lo.min(n);
    let hi = if char_hi == usize::MAX {
        n
    } else {
        char_hi.min(n).max(lo)
    };
    if lo >= hi {
        return line.clone();
    }

    // Rebuild spans, splitting at caret boundaries so only [lo, hi) is highlighted.
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize; // char index into the line
    for span in &line.spans {
        let content = span.content.as_ref();
        let span_len = content.chars().count();
        if span_len == 0 {
            continue;
        }
        let span_start = cursor;
        let span_end = cursor + span_len;
        // Before / selected / after, clipped to this span.
        for (a, b, selected) in [
            (span_start, span_end.min(lo), false),
            (span_start.max(lo), span_end.min(hi), true),
            (span_start.max(hi), span_end, false),
        ] {
            if a < b {
                let skip = a - span_start;
                let take = b - a;
                let s: String = content.chars().skip(skip).take(take).collect();
                let style = if selected {
                    span.style.patch(Theme::selection())
                } else {
                    span.style
                };
                out.push(Span::styled(s, style));
            }
        }
        cursor = span_end;
    }

    if out.is_empty() {
        Line::from(Span::styled(" ", Theme::selection()))
    } else {
        Line::from(out)
    }
}

/// Paint a full line with selection background (keeps glyph content).
fn highlight_line_full(line: &Line<'static>) -> Line<'static> {
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
/// Also returns per-line click targets (`Message` vs multi-tool `ToolGroup`).
fn build_chat_lines(
    app: &App,
    wrap_width: usize,
) -> (Vec<Line<'static>>, Vec<Option<ChatLineTarget>>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut owners: Vec<Option<ChatLineTarget>> = Vec::new();

    let push_owned = |lines: &mut Vec<Line<'static>>,
                      owners: &mut Vec<Option<ChatLineTarget>>,
                      chunk: Vec<Line<'static>>,
                      owner: Option<ChatLineTarget>| {
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
                let group_lines =
                    render_tool_group(&app.messages[i..i + streak], wrap_width, false);
                // Collapsed chip → group toggle (expand all tools in streak).
                push_owned(
                    &mut lines,
                    &mut owners,
                    group_lines,
                    Some(ChatLineTarget::ToolGroup(i)),
                );
                i += streak;
                continue;
            }
            // Expanded multi-tool stack: clickable ▾ header collapses back to chip.
            // Children nest under the header with ├/└ so the group reads as a parent.
            let show_group_header = tool_view::streak_shows_group_header(&app.messages, i, streak);
            // Tight stack: no blank between consecutive tools.
            for (k, tmsg) in app.messages[i..i + streak].iter().enumerate() {
                if k == 0 {
                    if !lines.is_empty() {
                        // Blank before the stack starts (separate from prior user/assistant).
                        let prev_was_tool = i > 0 && app.messages[i - 1].role == MessageRole::Tool;
                        if !prev_was_tool {
                            lines.push(Line::from(Span::styled("", Theme::bg())));
                            owners.push(None);
                        }
                    }
                    if show_group_header {
                        let header =
                            render_tool_group(&app.messages[i..i + streak], wrap_width, true);
                        push_owned(
                            &mut lines,
                            &mut owners,
                            header,
                            Some(ChatLineTarget::ToolGroup(i)),
                        );
                    }
                }
                let group_child = if show_group_header {
                    Some(GroupChild {
                        is_last: k + 1 == streak,
                    })
                } else {
                    None
                };
                let chunk = message_lines(tmsg, app, wrap_width, i + k, group_child);
                push_owned(
                    &mut lines,
                    &mut owners,
                    chunk,
                    Some(ChatLineTarget::Message(i + k)),
                );
            }
            i += streak;
            continue;
        }

        if !lines.is_empty() {
            lines.push(Line::from(Span::styled("", Theme::bg())));
            owners.push(None);
        }
        let chunk = message_lines(msg, app, wrap_width, i, None);
        let owner = if matches!(
            msg.role,
            MessageRole::Alert | MessageRole::Thinking | MessageRole::Tool
        ) {
            Some(ChatLineTarget::Message(i))
        } else {
            None
        };
        push_owned(&mut lines, &mut owners, chunk, owner);
        i += 1;
    }

    // Spinner while waiting for first token, or while only thinking has started.
    // Running tools already show their own cyan spinners — keep this row neutral
    // so it doesn't collide with blue focus or peach user chrome.
    if app.busy && app.stream_buffer.is_empty() && app.thinking_buffer.is_empty() {
        let show = app.messages.last().map(|m| !m.streaming).unwrap_or(true);
        if show {
            if !lines.is_empty() {
                lines.push(Line::from(""));
                owners.push(None);
            }
            let spin = SPINNER[app.spinner_frame % SPINNER.len()];
            let running_n = app
                .messages
                .iter()
                .filter(|m| m.tool_status == Some(ToolStatus::Running))
                .count();
            // Prefer "Running…" while tools are in flight. Once every tool row
            // is sealed (e.g. foreground `task` finished) but the parent is still
            // busy on the next model call, say so explicitly — "Thinking…" was
            // misleading when the subagent had already returned and the hang was
            // the parent waiting on the next LLM turn.
            let label = if running_n > 0 {
                if running_n == 1 {
                    "Running…".into()
                } else {
                    format!("Running ({running_n})…")
                }
            } else {
                "Waiting for model…".into()
            };
            // Cyan spinner family — same as running tools (not focus blue / user peach).
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{spin} "), Theme::tool_icon_running()),
                Span::styled(label, Theme::busy()),
            ]));
            owners.push(None);
        }
    }

    debug_assert_eq!(lines.len(), owners.len());
    (lines, owners)
}

/// Empty-session welcome: brand, advanced tips, try samples — no chrome
/// that already lives on the prompt meta strip / status footer.
fn empty_state_lines(app: &App, wrap_width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let blank = || Line::from(Span::styled("", Theme::bg()));
    let muted = |s: String| Line::from(vec![Span::raw("  "), Span::styled(s, Theme::meta())]);
    // One tip per row so keys scan as a left-aligned column.
    let tip_line = |key: &str, desc: &str| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{key:<12}"), Theme::status_key()),
            Span::styled(desc.to_string(), Theme::meta()),
        ])
    };

    lines.push(blank());
    // Pure-ASCII wordmark (small figlet-style). Falls back when the pane is tight.
    //
    //    ___  _ __   ___
    //   / _ \| '_ \ / _ \
    //  | (_) | | | |  __/
    //   \___/|_| |_|\___|
    const LOGO: &[&str] = &[
        r#"  ___  _ __   ___"#,
        r#" / _ \| '_ \ / _ \"#,
        r#"| (_) | | | |  __/"#,
        r#" \___/|_| |_|\___|"#,
    ];
    let logo_w = LOGO.iter().map(|l| l.len()).max().unwrap_or(0);
    let brand = Style::default()
        .fg(Theme::PRIMARY)
        .add_modifier(Modifier::BOLD);
    if wrap_width == 0 || wrap_width >= logo_w + 2 {
        for row in LOGO {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled((*row).to_string(), brand),
            ]));
        }
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("coding agent", Theme::meta()),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("one", brand),
            Span::styled("  ·  coding agent", Theme::meta()),
        ]));
    }
    // Agent / model / provider live on the prompt meta strip — do not repeat.
    lines.push(blank());

    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Describe a task to get started — tools run when needed.",
            Theme::assistant_body(),
        ),
    ]));
    lines.push(blank());

    // Advanced only: keys already on the status strip (Ctrl+G/L, ?) stay out.
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "tips",
            Style::default()
                .fg(Theme::MUTED)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(tip_line("Shift+Tab", "Cycle mode (Normal/Plan/YOLO)"));
    lines.push(tip_line("Ctrl+O", "Toggle always-approve (YOLO)"));
    lines.push(tip_line("Ctrl+J", "newline"));
    lines.push(tip_line(
        "Ctrl+V",
        "paste clipboard image (Ctrl+Alt+V on WSL)",
    ));
    lines.push(tip_line("Esc Esc", "rewind last turn"));
    lines.push(tip_line("/resume", "past sessions"));
    lines.push(blank());

    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "try",
            Style::default()
                .fg(Theme::MUTED)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  press 1–3", Theme::meta()),
    ]));
    for (i, example) in crate::state::WELCOME_TRY_PROMPTS.iter().enumerate() {
        // Muted [n] — readable index without a "clickable chip" false affordance;
        // keys 1–3 actually run these when the session is empty.
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("[{}]", i + 1), Theme::status_faint()),
            Span::styled(format!("  \"{example}\""), Theme::meta()),
        ]));
    }
    lines.push(blank());

    let footer = if app.mouse_capture {
        "type or paste below · drag to copy · Ctrl+Shift+M toggles mouse"
    } else {
        "type or paste below · PgUp/PgDn scroll · Ctrl+Shift+M toggles mouse"
    };
    if wrap_width > 0 && display_width(footer) + 2 > wrap_width {
        for part in wrap_str(footer, wrap_width.saturating_sub(2)) {
            lines.push(muted(part));
        }
    } else {
        lines.push(muted(footer.into()));
    }
    lines
}

/// Multi-tool group chip / expanded stack header.
///
/// ```text
///   ▸  5 tools  [todo_write] [grep ×2] [read ×2]
///   ▾  5 tools  [todo_write] [grep ×2] [read ×2]
///     ├ ✓ bash  …
///     ├ ✓ grep  …
///     └ ✓ read  …
/// ```
pub(super) fn render_tool_group(
    tools: &[Message],
    wrap_width: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    let n = tools.len();
    let names: Vec<String> = tools
        .iter()
        .map(|t| {
            let raw = t.tool_name.as_deref().unwrap_or("tool");
            tool_view::tool_display_name(raw, &t.content)
        })
        .collect();
    let mut joined = tool_view::aggregate_tool_names(&names);
    let budget = wrap_width.saturating_sub(16).max(12);
    if display_width(&joined) > budget {
        joined = truncate_display(&joined, budget);
    }
    let chevron = if expanded { "▾" } else { "▸" };
    vec![Line::from(vec![
        Span::raw("  "),
        Span::styled(chevron, Theme::tool_icon_done()),
        Span::styled(format!("  {n} tools  "), Theme::tool_group_title()),
        Span::styled(joined, Theme::tool_group()),
    ])]
}

/// Position of a tool row inside an expanded multi-tool group.
#[derive(Clone, Copy, Debug)]
struct GroupChild {
    is_last: bool,
}

fn message_lines(
    message: &Message,
    app: &App,
    wrap_width: usize,
    msg_index: usize,
    group_child: Option<GroupChild>,
) -> Vec<Line<'static>> {
    let focused = app.chat_focus == Some(msg_index);
    let mut lines = match message.role {
        MessageRole::User => render_user(&message.content, wrap_width),
        MessageRole::Alert => render_alert(message, wrap_width),
        MessageRole::Thinking => render_thinking(message, app, wrap_width),
        MessageRole::Assistant => {
            let mut lines = render_assistant(&message.content, wrap_width);
            if message.streaming {
                // Live turn chrome — same `╰` slot as the finished footer; spinner
                // stands in for duration so the row morphs into
                // `╰ Build · model · 6m25s` without a second typewriter caret.
                lines.push(streaming_turn_footer(app));
            } else if let Some(footer) = &message.footer {
                // Soft turn meta: muted hairline + peach mode glyph.
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("╰ ", Theme::meta()),
                    Span::styled(footer.clone(), Theme::meta()),
                ]));
            }
            lines
        }
        MessageRole::System => render_system(&message.content, wrap_width),
        MessageRole::Tool => render_tool(message, app, wrap_width, group_child),
    };
    if focused {
        apply_focus_rail(&mut lines);
    }
    lines
}

/// Left focus rail on the first visual line of a focused transcript row.
///
/// Blue rail + neutral wash — must not reuse user peach/warm bubble styles.
fn apply_focus_rail(lines: &mut [Line<'static>]) {
    let Some(first) = lines.first_mut() else {
        return;
    };
    let wash = Theme::focus_wash_bg();
    if let Some(span0) = first.spans.first_mut() {
        let content = span0.content.as_ref();
        if content == "  " {
            *span0 = Span::styled("▌ ", Theme::focus_rail());
            for span in first.spans.iter_mut().skip(1) {
                span.style = span.style.bg(wash);
            }
            return;
        }
    }
    let mut spans = vec![Span::styled("▌ ", Theme::focus_rail())];
    spans.extend(first.spans.iter().cloned());
    for span in spans.iter_mut().skip(1) {
        span.style = span.style.bg(wash);
    }
    *first = Line::from(spans);
}

/// Live assistant footer while tokens are still arriving.
///
/// Mirrors the finished turn footer (`╰ agent · mode · duration`) so the
/// chrome is continuous; the braille spinner (same family as tools / Working)
/// occupies the duration slot until the turn seals.
fn streaming_turn_footer(app: &App) -> Line<'static> {
    let spin = SPINNER[app.spinner_frame % SPINNER.len()];
    let agent = if app.agent_label.is_empty() {
        "Build"
    } else {
        app.agent_label.as_str()
    };
    let mut meta = agent.to_string();
    if !app.mode_label.is_empty() {
        meta.push_str(" · ");
        meta.push_str(&app.mode_label);
    }
    Line::from(vec![
        Span::raw("  "),
        Span::styled("╰ ", Theme::meta()),
        Span::styled(format!("{meta} · "), Theme::meta()),
        Span::styled(spin.to_string(), Theme::prompt_bar()),
    ])
}

/// While thinking is streaming, only keep the rolling tail so long chains
/// don't flood the transcript (last N wrapped lines).
pub(super) const THINKING_STREAM_TAIL_LINES: usize = 3;

/// Thinking / reasoning block — collapsible, muted.
///
/// ```text
///   ▸ [Thinking 1.2s]  Analyzing message…   (finished, default collapsed)
///   ▾ [Thinking] ⠋                          (streaming: spinner + last 3 lines)
///     …
///   ▾ [Thinking 1.2s]                       (expanded full body)
///     …
/// ```
///
/// Collapsed previews fill the remaining line width and **end-truncate** so
/// history does not look mid-cropped (`The user wants me …l next positions`).
pub(super) fn render_thinking(
    message: &Message,
    app: &App,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    // Live stream always shows a short tail; finished blocks honor per-message
    // expand (click) or the global Ctrl+T default (`show_thinking`).
    let expanded = message.streaming || message.thinking_expanded;
    let chevron = if expanded { "▾" } else { "▸" };
    let mut lines = Vec::new();
    let dur = duration_label(message);
    let badge = if message.streaming {
        let spin = SPINNER[app.spinner_frame % SPINNER.len()];
        format!("[Thinking {spin}]")
    } else if let Some(d) = &dur {
        format!("[Thinking {d}]")
    } else {
        "[Thinking]".into()
    };

    if expanded {
        let header = vec![
            Span::raw("  "),
            Span::styled(chevron, Theme::thinking_chevron()),
            Span::raw(" "),
            Span::styled(badge, Theme::thinking_badge()),
        ];
        lines.push(Line::from(header));
        let budget = wrap_width.saturating_sub(4).max(8);
        let mut body = wrap_paragraphs(&message.content, budget);
        // Live stream: rolling window of the last few lines only.
        if message.streaming && body.len() > THINKING_STREAM_TAIL_LINES {
            body = body[body.len() - THINKING_STREAM_TAIL_LINES..].to_vec();
        }
        for line in body {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(line, Theme::thinking_body()),
            ]));
        }
    } else {
        // "  ▸ " + badge + "  " + preview  — use leftover cols so wide terminals
        // show a full sentence instead of a 48-char mid-ellipsis stub.
        let prefix_w = 2 + display_width(chevron) + 1 + display_width(&badge) + 2;
        let preview_budget = wrap_width.saturating_sub(prefix_w).max(16);
        let preview = thinking_preview(&message.content, preview_budget);
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(chevron, Theme::thinking_chevron()),
            Span::raw(" "),
            Span::styled(badge, Theme::thinking_badge()),
        ];
        if !preview.is_empty() {
            // Quoted-ish preview: italic muted body, distinct from the badge.
            spans.push(Span::styled(format!("  {preview}"), Theme::thinking_body()));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// First words of a thinking block for collapsed headers.
///
/// Uses **display-width end-ellipsis** (not middle-truncate): natural language
/// should read from the start. `max_cols` is the remaining line budget.
fn thinking_preview(content: &str, max_cols: usize) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return String::new();
    }
    truncate_display(&flat, max_cols)
}

fn duration_label(message: &Message) -> Option<String> {
    message.duration_ms.map(|ms| {
        if ms < 1000 {
            format!("{ms}ms")
        } else if ms < 60_000 {
            format!("{:.1}s", ms as f64 / 1000.0)
        } else {
            let m = ms / 60_000;
            let s = (ms % 60_000) as f64 / 1000.0;
            format!("{m}m{s:.0}s")
        }
    })
}

/// User: peach left rail + warm elevated bubble, bold body (tight, no empty pad rows).
/// Visually louder than tools and distinct from the blue j/k focus rail.
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

/// Assistant: soft indent + full markdown (tables, code, lists, …).
///
/// Live activity is painted as the turn footer (`streaming_turn_footer`), not
/// an end-of-line caret — that kept colliding with the prompt typewriter bar.
fn render_assistant(content: &str, wrap_width: usize) -> Vec<Line<'static>> {
    // 2-space indent leaves room without crowding the user bubble.
    let budget = wrap_width.saturating_sub(2).max(8);
    let mut out = Vec::new();

    if content.trim().is_empty() {
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

    for line in md_lines {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(line.spans);
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
/// Success collapses to one line (✓ already means ok — no `exit 0` child):
/// ```text
///   ✓ bash  cp ./benches/out/…/tb-regex-checker  (25 lines · 190ms)
///   ✗ bash  cat ./missing                        exit 1 · 0.5s
///     └ boom: no such file
///   ⠋ bash  cd ./benches/out/tb-regex-checker     ← running: cyan spinner
/// ```
///
/// Inside an expanded multi-tool group (`group_child`), rows nest under the
/// `▾ N tools` header so the stack reads as one parent with children:
/// ```text
///   ▾  3 tools  [ls] [find ×2]
///     ├ ✓ ls    ./
///     ├ ✓ find  README*
///     └ ✓ find  **/*.{toml,…}
/// ```
fn render_tool(
    message: &Message,
    app: &App,
    wrap_width: usize,
    group_child: Option<GroupChild>,
) -> Vec<Line<'static>> {
    let raw_name = message.tool_name.clone().unwrap_or_else(|| "tool".into());
    let detail = message.content.trim();
    let name = tool_view::tool_display_name(&raw_name, detail);
    let status = message.tool_status.unwrap_or(ToolStatus::Done);
    let cwd = app.history_cwd.as_deref();
    let is_error = status == ToolStatus::Error;

    let (icon, icon_style) = match status {
        ToolStatus::Running => {
            let spin = SPINNER[app.spinner_frame % SPINNER.len()];
            (spin.to_string(), Theme::tool_icon_running())
        }
        ToolStatus::Done => ("✓".into(), Theme::tool_icon_done()),
        ToolStatus::Error => ("✗".into(), Theme::tool_icon_error()),
    };

    // Kind-colored name when done; cyan spinner when running (not focus blue / user peach);
    // red when error.
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
    let path_dir_style = Theme::meta(); // dim directory prefix
    let path_name_style = match status {
        ToolStatus::Done => Theme::tool_detail_done().add_modifier(Modifier::BOLD),
        _ => detail_style,
    };

    // Metrics suffix for collapsed success / failure: `(42 lines · 2.1s)` or `exit 1 · 0.5s`.
    let summary_raw = message.tool_summary.as_deref().unwrap_or("");
    let summary_clean = if summary_raw.is_empty() {
        String::new()
    } else {
        tool_view::single_line_preview(&tool_view::shorten_paths_in_text(summary_raw, cwd), 48)
    };
    let dur = duration_label(message);
    let metrics = {
        let mut parts: Vec<String> = Vec::new();
        if !summary_clean.is_empty() {
            parts.push(summary_clean.clone());
        }
        if let Some(d) = &dur {
            parts.push(d.clone());
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        }
    };

    // Collapse success to one line; failures / expanded keep a child row when useful.
    let inline_metrics = matches!(status, ToolStatus::Done | ToolStatus::Running)
        && !message.tool_expanded
        && !is_error;
    let metrics_w = if inline_metrics {
        metrics
            .as_ref()
            .map(|m| display_width(m) + 3) // "  (…)"
            .unwrap_or(0)
    } else {
        0
    };

    // Group children: `  ` + `├ `/`└ ` (2+2) instead of bare `  `.
    let lead_w = if group_child.is_some() { 4 } else { 2 };
    let name_w = display_width(&name).max(4).min(10);
    let budget = wrap_width
        .saturating_sub(lead_w + 2 + name_w + 2 + metrics_w)
        .max(8);
    let pretty = if detail.is_empty() {
        String::new()
    } else {
        // Paths: middle-truncate so `…/tb-regex-checker` survives.
        // Free-form commands: end-truncate so the start of a script stays readable
        // (full text recovers on expand — see below).
        let raw = pretty_tool_args(detail, cwd);
        if tool_view::looks_like_path(&raw) {
            truncate_display_middle(&raw, budget)
        } else {
            truncate_display(&raw, budget)
        }
    };

    let mut lines = Vec::new();
    // Header:  `  ✓ bash  cargo test  (12 lines · 45ms)`
    // Group:   `  ├ ✓ bash  cargo test  (12 lines · 45ms)`
    let mut spans = match group_child {
        Some(GroupChild { is_last }) => {
            let branch = if is_last { "└ " } else { "├ " };
            vec![
                Span::raw("  "),
                Span::styled(branch, Theme::tool_tree()),
                Span::styled(format!("{icon} "), icon_style),
                Span::styled(format!("{name:<name_w$}"), name_style),
            ]
        }
        None => vec![
            Span::raw("  "),
            Span::styled(format!("{icon} "), icon_style),
            Span::styled(format!("{name:<name_w$}"), name_style),
        ],
    };
    if !pretty.is_empty() {
        if tool_view::looks_like_path(&pretty) {
            let (dir, file) = tool_view::path_dir_and_name(&pretty);
            spans.push(Span::raw("  "));
            if !dir.is_empty() {
                spans.push(Span::styled(dir, path_dir_style));
            }
            spans.push(Span::styled(file, path_name_style));
        } else {
            spans.push(Span::styled(format!("  {pretty}"), detail_style));
        }
    }
    if inline_metrics {
        if let Some(m) = &metrics {
            spans.push(Span::styled(format!("  ({m})"), Theme::meta()));
        }
    } else if is_error && !message.tool_expanded {
        // Failure header carries exit code / duration inline; detail body below if expanded.
        if let Some(m) = &metrics {
            spans.push(Span::styled(format!("  {m}"), Theme::tool_summary_err()));
        }
    }
    lines.push(Line::from(spans));

    // Collapsed success/error: metrics stay on the header only (no second └ row).
    // Expanded body is rendered below when `tool_expanded`.

    // Expanded body with proper tree rails (├ / └), not a floating │ dump.
    // Caps are generous: the main chat is line-scrolled (full viewport), so long
    // tool output should participate in that scroll instead of feeling "clipped".
    if message.tool_expanded {
        let nest = if group_child.is_some() { 4 } else { 0 };
        let body_budget = wrap_width.saturating_sub(8 + nest).max(12);
        let rail_style = if status == ToolStatus::Error {
            Theme::error_bar()
        } else {
            Theme::tool_tree()
        };
        // Nested under a group: keep the parent spine (`│` / spaces) then hang the
        // tool body one level deeper so it reads as a child of `├ ✓ name`, not
        // a sibling of the group header.
        //
        //   ▾  3 tools
        //     ├ ✓ ls  ./
        //     │   └ body…
        //     └ ✓ find
        //         └ body…
        let (body_indent, body_cont) = match group_child {
            Some(GroupChild { is_last: true }) => ("      ", "    "),
            Some(GroupChild { is_last: false }) => ("  │   ", "  │ "),
            None => ("    ", "  "),
        };

        // Recover full args (paths shortened, no char cap) when the header
        // truncated a long bash/heredoc — otherwise history looks permanently cropped.
        // For delegated MCP calls (`use_tool`), keep the pretty argument block visible
        // when expanded so the user sees the real target/input instead of the wrapper JSON.
        let full_args = if detail.is_empty() {
            String::new()
        } else {
            tool_view::pretty_tool_detail_full(detail, cwd)
        };
        let show_full_args = !full_args.is_empty()
            && ((raw_name == "use_tool" && full_args != detail)
                || (matches!(raw_name.as_str(), "bash" | "sh" | "exec")
                    && (pretty.contains('…')
                        || full_args.lines().count() > 1
                        || display_width(&full_args) > budget)));

        let mut visual: Vec<(String, Style)> = Vec::new();
        if show_full_args {
            for line in full_args.lines() {
                for wrapped in wrap_str(line, body_budget) {
                    visual.push((wrapped, Theme::tool_detail_done()));
                }
            }
            if message
                .tool_output
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
            {
                visual.push((String::new(), Theme::tool_detail_done()));
            }
        }

        if let Some(raw_output) = message.tool_output.as_deref() {
            // Format leftover JSON at paint time (search_tool schemas, MCP
            // structuredContent) so expand never dumps the raw payload.
            let output = tool_view::display_tool_output(&raw_name, detail, raw_output, is_error);
            // IDE red/green gutter is only for edit/write patches. `read` / grep / bash
            // / etc. always render as ordinary plain text — never the modification UI
            // (markdown bullets and other `+/-` lines used to false-trigger looks_like_diff).
            let is_edit_write = matches!(name.as_str(), "edit" | "write" | "search_replace");
            let is_diff = is_edit_write && tool_view::looks_like_diff(&output);
            // Edit/write: Cursor-style numbered red/green rows (no unified +/- chrome).
            if is_diff && status != ToolStatus::Error {
                // Paint recovered args first (│ continues into the diff block).
                for (text, style) in visual {
                    let mut spans = vec![
                        Span::raw(body_indent.to_string()),
                        Span::styled("│ ", rail_style),
                    ];
                    if status != ToolStatus::Error && tool_view::is_json_line(&text) {
                        spans.extend(tool_view::highlight_json_line(&text));
                    } else {
                        spans.push(Span::styled(text, style));
                    }
                    lines.push(Line::from(spans));
                }
                let mut diff_lines = render_ide_diff(&output, wrap_width.saturating_sub(nest));
                // Prefix group spine so diffs stay nested under the parent tool.
                if group_child.is_some() {
                    for line in &mut diff_lines {
                        let mut spans =
                            vec![Span::styled(body_cont.to_string(), Theme::tool_tree())];
                        spans.extend(line.spans.iter().cloned());
                        *line = Line::from(spans);
                    }
                }
                lines.extend(diff_lines);
                return lines;
            }

            let max_lines = if status == ToolStatus::Error { 40 } else { 60 };
            let default_style = if status == ToolStatus::Error {
                Theme::error_body()
            } else {
                Theme::tool_detail_done()
            };

            // Flatten wrapped lines first so tree tips land on the true last visual row.
            let raw_lines: Vec<&str> = output.lines().collect();
            let total_raw = raw_lines.len();
            for line in raw_lines.iter().take(max_lines) {
                let style = if status == ToolStatus::Error {
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
        } else if !show_full_args {
            if let Some(m) = &metrics {
                // Expanded but no body — still show metrics under the chevron row.
                lines.push(Line::from(vec![
                    Span::raw(body_indent.to_string()),
                    Span::styled("└ ", Theme::tool_tree()),
                    Span::styled(m.clone(), Theme::meta()),
                ]));
            }
        }

        if !visual.is_empty() {
            let last = visual.len().saturating_sub(1);
            for (i, (text, style)) in visual.into_iter().enumerate() {
                let branch = if i == last { "└ " } else { "│ " };
                let mut spans = vec![
                    Span::raw(body_indent.to_string()),
                    Span::styled(branch, rail_style),
                ];
                if status != ToolStatus::Error {
                    if let Some(custom) = tool_view::highlight_tool_output_line(&text) {
                        spans.extend(custom);
                    } else if tool_view::is_json_line(&text) {
                        spans.extend(tool_view::highlight_json_line(&text));
                    } else {
                        spans.push(Span::styled(text, style));
                    }
                } else {
                    spans.push(Span::styled(text, style));
                }
                lines.push(Line::from(spans));
            }
        }
    }

    lines
}

/// Cursor / VS Code style edit diff: accent rail + gutter + word-level paint.
fn render_ide_diff(output: &str, wrap_width: usize) -> Vec<Line<'static>> {
    const MAX_ROWS: usize = 48;
    let rows = tool_view::parse_ide_diff_rows(output);
    if rows.is_empty() {
        // Fallback: plain paint of raw unified diff if parse failed.
        let mut out = Vec::new();
        let body_budget = wrap_width.saturating_sub(8).max(12);
        for line in output.lines().take(MAX_ROWS) {
            let style = match tool_view::classify_diff_line(line) {
                DiffLineKind::Add => Theme::diff_add(),
                DiffLineKind::Del => Theme::diff_del(),
                DiffLineKind::Meta => Theme::diff_meta(),
                DiffLineKind::Context | DiffLineKind::Plain => Theme::diff_context(),
            };
            for wrapped in wrap_str(line, body_budget) {
                out.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(wrapped, style),
                ]));
            }
        }
        return out;
    }

    // Pair consecutive del→add (same line_no preferred) for word-level highlights.
    let word_hi = compute_word_highlights(&rows);

    let max_ln = rows.iter().filter_map(|r| r.line_no).max().unwrap_or(1);
    let ln_w = max_ln.to_string().len().max(2).min(5);
    // mark(1) + space(1) + ln + sep(1) + space(1) + code
    let gutter = 1 + 1 + ln_w + 1 + 1;
    let body_budget = wrap_width.saturating_sub(gutter).max(8);

    let mut out = Vec::new();
    let total = rows.len();
    for (idx, row) in rows.iter().enumerate().take(MAX_ROWS) {
        let (mark_ch, mark_style, ln_style, code_style, word_style, sep_style) = match row.kind {
            DiffLineKind::Add => (
                "┃",
                Theme::diff_mark_add(),
                Theme::diff_ln_add(),
                Theme::diff_add(),
                Theme::diff_add_word(),
                Theme::diff_gutter_sep_add(),
            ),
            DiffLineKind::Del => (
                "┃",
                Theme::diff_mark_del(),
                Theme::diff_ln_del(),
                Theme::diff_del(),
                Theme::diff_del_word(),
                Theme::diff_gutter_sep_del(),
            ),
            _ => (
                " ",
                Theme::diff_mark_ctx(),
                Theme::diff_ln(),
                Theme::diff_context(),
                Theme::diff_context(),
                Theme::diff_gutter_sep(),
            ),
        };
        let ln_label = match row.line_no {
            Some(n) => format!("{n:>ln_w$}"),
            None => " ".repeat(ln_w),
        };

        let segments: Vec<(String, bool)> = word_hi
            .get(&idx)
            .cloned()
            .unwrap_or_else(|| vec![(row.text.clone(), false)]);

        let visual_rows = wrap_styled_segments(&segments, body_budget);
        if visual_rows.is_empty() {
            out.push(Line::from(vec![
                Span::styled(mark_ch, mark_style),
                Span::styled(" ", ln_style),
                Span::styled(ln_label, ln_style),
                Span::styled("│", sep_style),
                Span::styled(" ", code_style),
            ]));
            continue;
        }

        for (wi, pieces) in visual_rows.into_iter().enumerate() {
            let mut spans = if wi == 0 {
                vec![
                    Span::styled(mark_ch, mark_style),
                    Span::styled(" ", ln_style),
                    Span::styled(ln_label.clone(), ln_style),
                    Span::styled("│", sep_style),
                    Span::styled(" ", code_style),
                ]
            } else {
                // Continuation: keep rail + blank gutter so wrap stays aligned.
                vec![
                    Span::styled(mark_ch, mark_style),
                    Span::styled(" ", ln_style),
                    Span::styled(" ".repeat(ln_w), ln_style),
                    Span::styled("│", sep_style),
                    Span::styled(" ", code_style),
                ]
            };
            let mut used = 0usize;
            for (piece, emp) in pieces {
                let st = if emp { word_style } else { code_style };
                used = used.saturating_add(display_width(&piece));
                spans.push(Span::styled(piece, st));
            }
            // Pad so the red/green wash fills the remaining columns.
            let pad = body_budget.saturating_sub(used);
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), code_style));
            }
            out.push(Line::from(spans));
        }
    }
    if total > MAX_ROWS {
        out.push(Line::from(vec![
            Span::styled(" ", Theme::diff_mark_ctx()),
            Span::styled(
                format!("  … +{} lines", total - MAX_ROWS),
                Theme::diff_skip(),
            ),
        ]));
    }
    out
}

/// For each consecutive Del→Add pair, compute word-level emphasize masks.

fn compute_word_highlights(
    rows: &[tool_view::IdeDiffRow],
) -> std::collections::HashMap<usize, Vec<(String, bool)>> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    let mut i = 0;
    while i + 1 < rows.len() {
        let a = &rows[i];
        let b = &rows[i + 1];
        // Pair adjacent del→add when line numbers are equal or off-by-one (replace).
        let same_or_adj = match (a.line_no, b.line_no) {
            (Some(x), Some(y)) => x.abs_diff(y) <= 1,
            _ => true,
        };
        if a.kind == DiffLineKind::Del && b.kind == DiffLineKind::Add && same_or_adj {
            let (old_segs, new_segs) = tool_view::inline_diff_segments(&a.text, &b.text);
            // Only keep if there is at least one emphasized span (real word change).
            if old_segs.iter().any(|(_, e)| *e) || new_segs.iter().any(|(_, e)| *e) {
                map.insert(i, old_segs);
                map.insert(i + 1, new_segs);
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    map
}

/// Wrap a sequence of (text, emphasize) segments to `width` display columns.

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

/// Truncate by **display width** (CJK-safe), append … if needed (end ellipsis).

fn pretty_tool_args(s: &str, cwd: Option<&std::path::Path>) -> String {
    tool_view::pretty_tool_detail(s, cwd)
}
