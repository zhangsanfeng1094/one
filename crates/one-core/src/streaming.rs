#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
}

pub fn emit_text_chunks(
    text: &str,
    chunk_size: usize,
    on_event: &mut dyn FnMut(StreamEvent),
    abort: Option<&std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    for chunk in text.as_bytes().chunks(chunk_size) {
        if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            break;
        }
        if let Ok(s) = std::str::from_utf8(chunk) {
            on_event(StreamEvent::TextDelta(s.to_string()));
        }
    }
}