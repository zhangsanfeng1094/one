//! Child-process I/O capture, aligned with Codex `codex-rs/core/src/exec.rs`.
//!
//! Pattern:
//! 1. Spawn with piped stdout/stderr (and a process group on Unix).
//! 2. Concurrently drain both pipes while waiting for exit (avoids pipe deadlock).
//! 3. Cap retained bytes per stream; keep reading past the cap so writers never block.
//! 4. On timeout: SIGTERM the process group, wait [`SIGTERM_GRACE`], then SIGKILL;
//!    then await readers with a short drain timeout (grandchildren may inherit
//!    fds and keep pipes open forever).

use std::io::Write;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// Hard cap on bytes retained from each of stdout / stderr.
///
/// Mirrors Codex `EXEC_OUTPUT_MAX_BYTES` / `DEFAULT_OUTPUT_BYTES_CAP` intent:
/// a runaway command must not OOM the agent. Presentation may further
/// truncate via [`crate::truncate::present_tool_output`].
pub const EXEC_OUTPUT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Default cap on the on-disk background log (stdout+stderr interleaved).
pub const DEFAULT_OUTPUT_FILE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Resolve the on-disk log cap (`ONE_BASH_OUTPUT_FILE_MAX_BYTES`, 0 = default).
pub fn output_file_max_bytes() -> u64 {
    std::env::var("ONE_BASH_OUTPUT_FILE_MAX_BYTES")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_OUTPUT_FILE_MAX_BYTES)
}

/// Shared sink so stdout and stderr readers can tee into one log file.
#[derive(Clone)]
pub struct OutputFileSink {
    file: Arc<Mutex<std::fs::File>>,
    written: Arc<AtomicU64>,
    max_bytes: u64,
}

impl OutputFileSink {
    pub fn new(file: std::fs::File, max_bytes: u64) -> Self {
        Self {
            file: Arc::new(Mutex::new(file)),
            written: Arc::new(AtomicU64::new(0)),
            max_bytes: max_bytes.max(1),
        }
    }

    pub fn write_bytes(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let prev = self.written.load(Ordering::Relaxed);
        if prev >= self.max_bytes {
            return;
        }
        let room = (self.max_bytes - prev) as usize;
        let take = bytes.len().min(room);
        if take == 0 {
            return;
        }
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(&bytes[..take]);
        }
        self.written.fetch_add(take as u64, Ordering::Relaxed);
    }

    pub fn flush(&self) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
    }
}

/// After the child exits (or is killed), how long to wait for pipe readers.
///
/// Codex uses 2s (`IO_DRAIN_TIMEOUT_MS`). Grandchildren that inherited the
/// child's stdout/stderr can keep pipes open after we kill the direct child;
/// without this bound, reader tasks block on `read()` forever.
pub const IO_DRAIN_TIMEOUT: Duration = Duration::from_millis(2_000);

/// SIGTERM → SIGKILL grace (Grok / common harness). Gives `npm`/`cargo` a
/// chance to reap children and flush before the group is killed.
pub const SIGTERM_GRACE: Duration = Duration::from_secs(1);

const READ_CHUNK_SIZE: usize = 8_192;

/// Captured result of a shell-like exec.
#[derive(Debug)]
pub struct CapturedOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Apply Codex-style stdio + process-group settings for a shell tool spawn.
pub fn configure_shell_stdio(cmd: &mut Command) {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Own process group so timeout/kill can reap pipelines and descendants.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
}

/// Consume a spawned child: concurrent pipe drain + optional timeout.
///
/// `timeout_secs = None` waits indefinitely (background tasks without a hard limit).
/// `max_bytes = None` retains full stream content (trusted internal helpers).
pub async fn consume_child(
    mut child: Child,
    timeout_secs: Option<u64>,
    max_bytes: Option<usize>,
) -> std::io::Result<CapturedOutput> {
    let stdout_reader = child.stdout.take();
    let stderr_reader = child.stderr.take();

    let stdout_handle = tokio::spawn(async move {
        match stdout_reader {
            Some(out) => read_pipe_capped(out, max_bytes).await,
            None => Ok(String::new()),
        }
    });
    let stderr_handle = tokio::spawn(async move {
        match stderr_reader {
            Some(err) => read_pipe_capped(err, max_bytes).await,
            None => Ok(String::new()),
        }
    });

    let (status, timed_out) = match timeout_secs {
        Some(secs) => {
            match timeout(Duration::from_secs(secs), child.wait()).await {
                Ok(Ok(status)) => (status, false),
                Ok(Err(err)) => {
                    // Child wait failed; still try to reclaim readers.
                    let (stdout, stderr) =
                        join_readers(stdout_handle, stderr_handle, IO_DRAIN_TIMEOUT).await;
                    let _ = (stdout, stderr);
                    return Err(err);
                }
                Err(_elapsed) => {
                    let status = graceful_kill_child(&mut child)
                        .await
                        .unwrap_or_else(synthetic_killed_status);
                    (status, true)
                }
            }
        }
        None => match child.wait().await {
            Ok(status) => (status, false),
            Err(err) => {
                let _ = join_readers(stdout_handle, stderr_handle, IO_DRAIN_TIMEOUT).await;
                return Err(err);
            }
        },
    };

    // Child is reaped (or killed). Await pipe readers with a drain timeout so
    // open-fd grandchildren cannot hang the agent forever.
    let (stdout, stderr) = join_readers(stdout_handle, stderr_handle, IO_DRAIN_TIMEOUT).await;

    Ok(CapturedOutput {
        status,
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
        timed_out,
    })
}

/// SIGTERM the child's process group (Unix) then signal the direct child.
pub fn term_child_process_group(child: &mut Child) {
    if let Some(pid) = child.id() {
        term_process_group(pid);
    }
}

/// Kill the child's process group (Unix, SIGKILL) then signal the direct child.
pub fn kill_child_process_group(child: &mut Child) {
    if let Some(pid) = child.id() {
        kill_process_group(pid);
    }
    let _ = child.start_kill();
}

/// SIGTERM a process group by leader pid (negative pid on Unix).
pub fn term_process_group(pid: u32) {
    signal_process_group(pid, 15);
}

/// SIGKILL a process group by leader pid (negative pid on Unix).
pub fn kill_process_group(pid: u32) {
    signal_process_group(pid, 9);
}

fn signal_process_group(pid: u32, sig: i32) {
    #[cfg(unix)]
    {
        unsafe {
            extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            let _ = kill(-(pid as i32), sig);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, sig);
    }
}

/// SIGTERM, wait [`SIGTERM_GRACE`], then SIGKILL. Returns the exit status when
/// the child was reaped here (so the caller must not `wait` again).
pub async fn graceful_kill_child(child: &mut Child) -> Option<ExitStatus> {
    #[cfg(unix)]
    {
        term_child_process_group(child);
        match timeout(SIGTERM_GRACE, child.wait()).await {
            Ok(Ok(status)) => return Some(status),
            Ok(Err(_)) => return None,
            Err(_) => {}
        }
    }
    kill_child_process_group(child);
    match timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => None,
    }
}

/// Read a pipe until EOF, retaining at most `max_bytes` (if set).
///
/// Bytes beyond the cap are discarded but still consumed so the writer cannot
/// stall on a full OS pipe buffer (Codex: "Continue reading to EOF to avoid
/// back-pressure").
pub async fn read_pipe_capped<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    max_bytes: Option<usize>,
) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_SIZE];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        match max_bytes {
            Some(max) if buf.len() >= max => {
                truncated = true;
            }
            Some(max) => {
                let room = max - buf.len();
                let take = n.min(room);
                buf.extend_from_slice(&chunk[..take]);
                if take < n {
                    truncated = true;
                }
            }
            None => {
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        text.push_str("\n...[stream truncated at capture limit]...\n");
    }
    Ok(text)
}

const STREAM_TRUNCATION_MARKER: &str = "\n...[stream truncated at capture limit]...\n";

/// Stream a pipe into a shared buffer as chunks arrive (Codex mid-run snapshot style).
///
/// Unlike [`read_pipe_capped`], each chunk is appended immediately so
/// `bash_output` can observe partial stdout/stderr while the child is still
/// running. Bytes past `max_bytes` are still drained (no pipe deadlock) but not
/// retained; a single truncation marker is written once when the cap is hit.
///
/// Incomplete multi-byte UTF-8 sequences at a chunk boundary are carried into
/// the next read (not treated as the capture cap). Only a true size limit marks
/// truncation and drops further content.
pub async fn stream_pipe_into<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    target: Arc<Mutex<String>>,
    max_bytes: Option<usize>,
) -> std::io::Result<()> {
    stream_pipe_into_tee(reader, target, None, max_bytes).await
}

/// Like [`stream_pipe_into`], also copying raw bytes to an optional on-disk log.
///
/// The file sink is not bound by `max_bytes` (memory cap); it uses its own cap.
/// Bytes past the memory cap are still written to disk until the file cap.
pub async fn stream_pipe_into_tee<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    target: Arc<Mutex<String>>,
    file: Option<OutputFileSink>,
    max_bytes: Option<usize>,
) -> std::io::Result<()> {
    let mut chunk = [0u8; READ_CHUNK_SIZE];
    let mut pending: Vec<u8> = Vec::new();
    let mut retained = 0usize;
    let mut marked_truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            // EOF: flush any incomplete trailing bytes lossily (if still under cap).
            if !pending.is_empty() && !marked_truncated {
                let piece = String::from_utf8_lossy(&pending);
                push_stream_text(
                    &target,
                    &piece,
                    &mut retained,
                    max_bytes,
                    &mut marked_truncated,
                );
                pending.clear();
            }
            if let Some(ref f) = file {
                f.flush();
            }
            break;
        }
        if let Some(ref f) = file {
            f.write_bytes(&chunk[..n]);
        }
        if marked_truncated {
            // Cap already hit — drain (and tee to disk) so the writer never blocks.
            continue;
        }
        pending.extend_from_slice(&chunk[..n]);
        decode_pending_into_target(
            &mut pending,
            &target,
            &mut retained,
            max_bytes,
            &mut marked_truncated,
        );
    }
    Ok(())
}

/// Decode complete UTF-8 from `pending` into `target`, leaving an incomplete
/// trailing sequence (if any) in `pending` for the next chunk.
fn decode_pending_into_target(
    pending: &mut Vec<u8>,
    target: &Arc<Mutex<String>>,
    retained: &mut usize,
    max_bytes: Option<usize>,
    marked_truncated: &mut bool,
) {
    loop {
        if *marked_truncated {
            pending.clear();
            return;
        }
        if pending.is_empty() {
            return;
        }
        match std::str::from_utf8(pending) {
            Ok(s) => {
                push_stream_text(target, s, retained, max_bytes, marked_truncated);
                pending.clear();
                return;
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                if valid_up_to > 0 {
                    // SAFETY: valid_up_to is a UTF-8 boundary reported by from_utf8.
                    let s = std::str::from_utf8(&pending[..valid_up_to]).unwrap_or("");
                    push_stream_text(target, s, retained, max_bytes, marked_truncated);
                    pending.drain(..valid_up_to);
                    continue;
                }
                match e.error_len() {
                    // Incomplete multi-byte sequence — wait for more bytes.
                    None => return,
                    // Invalid sequence — skip it (U+FFFD) and continue.
                    Some(len) => {
                        push_stream_text(target, "\u{FFFD}", retained, max_bytes, marked_truncated);
                        let skip = len.min(pending.len());
                        pending.drain(..skip);
                    }
                }
            }
        }
    }
}

fn push_stream_text(
    target: &Arc<Mutex<String>>,
    s: &str,
    retained: &mut usize,
    max_bytes: Option<usize>,
    marked_truncated: &mut bool,
) {
    if s.is_empty() || *marked_truncated {
        return;
    }
    match max_bytes {
        Some(max) if *retained >= max => {
            append_truncation_marker(target);
            *marked_truncated = true;
        }
        Some(max) => {
            let room = max - *retained;
            if s.len() <= room {
                if let Ok(mut g) = target.lock() {
                    g.push_str(s);
                }
                *retained += s.len();
            } else {
                let mut take = room;
                while take > 0 && !s.is_char_boundary(take) {
                    take -= 1;
                }
                if take > 0 {
                    if let Ok(mut g) = target.lock() {
                        g.push_str(&s[..take]);
                    }
                    *retained += take;
                }
                append_truncation_marker(target);
                *marked_truncated = true;
                *retained = max;
            }
        }
        None => {
            if let Ok(mut g) = target.lock() {
                g.push_str(s);
            }
            *retained = retained.saturating_add(s.len());
        }
    }
}

fn append_truncation_marker(target: &Arc<Mutex<String>>) {
    if let Ok(mut g) = target.lock() {
        if !g.contains("[stream truncated at capture limit]") {
            g.push_str(STREAM_TRUNCATION_MARKER);
        }
    }
}

async fn join_readers(
    mut stdout_handle: JoinHandle<std::io::Result<String>>,
    mut stderr_handle: JoinHandle<std::io::Result<String>>,
    drain: Duration,
) -> (Option<String>, Option<String>) {
    let stdout = await_reader(&mut stdout_handle, drain).await;
    let stderr = await_reader(&mut stderr_handle, drain).await;
    (stdout, stderr)
}

async fn await_reader(
    handle: &mut JoinHandle<std::io::Result<String>>,
    drain: Duration,
) -> Option<String> {
    match timeout(drain, &mut *handle).await {
        Ok(Ok(Ok(text))) => Some(text),
        Ok(Ok(Err(_))) => Some(String::new()),
        Ok(Err(_join)) => Some(String::new()),
        Err(_elapsed) => {
            // Timeout: abort the task to avoid hanging on open pipes (Codex).
            handle.abort();
            Some(String::new())
        }
    }
}

#[cfg(unix)]
fn synthetic_killed_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    // 128 + SIGKILL
    ExitStatus::from_raw(128 + 9)
}

#[cfg(not(unix))]
fn synthetic_killed_status() -> ExitStatus {
    // Best-effort: no portable constructor; callers only use this if wait failed.
    std::process::Command::new("false")
        .status()
        .unwrap_or_else(|_| panic!("false"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn read_pipe_capped_drains_past_limit() {
        let (client, mut server) = tokio::io::duplex(1024);
        let writer = tokio::spawn(async move {
            let payload = vec![b'a'; 64 * 1024];
            server.write_all(&payload).await.unwrap();
            server.shutdown().await.unwrap();
        });
        let text = read_pipe_capped(client, Some(1024)).await.unwrap();
        writer.await.unwrap();
        assert!(text.starts_with('a'));
        assert!(
            text.contains("[stream truncated at capture limit]"),
            "expected truncation marker, got len={}",
            text.len()
        );
        let head = text
            .split("\n...[stream truncated at capture limit]...\n")
            .next()
            .unwrap();
        assert_eq!(head.len(), 1024);
    }

    #[tokio::test]
    async fn stream_pipe_into_is_visible_before_eof() {
        let (client, mut server) = tokio::io::duplex(1024);
        let target = Arc::new(Mutex::new(String::new()));
        let target2 = target.clone();
        let reader = tokio::spawn(async move {
            stream_pipe_into(client, target2, Some(EXEC_OUTPUT_MAX_BYTES))
                .await
                .unwrap();
        });
        server.write_all(b"hello-mid-run\n").await.unwrap();
        server.flush().await.unwrap();
        // Give the reader a chance to append before we close the pipe.
        for _ in 0..50 {
            if target
                .lock()
                .map(|g| g.contains("hello-mid-run"))
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            target.lock().unwrap().contains("hello-mid-run"),
            "mid-run snapshot must see streamed bytes before EOF"
        );
        server.shutdown().await.unwrap();
        reader.await.unwrap();
    }

    /// Multi-byte UTF-8 split across reads must not trip the capture-cap path.
    #[tokio::test]
    async fn stream_pipe_into_carries_split_utf8() {
        let (client, mut server) = tokio::io::duplex(64);
        let target = Arc::new(Mutex::new(String::new()));
        let target2 = target.clone();
        let reader = tokio::spawn(async move {
            stream_pipe_into(client, target2, Some(EXEC_OUTPUT_MAX_BYTES))
                .await
                .unwrap();
        });
        // "你好" is e4 bd a0 e5 a5 bd — split after 2 bytes of the first char.
        server.write_all(&[0xe4, 0xbd]).await.unwrap();
        server.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        server.write_all(&[0xa0, 0xe5, 0xa5, 0xbd]).await.unwrap();
        server.write_all(b" ok").await.unwrap();
        server.shutdown().await.unwrap();
        reader.await.unwrap();
        let text = target.lock().unwrap().clone();
        assert_eq!(text, "你好 ok", "got {text:?}");
        assert!(
            !text.contains("stream truncated"),
            "split UTF-8 must not mark truncation"
        );
    }

    #[tokio::test]
    async fn stream_pipe_into_cap_still_truncates() {
        let (client, mut server) = tokio::io::duplex(256);
        let target = Arc::new(Mutex::new(String::new()));
        let target2 = target.clone();
        let reader = tokio::spawn(async move {
            stream_pipe_into(client, target2, Some(16)).await.unwrap();
        });
        server
            .write_all(b"abcdefghijklmnopqrstuvwxyz")
            .await
            .unwrap();
        server.shutdown().await.unwrap();
        reader.await.unwrap();
        let text = target.lock().unwrap().clone();
        assert!(
            text.contains("[stream truncated at capture limit]"),
            "got {text:?}"
        );
        let head = text
            .split("\n...[stream truncated at capture limit]...\n")
            .next()
            .unwrap();
        assert_eq!(head.len(), 16);
    }

    #[tokio::test]
    async fn consume_child_large_stdout_no_deadlock() {
        let mut cmd = Command::new("python3");
        cmd.args(["-c", "print('x' * (512 * 1024), end='')"]);
        configure_shell_stdio(&mut cmd);
        cmd.kill_on_drop(true);
        let child = cmd.spawn().expect("spawn");
        let out = consume_child(child, Some(30), Some(EXEC_OUTPUT_MAX_BYTES))
            .await
            .expect("consume");
        assert!(!out.timed_out);
        assert!(out.status.success());
        assert!(out.stdout.contains('x'));
    }

    #[tokio::test]
    async fn consume_child_timeout_kills_and_returns() {
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        configure_shell_stdio(&mut cmd);
        cmd.kill_on_drop(true);
        let child = cmd.spawn().expect("spawn");
        let started = std::time::Instant::now();
        let out = consume_child(child, Some(1), Some(EXEC_OUTPUT_MAX_BYTES))
            .await
            .expect("consume");
        assert!(out.timed_out, "expected timeout");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timeout path should not hang on IO drain"
        );
    }
}
