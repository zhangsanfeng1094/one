//! Unit tests for [`App`] behavior (input, keys, streaming, settings hooks).

use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::helpers::path_token_at_end;
use super::*;
use crate::float::{FloatKind, FloatMenu};
use crate::message::{AlertLevel, Message, MessageRole, ToolStatus};
use crate::slash::PopupRow;
use crate::state::{
    display_col_to_caret, ConfigOp, RunOutcome, SelectKind, SelectPos, WELCOME_TRY_PROMPTS,
};
use crate::tool_view;

/// Serialize media-dir override so parallel tests do not clobber each other.
static MEDIA_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with image media store under a unique `/tmp` dir (bwrap-writable).
fn with_temp_media<T>(f: impl FnOnce(&Path) -> T) -> T {
    let _guard = MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "one-tui-media-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&dir);
    let prev = one_core::image::set_media_dir_override(Some(dir.clone()));
    let out = f(&dir);
    one_core::image::set_media_dir_override(prev);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: mods,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::empty(),
    }
}

#[test]
fn enter_submits_prompt() {
    let mut app = App::new("test");
    app.input = "hello".into();
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
        RunOutcome::Prompt(t) => assert_eq!(t, "hello"),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(app.messages.last().unwrap().content, "hello");
    assert_eq!(app.prompt_history, vec!["hello".to_string()]);
}

#[test]
fn settings_tool_output_panel_opens() {
    let mut app = App::new("test");
    app.set_tool_output_limits(100, 4096);
    app.open_settings_tool_output();
    let f = app.float.as_ref().unwrap();
    assert_eq!(f.kind, FloatKind::SettingsToolOutput);
    assert!(f
        .sections
        .iter()
        .flat_map(|s| s.items.iter())
        .any(|i| i.id == "max_lines"));
}

#[test]
fn settings_compaction_panel_opens() {
    let mut app = App::new("test");
    app.set_compaction_settings(true, 0.8, None, 10, true, 20_000, 1000);
    app.open_settings_compaction();
    let f = app.float.as_ref().unwrap();
    assert_eq!(f.kind, FloatKind::SettingsCompaction);
    let ids: Vec<_> = f
        .sections
        .iter()
        .flat_map(|s| s.items.iter().map(|i| i.id.as_str()))
        .collect();
    assert!(ids.contains(&"auto"));
    assert!(ids.contains(&"prune"));
    assert!(ids.contains(&"ratio"));
    // Esc returns to settings root.
    assert!(app.settings_go_back());
    assert_eq!(app.float.as_ref().unwrap().kind, FloatKind::Settings);
    assert!(app
        .float
        .as_ref()
        .unwrap()
        .sections
        .iter()
        .flat_map(|s| s.items.iter())
        .any(|i| i.id == "compaction"));
}

#[test]
fn ctrl_g_opens_settings_float() {
    let mut app = App::new("test");
    let out = app.handle_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
    assert!(matches!(out, RunOutcome::Noop));
    let f = app.float.as_ref().expect("settings float open");
    assert_eq!(f.kind, FloatKind::Settings);
    assert!(!f.filtered_entries().is_empty());
    assert!(f
        .sections
        .iter()
        .flat_map(|s| s.items.iter())
        .any(|i| i.id == "compaction"));
}

#[test]
fn busy_ctrl_g_opens_settings_float() {
    let mut app = App::new("test");
    app.begin_busy();
    app.handle_busy_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
    let f = app.float.as_ref().expect("settings float open while busy");
    assert_eq!(f.kind, FloatKind::Settings);
    assert!(!f.filtered_entries().is_empty());
}

#[test]
fn settings_models_ctrl_f_fetches_remote_models() {
    let mut app = App::new("test");
    app.settings_provider_focus = "proxy".into();
    app.open_settings_models_for_provider("proxy");

    let out = app.handle_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL));

    assert!(matches!(
        out,
        RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id }) if id == "proxy"
    ));
}

#[test]
fn settings_provider_detail_ctrl_f_fetches_remote_models() {
    let mut app = App::new("test");
    app.open_settings_provider_detail("proxy", "1 model");

    let out = app.handle_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL));

    assert!(matches!(
        out,
        RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id }) if id == "proxy"
    ));
}

#[test]
fn settings_models_ctrl_shift_f_and_legacy_ack_fetch() {
    let mut app = App::new("test");
    app.open_settings_models_for_provider("proxy");

    // Uppercase F + CONTROL (some terminals / Caps Lock).
    let out = app.handle_key(key(KeyCode::Char('F'), KeyModifiers::CONTROL));
    assert!(matches!(
        out,
        RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id }) if id == "proxy"
    ));

    // Legacy Ctrl+F as ASCII ACK (0x06).
    let out = app.handle_key(key(KeyCode::Char('\u{06}'), KeyModifiers::NONE));
    assert!(matches!(
        out,
        RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id }) if id == "proxy"
    ));
}

#[test]
fn settings_models_enter_on_fetch_row() {
    let mut app = App::new("test");
    app.open_settings_models_for_provider("proxy");
    // First row is "Fetch remote models".
    let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        out,
        RunOutcome::ConfigOp(ConfigOp::ProviderFetchModels { id }) if id == "proxy"
    ));
}

#[test]
fn provider_detail_rows_show_configured_values() {
    let mut app = App::new("test");
    app.set_settings_catalog(
        vec![("proxy".into(), "1 model".into())],
        vec![],
        vec![
            ("proxy:provider_type".into(), "openai-compatible".into()),
            ("proxy:base_url".into(), "https://proxy.example/v1".into()),
            ("proxy:api".into(), "openai-completions".into()),
            ("proxy:api_key".into(), "$PROXY_KEY".into()),
            ("proxy:default_model".into(), "m1".into()),
        ],
    );

    app.open_settings_provider_detail("proxy", "1 model");
    let entries = app.float.as_ref().unwrap().filtered_entries();

    assert!(entries
        .iter()
        .any(|e| e.item.id == "set_provider_type" && e.item.detail == "openai-compatible"));
    assert!(entries
        .iter()
        .any(|e| e.item.id == "set_base_url" && e.item.detail == "https://proxy.example/v1"));
    // api is merged into the protocol select row (no separate set_api row).
    assert!(!entries.iter().any(|e| e.item.id == "set_api"));
}

#[test]
fn settings_remote_model_list_filters_and_adds_model() {
    let mut app = App::new("test");
    app.open_settings_provider_detail("proxy", "1 model");
    app.open_settings_remote_models(
        "proxy",
        vec![
            ("gpt-4.1".into(), "remote".into()),
            ("o3".into(), "remote".into()),
        ],
    );

    let f = app.float.as_mut().expect("remote models float");
    assert_eq!(f.kind, FloatKind::SettingsRemoteModels);
    f.search = "o3".into();
    let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(
        out,
        RunOutcome::ConfigOp(ConfigOp::ModelAdd {
            spec,
            name,
            context_window: None,
        }) if spec == "proxy:o3" && name.as_deref() == Some("o3")
    ));
}

#[test]
fn provider_api_uses_enum_picker() {
    let mut app = App::new("test");
    app.open_settings_provider_detail("proxy", "1 model");
    let f = app.float.as_mut().expect("provider detail");
    let api_index = f
        .filtered_entries()
        .iter()
        .position(|e| e.item.id == "set_provider_type")
        .unwrap();
    f.selected = api_index;

    let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(out, RunOutcome::Noop));
    let f = app.float.as_ref().expect("api picker");
    assert_eq!(f.kind, FloatKind::SettingsProviderApi);
    assert!(f
        .filtered_entries()
        .iter()
        .any(|e| e.item.id == "api:openai-responses"));
    assert!(f
        .filtered_entries()
        .iter()
        .any(|e| e.item.id == "api:openai-completions"));
    assert!(f
        .filtered_entries()
        .iter()
        .any(|e| e.item.id == "api:anthropic-messages"));
    assert!(f
        .filtered_entries()
        .iter()
        .any(|e| e.item.id == "api:gemini-generate-content"));
}

#[test]
fn provider_api_picker_saves_fixed_values_and_unset() {
    let mut app = App::new("test");
    app.open_settings_provider_detail("proxy", "1 model");
    app.open_settings_provider_api("proxy");
    let f = app.float.as_mut().expect("api picker");
    f.selected = f
        .filtered_entries()
        .iter()
        .position(|e| e.item.id == "api:openai-responses")
        .unwrap();

    let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        out,
        RunOutcome::ConfigOp(ConfigOp::ProviderSet { id, key, value })
            if id == "proxy" && key == "api" && value == "openai-responses"
    ));

    app.open_settings_provider_api("proxy");
    let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        out,
        RunOutcome::ConfigOp(ConfigOp::ProviderSet { id, key, value })
            if id == "proxy" && key == "api" && value.is_empty()
    ));
}

#[test]
fn slash_settings_enter_opens_settings_float() {
    let mut app = App::new("test");
    app.input = "/settings".into();
    app.clamp_slash_selection();
    // Highlight /settings if filtered list has it.
    let rows = app.popup_rows();
    if let Some(i) = rows
        .iter()
        .position(|r| matches!(r, PopupRow::Command(c) if c.name == "/settings"))
    {
        app.slash_selected = i;
    }
    let out = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(out, RunOutcome::Noop), "got {out:?}");
    let f = app.float.as_ref().expect("settings float after /settings");
    assert_eq!(f.kind, FloatKind::Settings);
}

#[test]
fn up_down_navigates_prompt_history() {
    let mut app = App::new("test");
    app.push_prompt_history("first");
    app.push_prompt_history("second");
    app.input = "draft".into();

    app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input, "second");
    app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input, "first");
    app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.input, "second");
    app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.input, "draft");

    // Ctrl+P matches Up; Down still moves forward through history.
    app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(app.input, "second");
    app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.input, "draft");
}

#[test]
fn ctrl_n_confirms_before_starting_new_conversation() {
    let mut app = App::new("test");
    app.input = "unsent draft".into();

    let outcome = app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert!(matches!(outcome, RunOutcome::Noop));
    let float = app.float.as_ref().expect("new-conversation confirmation");
    assert_eq!(float.kind, FloatKind::NewSessionConfirm);
    assert_eq!(float.selected, 0, "cancel must be the safe default");
    assert_eq!(app.input, "unsent draft");

    // Enter on the safe default cancels without changing the draft.
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
        RunOutcome::Noop
    ));
    assert!(app.float.is_none());
    assert_eq!(app.input, "unsent draft");

    let _ = app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    let _ = app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
        RunOutcome::Prompt(command) => assert_eq!(command, "/new"),
        other => panic!("expected /new after confirmation, got {other:?}"),
    }
    assert!(app.float.is_none());
    assert!(app.input.is_empty());
}

#[test]
fn ui_slash_commands_skip_prompt_history_and_chat() {
    let mut app = App::new("test");
    app.push_prompt_history("real prompt");

    // Use args so the `/` completion popup is closed (space ⇒ no menu),
    // and Enter hits `submit_prompt` → `is_ui_slash` path.
    for cmd in [
        "/session detail",
        "/new",
        "/name my-session",
        "/model gpt-4",
        "/login",
        "/login openai-codex",
        "/logout",
        "/ps",
        "/agents",
        "/plan",
        "/compact",
    ] {
        app.input = cmd.into();
        // Bare `/new` / `/plan` still open the slash menu; complete+submit
        // via confirm_slash_menu → submit_prompt.
        if app.slash_menu_visible() {
            // Snap selection to the exact command row when possible.
            let rows = app.popup_rows();
            let want = cmd.split_whitespace().next().unwrap();
            if let Some(i) = rows
                .iter()
                .position(|r| matches!(r, PopupRow::Command(c) if c.name == want))
            {
                app.slash_selected = i;
            }
        }
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            RunOutcome::Prompt(t) => {
                // Menu confirm may expand bare name; with args it stays as typed.
                assert!(
                    t == cmd || t == cmd.split_whitespace().next().unwrap(),
                    "unexpected prompt text for {cmd}: {t}"
                );
            }
            other => panic!("expected Prompt for {cmd}, got {other:?}"),
        }
        assert!(
            app.input.is_empty(),
            "input should clear after submitting {cmd}"
        );
        assert!(
            app.messages.is_empty(),
            "UI slash {cmd} must not appear in chat transcript"
        );
        assert_eq!(
            app.prompt_history,
            vec!["real prompt".to_string()],
            "UI slash {cmd} must not pollute ↑ history"
        );
    }

    // Real user text still records both history and transcript.
    app.input = "hello".into();
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
        RunOutcome::Prompt(_)
    ));
    assert_eq!(
        app.prompt_history,
        vec!["real prompt".to_string(), "hello".to_string()]
    );
    assert_eq!(app.messages.last().unwrap().content, "hello");
}

#[test]
fn single_esc_clears_draft_into_history() {
    let mut app = App::new("test");
    app.input = "unsent draft".into();
    // One Esc clears immediately (must always feel responsive).
    assert!(matches!(
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)),
        RunOutcome::Noop
    ));
    assert!(app.input.is_empty());
    assert_eq!(
        app.prompt_history.last().map(String::as_str),
        Some("unsent draft")
    );
    app.history_prev();
    assert_eq!(app.input, "unsent draft");
}

#[test]
fn double_esc_empty_opens_rewind() {
    let mut app = App::new("test");
    assert!(app.input.is_empty());
    assert!(matches!(
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)),
        RunOutcome::Noop
    ));
    assert!(matches!(
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)),
        RunOutcome::OpenRewind
    ));
}

#[test]
fn single_esc_empty_does_not_open_rewind() {
    let mut app = App::new("test");
    assert!(matches!(
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)),
        RunOutcome::Noop
    ));
    // No second Esc — must not open rewind on a lonely press.
    assert!(!matches!(
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        RunOutcome::OpenRewind
    ));
}

#[test]
fn load_prompt_history_enables_cross_session_recall() {
    let mut app = App::new("test");
    // Simulate startup load from previous sessions / disk.
    app.load_prompt_history(vec![
        "from last session".into(),
        "another old prompt".into(),
    ]);
    assert_eq!(app.prompt_history_len(), 2);
    app.input.clear();
    app.history_prev();
    assert_eq!(app.input, "another old prompt");
    app.history_prev();
    assert_eq!(app.input, "from last session");
}

#[test]
fn multi_line_selection_range_and_text() {
    let mut app = App::new("t");
    app.chat_total_lines = 5;
    app.chat_line_text = vec![
        "line-0".into(),
        "line-1".into(),
        "line-2".into(),
        "line-3".into(),
        "line-4".into(),
    ];
    // Full lines 1..=3: caret at start of line 1 → end of line 3.
    app.select_anchor = Some(SelectPos::new(1, 0));
    app.select_end = Some(SelectPos::new(3, "line-3".chars().count()));
    assert!(app.selection_is_multi_line());
    assert_eq!(app.selection_range(), Some((1, 3)));
    let text = app.selection_text().unwrap();
    assert_eq!(text, "line-1\nline-2\nline-3");
    assert!(app.request_copy_selection());
    assert_eq!(
        app.clipboard_pending.as_deref(),
        Some("line-1\nline-2\nline-3")
    );
}

#[test]
fn partial_line_selection_text() {
    let mut app = App::new("t");
    app.chat_total_lines = 1;
    app.chat_line_text = vec!["hello world".into()];
    app.select_anchor = Some(SelectPos::new(0, 0));
    app.select_end = Some(SelectPos::new(0, 5));
    assert!(!app.selection_is_multi_line());
    assert_eq!(app.selection_text().as_deref(), Some("hello"));

    app.select_anchor = Some(SelectPos::new(0, 6));
    app.select_end = Some(SelectPos::new(0, 11));
    assert_eq!(app.selection_text().as_deref(), Some("world"));
}

#[test]
fn partial_multi_line_selection_text() {
    let mut app = App::new("t");
    app.chat_total_lines = 3;
    app.chat_line_text = vec!["abcdef".into(), "123456".into(), "uvwxyz".into()];
    // From 'c' on line 0 through 'x' on line 2 (exclusive end caret after 'x' = col 2).
    app.select_anchor = Some(SelectPos::new(0, 2));
    app.select_end = Some(SelectPos::new(2, 2));
    assert_eq!(app.selection_text().as_deref(), Some("cdef\n123456\nuv"));
}

#[test]
fn empty_selection_is_none() {
    let mut app = App::new("t");
    app.chat_total_lines = 1;
    app.chat_line_text = vec!["hello".into()];
    app.select_anchor = Some(SelectPos::new(0, 2));
    app.select_end = Some(SelectPos::new(0, 2));
    assert!(!app.has_selection());
    assert!(app.selection_text().is_none());
}

#[test]
fn display_col_to_caret_ascii_and_wide() {
    assert_eq!(display_col_to_caret("hello", 0), 0);
    assert_eq!(display_col_to_caret("hello", 3), 3);
    assert_eq!(display_col_to_caret("hello", 5), 5);
    assert_eq!(display_col_to_caret("hello", 99), 5);
    // CJK: each char width 2. Caret advances once the pointer leaves the
    // start cell of a glyph (half-open range includes that glyph).
    assert_eq!(display_col_to_caret("你好", 0), 0);
    assert_eq!(display_col_to_caret("你好", 1), 1);
    assert_eq!(display_col_to_caret("你好", 2), 1);
    assert_eq!(display_col_to_caret("你好", 4), 2);
}

#[test]
fn page_up_disables_follow() {
    let mut app = App::new("test");
    for i in 0..20 {
        app.push_system(format!("m{i}"));
    }
    assert!(app.follow_bottom);
    app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
    assert!(!app.follow_bottom);
    app.scroll_to_bottom();
    assert!(app.follow_bottom);
}

#[test]
fn shift_g_restores_live_follow_in_idle_and_busy_input() {
    let mut app = App::new("test");
    app.chat_total_lines = 20;
    app.chat_view_height = 5;
    app.scroll_to_top();
    assert!(!app.follow_bottom);

    let outcome = app.handle_key(key(KeyCode::Char('G'), KeyModifiers::SHIFT));
    assert!(matches!(outcome, RunOutcome::Noop));
    assert!(app.follow_bottom);

    app.scroll_to_top();
    assert!(!app.follow_bottom);
    app.handle_busy_key(key(KeyCode::Char('G'), KeyModifiers::SHIFT));
    assert!(app.follow_bottom);
}

#[test]
fn scrolling_down_to_bottom_reenters_live_follow() {
    let mut app = App::new("test");
    app.messages.push(Message::assistant("message"));
    app.chat_total_lines = 20;
    app.chat_view_height = 5;
    app.scroll_to_top();
    assert!(!app.follow_bottom);

    app.scroll_down(app.max_scroll());
    assert!(app.follow_bottom);
    assert_eq!(app.chat_scroll, 0);
}

#[test]
fn paste_preserves_newlines() {
    let mut app = App::new("test");
    app.handle_paste("foo\nbar");
    assert_eq!(app.input, "foo\nbar");
}

#[test]
fn alt_h_opens_help_even_with_draft() {
    let mut app = App::new("test");
    app.input = "draft".into();
    app.input_cursor = app.input.chars().count();
    app.handle_key(key(KeyCode::Char('h'), KeyModifiers::ALT));
    assert!(app.float_open());
    assert_eq!(
        app.float.as_ref().map(|f| f.kind),
        Some(crate::float::FloatKind::Help)
    );
    assert_eq!(app.input, "draft", "Alt+H must not mutate the draft");
}

#[test]
fn alt_h_uppercase_opens_help() {
    let mut app = App::new("test");
    app.handle_key(key(KeyCode::Char('H'), KeyModifiers::ALT));
    assert!(app.float_open());
    assert_eq!(
        app.float.as_ref().map(|f| f.kind),
        Some(crate::float::FloatKind::Help)
    );
}

#[test]
fn help_fallback_chords_still_open_help() {
    // Silent fallbacks (still work, not shown on status strip).
    let cases = [
        (KeyCode::Char('k'), KeyModifiers::CONTROL),
        (KeyCode::Char('K'), KeyModifiers::CONTROL),
        (KeyCode::Char('\u{0b}'), KeyModifiers::NONE),
        (KeyCode::F(1), KeyModifiers::NONE),
        (KeyCode::Char('/'), KeyModifiers::CONTROL),
        (KeyCode::Char('_'), KeyModifiers::CONTROL),
        (KeyCode::Char('\u{1f}'), KeyModifiers::NONE),
    ];
    for (code, mods) in cases {
        let mut app = App::new("test");
        app.handle_key(key(code, mods));
        assert!(app.float_open(), "expected help for {code:?} {mods:?}");
        assert_eq!(
            app.float.as_ref().map(|f| f.kind),
            Some(crate::float::FloatKind::Help)
        );
    }
}

#[test]
fn question_mark_is_plain_text() {
    let mut app = App::new("test");
    app.handle_key(key(KeyCode::Char('?'), KeyModifiers::NONE));
    assert!(!app.float_open());
    assert_eq!(app.input, "?");
}

#[test]
fn bare_slash_still_opens_slash_menu() {
    let mut app = App::new("test");
    app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE));
    assert!(!app.float_open());
    assert_eq!(app.input, "/");
    assert!(app.slash_menu_visible());
}

#[test]
fn paste_into_float_edit_does_not_touch_main_input() {
    let mut app = App::new("test");
    app.input = "draft".into();
    app.float = Some(FloatMenu::settings_provider_detail(
        "linuxdo",
        "custom",
        &[("base_url".into(), "unset".into())],
    ));
    app.start_settings_inline_edit("provider_set:linuxdo:base_url", "base_url", "");
    assert!(app.float_open());
    assert!(!app.prompt_focused());

    app.handle_paste("https://api.example.com/v1\n");
    assert_eq!(app.input, "draft", "main prompt must stay untouched");
    let search = app.float.as_ref().map(|f| f.search.clone()).unwrap();
    assert_eq!(search, "https://api.example.com/v1");
    assert!(app.float.as_ref().is_some_and(|f| f.edit_mode));
}

#[test]
fn transcript_browse_unfocuses_prompt_caret_until_typing() {
    // Grok-style: j/k browse owns focus — no blinking prompt caret.
    let mut app = App::new("test");
    assert!(app.prompt_focused());
    assert!(!app.transcript_browse_focused());

    app.chat_focus = Some(0);
    app.input.clear();
    assert!(app.transcript_browse_focused());
    assert!(!app.prompt_focused());

    // Blink tick while unfocused must not leave caret mid-off.
    app.cursor_on = false;
    app.toggle_cursor();
    assert!(app.cursor_on, "unfocused blink keeps caret ready for refocus");
    assert!(!app.prompt_focused());

    // Typing returns to the composer and clears row focus.
    app.insert_input_char('h');
    assert_eq!(app.input, "h");
    assert!(app.chat_focus.is_none());
    assert!(app.prompt_focused());
    assert!(!app.transcript_browse_focused());
}

#[test]
fn main_input_left_right_moves_cursor_and_inserts_mid() {
    let mut app = App::new("test");
    for ch in "hello".chars() {
        app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(app.input, "hello");
    assert_eq!(app.input_cursor, 5);

    // ←←← → caret before "llo"
    app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
    app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
    app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(app.input_cursor, 2);

    app.handle_key(key(KeyCode::Char('X'), KeyModifiers::NONE));
    assert_eq!(app.input, "heXllo");
    assert_eq!(app.input_cursor, 3);

    // Backspace deletes before caret.
    app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(app.input, "hello");
    assert_eq!(app.input_cursor, 2);

    // Delete removes after caret.
    app.handle_key(key(KeyCode::Delete, KeyModifiers::NONE));
    assert_eq!(app.input, "helo");
    assert_eq!(app.input_cursor, 2);

    // Right to end, then right stays at end.
    app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
    app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
    app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
    app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(app.input_cursor, app.input.chars().count());
    app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(app.input_cursor, app.input.chars().count());
}

#[test]
fn float_edit_left_right_moves_cursor_not_back() {
    let mut app = App::new("test");
    app.float = Some(FloatMenu::settings_provider_detail(
        "linuxdo",
        "custom",
        &[],
    ));
    app.start_settings_inline_edit(
        "provider_set:linuxdo:base_url",
        "base_url",
        "https://api.example.com",
    );
    let end = app.float.as_ref().unwrap().search_cursor;
    assert_eq!(end, "https://api.example.com".chars().count());

    // Left must move caret, not leave edit mode / pop the float.
    app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
    app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
    app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
    assert!(app.float_open());
    assert!(app.settings_inline_op.is_some());
    assert!(app.float.as_ref().is_some_and(|f| f.edit_mode));
    assert_eq!(app.float.as_ref().unwrap().search_cursor, end - 3);

    // Insert in the middle (caret is 3 chars before end → before "com").
    app.handle_key(key(KeyCode::Char('X'), KeyModifiers::NONE));
    assert_eq!(
        app.float.as_ref().map(|f| f.search.as_str()),
        Some("https://api.example.Xcom")
    );

    // Home / End
    app.handle_key(key(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(app.float.as_ref().unwrap().search_cursor, 0);
    app.handle_key(key(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(
        app.float.as_ref().unwrap().search_cursor,
        app.float.as_ref().unwrap().search.chars().count()
    );

    // Esc still cancels edit (not left).
    app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.settings_inline_op.is_none());
    assert!(app.float.as_ref().is_some_and(|f| !f.edit_mode));
}

#[test]
fn paste_into_float_filter_does_not_touch_main_input() {
    let mut app = App::new("test");
    app.input = "hello".into();
    app.open_settings_providers(&[("linuxdo".into(), "ok".into())]);
    app.handle_paste("linu");
    assert_eq!(app.input, "hello");
    assert_eq!(app.float.as_ref().map(|f| f.search.as_str()), Some("linu"));
}

fn drain_image_jobs(app: &mut App) {
    for _ in 0..200 {
        app.poll_image_jobs();
        if !app.has_loading_images() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("image job did not finish");
}

#[test]
fn paste_data_uri_inserts_image_token() {
    with_temp_media(|_| {
        let mut app = App::new("test");
        let uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        app.handle_paste(uri);
        // Chip appears immediately (loading).
        assert!(
            app.input.contains(one_core::image::IMAGE_TOKEN),
            "input={}",
            app.input
        );
        drain_image_jobs(&mut app);
        assert_eq!(app.pending_images.len(), 1);
        assert_eq!(app.pending_images[0].mime_type, "image/png");
        assert!(!app.pending_images[0].loading);
        let taken = app.take_pending_images();
        assert_eq!(taken.len(), 1);
    });
}

#[test]
fn set_input_for_edit_with_images_restores_chips() {
    with_temp_media(|_| {
        let mut app = App::new("test");
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let (path, mime) = one_core::image::store_image_base64(b64, Some("image/png")).unwrap();
        let token = one_core::image::image_token(1);
        app.set_input_for_edit_with_images(
            format!("这个是什么 {token} "),
            vec![(mime.clone(), path.display().to_string())],
        );
        assert!(app.input.contains(&token));
        assert_eq!(app.pending_images.len(), 1);
        assert_eq!(app.pending_images[0].mime_type, "image/png");
        let taken = app.take_pending_images();
        assert_eq!(taken.len(), 1);
        // Simulate submit path: chips present → committed on submit_prompt.
        app.set_input_for_edit_with_images(
            format!("再看 {token}"),
            vec![(mime, path.display().to_string())],
        );
        let outcome = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match outcome {
            RunOutcome::Prompt(p) => {
                assert!(p.contains("再看"), "{p}");
                // Image tokens stripped for agent text; bytes go via take_pending_images.
                assert!(!p.contains("[image ·"), "{p}");
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
        let imgs = app.take_pending_images();
        assert_eq!(imgs.len(), 1);
    });
}

#[test]
fn paste_image_path_file_attaches() {
    with_temp_media(|_| {
        let dir = std::env::temp_dir().join(format!("one-tui-img-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("dot.png");
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let bytes = one_core::image::decode_base64(b64).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        let mut app = App::new("test");
        app.handle_paste(path.to_str().unwrap());
        assert!(app.input.contains(one_core::image::IMAGE_TOKEN));
        drain_image_jobs(&mut app);
        assert_eq!(app.pending_images.len(), 1);
        assert_eq!(app.pending_images[0].mime_type, "image/png");
        assert!(!app.pending_images[0].loading);
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn ctrl_v_image_key_shows_placeholder_immediately() {
    let mut app = App::new("test");
    let key = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
    let _ = app.handle_key(key);
    // Optimistic chip appears before clipboard work finishes (do not wait
    // for PowerShell — it can hang for seconds under WSL).
    assert!(
        app.input.contains(one_core::image::IMAGE_TOKEN),
        "expected loading chip in input, got {}",
        app.input
    );
    assert!(
        app.pending_images.iter().any(|i| i.loading),
        "expected loading pending image"
    );
    assert!(app.has_loading_images());
    let toast = app.toast.as_ref().map(|t| t.text.as_str()).unwrap_or("");
    assert!(
        toast.contains("pasting"),
        "expected pasting toast, got {toast:?}"
    );
    // Abandon in-flight job (dropping app closes the channel).
}

#[test]
fn submit_blocked_while_image_loading() {
    let mut app = App::new("test");
    let _ = app.begin_loading_image("x.png");
    assert!(app.has_loading_images());
    let outcome = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(outcome, RunOutcome::Noop));
    let toast = app.toast.as_ref().map(|t| t.text.as_str()).unwrap_or("");
    assert!(
        toast.contains("pasting") || toast.contains("still"),
        "{toast}"
    );
}

#[test]
fn deleting_image_token_detaches() {
    with_temp_media(|_| {
        let mut app = App::new("test");
        let tiny = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        app.attach_image("image/png".into(), tiny.into(), "shot.png".into());
        assert_eq!(app.pending_images.len(), 1);
        // User deletes the whole token from input.
        app.input = "hello only".into();
        app.sync_pending_images();
        assert!(app.pending_images.is_empty());
    });
}

#[test]
fn backspace_removes_image_token_atomically() {
    with_temp_media(|_| {
        let mut app = App::new("test");
        app.input = "hello".into();
        let tiny = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        app.attach_image("image/png".into(), tiny.into(), "shot.png".into());
        // input is "hello [图片.img] "
        assert!(app.input.contains(one_core::image::IMAGE_TOKEN));
        assert_eq!(app.pending_images.len(), 1);
        // One Backspace wipes the whole token (+ spaces), not char-by-char.
        app.pop_input();
        assert_eq!(app.input, "hello");
        assert!(app.pending_images.is_empty());
    });
}

#[test]
fn long_paste_becomes_text_chip() {
    let mut app = App::new("test");
    let long = "line\n".repeat(30);
    app.handle_paste(&long);
    assert!(
        app.input.contains(one_core::image::TEXT_TOKEN),
        "input={}",
        app.input
    );
    assert!(!app.input.contains("line\nline"));
    assert_eq!(app.pending_texts.len(), 1);
    assert!(app.pending_texts[0].body.contains("line"));

    // Atomic backspace clears chip + body.
    app.pop_input();
    assert!(app.input.is_empty() || !app.input.contains("文本"));
    assert!(app.pending_texts.is_empty());
}

#[test]
fn submit_expands_text_chip_for_agent() {
    let mut app = App::new("test");
    app.attach_text_blob("SECRET_BODY_XYZ\nsecond".into());
    // Chip already in input with trailing space; append instruction.
    app.input.push_str("summarize");
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
        RunOutcome::Prompt(t) => {
            assert!(t.contains("SECRET_BODY_XYZ"), "agent text={t}");
            assert!(t.contains("summarize"), "agent text={t}");
            assert!(!t.contains("文本"), "chip should expand, got {t}");
        }
        other => panic!("unexpected {other:?}"),
    }
    // Transcript stays compact.
    let shown = &app.messages.last().unwrap().content;
    assert!(shown.contains(one_core::image::TEXT_TOKEN), "{shown}");
    assert!(!shown.contains("SECRET_BODY_XYZ"), "{shown}");
}

#[test]
fn submit_image_only_prompt() {
    with_temp_media(|_| {
        let mut app = App::new("test");
        let tiny = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        app.attach_image("image/png".into(), tiny.into(), "shot.png".into());
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            RunOutcome::Prompt(t) => assert!(t.is_empty(), "token should be stripped, got {t}"),
            other => panic!("unexpected {other:?}"),
        }
        // Staged for CLI take.
        let taken = app.take_pending_images();
        assert_eq!(taken.len(), 1);
        assert!(app
            .messages
            .last()
            .unwrap()
            .content
            .contains(one_core::image::IMAGE_TOKEN));
    });
}

#[test]
fn ctrl_j_inserts_newline() {
    let mut app = App::new("test");
    app.input = "a".into();
    app.input_cursor = 1;
    app.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
    assert_eq!(app.input, "a\n");
}

#[test]
fn busy_esc_aborts_ctrl_c_force_quits() {
    let mut app = App::new("test");
    app.begin_busy();

    // Soft cancel: Esc only (`q` is a normal character).
    app.handle_busy_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.take_abort());
    assert!(!app.force_quit_pending());

    // Bare `q` is steer/follow-up text, not abort.
    app.handle_busy_key(key(KeyCode::Char('q'), KeyModifiers::NONE));
    assert_eq!(app.input, "q");
    assert!(!app.take_abort());
    assert!(!app.force_quit_pending());

    // First Ctrl+C clears steer draft (does not force-quit).
    app.handle_busy_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.input.is_empty());
    assert!(!app.force_quit_pending());

    // Second Ctrl+C force-quits — never soft-cancel only.
    app.handle_busy_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.force_quit_pending());
    assert!(app.take_force_quit());
    // request_force_quit also trips abort so in-flight work stops.
    assert!(app.take_abort());
}

#[test]
fn busy_enter_queues_ps_ui_action() {
    let mut app = App::new("test");
    app.begin_busy();

    // Type `/ps` (no slash menu once a space is present; bare needs menu confirm).
    // Use `/ps detail` shape without menu, then bare via direct input clear.
    app.input = "/ps".into();
    // When slash menu is open, Enter confirms selection → same Prompt path.
    if app.slash_menu_visible() {
        let rows = app.popup_rows();
        if let Some(i) = rows
            .iter()
            .position(|r| matches!(r, PopupRow::Command(c) if c.name == "/ps"))
        {
            app.slash_selected = i;
        }
    }
    app.handle_busy_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.input.is_empty(), "input cleared after busy /ps");
    match app.take_busy_ui() {
        Some(RunOutcome::OpenBackgroundList) => {}
        other => panic!("expected OpenBackgroundList, got {other:?}"),
    }
    assert!(app.take_busy_ui().is_none());

    // Detail form with args (no menu).
    app.input = "/ps task-9".into();
    app.handle_busy_key(key(KeyCode::Enter, KeyModifiers::NONE));
    match app.take_busy_ui() {
        Some(RunOutcome::OpenBackgroundDetail { id }) => assert_eq!(id, "task-9"),
        other => panic!("expected OpenBackgroundDetail, got {other:?}"),
    }

    // Subagent path is separate from bash `/ps`.
    app.input = "/tasks job_1".into();
    app.handle_busy_key(key(KeyCode::Enter, KeyModifiers::NONE));
    match app.take_busy_ui() {
        Some(RunOutcome::OpenSubagentDetail { id }) => assert_eq!(id, "job_1"),
        other => panic!("expected OpenSubagentDetail, got {other:?}"),
    }
}

#[test]
fn ask_user_enter_keeps_result_against_prompt_reopen() {
    use crate::select::{SelectOption, SelectPrompt, SelectResult};

    let mut app = App::new("test");
    app.begin_busy();
    let mut prompt = SelectPrompt::single(
        "颜色选择",
        "你想选择哪种颜色?",
        vec![
            SelectOption::new("红色", "红色", ""),
            SelectOption::new("绿色", "绿色", ""),
            SelectOption::new("蓝色", "蓝色", ""),
        ],
    );
    prompt.allow_other = true;
    app.set_select_prompt(SelectKind::AskUser { id: 1 }, prompt);
    assert!(app.select_prompt().is_some());

    // Confirm first option (Enter).
    app.handle_busy_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        app.select_prompt().is_none(),
        "dock should close after confirm"
    );

    // Simulate the old buggy drain order: re-surface pending HITL before
    // taking the answer. Must not wipe select_result.
    let mut reopen = SelectPrompt::single(
        "颜色选择",
        "你想选择哪种颜色?",
        vec![
            SelectOption::new("红色", "红色", ""),
            SelectOption::new("绿色", "绿色", ""),
            SelectOption::new("蓝色", "蓝色", ""),
        ],
    );
    reopen.allow_other = true;
    app.set_select_prompt(SelectKind::AskUser { id: 1 }, reopen);

    let (kind, result) = app
        .take_select_result()
        .expect("result must survive reopen");
    assert!(matches!(kind, SelectKind::AskUser { id: 1 }));
    assert_eq!(
        result,
        SelectResult::Confirmed {
            ids: vec!["红色".into()],
            other: None,
        }
    );
}

#[test]
fn ask_user_tab_enters_other_typing() {
    use crate::select::{SelectOption, SelectPhase, SelectPrompt, SelectResult};

    let mut app = App::new("test");
    app.begin_busy();
    let mut prompt = SelectPrompt::single(
        "颜色选择",
        "你想选择哪种颜色?",
        vec![
            SelectOption::new("红色", "红色", ""),
            SelectOption::new("绿色", "绿色", ""),
        ],
    );
    prompt.allow_other = true;
    app.set_select_prompt(SelectKind::AskUser { id: 7 }, prompt);

    app.handle_busy_key(key(KeyCode::Tab, KeyModifiers::NONE));
    let p = app.select_prompt().expect("still open for typing");
    assert!(matches!(p.phase, SelectPhase::Typing { .. }));
    assert!(p.is_other_row(p.selected));

    app.handle_busy_key(key(KeyCode::Char('紫'), KeyModifiers::NONE));
    app.handle_busy_key(key(KeyCode::Enter, KeyModifiers::NONE));
    let (_, result) = app.take_select_result().unwrap();
    assert_eq!(
        result,
        SelectResult::Confirmed {
            ids: vec![],
            other: Some("紫".into()),
        }
    );
}

#[test]
fn idle_ctrl_c_requires_double_tap_to_quit() {
    let mut app = App::new("test");
    match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
        RunOutcome::Noop => {}
        other => panic!("expected Noop (arm quit), got {other:?}"),
    }
    match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
        RunOutcome::Quit => {}
        other => panic!("expected Quit on second Ctrl+C, got {other:?}"),
    }
}

#[test]
fn ctrl_c_closes_settings_then_quits() {
    let mut app = App::new("test");
    app.open_settings_float();
    assert!(app.float_open());

    match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
        RunOutcome::Noop => {}
        other => panic!("expected Noop (close float), got {other:?}"),
    }
    assert!(!app.float_open());

    match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
        RunOutcome::Quit => {}
        other => panic!("expected Quit on second Ctrl+C, got {other:?}"),
    }
}

#[test]
fn ctrl_c_clears_input_then_quits() {
    let mut app = App::new("test");
    app.input = "draft text".into();

    match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
        RunOutcome::Noop => {}
        other => panic!("expected Noop (clear input), got {other:?}"),
    }
    assert!(app.input.is_empty());

    match app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
        RunOutcome::Quit => {}
        other => panic!("expected Quit on second Ctrl+C, got {other:?}"),
    }
}

#[test]
fn ctrl_c_quit_arm_disarmed_by_other_key() {
    let mut app = App::new("test");
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        RunOutcome::Noop
    ));
    // Typing cancels the pending quit arm.
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        RunOutcome::Noop
    ));
    // Next Ctrl+C clears the typed char (does not quit — arm was disarmed).
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        RunOutcome::Noop
    ));
    assert!(app.input.is_empty());
    // Clearing re-arms: one more Ctrl+C quits.
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        RunOutcome::Quit
    ));
}

#[test]
fn expand_at_files_inlines_content() {
    let dir = std::env::temp_dir().join(format!("one-at-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("note.txt");
    std::fs::write(&path, "hello-at-file").unwrap();
    let input = format!("review @{}", path.display());
    let expanded = expand_at_files(&input);
    assert!(expanded.contains("hello-at-file"), "{expanded}");
    assert!(expanded.contains("file:"), "{expanded}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn path_token_detects_at_and_slash() {
    assert!(path_token_at_end("see @src/").is_some());
    assert!(path_token_at_end("open ./foo").is_some());
    assert!(path_token_at_end("just words").is_none());
}

#[test]
fn stream_sync_marks_assistant_streaming() {
    let mut app = App::new("test");
    app.begin_busy();
    app.append_stream("hi");
    app.sync_stream_message();
    let last = app.messages.last().unwrap();
    assert!(last.streaming);
    assert_eq!(last.content, "hi");
    app.finish_stream();
    assert!(!app.messages.last().unwrap().streaming);
    assert!(app.messages.last().unwrap().footer.is_some());
}

#[test]
fn thinking_stream_then_text() {
    let mut app = App::new("test");
    app.begin_busy();
    app.append_thinking_stream("ponder");
    app.sync_stream_message();
    assert_eq!(app.messages.last().unwrap().role, MessageRole::Thinking);
    assert!(app.messages.last().unwrap().streaming);
    app.append_stream("answer");
    app.sync_stream_message();
    // Thinking finalized, assistant streaming.
    assert_eq!(app.messages.len(), 2);
    assert_eq!(app.messages[0].role, MessageRole::Thinking);
    assert!(!app.messages[0].streaming);
    assert_eq!(app.messages[1].role, MessageRole::Assistant);
    assert_eq!(app.messages[1].content, "answer");
    app.finish_stream();
    assert!(!app.messages[1].streaming);
}

#[test]
fn thinking_tool_thinking_are_separate_segments() {
    // Interleaved think → tool → think must not accumulate prior text
    // into the second bubble (regression: seal forgot to finish thinking).
    let mut app = App::new("test");
    app.begin_busy();
    app.append_thinking_stream("first round plan. ");
    app.sync_stream_message();
    assert_eq!(app.messages.len(), 1);
    assert_eq!(app.messages[0].content, "first round plan. ");

    app.push_tool_call("web_search", "query");
    assert!(!app.messages[0].streaming, "thinking sealed before tool");
    assert!(
        app.thinking_buffer.is_empty(),
        "buffer cleared so next round starts clean"
    );
    assert_eq!(app.messages[0].content, "first round plan. ");
    // Default policy: collapse finished thinking so tool rows stay scannable.
    assert!(
        !app.messages[0].thinking_expanded,
        "finished thinking collapses by default"
    );
    assert_eq!(app.messages.last().unwrap().role, MessageRole::Tool);

    app.append_thinking_stream("second round only.");
    app.sync_stream_message();
    app.finish_stream();

    let thinking: Vec<_> = app
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Thinking)
        .collect();
    assert_eq!(thinking.len(), 2, "one bubble per thinking round");
    assert_eq!(thinking[0].content, "first round plan. ");
    assert_eq!(
        thinking[1].content, "second round only.",
        "second bubble must not re-include first round text"
    );
    assert!(!thinking[1].content.contains("first round"));
    assert!(
        thinking
            .iter()
            .all(|m| !m.thinking_expanded && !m.streaming),
        "both finished segments stay collapsed by default"
    );
}

#[test]
fn finished_thinking_collapses_by_default() {
    let mut app = App::new("test");
    app.begin_busy();
    app.append_thinking_stream("long chain of thought…");
    app.sync_stream_message();
    assert!(app.messages[0].streaming);
    assert!(app.messages[0].thinking_expanded); // live tail while streaming
    app.finish_stream();
    assert!(!app.messages[0].streaming);
    assert!(
        !app.messages[0].thinking_expanded,
        "after stream ends, default is ▸ collapsed header"
    );
    assert!(!app.show_thinking);
}

#[test]
fn ctrl_t_toggles_thinking_visibility() {
    let mut app = App::new("test");
    app.messages.push(Message::thinking("secret plan"));
    // Default: collapsed.
    assert!(!app.show_thinking);
    assert!(!app.messages[0].thinking_expanded);
    match app.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL)) {
        RunOutcome::Noop => {}
        other => panic!("expected Noop, got {other:?}"),
    }
    assert!(app.show_thinking);
    assert!(app.messages[0].thinking_expanded);
    // Toggle back collapses all.
    match app.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL)) {
        RunOutcome::Noop => {}
        other => panic!("expected Noop, got {other:?}"),
    }
    assert!(!app.show_thinking);
    assert!(!app.messages[0].thinking_expanded);
}

#[test]
fn shift_tab_cycles_agent_mode_space_does_not() {
    let mut app = App::new("test");
    // Empty-input Space used to cycle modes — now it types a space.
    assert!(matches!(
        app.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE)),
        RunOutcome::Noop
    ));
    assert_eq!(app.input, " ");
    app.input.clear();

    // Crossterm reports Shift+Tab as BackTab.
    assert!(matches!(
        app.handle_key(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
        RunOutcome::CycleAgentMode
    ));
    // Some terminals send Tab+SHIFT instead.
    assert!(matches!(
        app.handle_key(key(KeyCode::Tab, KeyModifiers::SHIFT)),
        RunOutcome::CycleAgentMode
    ));
    // Plain Tab is still completion, not mode cycle.
    assert!(matches!(
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE)),
        RunOutcome::Noop
    ));
}

#[test]
fn tool_lifecycle() {
    let mut app = App::new("test");
    app.push_tool_call("bash", "ls");
    assert_eq!(
        app.messages.last().unwrap().tool_status,
        Some(ToolStatus::Running)
    );
    app.finish_tool("bash", false);
    assert_eq!(
        app.messages.last().unwrap().tool_status,
        Some(ToolStatus::Done)
    );
}

#[test]
fn finish_tool_matches_by_call_id_not_only_name() {
    let mut app = App::new("test");
    // Parallel batch of same-named tools — finish the first id while later ones still run.
    app.push_tool_call_with_id("find", r#"{"path":"a"}"#, Some("c1".into()));
    app.push_tool_call_with_id("find", r#"{"path":"b"}"#, Some("c2".into()));
    app.push_tool_call_with_id("ls", r#"{"path":"."}"#, Some("c3".into()));

    app.finish_tool_with_output_id("find", false, Some("a-files".into()), Some("c1"));
    assert_eq!(app.messages[0].tool_status, Some(ToolStatus::Done));
    assert_eq!(app.messages[0].tool_output.as_deref(), Some("a-files"));
    assert_eq!(app.messages[1].tool_status, Some(ToolStatus::Running));
    assert_eq!(app.messages[2].tool_status, Some(ToolStatus::Running));

    // Out-of-order: finish ls before the second find.
    app.finish_tool_with_output_id("ls", false, Some("listing".into()), Some("c3"));
    assert_eq!(app.messages[2].tool_status, Some(ToolStatus::Done));
    assert_eq!(app.messages[1].tool_status, Some(ToolStatus::Running));

    app.finish_tool_with_output_id("find", false, Some("b-files".into()), Some("c2"));
    assert_eq!(app.messages[1].tool_status, Some(ToolStatus::Done));
    assert_eq!(app.messages[1].tool_output.as_deref(), Some("b-files"));
}

#[test]
fn tool_error_auto_expands_output() {
    let mut app = App::new("test");
    app.push_tool_call("bash", "cargo test");
    app.finish_tool_with_output(
        "bash",
        true,
        Some("error: could not compile `one`\n  --> src/lib.rs:1".into()),
    );
    let last = app.messages.last().unwrap();
    assert_eq!(last.tool_status, Some(ToolStatus::Error));
    assert!(last.tool_expanded);
    assert!(last
        .tool_output
        .as_ref()
        .unwrap()
        .contains("could not compile"));
    assert!(last.tool_summary.as_ref().unwrap().contains("error"));
}

#[test]
fn alert_is_ui_only_role() {
    let mut app = App::new("test");
    app.push_error_alert("provider timeout");
    let last = app.messages.last().unwrap();
    assert_eq!(last.role, MessageRole::Alert);
    assert_eq!(last.alert_level, Some(AlertLevel::Error));
}

#[test]
fn edit_tool_gets_diff_summary() {
    let mut app = App::new("test");
    app.push_tool_call(
        "edit",
        r#"{"path":"src/a.rs","old_string":"fn a(){}","new_string":"fn a(){\n  1\n}"}"#,
    );
    app.finish_tool_with_output("edit", false, Some("Updated src/a.rs".into()));
    let last = app.messages.last().unwrap();
    assert_eq!(last.tool_status, Some(ToolStatus::Done));
    let summary = last.tool_summary.as_deref().unwrap_or("");
    // Path lives on the header; summary is diff stats only.
    assert!(
        summary.contains('+') || summary.contains('−') || summary.contains("edited"),
        "{summary}"
    );
    let out = last.tool_output.as_deref().unwrap_or("");
    assert!(out.contains('+') || out.contains("Updated"), "{out}");
}

#[test]
fn welcome_try_keys_submit_sample_prompts() {
    let mut app = App::new("test");
    assert!(app.messages.is_empty());
    assert!(app.input.is_empty());

    let out = app.handle_key(key(KeyCode::Char('1'), KeyModifiers::NONE));
    match out {
        RunOutcome::Prompt(p) => {
            assert_eq!(p, WELCOME_TRY_PROMPTS[0]);
        }
        other => panic!("expected Prompt from try key, got {other:?}"),
    }
    assert!(app.input.is_empty(), "submit should clear input");
    // submit_prompt already pushed the user turn — digits no longer shortcut.
    assert!(!app.messages.is_empty());

    let out2 = app.handle_key(key(KeyCode::Char('2'), KeyModifiers::NONE));
    assert!(matches!(out2, RunOutcome::Noop));
    assert_eq!(app.input, "2");
}

#[test]
fn toast_expires_and_classifies_error() {
    let mut app = App::new("test");
    app.set_notice("error: boom");
    let t = app.toast_active().unwrap();
    assert_eq!(t.level, AlertLevel::Error);
    assert!(t.text.contains("boom"));
    // Force expiry.
    if let Some(toast) = app.toast.as_mut() {
        toast.created =
            Instant::now() - crate::state::TOAST_TTL - std::time::Duration::from_secs(1);
    }
    app.tick_toast();
    assert!(app.toast_active().is_none());
}

#[test]
fn three_done_tools_form_collapsible_group() {
    let mut app = App::new("test");
    for (name, args) in [
        ("read", r#"{"path":"a.rs"}"#),
        ("bash", r#"{"command":"ls"}"#),
        ("grep", r#"{"pattern":"x"}"#),
    ] {
        app.push_tool_call(name, args);
        app.finish_tool_with_output(name, false, Some("ok\nline2".into()));
        // Force collapsed body so group can form.
        if let Some(last) = app.messages.last_mut() {
            last.tool_expanded = false;
            last.tool_ungroup = false;
        }
    }
    assert!(tool_view::streak_can_collapse(&app.messages, 0, 3));
    app.toggle_last_tool_expand();
    assert!(app.messages.iter().all(|m| m.tool_ungroup));
    assert!(!tool_view::streak_can_collapse(&app.messages, 0, 3));
    assert!(tool_view::streak_shows_group_header(&app.messages, 0, 3));

    // Click middle tool: expand body only, do not re-chip.
    app.toggle_tool_at(1);
    assert!(app.messages[1].tool_expanded);
    assert!(app.messages.iter().all(|m| m.tool_ungroup));
    assert!(!tool_view::streak_can_collapse(&app.messages, 0, 3));

    // Group header click: collapse back to chip.
    app.toggle_tool_group_at(0);
    assert!(app
        .messages
        .iter()
        .all(|m| !m.tool_ungroup && !m.tool_expanded));
    assert!(tool_view::streak_can_collapse(&app.messages, 0, 3));

    // Ctrl+O expand then collapse last group.
    app.toggle_last_tool_expand();
    assert!(app.messages.iter().all(|m| m.tool_ungroup));
    app.toggle_last_tool_expand();
    assert!(tool_view::streak_can_collapse(&app.messages, 0, 3));
}

#[test]
fn ctrl_l_model_select_respects_enabled_models_filter() {
    use crate::slash::ModelChoice;

    let mut app = App::new("test");
    app.set_model_catalog(vec![
        ModelChoice {
            provider: "openai".into(),
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
        },
        ModelChoice {
            provider: "mock".into(),
            id: "mock-v1".into(),
            name: "Mock".into(),
        },
        ModelChoice {
            provider: "xai".into(),
            id: "grok-4.5".into(),
            name: "Grok".into(),
        },
    ]);
    app.set_current_model("openai", "gpt-4o");
    app.set_enabled_models(Some(vec!["mock:mock-v1".into()]));

    app.open_model_select();
    let prompt = app.select_prompt().expect("model select open");
    let ids: Vec<_> = prompt.options.iter().map(|o| o.id.as_str()).collect();
    // Current model always visible + enabled mock.
    assert!(ids.contains(&"openai:gpt-4o"));
    assert!(ids.contains(&"mock:mock-v1"));
    assert!(!ids.contains(&"xai:grok-4.5"));
    assert_eq!(ids.len(), 2);
}

#[test]
fn provider_models_space_toggles_ctrl_l_visibility() {
    use crate::slash::ModelChoice;

    let mut app = App::new("test");
    app.set_model_catalog(vec![
        ModelChoice {
            provider: "openai".into(),
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
        },
        ModelChoice {
            provider: "openai".into(),
            id: "o3".into(),
            name: "o3".into(),
        },
    ]);
    app.set_settings_catalog(
        vec![("openai".into(), "2 models".into())],
        vec![
            ("openai:gpt-4o".into(), "GPT-4o".into()),
            ("openai:o3".into(), "o3".into()),
        ],
        vec![],
    );
    app.set_current_model("openai", "gpt-4o");
    app.open_settings_models_for_provider("openai");
    {
        let f = app.float.as_ref().expect("models float");
        assert_eq!(f.kind, FloatKind::SettingsModels);
        // Model rows start with [x] when no filter is set.
        let labels: Vec<_> = f
            .sections
            .iter()
            .flat_map(|s| s.items.iter())
            .filter(|i| i.id.starts_with("m:"))
            .map(|i| i.label.clone())
            .collect();
        assert!(labels.iter().all(|l| l.starts_with("[x]")), "{labels:?}");
    }

    // Focus first model (after fetch action row at index 0).
    app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
    // Space toggles Ctrl+L visibility — not search.
    match app.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE)) {
        RunOutcome::ConfigOp(ConfigOp::SettingSet { key, value }) => {
            assert_eq!(key, "enabled_models");
            assert_eq!(value, "openai:o3");
        }
        other => panic!("expected SettingSet from Space toggle, got {other:?}"),
    }
    assert!(
        app.float
            .as_ref()
            .map(|f| f.search.is_empty())
            .unwrap_or(true),
        "Space must not enter float search"
    );
    assert_eq!(
        app.enabled_models.as_ref().map(|v| v.as_slice()),
        Some(["openai:o3".to_string()].as_slice())
    );
}
