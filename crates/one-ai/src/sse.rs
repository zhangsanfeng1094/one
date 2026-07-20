//! SSE reader with **correct streaming UTF-8** decoding and **fast abort**.
//!
//! TCP/HTTP chunks may split a multi-byte character (e.g. Chinese) across
//! boundaries. Using `String::from_utf8_lossy` on each chunk permanently
//! replaces incomplete sequences with U+FFFD (`�`) — that was the TUI 乱码.
//! We keep a pending byte buffer until a complete UTF-8 sequence is available.
//!
//! Abort is raced against every network wait (HTTP send + each SSE chunk).
//! Checking the flag only *after* `stream.next().await` made Esc feel stuck
//! whenever the model paused between tokens or TTFT was slow.

use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use one_core::error::{OneError, Result};
use one_core::streaming::wait_until_aborted;
use reqwest::Response;

pub fn parse_sse_data_lines(buffer: &str) -> Vec<&str> {
    buffer
        .lines()
        .filter_map(|line| line.strip_prefix("data: ").filter(|data| *data != "[DONE]"))
        .collect()
}

/// Append `chunk` into `pending` bytes, decode as much valid UTF-8 as possible
/// into `out`, leave incomplete trailing bytes in `pending`.
pub fn push_utf8_chunk(pending: &mut Vec<u8>, out: &mut String, chunk: &[u8]) {
    pending.extend_from_slice(chunk);
    loop {
        match std::str::from_utf8(pending) {
            Ok(s) => {
                out.push_str(s);
                pending.clear();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    // SAFETY: valid_up_to guarantees this prefix is valid UTF-8.
                    let s = std::str::from_utf8(&pending[..valid]).unwrap();
                    out.push_str(s);
                    pending.drain(..valid);
                    continue;
                }
                // valid == 0: either incomplete sequence at start, or invalid byte.
                match e.error_len() {
                    None => {
                        // Incomplete multi-byte sequence — wait for more bytes.
                        break;
                    }
                    Some(len) => {
                        // Truly invalid — skip and insert replacement (rare for SSE).
                        out.push('\u{FFFD}');
                        let skip = len.min(pending.len()).max(1);
                        pending.drain(..skip);
                        continue;
                    }
                }
            }
        }
    }
}

/// HTTP `send()` that returns within ~[`one_core::streaming::ABORT_POLL_INTERVAL`]
/// of Esc / abort, instead of waiting for headers / first body byte.
pub async fn send_with_abort(
    request: reqwest::RequestBuilder,
    abort: Option<&AtomicBool>,
) -> Result<Response> {
    if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err(OneError::Aborted);
    }
    tokio::select! {
        biased;
        result = request.send() => {
            result.map_err(|e| OneError::Provider(e.to_string()))
        }
        _ = wait_until_aborted(abort) => Err(OneError::Aborted),
    }
}

/// Read an SSE HTTP response, invoking `on_data` for each `data:` payload line.
///
/// Returns [`OneError::Aborted`] promptly when the abort flag is set, even if
/// the upstream is idle between tokens (drops the stream to cancel the socket).
pub async fn read_sse_response(
    response: Response,
    on_data: &mut (dyn FnMut(&str) + Send),
    abort: Option<&AtomicBool>,
) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut pending_utf8: Vec<u8> = Vec::new();
    let mut buffer = String::new();

    loop {
        if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(OneError::Aborted);
        }

        let next = tokio::select! {
            biased;
            chunk = stream.next() => chunk,
            _ = wait_until_aborted(abort) => {
                return Err(OneError::Aborted);
            }
        };

        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|e| OneError::Provider(e.to_string()))?;
        push_utf8_chunk(&mut pending_utf8, &mut buffer, &chunk);

        while let Some(pos) = buffer.find("\n\n") {
            if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return Err(OneError::Aborted);
            }
            let block = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();
            for line in block.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        return Ok(());
                    }
                    on_data(data);
                }
            }
        }
    }

    // Flush any remaining pending incomplete bytes as replacement (shouldn't happen
    // on a clean stream end).
    if !pending_utf8.is_empty() {
        if let Ok(s) = std::str::from_utf8(&pending_utf8) {
            buffer.push_str(s);
        } else {
            buffer.push('\u{FFFD}');
        }
        pending_utf8.clear();
    }

    if !buffer.trim().is_empty() {
        for line in buffer.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data != "[DONE]" {
                    on_data(data);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn chinese_split_across_chunks_not_corrupted() {
        // "你好" = E4 BD A0 E5 A5 BD
        let nihao = "你好".as_bytes();
        assert_eq!(nihao.len(), 6);

        let mut pending = Vec::new();
        let mut out = String::new();

        // Split mid-character: first 2 bytes of 你, rest later.
        push_utf8_chunk(&mut pending, &mut out, &nihao[..2]);
        assert!(out.is_empty(), "incomplete char must not flush yet");
        assert!(!pending.is_empty());

        push_utf8_chunk(&mut pending, &mut out, &nihao[2..]);
        assert_eq!(out, "你好");
        assert!(pending.is_empty());
    }

    #[test]
    fn no_fffd_for_split_cjk() {
        let text = "简单来说会话持久化树形代码全托";
        let bytes = text.as_bytes();
        let mut pending = Vec::new();
        let mut out = String::new();
        // Feed one byte at a time — worst case for streaming.
        for b in bytes {
            push_utf8_chunk(&mut pending, &mut out, &[*b]);
        }
        assert_eq!(out, text);
        assert!(!out.contains('\u{FFFD}'));
    }

    #[test]
    fn ascii_unaffected() {
        let mut pending = Vec::new();
        let mut out = String::new();
        push_utf8_chunk(&mut pending, &mut out, b"hello ");
        push_utf8_chunk(&mut pending, &mut out, b"world");
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn wait_until_aborted_returns_quickly() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = flag.clone();
        let waiter = tokio::spawn(async move {
            wait_until_aborted(Some(flag2.as_ref())).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        flag.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("abort wait should finish within poll interval")
            .expect("join");
    }
}
