use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often cooperative abort flags are polled while awaiting network/tools.
/// ~50ms keeps Esc interrupt feeling instant without spinning the CPU.
pub const ABORT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
}

/// Completes when `abort` is set. If `abort` is `None`, waits forever so it can
/// be used in `tokio::select!` without a separate code path.
///
/// Streaming used to check the flag only *after* each SSE chunk / tool step.
/// Between slow tokens (or while waiting on first byte / bash), Esc could hang
/// for a long time. Race long awaits against this future instead.
pub async fn wait_until_aborted(abort: Option<&AtomicBool>) {
    let Some(flag) = abort else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if flag.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(ABORT_POLL_INTERVAL).await;
    }
}

/// Run `fut`, cancelling cooperatively when `abort` is set.
///
/// Dropping `fut` on abort relies on cancel-safe cleanup (e.g. reqwest request
/// cancel-on-drop, `tokio::process::Command` with `kill_on_drop(true)`).
pub async fn race_abort<T>(
    fut: impl std::future::Future<Output = T>,
    abort: Option<&AtomicBool>,
) -> Result<T, ()> {
    tokio::select! {
        biased;
        result = fut => Ok(result),
        _ = wait_until_aborted(abort) => Err(()),
    }
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
                // Sleep is interruptible — otherwise typewriter delay blocks Esc.
                match race_abort(tokio::time::sleep(delay), abort).await {
                    Ok(()) => {}
                    Err(()) => break,
                }
            } else {
                tokio::task::yield_now().await;
            }
        }
    }
    if !buf.is_empty() && !abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        on_event(StreamEvent::TextDelta(buf));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    #[tokio::test]
    async fn race_abort_cancels_pending_future() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = flag.clone();
        let task = tokio::spawn(async move {
            race_abort(std::future::pending::<()>(), Some(flag2.as_ref())).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let t0 = Instant::now();
        flag.store(true, Ordering::Relaxed);
        let res = tokio::time::timeout(Duration::from_millis(200), task)
            .await
            .expect("should finish soon")
            .expect("join");
        assert!(res.is_err());
        assert!(t0.elapsed() < Duration::from_millis(150));
    }
}