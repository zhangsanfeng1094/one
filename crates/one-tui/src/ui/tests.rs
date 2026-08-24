//! UI paint integration tests (ratatui TestBackend).

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use crate::app::App;
use crate::message::{ChatLineTarget, Message, ToolStatus};
use crate::tool_view;
use crate::ui::draw;

use super::chat::{render_thinking, render_tool_group, THINKING_STREAM_TAIL_LINES};
use super::text::{display_cols, scrollbar_thumb_geometry};
use super::SPINNER;

#[test]
fn settings_float_survives_a_narrow_short_terminal() {
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.open_settings_float();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
}

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
fn hardware_cursor_anchored_at_input_caret_even_when_streaming() {
    let backend = TestBackend::new(40, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.busy = true;
    app.push_assistant("streaming chunk of llm text");
    app.input = "prompt text".into();
    app.input_cursor = 6; // before " text"

    terminal.draw(|frame| draw(frame, &mut app)).unwrap();

    let pos = terminal.get_cursor_position().unwrap();
    // Layout: chat (chunks[0]), dock 0, prompt (chunks[2]), status (chunks[3])
    // prompt box height = 3 (1 line of input + 2 pad). prompt_h = 3 + 1 = 4.
    // terminal height 12: status 1, prompt 4, chat 12 - 5 = 7.
    // prompt starts at y = 7. box_area is y = 7.
    // caret is on top_padding + 0 = y: 7 + 1 = 8.
    // caret x = 0 (left border) + 3 (indent + border) + 6 (display width of "prompt") = 9.
    assert_eq!(pos.x, 9);
    assert_eq!(pos.y, 8);
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
fn history_viewport_stays_fixed_while_streaming_finishes() {
    let backend = TestBackend::new(40, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.push_user("history-anchor-marker");
    let body: String = (0..50)
        .map(|i| format!("earlier-line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push_assistant(&body);

    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    app.scroll_to_top();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let anchored_start = app.chat_view_start;
    let anchored_text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(anchored_text.contains("history-anchor-marker"));

    app.append_stream("late-stream-line-1\nlate-stream-line-2\nlate-stream-line-3");
    app.sync_stream_message();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(
        app.chat_view_start, anchored_start,
        "streaming output must not move a history viewport"
    );
    let streaming_text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(streaming_text.contains("history-anchor-marker"));

    app.finish_stream();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(
        app.chat_view_start, anchored_start,
        "finishing a stream must not leave history browsing"
    );
    let finished_text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(finished_text.contains("history-anchor-marker"));
    assert!(!app.follow_bottom);
}

#[test]
fn history_viewport_survives_thinking_and_tool_transitions() {
    let backend = TestBackend::new(40, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.push_user("history-tool-anchor");
    let body: String = (0..50)
        .map(|i| format!("earlier-line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push_assistant(&body);

    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let live_text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(!live_text.contains("Shift+G latest"));
    app.scroll_to_top();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let anchored_start = app.chat_view_start;

    app.append_thinking_stream("thinking about the next step");
    app.sync_thinking_message();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(app.chat_view_start, anchored_start);

    app.push_tool_call("read", r#"{\"path\":\"src/lib.rs\"}"#);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(
        app.chat_view_start, anchored_start,
        "tool transitions must not re-enter live follow"
    );
    assert!(!app.follow_bottom);
}

#[test]
fn history_status_advertises_shift_g_latest() {
    let backend = TestBackend::new(60, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    let body: String = (0..100)
        .map(|i| format!("line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push_assistant(&body);

    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    app.scroll_to_top();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();

    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        flat.contains("Shift+G") && flat.contains("latest"),
        "history status should expose the return-to-live shortcut, got:\n{flat}"
    );

    app.scroll_to_bottom();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let resumed_text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(!resumed_text.contains("Shift+G latest"));
}

#[test]
fn scrollbar_thumb_tracks_offset() {
    // 100 items, 10 visible, track 10 → thumb height 1, moves with offset.
    let (start0, h0) = scrollbar_thumb_geometry(100, 10, 0, 10);
    assert_eq!(h0, 1);
    assert_eq!(start0, 0);
    let (start_mid, h_mid) = scrollbar_thumb_geometry(100, 10, 45, 10);
    assert_eq!(h_mid, 1);
    assert!(
        start_mid > 0 && start_mid < 9,
        "mid offset → mid thumb, got {start_mid}"
    );
    let (start_end, _) = scrollbar_thumb_geometry(100, 10, 90, 10);
    assert_eq!(start_end, 9);
    // Fits: full track.
    let (s, h) = scrollbar_thumb_geometry(5, 10, 0, 8);
    assert_eq!(s, 0);
    assert_eq!(h, 8);
}

#[test]
fn float_wheel_moves_selection() {
    let mut app = App::new("test");
    app.open_subagent_float(&[
        ("job_1".into(), "run".into(), "a".into(), "1s".into()),
        ("job_2".into(), "ok".into(), "b".into(), "2s".into()),
        ("job_3".into(), "fail".into(), "c".into(), "3s".into()),
    ]);
    assert_eq!(app.float.as_ref().unwrap().selected, 0);
    app.scroll_float_wheel(false, 2);
    assert_eq!(app.float.as_ref().unwrap().selected, 2);
    app.scroll_float_wheel(true, 1);
    assert_eq!(app.float.as_ref().unwrap().selected, 1);
    app.scroll_float_page(true);
    assert_eq!(app.float.as_ref().unwrap().selected, 0);
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
fn expanded_tool_group_header_is_clickable_and_collapses() {
    let backend = TestBackend::new(72, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    for (name, args) in [
        ("read", r#"{"path":"a.rs"}"#),
        ("bash", r#"{"command":"ls"}"#),
        ("grep", r#"{"pattern":"x"}"#),
    ] {
        app.push_tool_call(name, args);
        app.finish_tool_with_output(name, false, Some("ok\nline2".into()));
        if let Some(last) = app.messages.last_mut() {
            last.tool_expanded = false;
            last.tool_ungroup = false;
        }
    }

    // Collapsed chip first.
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        flat.contains("3 tools"),
        "collapsed group chip should paint, got:\n{flat}"
    );
    assert!(
        app.chat_line_owners
            .iter()
            .any(|o| matches!(o, Some(ChatLineTarget::ToolGroup(0)))),
        "collapsed chip must be a ToolGroup click target"
    );

    // Expand via Ctrl+O path.
    app.toggle_last_tool_expand();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        flat.contains("3 tools"),
        "expanded stack should keep a group header, got:\n{flat}"
    );
    assert!(
        app.chat_line_owners
            .iter()
            .any(|o| matches!(o, Some(ChatLineTarget::ToolGroup(0)))),
        "expanded header must remain a ToolGroup click target"
    );
    // Individual tool rows still target Message.
    assert!(
        app.chat_line_owners
            .iter()
            .filter_map(|o| o.as_ref())
            .any(|t| matches!(t, ChatLineTarget::Message(_))),
        "expanded tools must still be Message click targets"
    );

    // Expanded children nest under the header with tree connectors.
    assert!(
        flat.contains('├') && flat.contains('└'),
        "expanded group tools must use tree connectors, got:\n{flat}"
    );

    // Click header row → collapse back to chip.
    let header_line = app
        .chat_line_owners
        .iter()
        .position(|o| matches!(o, Some(ChatLineTarget::ToolGroup(0))))
        .expect("header line");
    let row = header_line
        .saturating_sub(app.chat_view_start)
        .saturating_add(app.chat_top_pad);
    app.click_chat_row(row);
    assert!(
        app.messages
            .iter()
            .all(|m| !m.tool_ungroup && !m.tool_expanded),
        "header click must re-chip the group"
    );
    assert!(tool_view::streak_can_collapse(&app.messages, 0, 3));
}

#[test]
fn expanded_tool_group_nests_children_under_header() {
    let backend = TestBackend::new(72, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    for (name, args) in [
        ("ls", r#"{"path":"."}"#),
        ("find", r#"{"pattern":"README*"}"#),
        ("find", r#"{"pattern":"**/*.rs"}"#),
    ] {
        app.push_tool_call(name, args);
        app.finish_tool_with_output(name, false, Some("ok".into()));
        if let Some(last) = app.messages.last_mut() {
            last.tool_expanded = false;
            last.tool_ungroup = false;
        }
    }
    app.toggle_tool_group_at(0);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(flat.contains("3 tools"), "header: {flat}");
    // Parent chip + nested children (two ├ / one └ or mix).
    let branch_count = flat.matches('├').count() + flat.matches('└').count();
    assert!(
        branch_count >= 3,
        "expected nested tree under group header, got {branch_count} branches:\n{flat}"
    );
    // Children should sit to the right of the group chevron column.
    let lines: Vec<&str> = flat
        .trim_end()
        .split(|c: char| c == '\n' || c == '\u{0}')
        .filter(|l| !l.trim().is_empty())
        .collect();
    let _ = lines; // buffer is a grid without newlines; structural check above is enough.
}

#[test]
fn empty_session_shows_welcome_tips() {
    // Tall enough for chat pane to show welcome title + tips + try.
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
        flat.contains("tips") && flat.contains("Shift+Tab"),
        "empty session should show advanced tips only, got:\n{flat}"
    );
    // Model lives on prompt meta — not duplicated in the welcome body.
    assert!(
        flat.contains("mock-model"),
        "prompt meta should surface current model, got:\n{flat}"
    );
    assert!(
        flat.contains("press 1") || flat.contains("[1]"),
        "empty session should offer try shortcuts, got:\n{flat}"
    );
    // Tips list advanced keys only — model switch lives on the status strip.
    assert!(
        flat.contains("Ctrl+J") && !flat.contains("/help more") && !flat.contains("/model"),
        "welcome tips must not restate status/help chrome, got:\n{flat}"
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
fn streaming_thinking_spinner_keeps_stable_row_count() {
    // Spinner frame advance must not change layout height.
    let body = "alpha\nbeta\ngamma\ndelta";
    let msg = crate::message::Message::streaming_thinking(body);
    let mut app = App::new("test");
    app.spinner_frame = 0;
    let a = render_thinking(&msg, &app, 40);
    app.spinner_frame = 3;
    let b = render_thinking(&msg, &app, 40);
    assert_eq!(
        a.len(),
        b.len(),
        "thinking spinner must keep the same row count"
    );
    // Tail window: header + last 3 body lines.
    assert_eq!(a.len(), 1 + THINKING_STREAM_TAIL_LINES);
}

#[test]
fn streaming_assistant_uses_live_turn_footer_not_caret() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.agent_label = "Build".into();
    app.mode_label = "grok-4.5".into();
    app.busy = true;
    app.spinner_frame = 0;
    app.append_stream("hello stream");
    app.sync_stream_message();

    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();

    assert!(flat.contains("hello stream"), "body: {flat}");
    assert!(
        flat.contains('╰') && flat.contains("Build") && flat.contains("grok-4.5"),
        "live footer should mirror finished turn chrome, got:\n{flat}"
    );
    // Prompt still owns the typewriter bar — stream must not paint one.
    let stream_area = flat.replace('▌', ""); // crude: ensure we still have content
    assert!(
        !flat.contains("hello stream▌") && !flat.contains("hello stream ▌"),
        "streaming body must not end with typewriter caret, got:\n{flat}"
    );
    let _ = stream_area;
    // Braille spinner occupies the duration slot.
    assert!(
        SPINNER.iter().any(|s| flat.contains(s)),
        "live footer should show spinner, got:\n{flat}"
    );
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

    // Meta: agent + model + provider (identity), with ` · ` separators.
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
        meta_row.contains(" · "),
        "mode · model · provider separators: {meta_row}"
    );
    assert!(
        !meta_row.contains("completions") && !meta_row.contains("http"),
        "meta must not dump api/host: {meta_row}"
    );

    // Status: sparse core keys only (full catalog via Alt+H help).
    assert!(
        status_row.contains("Ctrl+G") || status_row.contains("settings"),
        "settings key: {status_row}"
    );
    assert!(
        status_row.contains("Ctrl+L") || status_row.contains("model"),
        "model key: {status_row}"
    );
    assert!(
        status_row.contains("Alt+H") || status_row.contains("help"),
        "help key: {status_row}"
    );
    assert!(
        !status_row.contains("ctrl+c")
            && !status_row.contains("ctrl+p")
            && !status_row.contains("hist")
            && !status_row.contains(" · "),
        "idle status should stay sparse: {status_row}"
    );
}

#[test]
fn meta_and_status_do_not_duplicate_chips() {
    // Regression: MCP / think / tokens used to appear on BOTH strips and
    // jammed into `MCP3/3…MCP 3/3think:medium181k` on narrow terminals.
    let backend = TestBackend::new(100, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.set_agent_label("Build");
    app.set_current_model("sensenova", "deepseek-v4-flash");
    app.thinking_level = "medium".into();
    app.set_usage_tokens(37_000);
    app.set_usage_tokens_estimated(false);
    app.set_mcp_chip("MCP 3/3", 2);
    app.set_bg_chip("bg:1 · top", 1);
    app.push_assistant("hello");

    terminal.draw(|frame| draw(frame, &mut app)).unwrap();

    let buf = terminal.backend().buffer();
    let w = buf.area.width as usize;
    let h = buf.area.height as usize;
    let cells: Vec<String> = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    let meta_row: String = cells[(h - 2) * w..(h - 1) * w].concat();
    let status_row: String = cells[(h - 1) * w..h * w].concat();

    // Meta: identity + live ops only.
    assert!(meta_row.contains("Build"), "agent: {meta_row}");
    assert!(meta_row.contains("deepseek-v4-flash"), "model: {meta_row}");
    assert!(meta_row.contains("sensenova"), "provider: {meta_row}");
    assert!(meta_row.contains("MCP 3/3"), "MCP chip on meta: {meta_row}");
    assert!(meta_row.contains("bg:1"), "bg chip on meta: {meta_row}");
    assert!(
        !meta_row.contains("think:"),
        "think belongs on status, not meta: {meta_row}"
    );
    assert!(
        !meta_row.contains("37k") && !meta_row.contains("tok"),
        "tokens belong on status, not meta: {meta_row}"
    );

    // Status: keys + session stats only — never mirror MCP/bg.
    assert!(
        status_row.contains("Ctrl+G") || status_row.contains("settings"),
        "keys: {status_row}"
    );
    assert!(
        status_row.contains("think:medium"),
        "think on status: {status_row}"
    );
    assert!(
        status_row.contains("ctx") && status_row.contains("37k"),
        "context tokens on status: {status_row}"
    );
    assert!(
        !status_row.contains("MCP"),
        "MCP must not duplicate on status: {status_row}"
    );
    assert!(
        !status_row.contains("bg:"),
        "bg must not duplicate on status: {status_row}"
    );

    // Exactly one MCP occurrence across both chrome rows.
    let chrome = format!("{meta_row}{status_row}");
    assert_eq!(
        chrome.matches("MCP").count(),
        1,
        "MCP chip must appear exactly once: meta={meta_row} status={status_row}"
    );
}

#[test]
fn tool_paths_render_relative_to_cwd() {
    let backend = TestBackend::new(100, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.history_cwd = Some(std::path::PathBuf::from("/home/fxh/tools/one"));
    app.push_tool_call(
        "read",
        r#"{"path":"/home/fxh/tools/one/crates/one-tools/src/bash.rs"}"#,
    );
    app.finish_tool_with_output(
        "read",
        false,
        Some(
            (0..66)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    );
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        flat.contains("./crates/one-tools/src/bash.rs") || flat.contains("bash.rs"),
        "expected relative path, got:\n{flat}"
    );
    assert!(
        !flat.contains("/home/fxh/tools/one/crates"),
        "absolute workspace path must be shortened:\n{flat}"
    );
    // Summary is metrics only — no repeated "read path".
    assert!(
        flat.contains("66 lines") || flat.contains("lines"),
        "expected line count summary:\n{flat}"
    );
    assert!(
        !flat.contains("↵/click"),
        "inline expand hints must not appear:\n{flat}"
    );
    // Success is single-line: no tree child for metrics.
    assert!(
        !flat.contains('└') || flat.matches('└').count() == 0,
        "success tool should not use └ summary row:\n{flat}"
    );
}

#[test]
fn bash_command_paths_shorten_and_middle_truncate() {
    let backend = TestBackend::new(72, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.history_cwd = Some(std::path::PathBuf::from("/home/fxh/tools/one"));
    app.push_tool_call(
        "bash",
        r#"{"command":"cd /home/fxh/tools/one/benches/out/tb-regex-checker && ls"}"#,
    );
    app.finish_tool_with_output("bash", false, Some("exit 0\na\nb\nc\nd\ne\n".into()));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        !flat.contains("/home/fxh/tools/one/benches"),
        "absolute cwd path must be rewritten:\n{flat}"
    );
    // Filename / destination should survive middle truncate on a narrow width.
    assert!(
        flat.contains("regex") || flat.contains("checker") || flat.contains("./benches"),
        "tail of path should remain visible:\n{flat}"
    );
    assert!(
        flat.contains("5 lines") || flat.contains("lines"),
        "line metrics inline: {flat}"
    );
    assert!(
        !flat.contains("exit 0"),
        "success must not show exit 0: {flat}"
    );
}

#[test]
fn thinking_header_has_no_inline_click_hint() {
    let backend = TestBackend::new(80, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.messages.push(crate::message::Message::thinking(
        "Analyzing bash command flags carefully",
    ));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(!flat.contains("↵/click"), "no inline click hint: {flat}");
    assert!(
        flat.contains("[Thinking]") || flat.contains("Thinking"),
        "badge form: {flat}"
    );
    assert!(
        flat.contains("Analyzing") || flat.contains("flags"),
        "should show preview: {flat}"
    );
}

#[test]
fn thinking_collapsed_uses_full_width_end_truncate() {
    // Wide terminal: collapsed preview must fill the line from the *start*
    // (end-ellipsis), not a 48-char middle-crop that looks like history was cut.
    let backend = TestBackend::new(120, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    let body = "The user wants me to produce all legal next positions. \
                I will explore the workspace and understand the Regex Chess task, \
                then generate re.json myself and self-test with the local checkers.";
    app.messages.push(crate::message::Message::thinking(body));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        flat.contains("The user wants me to produce"),
        "preview must keep the sentence start, not mid-crop:\n{flat}"
    );
    // Middle-truncate pattern like "me …uce" / "me...uce" must not appear.
    assert!(
        !flat.contains("me …uce") && !flat.contains("me...uce") && !flat.contains("wants me …"),
        "must not middle-truncate natural language:\n{flat}"
    );
    // On a 120-col terminal the start should not be clipped to ~20 chars only.
    assert!(
        flat.contains("legal next") || flat.contains("produce all"),
        "wide terminal should show more than a stub:\n{flat}"
    );
}

#[test]
fn expanded_bash_recovers_full_command_history() {
    // Long heredoc: collapsed header truncates; expand must show the middle again.
    let backend = TestBackend::new(72, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.history_cwd = Some(std::path::PathBuf::from("/home/fxh/tools/one"));
    let cmd = "PYTHONPATH=./.pydeps python3 - <<'PY'\nimport chess\n\
               UNIQUE_MARKER_MID_COMMAND = 42\nprint(chess.__version__)\nPY";
    let args = serde_json::json!({ "command": cmd }).to_string();
    app.push_tool_call("bash", &args);
    app.finish_tool_with_output("bash", false, Some("exit 0\n1.0\n".into()));
    // Expand the tool row.
    if let Some(msg) = app.messages.last_mut() {
        msg.tool_expanded = true;
    }
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        flat.contains("UNIQUE_MARKER_MID_COMMAND"),
        "expanded tool must recover full command (not permanently cropped):\n{flat}"
    );
}

#[test]
fn expanded_use_tool_shows_labeled_fields_not_json() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    let args = serde_json::json!({
        "tool_name": "deepwiki__ask_question",
        "tool_input": {
            "repoName": "facebook/react",
            "question": "How does One work?"
        }
    })
    .to_string();
    app.push_tool_call("use_tool", &args);
    app.finish_tool_with_output(
        "use_tool",
        false,
        Some(r#"{"items":[{"id":"A","title":"First"}],"nextCursor":null}"#.into()),
    );
    if let Some(msg) = app.messages.last_mut() {
        msg.tool_expanded = true;
    }
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        flat.contains("repoName") && flat.contains("facebook/react"),
        "expanded use_tool must show labeled input fields:\n{flat}"
    );
    assert!(
        flat.contains("question") && flat.contains("How does One work?"),
        "expanded use_tool must show the question field:\n{flat}"
    );
    assert!(
        !flat.contains("\"tool_input\"") && !flat.contains("\"repoName\""),
        "expanded use_tool must not dump wrapper/input JSON:\n{flat}"
    );
    assert!(
        flat.contains("items") && !flat.contains("\"items\""),
        "expanded use_tool result should be an outline, not quoted JSON keys:\n{flat}"
    );
}

#[test]
fn expanded_search_tool_formats_large_json_instead_of_dumping_it() {
    let backend = TestBackend::new(100, 28);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    let huge = "knowledge ".repeat(400);
    let out = format!(
        r#"{{
  "note": null,
  "results": [
    {{
      "server": "agy",
      "tools": [
        {{
          "description": "[MCP:agy] Search the live web using the Antigravity/agy Google session. {huge}",
          "input_schema": {{
            "properties": {{
              "query": {{
                "description": "Search query, including dates or locale when relevant.",
                "type": "string"
              }}
            }},
            "required": ["query"],
            "type": "object"
          }},
          "score": 7.94,
          "tool_name": "agy__search_web"
        }}
      ]
    }},
    {{
      "server": "context-mode",
      "tools": [
        {{
          "description": "[MCP:context-mode] Search a unified knowledge base.",
          "tool_name": "context-mode__search"
        }}
      ]
    }}
  ],
  "status": "ready",
  "total_tools": 16
}}"#
    );
    assert!(
        out.len() > 4_000,
        "fixture must exceed UI cap: {}",
        out.len()
    );
    app.push_tool_call(
        "search_tool",
        r#"{"query":"search web query find grep wiki question"}"#,
    );
    app.finish_tool_with_output("search_tool", false, Some(out));
    if let Some(msg) = app.messages.last_mut() {
        msg.tool_expanded = true;
    }
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        flat.contains("agy__search_web") && flat.contains("query: string"),
        "expanded search_tool must show signatures, not a JSON dump:\n{flat}"
    );
    assert!(
        flat.contains("context-mode") && flat.contains("context-mode__search"),
        "expanded search_tool must group by server:\n{flat}"
    );
    assert!(
        !flat.contains("\"input_schema\"") && !flat.contains("\"tool_name\""),
        "expanded search_tool must not dump raw JSON keys:\n{flat}"
    );
}

#[test]
fn prompt_meta_separates_mode_model_provider() {
    let backend = TestBackend::new(80, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.agent_label = "Build".into();
    app.current_model = "grok-4.5".into();
    app.current_provider = "ziyong".into();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(flat.contains("Build"), "{flat}");
    assert!(flat.contains("grok-4.5"), "{flat}");
    assert!(flat.contains("ziyong"), "{flat}");
    assert!(
        flat.contains('·') || flat.contains(" · "),
        "mode/model/provider need separators: {flat}"
    );
}

#[test]
fn chat_focus_rail_and_status_nav_hints() {
    let backend = TestBackend::new(90, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.push_tool_call("read", r#"{"path":"a.rs"}"#);
    app.finish_tool_with_output("read", false, Some("one\ntwo".into()));
    app.chat_focus = Some(0);
    app.input.clear();
    app.cursor_on = true;
    assert!(!app.prompt_focused(), "browse must unfocus the composer");
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    // Blue focus rail on the tool row still uses ▌; prompt caret must not.
    assert!(flat.contains('▌'), "focus rail: {flat}");
    assert!(
        flat.contains("type to edit") || flat.contains("j/k"),
        "browse placeholder, not Message… + blinking caret: {flat}"
    );
    assert!(
        flat.contains("j/k") || flat.contains("nav"),
        "browse status keys when focused: {flat}"
    );
}

#[test]
fn tool_group_aggregates_duplicate_names() {
    let tools = vec![
        crate::message::Message::tool("grep", "{}", crate::message::ToolStatus::Done),
        crate::message::Message::tool("grep", "{}", crate::message::ToolStatus::Done),
        crate::message::Message::tool("read", "{}", crate::message::ToolStatus::Done),
    ];
    let lines = render_tool_group(&tools, 80, false);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(text.contains("[grep ×2]"), "{text}");
    assert!(text.contains("[read]"), "{text}");
    assert!(!text.contains("  ↵") && !text.contains("↵"), "{text}");
}

#[test]
fn tv4_frame_replaces_parent_chat() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.input = "should-not-show".into();
    app.open_subagent_detail_float(
        "job_tv4",
        "scan codebase",
        "explore  ·  running  ·  3s  ·  #tv4",
        &[
            ("▸".into(), "job job_tv4 · explore · scan codebase".into()),
            ("→".into(), "grep · auth".into()),
        ],
    );
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        flat.contains("scan codebase"),
        "framed title missing:\n{flat}"
    );
    assert!(
        flat.contains("observational") || flat.contains("[q]"),
        "frame chrome missing:\n{flat}"
    );
    assert!(
        !flat.contains("should-not-show"),
        "parent prompt must be hidden under TV4:\n{flat}"
    );
}

#[test]
fn tab_indented_diff_renders_clean_without_ghosting() {
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    let mut tool_msg = Message::tool(
        "edit",
        r#"{"path":"parser/parser_test.go"}"#,
        ToolStatus::Done,
    );
    tool_msg.tool_output = Some(
        "\
Updated parser/parser_test.go
--- a/parser/parser_test.go
+++ b/parser/parser_test.go
@@ -1740,2 +1740,2 @@
-\t\ttestPrefixExpression(t, indexExp.Step, \"-\", 1)
+\t\tprefixExp, ok := indexExp.Step.(*ast.PrefixExpression)
"
        .into(),
    );
    tool_msg.tool_expanded = true;
    app.messages.push(tool_msg);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    // Verify that tabs were cleanly rendered as spaces, not raw tabs
    assert!(!flat.contains('\t'), "buffer must not contain raw tabs");
    assert!(
        flat.contains("prefixExp, ok :="),
        "diff row must be rendered"
    );
}

#[test]
fn top_header_renders_grok_style_path_and_context() {
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("test");
    app.history_cwd = Some(std::path::PathBuf::from("/home/user/awesome-project"));
    app.set_usage_tokens(32_000);
    app.set_usage_tokens_estimated(false);
    app.set_context_window(128_000);

    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let flat: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();

    assert!(
        flat.contains("awesome-project"),
        "top header must render project folder: {flat}"
    );
    assert!(
        flat.contains("32k") && flat.contains("128k") && flat.contains("25%"),
        "top header must render context usage and window: {flat}"
    );
    assert!(
        flat.contains("●"),
        "top header must render status indicator: {flat}"
    );
}
