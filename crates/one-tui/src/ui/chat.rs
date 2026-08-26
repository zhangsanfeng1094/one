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
    display_width, fill_spans_to, pad_end, pad_start, scrollbar_thumb_geometry,
    tokenize_input_chips, truncate_display, truncate_display_middle, wrap_paragraphs, wrap_str,
    wrap_styled_segments, wrap_styled_spans, InputChipKind,
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
    // Render across the full viewport width.
    let row_width = (content.width as usize).max(16);
    let wrap_width = row_width;
    let view_h = content.height as usize;

    // Flatten every message into display lines, then window by **line** offset.
    // Full history stays in `app.messages`; we only paint a viewport slice.
    let (all_lines, owners, user_queries, turn_tools, turn_answer) =
        build_chat_lines(app, wrap_width, row_width);
    let total = all_lines.len();
    let max_from_bottom = total.saturating_sub(view_h);
    app.chat_turn_tools_line = turn_tools;
    app.chat_turn_answer_line = turn_answer;

    // Publish metrics so PgUp/Home know page size / max offset.
    // `chat_view_height` is overwritten with the painted pane height after
    // the sticky bar is reserved, so click mapping matches what is on screen.
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
        // Prefer the answer title when a short reply just appeared so the
        // viewport isn't parked on the tool-log top. Long answers still
        // follow the stream end. If the whole transcript fits, stay at 0.
        if total <= view_h {
            0
        } else if let Some(ans) = turn_answer {
            if total.saturating_sub(ans) < view_h {
                ans.min(max_from_bottom)
            } else {
                max_from_bottom
            }
        } else {
            max_from_bottom
        }
    } else {
        app.chat_scroll
    };
    app.chat_view_start = start;
    // New / short chats start at the top of the pane (0,0) — not pinned to the prompt.
    app.chat_top_pad = 0;
    // Content origin for mouse → caret mapping (matches horizontal pad above).
    app.chat_content_x = content.x;
    let sel = app.selection_span();

    // Sticky header: pin the user question that owns the top of the viewport
    // once that bubble has scrolled off — not only the last turn.
    let sticky_query = sticky_query_at(&user_queries, start);
    let need_sticky = sticky_query.is_some() && view_h >= 6;

    let (sticky_area, chat_area) = if need_sticky {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(content);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, content)
    };
    // Hit-testing uses the painted transcript, not the outer pane: the grok
    // header (and sticky bar) sit above this origin, so mouse.row must subtract it.
    app.chat_content_y = chat_area.y;
    app.chat_view_height = chat_area.height as usize;
    let end = (start + app.chat_view_height).min(total);

    if let (Some(s_area), Some(query)) = (sticky_area, sticky_query) {
        app.chat_sticky_line = Some(query.start_line);
        app.chat_sticky_y = Some(s_area.y);
        let single_line = query.text.replace('\n', " ");
        let width = s_area.width as usize;
        let hint = if width >= 50 {
            " ⇡ Pinned "
        } else if width >= 30 {
            " ⇡ "
        } else {
            ""
        };
        let hint_w = display_width(hint);
        let used = TURN_TIME_COL + hint_w;
        let budget = width.saturating_sub(used).max(4);
        let preview = truncate_display(&single_line, budget);

        let mut spans = vec![
            Span::styled("┃", Theme::sticky_query_accent()),
            Span::styled(" ", Theme::sticky_query_bg()),
            Span::styled(query.clock.clone(), Theme::sticky_query_time()),
            Span::styled("  ", Theme::sticky_query_bg()),
            Span::styled(preview, Theme::sticky_query_body()),
        ];

        let current_w: usize = spans
            .iter()
            .map(|s| display_width(s.content.as_ref()))
            .sum();
        let pad_cols = width.saturating_sub(current_w + hint_w);
        if pad_cols > 0 {
            spans.push(Span::styled(" ".repeat(pad_cols), Theme::sticky_query_bg()));
        }
        if !hint.is_empty() {
            spans.push(Span::styled(hint, Theme::sticky_query_hint()));
        }
        fill_spans_to(&mut spans, width, Theme::sticky_query_bg());

        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Theme::sticky_query_bg()),
            s_area,
        );
    } else {
        app.chat_sticky_line = None;
        app.chat_sticky_y = None;
    }

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

    frame.render_widget(Paragraph::new(window).style(Theme::bg()), chat_area);

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

/// A user bubble's source text and the transcript line span it occupies.
struct StickyQuery {
    text: String,
    clock: String,
    start_line: usize,
    end_line: usize,
}

/// Last user question that owns `view_start`, if its bubble has fully
/// scrolled off the top. Historical turns pin just like the current one.
fn sticky_query_at(queries: &[StickyQuery], view_start: usize) -> Option<&StickyQuery> {
    let owning = queries.iter().rev().find(|q| q.start_line <= view_start)?;
    (owning.end_line < view_start).then_some(owning)
}

/// Build the full chat transcript as terminal lines (wrap-aware).
/// Also returns per-line click targets (`Message` vs multi-tool `ToolGroup`)
/// and every real user bubble's line span (for the sticky prompt).
fn build_chat_lines(
    app: &App,
    wrap_width: usize,
    row_width: usize,
) -> (
    Vec<Line<'static>>,
    Vec<Option<ChatLineTarget>>,
    Vec<StickyQuery>,
    Option<usize>,
    Option<usize>,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut owners: Vec<Option<ChatLineTarget>> = Vec::new();
    let mut user_queries: Vec<StickyQuery> = Vec::new();
    let mut turn_tools: Option<usize> = None;
    let mut turn_answer: Option<usize> = None;

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
        return (lines, owners, Vec::new(), None, None);
    }

    let last_user_idx = last_user_message_index(&app.messages);

    let mut i = 0;
    let mut skip_folded_turn = false;
    let mut turn_spine = false;
    while i < app.messages.len() {
        let msg = &app.messages[i];
        if skip_folded_turn {
            if crate::user_fold::is_real_user(msg)
                || crate::message::Message::is_context_compacted(&msg.content)
                || crate::message::Message::is_context_compacting(&msg.content)
            {
                skip_folded_turn = false;
            } else if msg.role == MessageRole::Tool {
                i += tool_view::tool_streak_len(&app.messages, i);
                continue;
            } else {
                i += 1;
                continue;
            }
        }
        if msg.role == MessageRole::Tool {
            let streak = tool_view::tool_streak_len(&app.messages, i);
            if turn_tools.is_none() {
                turn_tools = Some(lines.len());
            }
            let slice = &app.messages[i..i + streak];
            let any_ungroup = slice.iter().any(|m| m.tool_ungroup);
            let any_running = slice
                .iter()
                .any(|m| m.tool_status == Some(ToolStatus::Running));
            // Default: fold finished tools into a summary and keep only the
            // in-flight row visible until the user expands the group.
            let summary_running = any_running
                && !any_ungroup
                && streak >= 2
                && slice
                    .iter()
                    .all(|m| !m.tool_expanded || m.tool_status == Some(ToolStatus::Running));

            if !lines.is_empty() {
                let prev_was_tool = i > 0 && app.messages[i - 1].role == MessageRole::Tool;
                if !prev_was_tool {
                    let blank = Line::from(Span::styled("", Theme::bg()));
                    lines.push(if turn_spine {
                        with_turn_spine(blank)
                    } else {
                        blank
                    });
                    owners.push(None);
                }
            }

            if tool_view::streak_can_collapse(&app.messages, i, streak) {
                let group_lines =
                    render_tool_group(&app.messages[i..i + streak], wrap_width, row_width, false);
                push_owned(
                    &mut lines,
                    &mut owners,
                    spine_lines(group_lines, turn_spine),
                    Some(ChatLineTarget::ToolGroup(i)),
                );
                i += streak;
                continue;
            }

            if summary_running {
                let header = render_tool_group(slice, wrap_width, row_width, false);
                push_owned(
                    &mut lines,
                    &mut owners,
                    spine_lines(header, turn_spine),
                    Some(ChatLineTarget::ToolGroup(i)),
                );
                for (k, tmsg) in slice.iter().enumerate() {
                    if tmsg.tool_status == Some(ToolStatus::Running) {
                        let chunk = message_lines(tmsg, app, wrap_width, row_width, i + k, None);
                        push_owned(
                            &mut lines,
                            &mut owners,
                            spine_lines(chunk, turn_spine),
                            Some(ChatLineTarget::Message(i + k)),
                        );
                    }
                }
                i += streak;
                continue;
            }

            let show_group_header =
                streak >= 3 || tool_view::streak_shows_group_header(&app.messages, i, streak);
            if show_group_header {
                let header = render_tool_group(slice, wrap_width, row_width, true);
                push_owned(
                    &mut lines,
                    &mut owners,
                    spine_lines(header, turn_spine),
                    Some(ChatLineTarget::ToolGroup(i)),
                );
            }
            let mut k = 0;
            while k < streak {
                let tmsg = &app.messages[i + k];
                let running = tmsg.tool_status == Some(ToolStatus::Running);
                // Merge repeated names on compact lists. Expanded group
                // headers keep one row per call so the tree stays clickable.
                if !show_group_header && !running && !tmsg.tool_expanded && !tmsg.tool_ungroup {
                    let same = tool_view::same_name_run(&app.messages, i + k, i + streak);
                    if same >= 2 {
                        let is_last = k + same == streak;
                        let group_child = show_group_header.then_some(GroupChild { is_last });
                        let merged = render_merged_tools(
                            &app.messages[i + k..i + k + same],
                            wrap_width,
                            row_width,
                            group_child,
                        );
                        push_owned(
                            &mut lines,
                            &mut owners,
                            spine_lines(merged, turn_spine),
                            Some(ChatLineTarget::Message(i + k)),
                        );
                        k += same;
                        continue;
                    }
                }
                let group_child = if show_group_header {
                    Some(GroupChild {
                        is_last: k + 1 == streak,
                    })
                } else {
                    None
                };
                let chunk = message_lines(tmsg, app, wrap_width, row_width, i + k, group_child);
                push_owned(
                    &mut lines,
                    &mut owners,
                    spine_lines(chunk, turn_spine),
                    Some(ChatLineTarget::Message(i + k)),
                );
                k += 1;
            }
            i += streak;
            continue;
        }

        if msg.role == MessageRole::User {
            turn_tools = None;
            turn_answer = None;
        }

        if msg.role == MessageRole::User {
            let extracted = extract_user_and_reminders(&msg.content);
            if extracted.user_text.is_empty() && extracted.reminders.is_empty() {
                i += 1;
                continue;
            }
            let is_real = !extracted.user_text.is_empty();
            let is_current = last_user_idx == Some(i);
            let turn = if is_real {
                turn_chrome(&app.messages, i, is_current)
            } else {
                None
            };
            let turn_folded = turn.as_ref().is_some_and(|t| t.folded);
            let focused = is_real && turn_owns_focus(&app.messages, i, app.chat_focus);
            if is_real {
                turn_spine = !turn_folded && turn.is_some();
                let start_line = lines.len();
                let paint = render_user(
                    msg,
                    &extracted.user_text,
                    wrap_width,
                    row_width,
                    is_current,
                    turn.as_ref(),
                    focused,
                );
                let end_line = start_line + paint.lines.len().saturating_sub(1);
                user_queries.push(StickyQuery {
                    text: extracted.user_text.clone(),
                    clock: format_user_clock(msg.created_at),
                    start_line,
                    end_line,
                });
                for (k, line) in paint.lines.into_iter().enumerate() {
                    let owner = if paint.content_targets.contains(&k) {
                        ChatLineTarget::UserContent(i)
                    } else {
                        ChatLineTarget::User(i)
                    };
                    lines.push(line);
                    owners.push(Some(owner));
                }
                if turn_folded {
                    skip_folded_turn = true;
                    turn_spine = false;
                }
            }
            if !turn_folded {
                for reminder in &extracted.reminders {
                    if !lines.is_empty() {
                        let blank = Line::from(Span::raw(""));
                        lines.push(if turn_spine {
                            with_turn_spine(blank)
                        } else {
                            blank
                        });
                        owners.push(None);
                    }
                    let rem =
                        render_reminder_card(reminder, wrap_width, row_width, msg.info_expanded);
                    push_owned(
                        &mut lines,
                        &mut owners,
                        spine_lines(rem, turn_spine),
                        Some(ChatLineTarget::Message(i)),
                    );
                }
            }
            i += 1;
            continue;
        }

        let chunk = message_lines(msg, app, wrap_width, row_width, i, None);
        if chunk.is_empty() {
            i += 1;
            continue;
        }

        if !lines.is_empty() {
            let blank = Line::from(Span::styled("", Theme::bg()));
            lines.push(if turn_spine {
                with_turn_spine(blank)
            } else {
                blank
            });
            owners.push(None);
        }
        if msg.role == MessageRole::Assistant && turn_answer.is_none() {
            turn_answer = Some(lines.len());
        }
        let owner = if matches!(
            msg.role,
            MessageRole::Alert | MessageRole::Thinking | MessageRole::Tool
        ) {
            Some(ChatLineTarget::Message(i))
        } else {
            None
        };
        push_owned(
            &mut lines,
            &mut owners,
            spine_lines(chunk, turn_spine),
            owner,
        );
        i += 1;
    }

    // Spinner while waiting for first token, or while only thinking has started.
    // Running tools already show their own cyan spinners — keep this row neutral
    // so it doesn't collide with blue focus or peach user chrome.
    if app.busy && app.stream_buffer.is_empty() && app.thinking_buffer.is_empty() {
        let show = app.messages.last().map(|m| !m.streaming).unwrap_or(true);
        if show {
            if !lines.is_empty() {
                let blank = Line::from("");
                lines.push(if turn_spine {
                    with_turn_spine(blank)
                } else {
                    blank
                });
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
            let wait = Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{spin} "), Theme::tool_icon_running()),
                Span::styled(label, Theme::busy()),
            ]);
            lines.push(if turn_spine {
                with_turn_spine(wait)
            } else {
                wait
            });
            owners.push(None);
        }
    }

    // Trailing blank spacer at the bottom of transcript so the last message
    // and turn footer have breathing room and don't crowd directly against the prompt bar.
    if !lines.is_empty() {
        let blank = Line::from(Span::styled("", Theme::bg()));
        lines.push(blank);
        owners.push(None);
    }

    debug_assert_eq!(lines.len(), owners.len());
    (lines, owners, user_queries, turn_tools, turn_answer)
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
///   ▸  5 tools · 9.8s  [todo_write] [grep ×2] [read ×2]
///   ▾  6 tools · 9.8s          ⟳ agy_search_web  running…
/// ```
pub(super) fn render_tool_group(
    tools: &[Message],
    wrap_width: usize,
    row_width: usize,
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
    let dur = format_ms(tool_view::tools_duration_ms(tools));
    let running = tool_view::first_running(tools);
    let chevron = if expanded { "▾" } else { "▸" };

    let mut left = vec![
        Span::raw("  "),
        Span::styled(chevron, Theme::tool_icon_done()),
        Span::styled(format!(" {n} tools"), Theme::tool_group_title()),
    ];
    if let Some(d) = &dur {
        left.push(Span::styled(format!(" · {d}"), Theme::meta()));
    }

    let right: Vec<Span<'static>> = if let Some(r) = running {
        let raw = r.tool_name.as_deref().unwrap_or("tool");
        let name = tool_view::tool_display_name(raw, &r.content);
        vec![
            Span::styled("⟳ ", Theme::tool_icon_running()),
            Span::styled(name, Theme::tool_name_running()),
            Span::styled("  running…", Theme::meta()),
        ]
    } else {
        let budget = wrap_width.saturating_sub(18).max(8);
        if display_width(&joined) > budget {
            joined = truncate_display(&joined, budget);
        }
        vec![Span::styled(joined, Theme::tool_group())]
    };

    let left_w: usize = left.iter().map(|s| display_width(s.content.as_ref())).sum();
    let right_w: usize = right
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum();
    let pad = row_width.saturating_sub(left_w + right_w);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);
    vec![Line::from(spans)]
}

/// Position of a tool row inside an expanded multi-tool group.
#[derive(Clone, Copy, Debug)]
struct GroupChild {
    is_last: bool,
}

/// Collapsed same-name cluster: `✓  web_search ×3   q1 / q2 / q3   6 hits  1.7s`
fn render_merged_tools(
    tools: &[Message],
    wrap_width: usize,
    row_width: usize,
    group_child: Option<GroupChild>,
) -> Vec<Line<'static>> {
    let first = &tools[0];
    let raw = first.tool_name.as_deref().unwrap_or("tool");
    let name = tool_view::tool_display_name(raw, &first.content);
    let n = tools.len();
    let any_error = tools
        .iter()
        .any(|t| t.tool_status == Some(ToolStatus::Error));
    let cwd = None;
    let queries: Vec<String> = tools
        .iter()
        .map(|t| pretty_tool_args(&t.content, cwd))
        .filter(|q| !q.is_empty())
        .collect();
    let query = queries.join(" / ");
    let summaries: Vec<String> = tools
        .iter()
        .filter_map(|t| t.tool_summary.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let result = if summaries.is_empty() {
        String::new()
    } else {
        summaries[0].clone()
    };
    let dur = format_ms(tool_view::tools_duration_ms(tools)).unwrap_or_default();
    let icon = if any_error { "✗" } else { "✓" };
    let icon_style = if any_error {
        Theme::tool_icon_error()
    } else {
        Theme::tool_icon_done()
    };
    let row_style = if any_error {
        Some(Theme::tool_text_error())
    } else {
        None
    };
    let paint = |s: String, st: ratatui::style::Style| Span::styled(s, row_style.unwrap_or(st));

    let lead_w = if group_child.is_some() { 4 } else { 2 };
    let label = format!("{name} ×{n}");
    let name_w = display_width(&label).max(4).min(18);
    let result_w = 10usize;
    let time_w = 6usize;
    let query_budget = wrap_width
        .saturating_sub(lead_w + 2 + name_w + 2 + result_w + 1 + time_w + 2)
        .max(8);
    let q = truncate_display_middle(&query, query_budget);

    let mut spans = match group_child {
        Some(GroupChild { is_last }) => {
            let branch = if is_last { "└ " } else { "├ " };
            vec![
                Span::raw("  "),
                Span::styled(branch, Theme::tool_tree()),
                Span::styled(format!("{icon} "), icon_style),
            ]
        }
        None => vec![
            Span::raw("  "),
            Span::styled(format!("{icon} "), icon_style),
        ],
    };
    spans.push(paint(pad_end(&label, name_w), Theme::tool_kind(&name)));
    spans.push(Span::raw("  "));
    spans.push(paint(pad_end(&q, query_budget), Theme::tool_detail_done()));
    spans.push(Span::raw(" "));
    spans.push(paint(
        pad_start(&truncate_display(&result, result_w), result_w),
        Theme::meta(),
    ));
    spans.push(Span::raw(" "));
    spans.push(paint(pad_start(&dur, time_w), Theme::meta()));
    spans.push(Span::styled("  ▸", Theme::meta()));
    fill_spans_to(&mut spans, row_width, Theme::bg());
    vec![Line::from(spans)]
}

fn message_lines(
    message: &Message,
    app: &App,
    wrap_width: usize,
    row_width: usize,
    msg_index: usize,
    group_child: Option<GroupChild>,
) -> Vec<Line<'static>> {
    let focused = app.chat_focus == Some(msg_index);
    let mut lines = match message.role {
        MessageRole::User => {
            let extracted = extract_user_and_reminders(&message.content);
            let mut res = Vec::new();
            if !extracted.user_text.is_empty() {
                let is_current = last_user_message_index(&app.messages) == Some(msg_index);
                let turn = turn_chrome(&app.messages, msg_index, is_current);
                res.extend(
                    render_user(
                        message,
                        &extracted.user_text,
                        wrap_width,
                        row_width,
                        is_current,
                        turn.as_ref(),
                        focused,
                    )
                    .lines,
                );
            }
            for reminder in &extracted.reminders {
                if !res.is_empty() {
                    res.push(Line::from(Span::raw("")));
                }
                res.extend(render_reminder_card(
                    reminder,
                    wrap_width,
                    row_width,
                    message.info_expanded,
                ));
            }
            res
        }
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
        MessageRole::System => render_system(&message.content, wrap_width, app.spinner_frame),
        MessageRole::Tool => render_tool(message, app, wrap_width, row_width, group_child),
    };
    if focused && message.role != MessageRole::User {
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
        let raw_content = if message.streaming {
            message.content.trim_start()
        } else {
            message.content.trim()
        };
        let mut body = wrap_thinking_body(raw_content, budget);
        // Live stream: rolling window of the last few lines only.
        if message.streaming && body.len() > THINKING_STREAM_TAIL_LINES {
            body = body[body.len() - THINKING_STREAM_TAIL_LINES..].to_vec();
        }
        for line in body {
            lines.push(Line::from(vec![
                Span::styled("  │ ", Theme::thinking_meta()),
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

/// Wrap thinking body while stripping leading/trailing blanks and collapsing consecutive empty lines.
fn wrap_thinking_body(content: &str, width: usize) -> Vec<String> {
    if content.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut last_was_empty = false;
    for para in content.split('\n') {
        let trimmed = para.trim_end();
        if trimmed.is_empty() {
            if !last_was_empty && !out.is_empty() {
                out.push(String::new());
                last_was_empty = true;
            }
            continue;
        }
        last_was_empty = false;
        let wrapped = wrap_str(trimmed, width);
        if wrapped.is_empty() {
            out.push(String::new());
        } else {
            out.extend(wrapped);
        }
    }
    if out.is_empty() {
        vec![String::new()]
    } else {
        out
    }
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
    message.duration_ms.and_then(format_ms)
}

fn format_ms(ms: u64) -> Option<String> {
    if ms == 0 {
        return None;
    }
    Some(if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let m = ms / 60_000;
        let s = (ms % 60_000) as f64 / 1000.0;
        format!("{m}m{s:.0}s")
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExtractedUserContent {
    pub user_text: String,
    pub reminders: Vec<String>,
}

/// Extract user visible text and `<system-reminder>...</system-reminder>` blocks.
pub(super) fn extract_user_and_reminders(text: &str) -> ExtractedUserContent {
    let mut reminders = Vec::new();
    let mut user_parts = Vec::new();
    let mut rest = text;

    while let Some(start_idx) = rest.find("<system-reminder>") {
        let before = &rest[..start_idx];
        if !before.trim().is_empty() {
            user_parts.push(before.trim());
        }
        let after_start = &rest[start_idx + "<system-reminder>".len()..];
        if let Some(end_idx) = after_start.find("</system-reminder>") {
            let body = after_start[..end_idx].trim();
            if !body.is_empty() {
                reminders.push(body.to_string());
            }
            rest = &after_start[end_idx + "</system-reminder>".len()..];
        } else {
            let body = after_start.trim();
            if !body.is_empty() {
                reminders.push(body.to_string());
            }
            rest = "";
            break;
        }
    }

    if !rest.trim().is_empty() {
        let mut rem_rest = rest;
        while let Some(start_idx) = rem_rest.find("<reminder>") {
            let before = &rem_rest[..start_idx];
            if !before.trim().is_empty() {
                user_parts.push(before.trim());
            }
            let after_start = &rem_rest[start_idx + "<reminder>".len()..];
            if let Some(end_idx) = after_start.find("</reminder>") {
                let body = after_start[..end_idx].trim();
                if !body.is_empty() {
                    reminders.push(body.to_string());
                }
                rem_rest = &after_start[end_idx + "</reminder>".len()..];
            } else {
                let body = after_start.trim();
                if !body.is_empty() {
                    reminders.push(body.to_string());
                }
                rem_rest = "";
                break;
            }
        }
        if !rem_rest.trim().is_empty() {
            user_parts.push(rem_rest.trim());
        }
    }

    ExtractedUserContent {
        user_text: user_parts.join("\n\n"),
        reminders,
    }
}

/// Strip `<system-reminder>...</system-reminder>` blocks (and `<reminder>...</reminder>`) from text.
pub(super) fn strip_system_reminders(text: &str) -> String {
    extract_user_and_reminders(text).user_text
}

/// Render an injected `<system-reminder>` block as a lightweight, low-contrast Context block
/// (Claude Code / Linear style: "◇ Context" with clean indented summaries, no heavy boxes).
pub(super) fn render_reminder_card(
    content: &str,
    wrap_width: usize,
    row_width: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if trimmed.contains("MCP servers connected:") || trimmed.contains("MCP servers") {
        return render_mcp_context(trimmed, wrap_width, row_width, expanded);
    }

    if trimmed.contains("Graph Intent Guidance") || trimmed.contains("激活的策略与约束提醒")
    {
        return render_intent_context(trimmed, wrap_width, row_width, expanded);
    }

    if trimmed.contains("Active Intent") || trimmed.contains("Learned Tool Intent") {
        return render_active_intent_context(trimmed, wrap_width, row_width, expanded);
    }

    render_generic_reminder(trimmed, wrap_width, row_width, expanded)
}

fn parse_mcp_servers(content: &str) -> (usize, usize, Vec<String>, bool) {
    let mut servers = Vec::new();
    let mut total_tools = 0;
    let mut has_usage_hint = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('-') || trimmed.starts_with('•') || trimmed.starts_with('*') {
            let item = trimmed
                .trim_start_matches(|c| c == '-' || c == '•' || c == '*' || c == ' ')
                .trim();
            if let Some(open_paren) = item.find('(') {
                let name = item[..open_paren].trim();
                let rest = &item[open_paren + 1..];
                if let Some(close_paren) = rest.find(')') {
                    let tool_part = rest[..close_paren].trim();
                    let count_str = tool_part.split_whitespace().next().unwrap_or("0");
                    let count: usize = count_str.parse().unwrap_or(0);
                    if !name.is_empty() {
                        servers.push((name.to_string(), count));
                        total_tools += count;
                    }
                }
            } else if let Some(colon) = item.find(':') {
                let name = item[..colon].trim();
                if !name.is_empty() && !name.contains(' ') {
                    servers.push((name.to_string(), 0));
                }
            }
        }
        if trimmed.contains("search_tool") || trimmed.contains("use_tool") {
            has_usage_hint = true;
        }
    }

    let count = servers.len();
    let names = servers.into_iter().map(|(n, _)| n).collect();
    (count, total_tools, names, has_usage_hint)
}

fn render_mcp_context(
    content: &str,
    wrap_width: usize,
    row_width: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    let (server_count, tool_count, server_names, has_usage_hint) = parse_mcp_servers(content);
    let server_list = server_names.join(" · ");
    let summary = if server_count > 0 && tool_count > 0 {
        format!("{server_count} MCP · {tool_count} tools ({server_list})")
    } else if server_count > 0 {
        format!("{server_count} MCP ({server_list})")
    } else {
        "MCP servers connected".into()
    };
    if !expanded {
        return vec![info_summary_line(&summary, wrap_width, row_width)];
    }
    let mut pairs = vec![("MCP", summary)];
    if has_usage_hint {
        pairs.push(("hint", "search_tool before use_tool".into()));
    }
    info_kv_table(&pairs, wrap_width, row_width)
}

fn render_intent_context(
    content: &str,
    wrap_width: usize,
    row_width: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    let parsed = parse_intent_fields(content);
    if parsed.title.is_none() && parsed.policies.is_empty() && parsed.tools.is_empty() {
        return render_generic_reminder(content, wrap_width, row_width, expanded);
    }
    if !expanded {
        let mut bits = Vec::new();
        if let Some(title) = &parsed.title {
            let conf = parsed
                .confidence
                .map(|c| format!(" ({c:.2})"))
                .unwrap_or_default();
            bits.push(format!("意图: {title}{conf}"));
        }
        if let Some(pol) = parsed.policies.first() {
            bits.push(format!("策略: {pol}"));
        }
        let summary = if bits.is_empty() {
            "Intent".into()
        } else {
            bits.join("  ·  ")
        };
        let mut line = info_summary_line(&summary, wrap_width, row_width);
        if let Some(score) = parsed.confidence {
            colorize_confidence(&mut line, score);
        }
        return vec![line];
    }
    let mut pairs: Vec<(&str, String)> = Vec::new();
    if let Some(title) = parsed.title {
        pairs.push(("意图", title));
    }
    if let Some(score) = parsed.confidence {
        pairs.push(("置信度", format!("{score:.2}")));
    }
    for pol in parsed.policies.iter().take(3) {
        pairs.push(("策略", pol.clone()));
    }
    if !parsed.tools.is_empty() {
        pairs.push(("工具", parsed.tools.join(", ")));
    }
    let mut out = info_kv_table(&pairs, wrap_width, row_width);
    if let Some(score) = parsed.confidence {
        colorize_confidence_in_table(&mut out, score);
    }
    out
}

fn render_active_intent_context(
    content: &str,
    wrap_width: usize,
    row_width: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    let parsed = parse_intent_fields(content);
    let mut items = parsed.policies;
    if items.is_empty() {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let clean = trimmed
                .trim_start_matches(|c| c == '-' || c == '•' || c == '*' || c == ' ')
                .trim();
            if !clean.is_empty() {
                items.push(clean.to_string());
            }
        }
    }
    if items.is_empty() && parsed.title.is_none() {
        return render_generic_reminder(content, wrap_width, row_width, expanded);
    }
    if !expanded {
        let mut bits = Vec::new();
        if let Some(title) = &parsed.title {
            bits.push(format!("意图: {title}"));
        }
        if let Some(item) = items.first() {
            bits.push(item.clone());
        }
        return vec![info_summary_line(
            &bits.join("  ·  "),
            wrap_width,
            row_width,
        )];
    }
    let mut pairs: Vec<(&str, String)> = Vec::new();
    if let Some(title) = parsed.title {
        pairs.push(("意图", title));
    }
    for item in items.iter().take(4) {
        pairs.push(("策略", item.clone()));
    }
    info_kv_table(&pairs, wrap_width, row_width)
}

fn render_generic_reminder(
    content: &str,
    wrap_width: usize,
    row_width: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    let preview = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("Reminder");
    if !expanded {
        return vec![info_summary_line(preview, wrap_width, row_width)];
    }
    info_kv_table(&[("备注", preview.to_string())], wrap_width, row_width)
}

struct IntentFields {
    title: Option<String>,
    confidence: Option<f64>,
    policies: Vec<String>,
    tools: Vec<String>,
}

fn parse_intent_fields(content: &str) -> IntentFields {
    let mut title = None;
    let mut confidence = None;
    let mut policies = Vec::new();
    let mut tools = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.split("置信度:").nth(1) {
            let num: String = rest
                .chars()
                .skip_while(|c| !c.is_ascii_digit() && *c != '.')
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(v) = num.parse::<f64>() {
                confidence = Some(v);
            }
        }
        if trimmed.contains("**[") {
            if let Some(start) = trimmed.find("**[") {
                let after = &trimmed[start + 3..];
                if let Some(end) = after.find("]**") {
                    let name = after[..end].trim();
                    if !name.is_empty() && title.is_none() {
                        title = Some(name.to_string());
                    }
                    let desc = after[end + 3..]
                        .trim()
                        .trim_start_matches('：')
                        .trim_start_matches(':')
                        .trim();
                    // Drop "(置信度: …)" tail from the policy blurb.
                    let desc = if let Some(idx) = desc.find("(置信度") {
                        desc[..idx].trim()
                    } else {
                        desc
                    };
                    let desc = desc.trim_start_matches('·').trim();
                    if !desc.is_empty() {
                        policies.push(desc.to_string());
                    } else if !name.is_empty() {
                        policies.push(name.to_string());
                    }
                }
            }
        }
        if trimmed.contains("建议优先考虑工具") || trimmed.contains("推荐工具") {
            if let Some(tool_start) = trimmed.find('`') {
                let after = &trimmed[tool_start + 1..];
                if let Some(tool_end) = after.find('`') {
                    let t = after[..tool_end].trim();
                    if !t.is_empty() && !tools.contains(&t.to_string()) {
                        tools.push(t.to_string());
                    }
                }
            }
        }
    }
    IntentFields {
        title,
        confidence,
        policies,
        tools,
    }
}

fn info_summary_line(summary: &str, wrap_width: usize, row_width: usize) -> Line<'static> {
    let chevron = "▸";
    let prefix_w = 2 + 3;
    let suffix_w = 3;
    let budget = wrap_width
        .min(row_width)
        .saturating_sub(prefix_w + suffix_w)
        .max(8);
    let text = truncate_display(summary, budget);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("ℹ  ", Theme::context_glyph()),
        Span::styled(text, Theme::context_body()),
    ];
    let used: usize = spans
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum();
    let gap = row_width.saturating_sub(used + suffix_w).max(1);
    spans.push(Span::styled(" ".repeat(gap), Theme::bg()));
    spans.push(Span::styled(format!("  {chevron}"), Theme::meta()));
    Line::from(spans)
}

fn info_kv_table(
    pairs: &[(&str, String)],
    wrap_width: usize,
    row_width: usize,
) -> Vec<Line<'static>> {
    let mut header = vec![
        Span::raw("  "),
        Span::styled("ℹ  ", Theme::context_glyph()),
        Span::styled("Context", Theme::context_header()),
        Span::styled("  ▾", Theme::meta()),
    ];
    fill_spans_to(&mut header, row_width, Theme::bg());
    let mut out = vec![Line::from(header)];
    let label_w = pairs
        .iter()
        .map(|(k, _)| display_width(k))
        .max()
        .unwrap_or(4)
        .max(4);
    let budget = wrap_width.saturating_sub(label_w + 8).max(8);
    for (label, value) in pairs {
        let wrapped = wrap_str(value, budget);
        for (i, row) in wrapped.into_iter().enumerate() {
            let lab = if i == 0 {
                pad_start(label, label_w)
            } else {
                " ".repeat(label_w)
            };
            let mut spans = vec![
                Span::raw("    "),
                Span::styled(lab, Theme::context_body()),
                Span::raw("  "),
                Span::styled(row, Theme::context_highlight()),
            ];
            fill_spans_to(&mut spans, row_width, Theme::bg());
            out.push(Line::from(spans));
        }
    }
    out
}

fn colorize_confidence(line: &mut Line<'static>, score: f64) {
    let needle = format!("({score:.2})");
    for span in &mut line.spans {
        if span.content.contains(&needle) {
            span.style = Theme::confidence(score);
        }
    }
}

fn colorize_confidence_in_table(lines: &mut [Line<'static>], score: f64) {
    let needle = format!("{score:.2}");
    for line in lines {
        let is_conf = line.spans.iter().any(|s| s.content.contains("置信度"));
        if !is_conf {
            continue;
        }
        for span in &mut line.spans {
            if span.content.contains(&needle) {
                span.style = Theme::confidence(score);
            }
        }
    }
}

fn last_user_message_index(messages: &[Message]) -> Option<usize> {
    messages.iter().enumerate().rev().find_map(|(i, m)| {
        if m.role == MessageRole::User
            && !extract_user_and_reminders(&m.content).user_text.is_empty()
        {
            Some(i)
        } else {
            None
        }
    })
}

fn format_user_clock(ts: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(ts)
        .format("%H:%M")
        .to_string()
}

/// `┃ `/`  ` + `HH:MM` + two spaces before the question.
const TURN_TIME_COL: usize = 9;

fn turn_owns_focus(messages: &[Message], user_idx: usize, focus: Option<usize>) -> bool {
    let Some(f) = focus else {
        return false;
    };
    if f == user_idx {
        return true;
    }
    let end = crate::user_fold::turn_end(messages, user_idx);
    f > user_idx && f < end
}

fn spine_lines(lines: Vec<Line<'static>>, spine: bool) -> Vec<Line<'static>> {
    if !spine {
        return lines;
    }
    lines.into_iter().map(with_turn_spine).collect()
}

fn with_turn_spine(line: Line<'static>) -> Line<'static> {
    let rail = Span::styled("┃", Theme::turn_rail());
    let gap = Span::raw(" ");
    if line.spans.is_empty() {
        return Line::from(vec![rail, gap]);
    }
    let first = line.spans[0].content.as_ref();
    if first.starts_with('┃') {
        return line;
    }
    if first == "▌ " || first == "▌" || first == "  " {
        let mut spans = vec![rail, gap];
        spans.extend(line.spans.into_iter().skip(1));
        return Line::from(spans);
    }
    if let Some(rest) = first.strip_prefix("  ") {
        let rest = rest.to_string();
        let style = line.spans[0].style;
        let mut spans = vec![rail, gap];
        if !rest.is_empty() {
            spans.push(Span::styled(rest, style));
        }
        spans.extend(line.spans.into_iter().skip(1));
        return Line::from(spans);
    }
    if first.is_empty() {
        let mut spans = vec![rail, gap];
        spans.extend(line.spans.into_iter().skip(1));
        return Line::from(spans);
    }
    let mut spans = vec![rail, gap];
    spans.extend(line.spans);
    Line::from(spans)
}

fn timeline_prefix(rail: bool) -> Vec<Span<'static>> {
    if rail {
        vec![
            Span::styled("┃", Theme::turn_rail_user()),
            Span::styled(" ", Theme::user_bg()),
        ]
    } else {
        vec![Span::styled("  ", Theme::user_bg())]
    }
}

fn timeline_header(
    clock: &str,
    text: &str,
    chevron: Option<char>,
    rail: bool,
    dim: bool,
    row_width: usize,
) -> Line<'static> {
    let mut spans = timeline_prefix(rail);
    spans.push(Span::styled(clock.to_string(), Theme::turn_time()));
    spans.push(Span::styled("  ", Theme::user_bg()));
    let chevron_w = if chevron.is_some() { 2 } else { 0 };
    let used = TURN_TIME_COL + chevron_w;
    let budget = row_width.saturating_sub(used).max(4);
    let shown = truncate_display(text, budget);
    let style = if dim {
        Theme::turn_preview_dim()
    } else {
        Theme::turn_preview()
    };
    spans.push(Span::styled(shown, style));
    if let Some(ch) = chevron {
        let left_w: usize = spans
            .iter()
            .map(|s| display_width(s.content.as_ref()))
            .sum();
        let gap = row_width.saturating_sub(left_w + 1).max(1);
        spans.push(Span::styled(" ".repeat(gap), Theme::user_bg()));
        spans.push(Span::styled(ch.to_string(), Theme::user_fold_key()));
    }
    fill_spans_to(&mut spans, row_width, Theme::user_bg());
    Line::from(spans)
}

fn timeline_cont(text: &str, rail: bool, dim: bool, row_width: usize) -> Line<'static> {
    let mut spans = timeline_prefix(rail);
    spans.push(Span::styled("       ", Theme::user_bg()));
    let budget = row_width.saturating_sub(TURN_TIME_COL).max(4);
    let shown = truncate_display(text, budget);
    let style = if dim {
        Theme::turn_preview_dim()
    } else {
        Theme::turn_preview()
    };
    spans.push(Span::styled(shown, style));
    fill_spans_to(&mut spans, row_width, Theme::user_bg());
    Line::from(spans)
}

fn timeline_remainder(label: &str, rail: bool, row_width: usize) -> Line<'static> {
    let mut spans = timeline_prefix(rail);
    spans.push(Span::styled("       ", Theme::user_bg()));
    spans.push(Span::styled(format!("· {label}"), Theme::turn_time()));
    fill_spans_to(&mut spans, row_width, Theme::user_bg());
    Line::from(spans)
}

struct TurnChrome {
    folded: bool,
}

struct UserPaint {
    lines: Vec<Line<'static>>,
    content_targets: Vec<usize>,
}

fn turn_chrome(messages: &[Message], user_idx: usize, is_current: bool) -> Option<TurnChrome> {
    let msg = messages.get(user_idx)?;
    if !crate::user_fold::is_real_user(msg) {
        return None;
    }
    if !crate::user_fold::turn_has_followup(messages, user_idx) {
        return None;
    }
    let expanded = msg.turn_expanded;
    let folded = crate::user_fold::is_turn_folded(expanded, is_current, true);
    Some(TurnChrome { folded })
}

fn render_user(
    msg: &Message,
    content: &str,
    wrap_width: usize,
    row_width: usize,
    is_current: bool,
    turn: Option<&TurnChrome>,
    focused: bool,
) -> UserPaint {
    let content_folded = crate::user_fold::is_folded(msg.user_expanded, is_current, content);
    let clock = format_user_clock(msg.created_at);
    let turn_foldable = turn.is_some();
    let turn_folded = turn.is_some_and(|t| t.folded);
    let rail = focused || (turn_foldable && !turn_folded);
    let dim = turn_folded && !is_current && !focused;
    let chevron = if turn_foldable {
        Some(if turn_folded { '▸' } else { '▾' })
    } else {
        None
    };
    let budget = wrap_width.saturating_sub(TURN_TIME_COL).max(8);

    if turn_folded {
        let preview = crate::user_fold::turn_preview(content);
        return UserPaint {
            lines: vec![timeline_header(
                &clock, &preview, chevron, rail, dim, row_width,
            )],
            content_targets: Vec::new(),
        };
    }

    let mut out = Vec::new();
    let mut content_targets = Vec::new();

    if content_folded {
        match crate::user_fold::collapse_plan(content) {
            Some(crate::user_fold::Collapse::Code {
                keep_before,
                body_lines,
                ..
            }) => {
                let kept: Vec<&str> = content.lines().take(keep_before).collect();
                let head = kept
                    .first()
                    .copied()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or("代码块");
                out.push(timeline_header(&clock, head, chevron, rail, dim, row_width));
                for line in kept.iter().skip(1) {
                    for wrapped in wrap_paragraphs(line, budget) {
                        out.push(timeline_cont(&wrapped, rail, dim, row_width));
                    }
                }
                content_targets.push(out.len());
                out.push(timeline_remainder(
                    &format!("代码块 ({body_lines} 行)"),
                    rail,
                    row_width,
                ));
            }
            Some(crate::user_fold::Collapse::Text {
                keep,
                hidden,
                chars,
                ..
            }) => {
                let kept: Vec<&str> = content.lines().take(keep).collect();
                let mut remain = hidden;
                let mut visual: Vec<String> = Vec::new();
                for line in &kept {
                    visual.extend(wrap_paragraphs(line, budget));
                }
                if chars > crate::user_fold::FOLD_CHAR_THRESHOLD
                    && visual.len() > crate::user_fold::PREVIEW_LINES
                {
                    remain = remain.saturating_add(visual.len() - crate::user_fold::PREVIEW_LINES);
                    visual.truncate(crate::user_fold::PREVIEW_LINES);
                }
                let head = if visual.is_empty() {
                    crate::user_fold::turn_preview(content)
                } else {
                    visual.remove(0)
                };
                out.push(timeline_header(
                    &clock, &head, chevron, rail, dim, row_width,
                ));
                for line in visual {
                    out.push(timeline_cont(&line, rail, dim, row_width));
                }
                if remain > 0 {
                    content_targets.push(out.len());
                    out.push(timeline_remainder(
                        &format!("还有 {remain} 行"),
                        rail,
                        row_width,
                    ));
                }
            }
            None => {
                push_expanded_user_body(
                    &mut out, content, &clock, chevron, rail, dim, budget, row_width,
                );
            }
        }
    } else {
        push_expanded_user_body(
            &mut out, content, &clock, chevron, rail, dim, budget, row_width,
        );
    }
    UserPaint {
        lines: out,
        content_targets,
    }
}

fn push_expanded_user_body(
    out: &mut Vec<Line<'static>>,
    content: &str,
    clock: &str,
    chevron: Option<char>,
    rail: bool,
    dim: bool,
    budget: usize,
    row_width: usize,
) {
    let mut source = content.lines();
    let first = source.next().unwrap_or("");
    let head = if first.trim().is_empty() {
        crate::user_fold::turn_preview(content)
    } else {
        first.to_string()
    };
    let wrapped = wrap_paragraphs(&head, budget);
    if wrapped.is_empty() {
        out.push(timeline_header(clock, &head, chevron, rail, dim, row_width));
    } else {
        out.push(timeline_header(
            clock,
            &wrapped[0],
            chevron,
            rail,
            dim,
            row_width,
        ));
        for line in wrapped.into_iter().skip(1) {
            out.push(timeline_cont(&line, rail, dim, row_width));
        }
    }
    let rest: String = source.collect::<Vec<_>>().join("\n");
    if rest.trim().is_empty()
        && !content.contains("```")
        && !content.contains("[文本")
        && !content.contains("[图片")
    {
        return;
    }
    if !rest.trim().is_empty() {
        out.extend(render_user_body(&rest, budget, row_width, rail, dim));
    } else if content.contains("```") || content.contains("[文本") || content.contains("[图片")
    {
        out.extend(render_user_body(content, budget, row_width, rail, dim));
        if out.len() > 1 {
            // First line already painted as the header; drop the duplicate body head
            // when we re-rendered the whole content for chips/code.
            let header = out.remove(0);
            if !out.is_empty() {
                out[0] = header;
            } else {
                out.push(header);
            }
        }
    }
}

enum UserSeg {
    Text(String),
    Code { lang: String, body: String },
}

fn split_user_segments(content: &str) -> Vec<UserSeg> {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < lines.len() {
        if crate::user_fold::is_fence(lines[i]) {
            if !buf.is_empty() {
                out.push(UserSeg::Text(std::mem::take(&mut buf)));
            }
            let lang = crate::user_fold::fence_lang(lines[i]).to_string();
            i += 1;
            let mut body = String::new();
            while i < lines.len() && !crate::user_fold::is_fence(lines[i]) {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(lines[i]);
                i += 1;
            }
            out.push(UserSeg::Code { lang, body });
            if i < lines.len() {
                i += 1;
            }
            continue;
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(lines[i]);
        i += 1;
    }
    if !buf.is_empty() {
        out.push(UserSeg::Text(buf));
    }
    out
}

fn render_user_body(
    content: &str,
    budget: usize,
    row_width: usize,
    rail: bool,
    dim: bool,
) -> Vec<Line<'static>> {
    let segs = split_user_segments(content);
    if segs.is_empty() {
        return vec![timeline_cont("", rail, dim, row_width)];
    }
    let mut out = Vec::new();
    for seg in segs {
        match seg {
            UserSeg::Text(t) => out.extend(render_user_text(&t, budget, row_width, rail, dim)),
            UserSeg::Code { lang, body } => {
                out.extend(render_user_code(&lang, &body, budget, row_width, rail))
            }
        }
    }
    out
}

fn render_user_text(
    content: &str,
    budget: usize,
    row_width: usize,
    rail: bool,
    dim: bool,
) -> Vec<Line<'static>> {
    let has_chips = content.contains("[文本") || content.contains("[图片");
    if has_chips {
        return render_user_with_chips(content, budget, row_width, rail);
    }
    let wrapped = wrap_paragraphs(content, budget);
    let mut out = Vec::with_capacity(wrapped.len().max(1));
    for line in wrapped {
        out.push(timeline_cont(&line, rail, dim, row_width));
    }
    out
}

fn render_user_with_chips(
    content: &str,
    budget: usize,
    row_width: usize,
    rail: bool,
) -> Vec<Line<'static>> {
    let spans: Vec<Span<'static>> = tokenize_input_chips(content)
        .into_iter()
        .map(|(text, kind)| {
            let style = match kind {
                Some(InputChipKind::Text) => Theme::user_text_chip(),
                Some(InputChipKind::Image) => Theme::user_image_chip(),
                None => Theme::user_body(),
            };
            Span::styled(text, style)
        })
        .collect();
    let rows = wrap_styled_spans(&spans, budget);
    let mut out = Vec::with_capacity(rows.len().max(1));
    if rows.is_empty() {
        out.push(timeline_cont("", rail, false, row_width));
        return out;
    }
    for row in rows {
        let mut spans = timeline_prefix(rail);
        spans.push(Span::styled("       ", Theme::user_bg()));
        if !(row.is_empty() || row.iter().all(|s| s.content.trim().is_empty())) {
            spans.extend(row);
        }
        fill_spans_to(&mut spans, row_width, Theme::user_bg());
        out.push(Line::from(spans));
    }
    out
}

fn render_user_code(
    lang: &str,
    body: &str,
    budget: usize,
    row_width: usize,
    rail: bool,
) -> Vec<Line<'static>> {
    let inner = budget.max(4);
    let mut out = Vec::new();
    let open = if lang.is_empty() {
        "```".to_string()
    } else {
        format!("```{lang}")
    };
    out.push(timeline_cont(&open, rail, true, row_width));

    if body.is_empty() {
        out.push(user_code_fill_line("", inner, row_width, rail));
    } else {
        for line in body.split('\n') {
            out.push(user_code_fill_line(line, inner, row_width, rail));
        }
    }

    out.push(timeline_cont("```", rail, true, row_width));
    out
}

fn user_code_fill_line(line: &str, inner: usize, row_width: usize, rail: bool) -> Line<'static> {
    let shown = truncate_display(line, inner);
    let mut spans = timeline_prefix(rail);
    spans.push(Span::raw("       "));
    let rest = row_width.saturating_sub(TURN_TIME_COL);
    let pad = rest.saturating_sub(display_width(&shown));
    spans.push(Span::styled(shown, Theme::user_code()));
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), Theme::user_code()));
    }
    Line::from(spans)
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

fn render_system(content: &str, wrap_width: usize, spinner_frame: usize) -> Vec<Line<'static>> {
    // Compaction / meta style: subtle top rule for multi-word notices, else faint line.
    let budget = wrap_width.saturating_sub(4).max(8);
    let mut out = Vec::new();

    if content.eq_ignore_ascii_case("compaction") || content.starts_with("──") {
        let bar = "─".repeat(wrap_width.saturating_sub(2));
        out.push(Line::from(Span::styled(format!(" {bar}"), Theme::meta())));
        return out;
    }

    if crate::message::Message::is_context_compacting(content) {
        let spin = SPINNER[spinner_frame % SPINNER.len()];
        let header = format!("{spin} Compacting context…");
        let bar_len = wrap_width.saturating_sub(header.chars().count() + 6).max(2) / 2;
        let bar = "─".repeat(bar_len);
        out.push(Line::from(vec![
            Span::styled(format!(" {bar} "), Theme::meta()),
            Span::styled(spin.to_string(), Theme::tool_icon_running()),
            Span::styled(
                " Compacting context…",
                Theme::meta().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(format!(" {bar}"), Theme::meta()),
        ]));
        return out;
    }

    if crate::message::Message::is_context_compacted(content) {
        let label = content
            .trim_start_matches(crate::message::Message::CONTEXT_COMPACTED_PREFIX)
            .trim()
            .trim_start_matches('·')
            .trim();
        let header = if label.is_empty() {
            "Context compacted".to_string()
        } else {
            format!("Context compacted · {label}")
        };
        let bar_len = wrap_width.saturating_sub(header.chars().count() + 6).max(2) / 2;
        let bar = "─".repeat(bar_len);
        out.push(Line::from(vec![
            Span::styled(format!(" {bar} "), Theme::meta()),
            Span::styled(
                header,
                Theme::meta().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(format!(" {bar}"), Theme::meta()),
        ]));
        return out;
    }

    if content.starts_with("[Compaction summary]") {
        // Legacy full-dump summary (pre-marker UI). Collapse to the same
        // one-line divider — the LLM still sees the summary in agent context.
        let header = "Context compacted".to_string();
        let bar_len = wrap_width.saturating_sub(header.chars().count() + 6).max(2) / 2;
        let bar = "─".repeat(bar_len);
        out.push(Line::from(vec![
            Span::styled(format!(" {bar} "), Theme::meta()),
            Span::styled(
                header,
                Theme::meta().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(format!(" {bar}"), Theme::meta()),
        ]));
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
    row_width: usize,
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

    let name_style = match status {
        ToolStatus::Running => Theme::tool_name_running(),
        ToolStatus::Error => Theme::tool_name_error(),
        ToolStatus::Done => Theme::tool_kind(&name),
    };
    let query_style = match status {
        ToolStatus::Running => Theme::tool_detail_running(),
        ToolStatus::Error => Theme::tool_text_error(),
        ToolStatus::Done => Theme::tool_detail_done(),
    };

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

    let lead_w = if group_child.is_some() { 4 } else { 2 };
    let name_w = display_width(&name).max(4).min(16);
    let result_w = 10usize;
    let time_w = 6usize;
    let query_budget = wrap_width
        .saturating_sub(lead_w + 2 + name_w + 2 + result_w + 1 + time_w)
        .max(8);
    let raw_query = if detail.is_empty() {
        String::new()
    } else {
        pretty_tool_args(detail, cwd)
    };
    let pretty = if raw_query.is_empty() {
        String::new()
    } else if tool_view::looks_like_path(&raw_query) {
        truncate_display_middle(&raw_query, query_budget)
    } else {
        truncate_display(&raw_query, query_budget)
    };
    // Keep `pretty` for the expanded-body truncation check below.
    let budget = query_budget;

    let mut lines = Vec::new();
    let mut spans = match group_child {
        Some(GroupChild { is_last }) => {
            let branch = if is_last { "└ " } else { "├ " };
            vec![
                Span::raw("  "),
                Span::styled(branch, Theme::tool_tree()),
                Span::styled(format!("{icon} "), icon_style),
            ]
        }
        None => vec![
            Span::raw("  "),
            Span::styled(format!("{icon} "), icon_style),
        ],
    };
    let name_cell = pad_end(&name, name_w);
    let result_cell = pad_start(&truncate_display(&summary_clean, result_w), result_w);
    let time_cell = pad_start(dur.as_deref().unwrap_or(""), time_w);
    let row_style = if is_error {
        Some(Theme::tool_text_error())
    } else {
        None
    };
    let paint = |s: String, st: ratatui::style::Style| Span::styled(s, row_style.unwrap_or(st));
    spans.push(paint(name_cell, name_style));
    spans.push(Span::raw("  "));
    if !pretty.is_empty() {
        spans.push(paint(pad_end(&pretty, query_budget), query_style));
    } else {
        spans.push(Span::raw(" ".repeat(query_budget)));
    }
    spans.push(Span::raw(" "));
    spans.push(paint(result_cell, Theme::meta()));
    spans.push(Span::raw(" "));
    spans.push(paint(time_cell, Theme::meta()));
    fill_spans_to(&mut spans, row_width, Theme::bg());
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

#[cfg(test)]
mod sticky_query_tests {
    use super::*;

    fn q(text: &str, start_line: usize, end_line: usize) -> StickyQuery {
        StickyQuery {
            text: text.into(),
            clock: "12:00".into(),
            start_line,
            end_line,
        }
    }

    #[test]
    fn no_sticky_while_owning_bubble_is_on_screen() {
        let queries = [q("first", 0, 2), q("second", 20, 22)];
        assert!(sticky_query_at(&queries, 0).is_none());
        assert!(sticky_query_at(&queries, 2).is_none());
        assert!(sticky_query_at(&queries, 20).is_none());
        assert!(sticky_query_at(&queries, 22).is_none());
    }

    #[test]
    fn pins_nearest_scrolled_off_query() {
        let queries = [q("first", 0, 2), q("second", 20, 22)];
        assert_eq!(sticky_query_at(&queries, 3).unwrap().text, "first");
        assert_eq!(sticky_query_at(&queries, 19).unwrap().text, "first");
        assert_eq!(sticky_query_at(&queries, 23).unwrap().text, "second");
    }
}
