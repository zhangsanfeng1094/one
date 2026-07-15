use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
}

/// Emit text as char-safe chunks (never splits multi-byte UTF-8).
///
/// Prefer [`emit_text_chunks_async`] in async providers so the TUI can paint
/// between chunks (typewriter). This sync helper is for tests / non-async paths.
pub fn emit_text_chunks(
    text: &str,
    chunk_chars: usize,
    on_event: &mut dyn FnMut(StreamEvent),
    abort: Option<&AtomicBool>,
) {
    let n = chunk_chars.max(1);
    let mut buf = String::with_capacity(n * 4);
    let mut count = 0usize;
    for ch in text.chars() {
        if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            break;
        }
        buf.push(ch);
        count += 1;
        if count >= n {
            on_event(StreamEvent::TextDelta(std::mem::take(&mut buf)));
            count = 0;
        }
    }
    if !buf.is_empty() {
        on_event(StreamEvent::TextDelta(buf));
    }
}

/// Like [`emit_text_chunks`] but yields between chunks so the TUI typewriter can paint.
pub async fn emit_text_chunks_async(
    text: &str,
    chunk_chars: usize,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
    abort: Option<&AtomicBool>,
    delay: Duration,
) {
    let n = chunk_chars.max(1);
    let mut buf = String::with_capacity(n * 4);
    let mut count = 0usize;
    for ch in text.chars() {
        if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            break;
        }
        buf.push(ch);
        count += 1;
        if count >= n {
            on_event(StreamEvent::TextDelta(std::mem::take(&mut buf)));
            count = 0;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
    }
    if !buf.is_empty() {
        on_event(StreamEvent::TextDelta(buf));
    }
}