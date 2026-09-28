//! Chat transcript paint: messages, tools, thinking, selection highlight.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::markdown;
use crate::message::{AlertLevel, ChatLineTarget, Message, MessageRole, ToolStatus};
use crate::theme::Theme;
use crate::tool_view::{self, DiffLineKind};

use super::text::{
    display_width, fill_spans_to, pad_end, pad_start, scrollbar_thumb_geometry, truncate_display,
    truncate_display_middle, wrap_paragraphs, wrap_str, wrap_styled_segments,
};
use super::SPINNER;

/// Turn header indicator uses ASCII pulsing dots so it is guaranteed 1-column wide in all locales.
const TURN_PULSE_SPINNER: &[&str] = &[".", "o", "O", "o"];

pub(super) fn draw_chat(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    // Outer padding: left gutter · content · right gap · right gutter (scrollbar track).
    let pad = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    let content_full = pad[1];
    let sb_col = pad[3];
    // Render across the full viewport width.
    let row_width = (content_full.width as usize).max(16);
    let wrap_width = row_width;

    // Pinned busy strip: queued steer/follow-up chips and the waiting spinner
    // stay anchored just above the prompt instead of drifting up with a short
    // transcript. Reserved before viewport math so scroll offsets account for it.
    let strip_lines = busy_strip_lines(app, wrap_width);
    let strip_h = strip_lines.len() as u16;
    let (content, busy_area) = if strip_h > 0 && content_full.height > strip_h + 3 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(strip_h)])
            .split(content_full);
        (chunks[0], Some(chunks[1]))
    } else {
        (content_full, None)
    };
    let sb_col = Rect {
        height: content.height,
        ..sb_col
    };
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
        max_from_bottom
    } else {
        app.chat_scroll
    };
    app.chat_view_start = start;
    // New / short chats start at the top of the pane (0,0) — not pinned to the prompt.
    app.chat_top_pad = 0;
    // Content origin for mouse → caret mapping (matches horizontal pad above).
    app.chat_content_x = content.x;
    let sel = app.selection_span();

    // Sticky header: one timeline row for the user prompt that owns the
    // viewport once that row has scrolled off. Same chrome as the turn header
    // (chevron, clock, title, tools/duration/status) — no pin icon, no box.
    let sticky_query = sticky_query_at(&user_queries, start);
    let need_sticky = sticky_query.is_some() && view_h >= 4;

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
        let width = s_area.width as usize;
        let sticky_chevron = if query.running {
            let spin = TURN_PULSE_SPINNER[app.spinner_frame % TURN_PULSE_SPINNER.len()];
            Some(spin)
        } else {
            query.chevron.as_deref()
        };
        let line = render_turn_header_line(
            sticky_chevron,
            &query.clock,
            &query.text.replace('\n', " "),
            &query.meta,
            query.running,
            width,
            true,
            false,
        );
        frame.render_widget(Paragraph::new(vec![line]), s_area);
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

    // Pinned busy strip above the prompt (reserved at the bottom of the pane).
    if let Some(strip_area) = busy_area {
        frame.render_widget(Paragraph::new(strip_lines).style(Theme::bg()), strip_area);
    }

    // Right-edge progress scrollbar when the transcript is taller than the viewport.
    if total > view_h && view_h > 0 && sb_col.width > 0 && sb_col.height > 0 {
        // A fresh-turn pin can put `start` past the usual bottom offset (blank
        // space below the question) — clamp so the thumb stays on the track.
        draw_chat_scrollbar(frame, sb_col, total, view_h, start.min(max_from_bottom));
    }

    // One-line jump hint when browsing history. Clickable; not a bordered card.
    let show_jump_to_bottom = !app.follow_bottom && total > view_h;
    if show_jump_to_bottom && chat_area.height >= 2 && chat_area.width >= 18 {
        app.chat_jump_to_bottom_rect = Some(draw_jump_to_bottom_hint(frame, chat_area));
    } else {
        app.chat_jump_to_bottom_rect = None;
    }
}

/// Right-aligned one-line overlay: `↓ Jump to bottom  ctrl+End`.
fn draw_jump_to_bottom_hint(frame: &mut Frame<'_>, chat_area: Rect) -> Rect {
    let full = " ↓ Jump to bottom  ctrl+End ";
    let short = " ↓ latest ";
    let use_full = chat_area.width as usize >= display_width(full) + 1;
    let label = if use_full { full } else { short };
    let w = (display_width(label) as u16).min(chat_area.width.saturating_sub(1).max(1));
    let area = Rect {
        x: chat_area
            .x
            .saturating_add(chat_area.width.saturating_sub(w + 1)),
        y: chat_area
            .y
            .saturating_add(chat_area.height.saturating_sub(1)),
        width: w,
        height: 1,
    };
    frame.render_widget(Clear, area);
    let bg = Style::default().bg(Theme::STICKY_BG);
    let mut spans = vec![
        Span::styled(" ↓ ", bg.fg(Theme::PRIMARY)),
        Span::styled(
            if use_full { "Jump to bottom" } else { "latest" },
            bg.fg(Theme::FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if use_full { "  ctrl+End " } else { " " },
            bg.fg(Theme::MUTED),
        ),
    ];
    fill_spans_to(&mut spans, w as usize, bg);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    area
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
    chevron: Option<String>,
    meta: String,
    running: bool,
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
            let in_current_turn = last_user_idx.is_some_and(|u| i > u);
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
                    lines.push(blank);
                    owners.push(None);
                }
            }

            let is_in_flight_tail = app.busy && (i + streak == app.messages.len());

            // During an active in-flight turn, keep the active/running tool prominently
            // visible on the outside as a full single tool call row, while folding preceding
            // completed tools once there are at least 2 of them.
            if is_in_flight_tail && !any_ungroup && streak >= 2 {
                let last_k = streak - 1;
                let prev_slice = &slice[..last_k];
                let has_group_header = prev_slice.len() >= 2
                    && tool_view::streak_can_collapse(&app.messages, i, last_k);
                if has_group_header {
                    let header = render_tool_group(prev_slice, wrap_width, row_width, false);
                    push_owned(
                        &mut lines,
                        &mut owners,
                        header,
                        Some(ChatLineTarget::ToolGroup(i)),
                    );
                } else {
                    for k in 0..last_k {
                        let tmsg = &slice[k];
                        let chunk = message_lines(tmsg, app, wrap_width, row_width, i + k, None);
                        push_owned(
                            &mut lines,
                            &mut owners,
                            chunk,
                            Some(ChatLineTarget::Message(i + k)),
                        );
                    }
                }

                // Active tool (running or newly completed awaiting model next turn)
                // If preceded by a group header, connect with a visual tree branch `└ `
                let group_child = if has_group_header {
                    Some(GroupChild { is_last: true })
                } else {
                    None
                };
                let tmsg = &slice[last_k];
                let chunk =
                    message_lines(tmsg, app, wrap_width, row_width, i + last_k, group_child);
                push_owned(
                    &mut lines,
                    &mut owners,
                    chunk,
                    Some(ChatLineTarget::Message(i + last_k)),
                );
                i += streak;
                continue;
            }

            if !in_current_turn && tool_view::streak_can_collapse(&app.messages, i, streak) {
                let group_lines =
                    render_tool_group(&app.messages[i..i + streak], wrap_width, row_width, false);
                push_owned(
                    &mut lines,
                    &mut owners,
                    group_lines,
                    Some(ChatLineTarget::ToolGroup(i)),
                );
                i += streak;
                continue;
            }

            if summary_running {
                let done_indices: Vec<usize> = slice
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| m.tool_status == Some(ToolStatus::Done))
                    .map(|(k, _)| k)
                    .collect();
                let running_indices: Vec<usize> = slice
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| m.tool_status == Some(ToolStatus::Running))
                    .map(|(k, _)| k)
                    .collect();

                let has_done_header = done_indices.len() >= 2;
                if has_done_header {
                    let done_slice: Vec<Message> =
                        done_indices.iter().map(|&k| slice[k].clone()).collect();
                    let header = render_tool_group(&done_slice, wrap_width, row_width, false);
                    push_owned(
                        &mut lines,
                        &mut owners,
                        header,
                        Some(ChatLineTarget::ToolGroup(i)),
                    );
                } else {
                    for &k in &done_indices {
                        let tmsg = &slice[k];
                        let chunk = message_lines(tmsg, app, wrap_width, row_width, i + k, None);
                        push_owned(
                            &mut lines,
                            &mut owners,
                            chunk,
                            Some(ChatLineTarget::Message(i + k)),
                        );
                    }
                }

                // In-flight running tools stay prominently visible outside/below with tree branches
                let n_running = running_indices.len();
                for (rn, &k) in running_indices.iter().enumerate() {
                    let tmsg = &slice[k];
                    let group_child = if has_done_header {
                        Some(GroupChild {
                            is_last: rn + 1 == n_running,
                        })
                    } else {
                        None
                    };
                    let chunk = message_lines(tmsg, app, wrap_width, row_width, i + k, group_child);
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

            let show_group_header = (in_current_turn && streak >= 2)
                || streak >= 3
                || tool_view::streak_shows_group_header(&app.messages, i, streak);
            if show_group_header {
                let header = render_tool_group(slice, wrap_width, row_width, true);
                push_owned(
                    &mut lines,
                    &mut owners,
                    header,
                    Some(ChatLineTarget::ToolGroup(i)),
                );
            }
            let visible_end = if show_group_header {
                streak.min(TOOL_BATCH_VISIBLE)
            } else {
                streak
            };
            let mut k = 0;
            while k < visible_end {
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
                            merged,
                            Some(ChatLineTarget::Message(i + k)),
                        );
                        k += same;
                        continue;
                    }
                }
                let group_child = if show_group_header {
                    Some(GroupChild {
                        is_last: k + 1 == visible_end && visible_end == streak,
                    })
                } else {
                    None
                };
                let chunk = message_lines(tmsg, app, wrap_width, row_width, i + k, group_child);
                push_owned(
                    &mut lines,
                    &mut owners,
                    chunk,
                    Some(ChatLineTarget::Message(i + k)),
                );
                k += 1;
            }
            if visible_end < streak {
                let hidden = streak - visible_end;
                push_owned(
                    &mut lines,
                    &mut owners,
                    vec![Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("... {hidden} more tools"), Theme::meta()),
                    ])],
                    Some(ChatLineTarget::ToolGroup(i)),
                );
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
                turn_chrome(&app.messages, i, is_current, app.busy)
            } else {
                None
            };
            let turn_folded = turn.as_ref().is_some_and(|t| t.folded);
            let focused = is_real && app.chat_focus == Some(i);
            if is_real {
                let start_line = lines.len();
                let paint = render_user(
                    msg,
                    &extracted.user_text,
                    wrap_width,
                    row_width,
                    is_current,
                    turn.as_ref(),
                    focused,
                    app.spinner_frame,
                );
                let end_line = start_line + paint.lines.len().saturating_sub(1);
                user_queries.push(StickyQuery {
                    text: crate::user_fold::turn_preview(&extracted.user_text).to_string(),
                    clock: format_user_clock(msg.created_at),
                    chevron: turn.as_ref().and_then(|t| {
                        if t.has_followup {
                            Some(if t.folded { "▸" } else { "▾" }.to_string())
                        } else {
                            None
                        }
                    }),
                    meta: turn.as_ref().map(|t| t.meta.clone()).unwrap_or_default(),
                    running: turn.as_ref().is_some_and(|t| t.running),
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
                }
            }
            if !turn_folded {
                for reminder in &extracted.reminders {
                    if !lines.is_empty() {
                        let blank = Line::from(Span::raw(""));
                        lines.push(blank);
                        owners.push(None);
                    }
                    let rem =
                        render_reminder_card(reminder, wrap_width, row_width, msg.info_expanded);
                    push_owned(
                        &mut lines,
                        &mut owners,
                        rem,
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
            lines.push(blank);
            owners.push(None);
        }
        if msg.role == MessageRole::Assistant && turn_answer.is_none() {
            turn_answer = Some(lines.len());
        }
        let owner = if matches!(
            msg.role,
            MessageRole::Alert | MessageRole::Thinking | MessageRole::Tool | MessageRole::Assistant
        ) {
            Some(ChatLineTarget::Message(i))
        } else {
            None
        };
        push_owned(&mut lines, &mut owners, chunk, owner);
        i += 1;
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

/// Pinned busy strip anchored just above the prompt: queued steer / follow-up
/// chips plus the waiting spinner. Lives outside the transcript flow so it
/// stays at the bottom of the pane no matter how short the transcript is.
fn busy_strip_lines(app: &App, wrap_width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if !app.busy {
        return out;
    }

    // Compact queued controls sit above Waiting, not as user bubbles.
    let queued_steers = app.queued_steers();
    let queued_followups = app.queued_followups();
    for (idx, text) in queued_steers.iter().enumerate() {
        let prefix = if queued_steers.len() == 1 {
            "↳ 你补充 ".to_string()
        } else {
            format!("↳ 你补充 {} ", idx + 1)
        };
        let budget = wrap_width
            .saturating_sub(2usize.saturating_add(display_width(&prefix)))
            .max(4);
        let body = truncate_display(text, budget);
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(prefix, Theme::meta()),
            Span::styled(body, Theme::user_steer()),
        ]));
    }
    for (idx, text) in queued_followups.iter().enumerate() {
        let prefix = if queued_followups.len() == 1 {
            "↳ follow-up ".to_string()
        } else {
            format!("↳ follow-up {} ", idx + 1)
        };
        let budget = wrap_width
            .saturating_sub(2usize.saturating_add(display_width(&prefix)))
            .max(4);
        let body = truncate_display(text, budget);
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(prefix, Theme::meta()),
            Span::styled(body, Theme::meta()),
        ]));
    }

    // Spinner while waiting for first token or while waiting for model next step.
    // Actively running tools are already rendered prominently on the outside as full tool call rows.
    if app.stream_buffer.is_empty() && app.thinking_buffer.is_empty() {
        let show = app.messages.last().map(|m| !m.streaming).unwrap_or(true);
        if show {
            let running_n = app
                .messages
                .iter()
                .filter(|m| m.tool_status == Some(ToolStatus::Running))
                .count();
            if running_n == 0 {
                let spin = SPINNER[app.spinner_frame % SPINNER.len()];
                let label: String = if let Some((mode, count)) = app.wait_park {
                    let noun = if count == 1 { "task" } else { "tasks" };
                    format!("Waiting on {count} {noun} ({mode})…")
                } else if app.busy_activity == "compacting" {
                    "Compacting context…".into()
                } else {
                    "Waiting for model…".into()
                };
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{spin} "), Theme::tool_icon_running()),
                    Span::styled(label, Theme::busy()),
                ]));
            }
        }
    }
    out
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

/// Multi-tool batch header.
///
/// ```text
///   ▾ Tools · 19 tools · 16.4s · grep x2 · ls x4 · read x9          16.4s
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
    let joined = tool_view::aggregate_tool_names(&names);
    let dur = format_ms(tool_view::tools_duration_ms(tools));
    let chevron = if expanded { "▾" } else { "▸" };
    let count = if n == 1 {
        "1 tool".to_string()
    } else {
        format!("{n} tools")
    };

    let mut left = vec![
        Span::raw("  "),
        Span::styled(format!("{chevron} "), Theme::tool_icon_done()),
        Span::styled("Tools", Theme::tool_group_title()),
        Span::styled(format!(" · {count}"), Theme::tool_group_title()),
    ];
    if let Some(d) = &dur {
        left.push(Span::styled(format!(" · {d}"), Theme::meta()));
    }
    if !joined.is_empty() {
        let used: usize = left.iter().map(|s| display_width(s.content.as_ref())).sum();
        let budget = wrap_width.saturating_sub(used + 12).max(8);
        let mut j = joined;
        if display_width(&j) > budget {
            j = truncate_display(&j, budget);
        }
        left.push(Span::styled(format!(" · {j}"), Theme::tool_group()));
    }

    let right = dur
        .as_deref()
        .map(|d| vec![Span::styled(d.to_string(), Theme::meta())])
        .unwrap_or_default();
    vec![split_row(left, right, row_width, Theme::bg())]
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
                let turn = turn_chrome(&app.messages, msg_index, is_current, app.busy);
                res.extend(
                    render_user(
                        message,
                        &extracted.user_text,
                        wrap_width,
                        row_width,
                        is_current,
                        turn.as_ref(),
                        focused,
                        app.spinner_frame,
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
        MessageRole::Steer => render_steer(message, wrap_width, row_width),
        MessageRole::Thinking => render_thinking(message, app, wrap_width, row_width),
        MessageRole::Assistant => render_assistant(message, app, wrap_width, row_width),
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

/// While thinking is streaming, only keep the rolling tail so long chains
/// don't flood the transcript (last N wrapped lines).
pub(super) const THINKING_STREAM_TAIL_LINES: usize = 3;
/// Visible tool rows under an expanded batch before `... N more tools`.
const TOOL_BATCH_VISIBLE: usize = 10;

/// Thinking / reasoning block — collapsible, muted.
///
/// ```text
///   ○ Thinking                                              3.2s
///   │  analyzing the prompt config…
///   ▸ Thinking                                              1.2s
///   │  preview of a collapsed block…
/// ```
pub(super) fn render_thinking(
    message: &Message,
    app: &App,
    wrap_width: usize,
    row_width: usize,
) -> Vec<Line<'static>> {
    let expanded = message.streaming || message.thinking_expanded;
    let mut lines = Vec::new();
    let dur = duration_label(message).unwrap_or_else(|| "0ms".into());
    let glyph = if message.streaming {
        SPINNER[app.spinner_frame % SPINNER.len()].to_string()
    } else if expanded {
        "○".to_string()
    } else {
        "▸".to_string()
    };

    let left = vec![
        Span::raw("  "),
        Span::styled(format!("{glyph} "), Theme::thinking_chevron()),
        Span::styled("Thinking", Theme::thinking_badge()),
    ];
    let right = vec![Span::styled(dur, Theme::thinking_meta())];
    lines.push(split_row(left, right, row_width, Theme::bg()));

    if expanded {
        let budget = wrap_width.saturating_sub(4).max(8);
        let raw_content = if message.streaming {
            message.content.trim_start()
        } else {
            message.content.trim()
        };
        let mut body = wrap_thinking_body(raw_content, budget);
        if message.streaming && body.len() > THINKING_STREAM_TAIL_LINES {
            body = body[body.len() - THINKING_STREAM_TAIL_LINES..].to_vec();
        }
        for line in body {
            lines.push(Line::from(vec![
                Span::styled("  │  ", Theme::thinking_meta()),
                Span::styled(line, Theme::thinking_body()),
            ]));
        }
    } else {
        let preview_budget = wrap_width.saturating_sub(6).max(16);
        let preview = thinking_preview(&message.content, preview_budget);
        if !preview.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  │  ", Theme::thinking_meta()),
                Span::styled(preview, Theme::thinking_body()),
            ]));
        }
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
    if let Some(ms) = message.duration_ms {
        if ms == 0 {
            Some("<1ms".to_string())
        } else {
            format_ms(ms)
        }
    } else if let Some(t0) = message.started_at {
        let s = t0.elapsed().as_secs_f32();
        if s < 10.0 {
            Some(format!("{:.1}s", s))
        } else {
            Some(format!("{}s", s as u64))
        }
    } else {
        None
    }
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

fn is_system_notification_or_meta(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("[Background task completed]")
        || trimmed.starts_with("[job completed]")
        || trimmed.starts_with("[Monitor stopped]")
        || trimmed.starts_with("[System reminder]")
        || trimmed.starts_with("<env>")
        || trimmed.starts_with("<context>")
        || trimmed.starts_with("<memory-catalog>")
        || trimmed.starts_with("### Learned Tool Intent")
        || trimmed.starts_with("### Graph Intent Guidance")
}

/// Extract user visible text and `<system-reminder>...</system-reminder>` blocks.
pub(super) fn extract_user_and_reminders(text: &str) -> ExtractedUserContent {
    let trimmed = text.trim();
    if is_system_notification_or_meta(trimmed) {
        return ExtractedUserContent {
            user_text: String::new(),
            reminders: vec![trimmed.to_string()],
        };
    }

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

    let mut real_user_parts = Vec::new();
    for part in user_parts {
        if is_system_notification_or_meta(part) {
            reminders.push(part.to_string());
        } else {
            real_user_parts.push(part);
        }
    }

    ExtractedUserContent {
        user_text: one_core::extract_user_query(&real_user_parts.join("\n\n")),
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

fn format_user_clock(ts: Option<std::time::SystemTime>) -> String {
    let Some(ts) = ts else {
        return String::new();
    };
    chrono::DateTime::<chrono::Local>::from(ts)
        .format("%H:%M")
        .to_string()
}

fn split_row(
    mut left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    row_width: usize,
    fill: ratatui::style::Style,
) -> Line<'static> {
    let left_w: usize = left.iter().map(|s| display_width(s.content.as_ref())).sum();
    let right_w: usize = right
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum();
    let pad = row_width.saturating_sub(left_w + right_w);
    if pad > 0 {
        left.push(Span::styled(" ".repeat(pad), fill));
    }
    left.extend(right);
    fill_spans_to(&mut left, row_width, fill);
    Line::from(left)
}

/// `▾ 19:14  title                    19 tools · 16.4s · ● running`
fn render_turn_header_line(
    chevron: Option<&str>,
    clock: &str,
    title: &str,
    meta: &str,
    running: bool,
    row_width: usize,
    highlight: bool,
    dim: bool,
) -> Line<'static> {
    let fill = if highlight {
        Theme::turn_active_bg()
    } else {
        Theme::bg()
    };
    let chevron_style = if highlight {
        Theme::sticky_query_accent()
    } else if dim {
        Theme::turn_row_dim()
    } else {
        Theme::meta()
    };
    let time_style = if highlight {
        Theme::turn_active_time()
    } else if dim {
        Theme::turn_row_dim()
    } else {
        Theme::turn_row_time()
    };
    let title_style = if highlight {
        Theme::turn_active_title()
    } else if dim {
        Theme::turn_row_dim()
    } else {
        Theme::user_body()
    };
    let meta_style = if highlight {
        Theme::turn_active_meta()
    } else {
        Theme::turn_row_meta()
    };

    let mut left = Vec::new();
    if let Some(ch) = chevron {
        left.push(Span::styled(format!("{ch} "), chevron_style));
    } else {
        left.push(Span::raw("  "));
    }
    if !clock.is_empty() {
        left.push(Span::styled(format!("{clock}  "), time_style));
    }

    let mut right: Vec<Span<'static>> = Vec::new();
    if !meta.is_empty() {
        let style = if running && highlight {
            Theme::tool_icon_running().bg(Theme::STICKY_BG)
        } else if running {
            Theme::tool_icon_running()
        } else {
            meta_style
        };
        right.push(Span::styled(meta.to_string(), style));
    }
    let used_left: usize = left.iter().map(|s| display_width(s.content.as_ref())).sum();
    let used_right: usize = right
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum();
    let mut title_budget = row_width.saturating_sub(used_left + used_right + 1).max(4);
    // Prompt text wins over status meta on a narrow row.
    if display_width(title) > title_budget && used_right > 0 {
        right.clear();
        title_budget = row_width.saturating_sub(used_left + 1).max(4);
    }
    let preview = truncate_display(title, title_budget);
    left.push(Span::styled(preview, title_style));
    split_row(left, right, row_width, fill)
}

struct TurnChrome {
    folded: bool,
    has_followup: bool,
    running: bool,
    meta: String,
}

struct UserPaint {
    lines: Vec<Line<'static>>,
    content_targets: Vec<usize>,
}

fn turn_chrome(
    messages: &[Message],
    user_idx: usize,
    is_current: bool,
    busy: bool,
) -> Option<TurnChrome> {
    let msg = messages.get(user_idx)?;
    if !crate::user_fold::is_real_user(msg) {
        return None;
    }
    let has_followup = crate::user_fold::turn_has_followup(messages, user_idx);
    let folded = crate::user_fold::is_turn_folded(msg.turn_expanded, is_current, has_followup);
    let stats = crate::user_fold::turn_stats(messages, user_idx);
    let end = crate::user_fold::turn_end(messages, user_idx);
    let slice_running = messages
        .get(user_idx..end)
        .into_iter()
        .flatten()
        .any(|m| m.streaming || m.tool_status == Some(ToolStatus::Running));
    let running = is_current && (busy || slice_running);
    let duration_ms = crate::user_fold::turn_duration_ms(messages, user_idx);
    let meta = crate::user_fold::format_turn_meta(stats, duration_ms, running);
    Some(TurnChrome {
        folded,
        has_followup,
        running,
        meta,
    })
}

fn render_user(
    msg: &Message,
    content: &str,
    wrap_width: usize,
    row_width: usize,
    is_current: bool,
    turn: Option<&TurnChrome>,
    _focused: bool,
    spinner_frame: usize,
) -> UserPaint {
    let content_folded = crate::user_fold::is_folded(msg.user_expanded, is_current, content);
    let turn_folded = turn.is_some_and(|t| t.folded);
    let has_followup = turn.is_some_and(|t| t.has_followup);
    let dim = turn_folded && !is_current;
    let highlight = is_current && has_followup && !turn_folded;
    let clock = format_user_clock(msg.created_at);
    let inner_w = wrap_width.saturating_sub(6).max(8);
    let meta = turn.map(|t| t.meta.as_str()).unwrap_or("");
    let running = turn.is_some_and(|t| t.running);
    let chevron = if running && !turn_folded {
        let spin = TURN_PULSE_SPINNER[spinner_frame % TURN_PULSE_SPINNER.len()];
        Some(spin)
    } else if has_followup {
        Some(if turn_folded { "▸" } else { "▾" })
    } else {
        None
    };

    let header_title = if turn_folded {
        crate::user_fold::turn_preview(content).to_string()
    } else {
        crate::user_fold::turn_preview(content).to_string()
    };
    let mut out = vec![render_turn_header_line(
        chevron,
        &clock,
        &header_title,
        meta,
        running,
        row_width,
        highlight,
        dim,
    )];
    let mut content_targets = Vec::new();

    if turn_folded {
        return UserPaint {
            lines: out,
            content_targets,
        };
    }

    let mut visual: Vec<String> = Vec::new();
    if content_folded {
        match crate::user_fold::collapse_plan(content) {
            Some(crate::user_fold::Collapse::Code {
                keep_before,
                body_lines,
                ..
            }) => {
                let kept: Vec<&str> = content.lines().take(keep_before).collect();
                for line in kept {
                    visual.extend(wrap_paragraphs(line, inner_w));
                }
                visual.push(format!("代码块 ({body_lines} 行)"));
                content_targets.push(out.len() + visual.len() - 1);
            }
            Some(crate::user_fold::Collapse::Text {
                keep,
                hidden,
                chars,
                ..
            }) => {
                let kept: Vec<&str> = content.lines().take(keep).collect();
                for line in kept {
                    visual.extend(wrap_paragraphs(line, inner_w));
                }
                let mut remain = hidden;
                if chars > crate::user_fold::FOLD_CHAR_THRESHOLD
                    && visual.len() > crate::user_fold::PREVIEW_LINES
                {
                    remain = remain.saturating_add(visual.len() - crate::user_fold::PREVIEW_LINES);
                    visual.truncate(crate::user_fold::PREVIEW_LINES);
                }
                if remain > 0 {
                    visual.push(format!("还有 {remain} 行"));
                    content_targets.push(out.len() + visual.len() - 1);
                }
            }
            None => visual.extend(wrap_paragraphs(content, inner_w)),
        }
    } else {
        let lines: Vec<&str> = content.lines().collect();
        // Header already shows the first source line; extra lines hang under it.
        let rest = if lines.len() > 1 {
            lines[1..].join("\n")
        } else {
            String::new()
        };
        if !rest.trim().is_empty() {
            visual.extend(wrap_paragraphs(&rest, inner_w));
        }
    }

    for line in visual {
        out.push(Line::from(vec![
            Span::styled("  │  ", Theme::meta()),
            Span::styled(
                line,
                if dim {
                    Theme::turn_row_dim()
                } else {
                    Theme::user_body()
                },
            ),
        ]));
    }
    UserPaint {
        lines: out,
        content_targets,
    }
}

/// Assistant: `Answer` title + full markdown body. Never folded; no tree spine.
fn render_assistant(
    message: &Message,
    app: &App,
    wrap_width: usize,
    row_width: usize,
) -> Vec<Line<'static>> {
    let budget = wrap_width.saturating_sub(2).max(8);
    let mut out = Vec::new();
    let dur = if message.streaming {
        let spin = SPINNER[app.spinner_frame % SPINNER.len()];
        format!("{spin} streaming")
    } else {
        duration_label(message).unwrap_or_default()
    };

    let left = vec![
        Span::raw("  "),
        Span::styled("Answer", Theme::heading_sub()),
    ];
    let right = if dur.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(dur, Theme::meta())]
    };
    out.push(split_row(left, right, row_width, Theme::bg()));

    if message.content.trim().is_empty() {
        return out;
    }

    let mut md_lines = markdown::render(&message.content, budget);
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
    let status = message.tool_status.unwrap_or(ToolStatus::Done);
    let cwd = app.history_cwd.as_deref();
    let is_error = status == ToolStatus::Error;
    let dur = duration_label(message);

    let name = if raw_name == "task" {
        match status {
            ToolStatus::Running => "Task started:".to_string(),
            ToolStatus::Done => {
                if let Some(d) = &dur {
                    format!("Task completed in {d}:")
                } else {
                    "Task completed:".to_string()
                }
            }
            ToolStatus::Error => "Task failed:".to_string(),
        }
    } else {
        tool_view::tool_display_name(&raw_name, detail)
    };

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

    let lead_w = if group_child.is_some() { 6 } else { 4 };
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
        let output_has_cmd = message.tool_output.as_deref().is_some_and(|s| {
            s.lines()
                .any(|l| l.starts_with("$ ") || l.starts_with("command: "))
        });
        let show_full_args = !full_args.is_empty()
            && ((raw_name == "use_tool" && full_args != detail)
                || (matches!(raw_name.as_str(), "bash" | "sh" | "exec")
                    && !output_has_cmd
                    && (pretty != full_args
                        || pretty.contains('…')
                        || full_args.lines().count() > 1
                        || display_width(&full_args) > budget)));

        let mut visual: Vec<(String, Style)> = Vec::new();
        if show_full_args {
            let is_shell = matches!(raw_name.as_str(), "bash" | "sh" | "exec");
            for (idx, line) in full_args.lines().enumerate() {
                let formatted = if is_shell && idx == 0 && !line.starts_with("$ ") {
                    format!("$ {line}")
                } else {
                    line.to_string()
                };
                for wrapped in wrap_str(&formatted, body_budget) {
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
            // Redundant default sandbox banner is omitted to keep output clean.
            let raw_lines: Vec<&str> = output
                .lines()
                .filter(|l| !l.starts_with("sandbox: bwrap"))
                .collect();
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

/// Mid-run steer row — lightweight timeline injection, never a user turn.
///
/// ```text
///   ↳ 你补充：不要重构 shared package · 已应用
/// ```
///
/// `applied` is only true when the agent actually drained the steer into the
/// model context (live `SteerApplied` event / session reload); pending rows
/// show no state chip rather than a fabricated one.
fn render_steer(message: &Message, wrap_width: usize, row_width: usize) -> Vec<Line<'static>> {
    let state = if message.steer_applied {
        " · 已应用"
    } else {
        ""
    };
    let prefix_w = display_width("↳ 你补充：") + display_width(state);
    let budget = wrap_width.saturating_sub(2 + prefix_w).max(8);
    let mut out = Vec::new();
    let mut first = true;
    for line in wrap_paragraphs(message.content.trim(), budget) {
        let spans = if first {
            first = false;
            vec![
                Span::styled("  ", Theme::bg()),
                Span::styled("↳ ", Theme::meta()),
                Span::styled("你补充：", Theme::meta()),
                Span::styled(line, Theme::user_steer()),
                Span::styled(state, Theme::meta()),
            ]
        } else {
            vec![
                Span::styled("  ", Theme::bg()),
                Span::styled(line, Theme::user_steer()),
            ]
        };
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        out.push(Line::from(vec![
            Span::styled("  ", Theme::bg()),
            Span::styled("↳ ", Theme::meta()),
            Span::styled("你补充：", Theme::meta()),
            Span::styled(state, Theme::meta()),
        ]));
    }
    fill_spans_to(&mut out[0].spans, row_width, Theme::bg());
    out
}

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
            chevron: Some("▾".into()),
            meta: String::new(),
            running: false,
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
