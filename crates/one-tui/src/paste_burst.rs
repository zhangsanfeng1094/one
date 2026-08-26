//! Coalesce a drain of terminal events into paste blocks.
//!
//! Without this, a large clipboard paste that the host delivers as individual
//! `Key` events (no bracketed-paste, or a host that splits it) is applied
//! character-by-character with a full TUI redraw between each one. That is
//! both slow and ugly: the draft fans out line-by-line instead of landing as
//! a single `[文本.txt]` chip.
//!
//! Enter / Tab are *not* always burst chars. A typed command plus Enter often
//! lands in the same drain (`hi` + Enter); swallowing that Enter as `\n` would
//! insert a newline instead of submitting. They only join a paste when more
//! printable keys follow (unbracketed multi-line dump) or we are already in a
//! bracketed `Event::Paste`.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};

/// One item after merging consecutive paste / printable-key bursts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoalescedEvent {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Other,
}

/// True when `events` already looks like a clipboard dump, so the reader
/// should wait a beat for stragglers before painting.
pub(crate) fn looks_like_paste_burst(events: &[Event]) -> bool {
    let mut keys = 0usize;
    for ev in events {
        match ev {
            Event::Paste(_) => return true,
            Event::Key(key) if burst_printable(key).is_some() || enter_or_tab(key).is_some() => {
                keys += 1;
                if keys >= 4 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Merge consecutive bracketed pastes and unbracketed printable keys.
///
/// A lone typed character stays a `Key` so welcome `1`–`3`, `j`/`k` browse,
/// and `/` still hit [`App::handle_key`]. Anything longer — or any
/// `Event::Paste` — becomes one `Paste` string for [`App::handle_paste`].
pub(crate) fn coalesce_events(events: Vec<Event>) -> Vec<CoalescedEvent> {
    let mut out = Vec::with_capacity(events.len().min(32));
    let mut buf = String::new();
    let mut from_bracketed = false;
    let mut first_key: Option<KeyEvent> = None;
    let mut key_chars = 0usize;

    let flush = |out: &mut Vec<CoalescedEvent>,
                 buf: &mut String,
                 from_bracketed: &mut bool,
                 first_key: &mut Option<KeyEvent>,
                 key_chars: &mut usize| {
        if buf.is_empty() && !*from_bracketed {
            *first_key = None;
            *key_chars = 0;
            return;
        }
        // A lone typed char stays a Key so welcome digits / j/k / `/` still
        // hit handle_key. Two or more burst keys (or any bracketed paste)
        // become one block.
        let as_paste = *from_bracketed || *key_chars > 1;
        if as_paste {
            out.push(CoalescedEvent::Paste(std::mem::take(buf)));
        } else if let Some(key) = first_key.take() {
            out.push(CoalescedEvent::Key(key));
            buf.clear();
        } else if !buf.is_empty() {
            out.push(CoalescedEvent::Paste(std::mem::take(buf)));
        }
        *from_bracketed = false;
        *first_key = None;
        *key_chars = 0;
    };

    for i in 0..events.len() {
        match &events[i] {
            Event::Paste(text) => {
                from_bracketed = true;
                buf.push_str(text);
            }
            Event::Key(key) => {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if let Some(ch) = burst_printable(key) {
                    if first_key.is_none() {
                        first_key = Some(*key);
                    }
                    buf.push(ch);
                    key_chars += 1;
                } else if let Some(ch) = enter_or_tab(key) {
                    // Join the paste only when this is clearly clipboard text:
                    // already in a bracketed paste, more printable keys follow
                    // (unbracketed multi-line dump), or we already absorbed a
                    // newline/tab so a trailing terminator is clipboard too.
                    let join = from_bracketed
                        || rest_continues_paste(&events[i + 1..])
                        || buf.contains('\n')
                        || (ch == '\t' && buf.contains('\t'));
                    if join {
                        if first_key.is_none() {
                            first_key = Some(*key);
                        }
                        buf.push(ch);
                        key_chars += 1;
                    } else {
                        flush(
                            &mut out,
                            &mut buf,
                            &mut from_bracketed,
                            &mut first_key,
                            &mut key_chars,
                        );
                        out.push(CoalescedEvent::Key(*key));
                    }
                } else {
                    flush(
                        &mut out,
                        &mut buf,
                        &mut from_bracketed,
                        &mut first_key,
                        &mut key_chars,
                    );
                    out.push(CoalescedEvent::Key(*key));
                }
            }
            Event::Mouse(mouse) => {
                flush(
                    &mut out,
                    &mut buf,
                    &mut from_bracketed,
                    &mut first_key,
                    &mut key_chars,
                );
                out.push(CoalescedEvent::Mouse(*mouse));
            }
            Event::Resize(w, h) => {
                flush(
                    &mut out,
                    &mut buf,
                    &mut from_bracketed,
                    &mut first_key,
                    &mut key_chars,
                );
                out.push(CoalescedEvent::Resize(*w, *h));
            }
            _ => {
                flush(
                    &mut out,
                    &mut buf,
                    &mut from_bracketed,
                    &mut first_key,
                    &mut key_chars,
                );
                out.push(CoalescedEvent::Other);
            }
        }
    }
    flush(
        &mut out,
        &mut buf,
        &mut from_bracketed,
        &mut first_key,
        &mut key_chars,
    );
    out
}

/// Printable char that can join an unbracketed paste burst. Enter / Tab are
/// handled separately so a typed command + Enter still submits.
fn burst_printable(key: &KeyEvent) -> Option<char> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) if c != '\n' && c != '\t' && c != '\r' && !c.is_control() => Some(c),
        _ => None,
    }
}

fn enter_or_tab(key: &KeyEvent) -> Option<char> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return None;
    }
    match key.code {
        KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => Some('\n'),
        KeyCode::Tab | KeyCode::Char('\t') => Some('\t'),
        _ => None,
    }
}

/// True when more paste body follows (printable keys or another bracketed
/// paste). Consecutive Enter / Tab are skipped so `\n\nfoo` still joins.
fn rest_continues_paste(rest: &[Event]) -> bool {
    for ev in rest {
        match ev {
            Event::Paste(text) if !text.is_empty() => return true,
            Event::Key(key) if key.kind == KeyEventKind::Release => continue,
            Event::Key(key) if burst_printable(key).is_some() => return true,
            Event::Key(key) if enter_or_tab(key).is_some() => continue,
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn press_mod(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    #[test]
    fn single_typed_char_stays_a_key() {
        let out = coalesce_events(vec![press(KeyCode::Char('a'))]);
        assert!(matches!(
            &out[..],
            [CoalescedEvent::Key(k)] if k.code == KeyCode::Char('a')
        ));
    }

    #[test]
    fn many_chars_become_one_paste() {
        let events: Vec<Event> = "hello".chars().map(|c| press(KeyCode::Char(c))).collect();
        let out = coalesce_events(events);
        assert_eq!(out, vec![CoalescedEvent::Paste("hello".into())]);
    }

    #[test]
    fn enter_in_a_burst_is_a_newline_not_submit() {
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
        ];
        let out = coalesce_events(events);
        assert_eq!(out, vec![CoalescedEvent::Paste("a\nb".into())]);
    }

    #[test]
    fn consecutive_bracketed_pastes_merge() {
        let out = coalesce_events(vec![
            Event::Paste("foo\n".into()),
            Event::Paste("bar".into()),
        ]);
        assert_eq!(out, vec![CoalescedEvent::Paste("foo\nbar".into())]);
    }

    #[test]
    fn empty_bracketed_paste_is_kept() {
        let out = coalesce_events(vec![Event::Paste(String::new())]);
        assert_eq!(out, vec![CoalescedEvent::Paste(String::new())]);
    }

    #[test]
    fn ctrl_c_does_not_join_a_burst() {
        let out = coalesce_events(vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL),
            press(KeyCode::Char('d')),
        ]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], CoalescedEvent::Paste("ab".into()));
        assert!(matches!(
            &out[1],
            CoalescedEvent::Key(k)
                if k.code == KeyCode::Char('c')
                    && k.modifiers.contains(KeyModifiers::CONTROL)
        ));
        assert!(matches!(
            &out[2],
            CoalescedEvent::Key(k) if k.code == KeyCode::Char('d')
        ));
    }

    #[test]
    fn lone_enter_stays_a_key() {
        let out = coalesce_events(vec![press(KeyCode::Enter)]);
        assert!(
            matches!(
                &out[..],
                [CoalescedEvent::Key(k)] if k.code == KeyCode::Enter
            ),
            "lone Enter must submit, not become a newline paste: {out:?}"
        );
    }

    #[test]
    fn lone_tab_stays_a_key() {
        let out = coalesce_events(vec![press(KeyCode::Tab)]);
        assert!(matches!(
            &out[..],
            [CoalescedEvent::Key(k)] if k.code == KeyCode::Tab
        ));
    }

    #[test]
    fn trailing_enter_after_typed_chars_submits() {
        let out = coalesce_events(vec![
            press(KeyCode::Char('h')),
            press(KeyCode::Char('i')),
            press(KeyCode::Enter),
        ]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0], CoalescedEvent::Paste("hi".into()));
        assert!(
            matches!(&out[1], CoalescedEvent::Key(k) if k.code == KeyCode::Enter),
            "typed command + Enter must submit, not insert a newline: {out:?}"
        );
    }

    #[test]
    fn last_char_then_enter_submits() {
        let out = coalesce_events(vec![press(KeyCode::Char('x')), press(KeyCode::Enter)]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(matches!(
            &out[0],
            CoalescedEvent::Key(k) if k.code == KeyCode::Char('x')
        ));
        assert!(matches!(
            &out[1],
            CoalescedEvent::Key(k) if k.code == KeyCode::Enter
        ));
    }

    #[test]
    fn trailing_tab_after_chars_stays_completion() {
        let out = coalesce_events(vec![
            press(KeyCode::Char('s')),
            press(KeyCode::Char('r')),
            press(KeyCode::Tab),
        ]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0], CoalescedEvent::Paste("sr".into()));
        assert!(matches!(
            &out[1],
            CoalescedEvent::Key(k) if k.code == KeyCode::Tab
        ));
    }

    #[test]
    fn trailing_enter_on_multiline_paste_stays_in_block() {
        let out = coalesce_events(vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
            press(KeyCode::Enter),
        ]);
        assert_eq!(out, vec![CoalescedEvent::Paste("a\nb\n".into())]);
    }

    #[test]
    fn bracketed_paste_keeps_trailing_newline() {
        let out = coalesce_events(vec![Event::Paste("hello\n".into())]);
        assert_eq!(out, vec![CoalescedEvent::Paste("hello\n".into())]);
    }

    #[test]
    fn shift_enter_trailing_stays_a_key() {
        let out = coalesce_events(vec![
            press(KeyCode::Char('a')),
            press_mod(KeyCode::Enter, KeyModifiers::SHIFT),
        ]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(matches!(
            &out[0],
            CoalescedEvent::Key(k) if k.code == KeyCode::Char('a')
        ));
        assert!(matches!(
            &out[1],
            CoalescedEvent::Key(k)
                if k.code == KeyCode::Enter && k.modifiers.contains(KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn cr_in_burst_becomes_newline() {
        let out = coalesce_events(vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('\r')),
            press(KeyCode::Char('b')),
        ]);
        assert_eq!(out, vec![CoalescedEvent::Paste("a\nb".into())]);
    }

    #[test]
    fn looks_like_burst_for_paste_or_many_keys() {
        assert!(looks_like_paste_burst(&[Event::Paste("x".into())]));
        assert!(!looks_like_paste_burst(&[press(KeyCode::Char('a'))]));
        let four: Vec<Event> = (0..4).map(|_| press(KeyCode::Char('x'))).collect();
        assert!(looks_like_paste_burst(&four));
    }
}
