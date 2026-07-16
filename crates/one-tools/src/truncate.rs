//! Shared truncation for tool outputs.
//!
//! **Inline cap (Pi-style):** line limit (2000) and byte limit (50 KiB) —
//! whichever is hit first. Prefer complete lines.
//!
//! **Spill (Claude Code-style):** when output exceeds the inline cap, the
//! full content is written under `~/.one/agent/tool-outputs/` and the model
//! only receives a short **head preview** plus the absolute path so it can
//! `read` / `grep` the rest.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default max lines in a tool result shown to the model.
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Default max UTF-8 bytes in a tool result shown to the model.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Max characters per grep match line (Pi: 500).
pub const GREP_MAX_LINE_LENGTH: usize = 500;
/// Claude Code default bash output length (chars) before spill; overridable via
/// `BASH_MAX_OUTPUT_LENGTH` / `ONE_BASH_MAX_OUTPUT_LENGTH`.
pub const DEFAULT_BASH_MAX_OUTPUT_CHARS: usize = 30_000;
/// Head preview size when spilling full output to disk.
pub const DEFAULT_SPILL_PREVIEW_CHARS: usize = 4_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<&'static str>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl TruncationResult {
    /// Notice line for the model / user when content was cut.
    pub fn notice(&self) -> Option<String> {
        if !self.truncated {
            return None;
        }
        let by = self.truncated_by.unwrap_or("limit");
        Some(format!(
            "[truncated by {by}: showing {} lines / {} of {} lines / {}; limits {} lines / {}]",
            self.output_lines,
            format_size(self.output_bytes),
            self.total_lines,
            format_size(self.total_bytes),
            self.max_lines,
            format_size(self.max_bytes),
        ))
    }

    /// Append notice under content when truncated.
    pub fn with_notice(self) -> String {
        match self.notice() {
            Some(n) if self.content.is_empty() => n,
            Some(n) => format!("{}\n\n{n}", self.content),
            None => self.content,
        }
    }
}

/// Human-readable size.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn byte_len(s: &str) -> usize {
    s.len() // UTF-8 bytes
}

/// Keep the **start** of content (files / grep / find).
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = byte_len(content);
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            max_lines,
            max_bytes,
        };
    }

    if lines.is_empty() {
        return TruncationResult {
            content: String::new(),
            truncated: total_bytes > max_bytes,
            truncated_by: if total_bytes > max_bytes {
                Some("bytes")
            } else {
                None
            },
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            max_lines,
            max_bytes,
        };
    }

    // First line alone exceeds byte limit → empty + notice (Pi behavior).
    if byte_len(lines[0]) > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some("bytes"),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            max_lines,
            max_bytes,
        };
    }

    let mut out: Vec<&str> = Vec::new();
    let mut out_bytes = 0usize;
    let mut truncated_by = "lines";

    for (i, line) in lines.iter().enumerate() {
        if i >= max_lines {
            truncated_by = "lines";
            break;
        }
        let add = byte_len(line) + if i > 0 { 1 } else { 0 };
        if out_bytes + add > max_bytes {
            truncated_by = "bytes";
            break;
        }
        out.push(line);
        out_bytes += add;
    }

    let output = out.join("\n");
    TruncationResult {
        content: output.clone(),
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: out.len(),
        output_bytes: byte_len(&output),
        max_lines,
        max_bytes,
    }
}

/// Keep the **end** of content (bash stdout/stderr).
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = byte_len(content);
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            max_lines,
            max_bytes,
        };
    }

    if lines.is_empty() {
        return TruncationResult {
            content: String::new(),
            truncated: total_bytes > max_bytes,
            truncated_by: if total_bytes > max_bytes {
                Some("bytes")
            } else {
                None
            },
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            max_lines,
            max_bytes,
        };
    }

    let mut out: Vec<String> = Vec::new();
    let mut out_bytes = 0usize;
    let mut truncated_by: &'static str = "lines";

    for line in lines.iter().rev() {
        if out.len() >= max_lines {
            truncated_by = "lines";
            break;
        }
        let add = byte_len(line) + if out.is_empty() { 0 } else { 1 };
        if out_bytes + add > max_bytes {
            truncated_by = "bytes";
            if out.is_empty() {
                // Single huge line: keep the tail of the line.
                out.push(truncate_string_to_bytes_from_end(line, max_bytes));
            }
            break;
        }
        out.insert(0, (*line).to_string());
        out_bytes += add;
    }

    let output = out.join("\n");
    TruncationResult {
        content: output.clone(),
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: out.len(),
        output_bytes: byte_len(&output),
        max_lines,
        max_bytes,
    }
}

fn truncate_string_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
    let buf = s.as_bytes();
    if buf.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = buf.len() - max_bytes;
    // UTF-8 boundary: skip continuation bytes.
    while start < buf.len() && (buf[start] & 0xc0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&buf[start..]).into_owned()
}

/// Truncate a single grep match line.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    if line.chars().count() <= max_chars {
        return (line.to_string(), false);
    }
    let kept: String = line.chars().take(max_chars).collect();
    (format!("{kept}... [truncated]"), true)
}

/// Convenience: head truncate with defaults + notice footer.
pub fn apply_head_default(content: &str) -> String {
    truncate_head(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES).with_notice()
}

/// Convenience: tail truncate with defaults + notice footer.
pub fn apply_tail_default(content: &str) -> String {
    truncate_tail(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES).with_notice()
}

/// How to pick the inline preview when spilling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewStyle {
    /// Keep the start (Claude Code bash: head preview).
    Head,
    /// Keep the end (useful for build/test logs).
    Tail,
}

/// Result of preparing tool text for the model (maybe spilled to disk).
#[derive(Debug, Clone)]
pub struct PresentedOutput {
    pub text: String,
    pub truncated: bool,
    pub spill_path: Option<PathBuf>,
    pub total_bytes: usize,
    pub total_chars: usize,
}

/// Max inline chars (Claude-compatible env overrides).
pub fn max_inline_output_chars() -> usize {
    std::env::var("ONE_BASH_MAX_OUTPUT_LENGTH")
        .or_else(|_| std::env::var("BASH_MAX_OUTPUT_LENGTH"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&n| n >= 1_000 && n <= 150_000)
        .unwrap_or(DEFAULT_BASH_MAX_OUTPUT_CHARS)
}

fn tool_outputs_dir(cwd: &Path) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let slug = cwd
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "-");
    home.join(".one")
        .join("agent")
        .join("tool-outputs")
        .join(format!("--{slug}--"))
}

/// Write full content to disk; return absolute path.
pub fn spill_full_output(content: &str, tool: &str, cwd: &Path) -> std::io::Result<PathBuf> {
    let dir = tool_outputs_dir(cwd);
    std::fs::create_dir_all(&dir)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("{tool}-{ts}-{}.txt", std::process::id());
    let path = dir.join(name);
    std::fs::write(&path, content)?;
    // Prefer absolute for model `read`.
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}

fn preview_chars(content: &str, style: PreviewStyle, max_chars: usize) -> String {
    let total = content.chars().count();
    if total <= max_chars {
        return content.to_string();
    }
    match style {
        PreviewStyle::Head => {
            let kept: String = content.chars().take(max_chars).collect();
            kept
        }
        PreviewStyle::Tail => {
            let skip = total.saturating_sub(max_chars);
            content.chars().skip(skip).collect()
        }
    }
}

/// Present tool output to the model: keep small results inline; for large
/// results spill full text to disk and return a short preview + path.
///
/// Inline path also applies line/byte caps so a 25k-char, 10k-line dump still
/// gets thinned. Spill triggers when char count exceeds
/// [`max_inline_output_chars`] or UTF-8 size exceeds [`DEFAULT_MAX_BYTES`].
pub fn present_tool_output(
    content: &str,
    tool: &str,
    cwd: &Path,
    style: PreviewStyle,
) -> PresentedOutput {
    let content = content.trim_end();
    let total_bytes = content.len();
    let total_chars = content.chars().count();
    let max_chars = max_inline_output_chars();

    let needs_spill = total_chars > max_chars || total_bytes > DEFAULT_MAX_BYTES;

    if !needs_spill {
        // Mild Pi-style cap for line floods under the char limit.
        let capped = match style {
            PreviewStyle::Head => {
                truncate_head(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES).with_notice()
            }
            PreviewStyle::Tail => {
                truncate_tail(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES).with_notice()
            }
        };
        let truncated = capped != content;
        return PresentedOutput {
            text: capped,
            truncated,
            spill_path: None,
            total_bytes,
            total_chars,
        };
    }

    let spill_path = match spill_full_output(content, tool, cwd) {
        Ok(p) => Some(p),
        Err(e) => {
            // Fall back to hard truncate without path.
            let fallback = match style {
                PreviewStyle::Head => {
                    truncate_head(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES).with_notice()
                }
                PreviewStyle::Tail => {
                    truncate_tail(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES).with_notice()
                }
            };
            return PresentedOutput {
                text: format!(
                    "{fallback}\n\n[spill failed: {e}; full output not saved to disk]"
                ),
                truncated: true,
                spill_path: None,
                total_bytes,
                total_chars,
            };
        }
    };

    let preview = preview_chars(content, style, DEFAULT_SPILL_PREVIEW_CHARS);
    let path_disp = spill_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unknown)".into());
    let text = format!(
        "{preview}\n\n\
         --- output truncated (Claude-style spill) ---\n\
         full_output: {path_disp}\n\
         size: {} ({total_chars} chars)\n\
         preview: first {DEFAULT_SPILL_PREVIEW_CHARS} chars shown above\n\
         Use the `read` tool (with offset/limit) or `grep` on full_output for the rest.",
        format_size(total_bytes),
    );

    PresentedOutput {
        text,
        truncated: true,
        spill_path,
        total_bytes,
        total_chars,
    }
}

/// Head truncate for files with Claude-style PARTIAL view wording.
pub fn present_file_read(numbered: &str, file_lines: usize, offset: usize) -> PresentedOutput {
    let total_bytes = numbered.len();
    let trunc = truncate_head(numbered, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    if !trunc.truncated {
        return PresentedOutput {
            text: trunc.content,
            truncated: false,
            spill_path: None,
            total_bytes,
            total_chars: numbered.chars().count(),
        };
    }
    let shown = trunc.output_lines.max(1);
    let next_offset = offset.saturating_add(shown);
    let notice = format!(
        "\n\n--- PARTIAL view ---\n\
         showing ~{shown} lines from offset {offset} (file has {file_lines} lines total, {}).\n\
         To continue: read again with offset={next_offset} and a smaller limit, or use grep for a pattern.",
        format_size(total_bytes),
    );
    PresentedOutput {
        text: format!("{}{notice}", trunc.content),
        truncated: true,
        spill_path: None,
        total_bytes,
        total_chars: numbered.chars().count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_no_truncation() {
        let r = truncate_head("a\nb\nc", 10, 1000);
        assert!(!r.truncated);
        assert_eq!(r.content, "a\nb\nc");
    }

    #[test]
    fn head_by_lines() {
        let content = (0..50).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let r = truncate_head(&content, 5, 10_000);
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some("lines"));
        assert_eq!(r.output_lines, 5);
        assert!(r.content.starts_with("line0"));
        assert!(r.content.contains("line4"));
        assert!(!r.content.contains("line5"));
    }

    #[test]
    fn head_by_bytes() {
        let content = "aaaa\nbbbb\ncccc\n";
        let r = truncate_head(content, 100, 6); // "aaaa\nb" = 6 bytes would need partial; stop before bbbb
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some("bytes"));
        assert_eq!(r.content, "aaaa");
    }

    #[test]
    fn tail_keeps_end() {
        let content = (0..20).map(|i| format!("L{i}")).collect::<Vec<_>>().join("\n");
        let r = truncate_tail(&content, 3, 10_000);
        assert!(r.truncated);
        assert_eq!(r.output_lines, 3);
        assert!(r.content.contains("L17"));
        assert!(r.content.contains("L19"));
        assert!(!r.content.contains("L0"));
    }

    #[test]
    fn notice_appended() {
        let content = (0..30).map(|i| format!("{i}")).collect::<Vec<_>>().join("\n");
        let s = truncate_head(&content, 2, 10_000).with_notice();
        assert!(s.contains("[truncated"));
        assert!(s.starts_with("0\n1"));
    }

    #[test]
    fn grep_line() {
        let long = "x".repeat(600);
        let (t, cut) = truncate_line(&long, 500);
        assert!(cut);
        assert!(t.ends_with("... [truncated]"));
    }

    #[test]
    fn spill_large_bash_output() {
        let dir = std::env::temp_dir().join(format!(
            "one-spill-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let big = "line\n".repeat(20_000); // well over 30k chars
        let presented = present_tool_output(&big, "bash", &dir, PreviewStyle::Head);
        assert!(presented.truncated);
        assert!(presented.spill_path.is_some());
        let path = presented.spill_path.unwrap();
        assert!(path.exists());
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, big.trim_end());
        assert!(presented.text.contains("full_output:"));
        assert!(presented.text.contains("PARTIAL") || presented.text.contains("truncated"));
        let _ = std::fs::remove_file(&path);
    }
}
