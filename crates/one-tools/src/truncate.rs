//! Shared truncation for tool outputs (OpenCode-aligned).
//!
//! **Unified strategy** (same pipeline for bash, grep, find, MCP, …):
//! - Inline cap: [`DEFAULT_MAX_LINES`] (2000) **and** [`DEFAULT_MAX_BYTES`] (50 KiB)
//! - When over either limit: write the **full** text under
//!   `~/.one/agent/tool-outputs/`, return a head/tail **preview** that fits the
//!   limits plus a path hint so the model can `read` / `grep` the rest.
//!
//! Limits are configurable via [`set_tool_output_limits`] (settings / env).

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default max lines in a tool result shown to the model.
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Default max UTF-8 bytes in a tool result shown to the model.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Max characters per grep match line (Pi: 500).
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Resolved truncation limits (OpenCode `tool_output`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolOutputLimits {
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl Default for ToolOutputLimits {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl ToolOutputLimits {
    /// Build from optional overrides (None keeps the current field default).
    pub fn resolve(max_lines: Option<usize>, max_bytes: Option<usize>) -> Self {
        let mut lim = Self::default();
        if let Some(n) = max_lines.filter(|&n| n >= 1) {
            lim.max_lines = n;
        }
        if let Some(n) = max_bytes.filter(|&n| n >= 1) {
            lim.max_bytes = n;
        }
        lim
    }

    /// Defaults, then settings-style overrides, then env
    /// (`ONE_TOOL_OUTPUT_MAX_LINES` / `ONE_TOOL_OUTPUT_MAX_BYTES`).
    pub fn from_env_and_overrides(max_lines: Option<usize>, max_bytes: Option<usize>) -> Self {
        let mut lim = Self::resolve(max_lines, max_bytes);
        if let Some(n) = env_usize("ONE_TOOL_OUTPUT_MAX_LINES") {
            lim.max_lines = n;
        }
        if let Some(n) = env_usize("ONE_TOOL_OUTPUT_MAX_BYTES") {
            lim.max_bytes = n;
        }
        lim
    }
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&n| n >= 1)
}

/// Strip ANSI / CSI / OSC and other C0 controls (keep `\n` / `\t`).
///
/// Colored CLI output (rustfmt, cargo, grep --color) otherwise injects ESC
/// into tool results and corrupts the Ratatui screen when painted as spans.
///
/// Binary-ish tool output may contain lone `ESC` bytes followed by multi-byte
/// UTF-8. Sequence parsers must never advance into the middle of a character —
/// `input[i..]` panics when `i` is not a char boundary.
pub fn strip_ansi_escapes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        // Safety net: never index mid-character (orphan continuation after a
        // mis-parsed ESC in binary payloads).
        if (bytes[i] & 0xc0) == 0x80 {
            i += 1;
            continue;
        }
        let b = bytes[i];
        if b == 0x1b {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'[' => {
                    i += 1;
                    while i < bytes.len() {
                        let c = bytes[i];
                        i += 1;
                        if (0x40..=0x7e).contains(&c) {
                            break;
                        }
                    }
                }
                b']' => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                // Charset designators: ESC ( B, ESC ) 0, etc. — both args ASCII.
                // Advance one *character* (not one byte) so multi-byte UTF-8
                // after a malformed sequence stays on a char boundary.
                b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => {
                    i += 1;
                    if i < bytes.len() {
                        i = advance_one_utf8_char(bytes, i);
                    }
                }
                // Unknown ESC-introducer: drop only the ESC. Do **not** skip
                // the following byte — it may be the lead of a multi-byte char
                // (common in binary tool output / `file` on ELF + paths).
                _ => {}
            }
            continue;
        }
        if b < 0x20 && b != b'\n' && b != b'\t' {
            i += 1;
            continue;
        }
        if b == 0x7f {
            i += 1;
            continue;
        }
        // `i` is a char boundary for valid UTF-8 (or we skipped continuations).
        let ch = input[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Advance `i` past one UTF-8 character starting at `i` (or one byte if
/// the lead is invalid / truncated).
fn advance_one_utf8_char(bytes: &[u8], i: usize) -> usize {
    if i >= bytes.len() {
        return i;
    }
    let width = match bytes[i] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        // Continuation or illegal lead: skip a single byte.
        _ => 1,
    };
    (i + width).min(bytes.len())
}

fn limits_cell() -> &'static RwLock<ToolOutputLimits> {
    static CELL: OnceLock<RwLock<ToolOutputLimits>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(ToolOutputLimits::from_env_and_overrides(None, None)))
}

/// Install process-wide limits (CLI startup / `/settings` / tests).
pub fn set_tool_output_limits(limits: ToolOutputLimits) {
    if let Ok(mut g) = limits_cell().write() {
        *g = limits;
    }
}

/// Current process-wide limits.
pub fn tool_output_limits() -> ToolOutputLimits {
    limits_cell().read().map(|g| *g).unwrap_or_default()
}

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
    /// Notice line for the model / user when content was cut (no spill).
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

/// Keep the **end** of content (bash logs when tail is preferred).
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

/// Head truncate with process limits + notice (no spill). Prefer
/// [`present_tool_output`] for model-facing tool results.
pub fn apply_head_default(content: &str) -> String {
    let lim = tool_output_limits();
    truncate_head(content, lim.max_lines, lim.max_bytes).with_notice()
}

/// Tail truncate with process limits + notice (no spill).
pub fn apply_tail_default(content: &str) -> String {
    let lim = tool_output_limits();
    truncate_tail(content, lim.max_lines, lim.max_bytes).with_notice()
}

/// How to pick the inline preview when spilling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewStyle {
    /// Keep the start (default — OpenCode / file listings).
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

/// Retention for spilled tool outputs (OpenCode-aligned).
pub const TOOL_OUTPUT_RETENTION_DAYS: u64 = 7;

fn tool_outputs_root_override() -> &'static RwLock<Option<PathBuf>> {
    static CELL: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(None))
}

/// Override spill root (tests). Pass `None` to clear. Returns previous override.
///
/// Default production path is `$HOME/.one/agent/tool-outputs`, which is **not**
/// writable under bash bwrap. Unit tests must point this at `/tmp` (or another
/// sandbox-writable dir) so spill can succeed without escalating the OS sandbox.
pub fn set_tool_outputs_root_override(path: Option<PathBuf>) -> Option<PathBuf> {
    let mut g = tool_outputs_root_override()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *g, path)
}

/// Root directory for all spill files: `~/.one/agent/tool-outputs/`.
///
/// Resolution: [`set_tool_outputs_root_override`] → `ONE_TOOL_OUTPUTS_DIR` → default.
pub fn tool_outputs_root() -> PathBuf {
    if let Ok(g) = tool_outputs_root_override().read() {
        if let Some(ref p) = *g {
            return p.clone();
        }
    }
    if let Ok(p) = std::env::var("ONE_TOOL_OUTPUTS_DIR") {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".one").join("agent").join("tool-outputs")
}

fn tool_outputs_dir(cwd: &Path) -> PathBuf {
    let slug = cwd
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "-");
    tool_outputs_root().join(format!("--{slug}--"))
}

/// Result of pruning old spill files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed_files: usize,
    pub removed_bytes: u64,
    pub removed_dirs: usize,
    pub errors: usize,
}

/// Delete spill files under [`tool_outputs_root`] older than `retention_days`
/// (mtime). Empty project subdirs are removed afterward.
///
/// Mirrors OpenCode `Truncate.cleanup` (7-day retention). Safe to call on
/// every startup; no-ops when the directory is missing.
pub fn cleanup_tool_outputs(retention_days: u64) -> CleanupReport {
    let cutoff = SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            retention_days.saturating_mul(24 * 60 * 60),
        ))
        .unwrap_or(UNIX_EPOCH);
    cleanup_tool_outputs_before(&tool_outputs_root(), cutoff)
}

/// Like [`cleanup_tool_outputs`] but with an explicit root and cutoff (for tests).
pub fn cleanup_tool_outputs_before(root: &Path, cutoff: SystemTime) -> CleanupReport {
    if !root.is_dir() {
        return CleanupReport::default();
    }
    let mut report = CleanupReport::default();
    cleanup_dir_recursive(root, cutoff, &mut report, /*is_root*/ true);
    report
}

fn cleanup_dir_recursive(
    dir: &Path,
    cutoff: SystemTime,
    report: &mut CleanupReport,
    is_root: bool,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            report.errors += 1;
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            report.errors += 1;
            continue;
        };
        if meta.is_dir() {
            cleanup_dir_recursive(&path, cutoff, report, false);
            // Drop empty project spill dirs (not the root).
            if !is_root {
                if let Ok(mut remaining) = std::fs::read_dir(&path) {
                    if remaining.next().is_none() {
                        if std::fs::remove_dir(&path).is_ok() {
                            report.removed_dirs += 1;
                        }
                    }
                }
            }
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if mtime >= cutoff {
            continue;
        }
        let len = meta.len();
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.removed_files += 1;
                report.removed_bytes = report.removed_bytes.saturating_add(len);
            }
            Err(_) => report.errors += 1,
        }
    }
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

/// Present tool output to the model (OpenCode unified strategy).
///
/// - Within `max_lines` **and** `max_bytes` → return text unchanged.
/// - Otherwise → spill full text to disk; model gets a preview that fits the
///   limits plus a path hint (`read` / `grep` the spill).
pub fn present_tool_output(
    content: &str,
    tool: &str,
    cwd: &Path,
    style: PreviewStyle,
) -> PresentedOutput {
    present_tool_output_with(content, tool, cwd, style, None)
}

/// Like [`present_tool_output`] with optional per-call limit overrides
/// (e.g. MCP `maxOutputBytes`).
pub fn present_tool_output_with(
    content: &str,
    tool: &str,
    cwd: &Path,
    style: PreviewStyle,
    overrides: Option<ToolOutputLimits>,
) -> PresentedOutput {
    // Strip ANSI so model + TUI never see ESC noise from colored CLIs
    // (rustfmt --check, cargo, grep --color). Spill keeps the cleaned text.
    let cleaned = strip_ansi_escapes(content);
    let content = cleaned.trim_end();
    let total_bytes = content.len();
    let total_chars = content.chars().count();
    let lim = overrides.unwrap_or_else(tool_output_limits);

    let trunc = match style {
        PreviewStyle::Head => truncate_head(content, lim.max_lines, lim.max_bytes),
        PreviewStyle::Tail => truncate_tail(content, lim.max_lines, lim.max_bytes),
    };

    if !trunc.truncated {
        return PresentedOutput {
            text: content.to_string(),
            truncated: false,
            spill_path: None,
            total_bytes,
            total_chars,
        };
    }

    let spill_path = match spill_full_output(content, tool, cwd) {
        Ok(p) => Some(p),
        Err(e) => {
            // Fall back to hard truncate without path.
            return PresentedOutput {
                text: format!(
                    "{}\n\n[spill failed: {e}; full output not saved to disk]",
                    trunc.with_notice()
                ),
                truncated: true,
                spill_path: None,
                total_bytes,
                total_chars,
            };
        }
    };

    let path_disp = spill_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unknown)".into());

    let hit_bytes = trunc.truncated_by == Some("bytes");
    let removed = if hit_bytes {
        total_bytes.saturating_sub(trunc.output_bytes)
    } else {
        trunc.total_lines.saturating_sub(trunc.output_lines)
    };
    let unit = if hit_bytes { "bytes" } else { "lines" };
    let preview = trunc.content;
    let hint = one_core::system_reminder(format!(
        "Output truncated ({removed} {unit} omitted). Full output saved to: {path_disp}\n\
         Prefer `read` / `grep` on that path — do not re-run a wider command."
    ));

    let text = match style {
        PreviewStyle::Head => {
            format!("{preview}\n\n...{removed} {unit} truncated...\n\n{hint}")
        }
        PreviewStyle::Tail => {
            format!("...{removed} {unit} truncated...\n\n{hint}\n\n{preview}")
        }
    };

    PresentedOutput {
        text,
        truncated: true,
        spill_path,
        total_bytes,
        total_chars,
    }
}

/// Head truncate for files with PARTIAL view wording (uses process limits).
pub fn present_file_read(numbered: &str, file_lines: usize, offset: usize) -> PresentedOutput {
    let total_bytes = numbered.len();
    let lim = tool_output_limits();
    let trunc = truncate_head(numbered, lim.max_lines, lim.max_bytes);
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
    fn strip_ansi_removes_sgr_and_keeps_text() {
        let raw = "\x1b[31m-old\x1b[0m\n\x1b[32m+new\x1b[m\x0f";
        let clean = strip_ansi_escapes(raw);
        assert_eq!(clean, "-old\n+new");
        assert!(!clean.contains("31m"));
        assert!(!clean.contains('\u{1b}'));
    }

    /// Regression: ESC followed by multi-byte UTF-8 must not panic.
    ///
    /// Repro shape from panic log: tool output mixed Japanese path text with
    /// ELF bytes (`\x7fELF…`). A lone ESC before a multi-byte char used to
    /// advance one byte into the character, then `input[i..]` panicked with
    /// "byte index N is not a char boundary".
    #[test]
    fn strip_ansi_esc_before_multibyte_utf8_no_panic() {
        // ESC + U+030F (combining double grave, 2 bytes) — exact panic char.
        let raw = "\u{1b}\u{30f}tail";
        let clean = strip_ansi_escapes(raw);
        assert!(clean.contains('t'));
        assert!(!clean.contains('\u{1b}'));

        // ESC + Japanese "月" (3-byte UTF-8) as in `7月 15`.
        let raw = "7\u{1b}月 15";
        let clean = strip_ansi_escapes(raw);
        assert_eq!(clean, "7月 15");

        // Charset designator form then multi-byte.
        let raw = "\u{1b}(月x";
        let _ = strip_ansi_escapes(raw);

        // Binary-ish payload: DEL+ELF magic, NULs, ESC mid-stream, multi-byte.
        let mut raw = String::from("path 7月\n");
        raw.push('\u{7f}');
        raw.push_str("ELF");
        raw.push('\0');
        raw.push('\u{1b}');
        raw.push('\u{30f}');
        raw.push_str("more");
        let clean = strip_ansi_escapes(&raw);
        assert!(clean.contains("7月"));
        assert!(clean.contains("ELF"));
        assert!(clean.contains("more"));
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\u{7f}'));
    }

    #[test]
    fn present_tool_output_binary_mixed_utf8_no_panic() {
        with_temp_spill_root(|_| {
            let dir = std::env::temp_dir().join(format!(
                "one-bin-utf8-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let _ = std::fs::create_dir_all(&dir);
            // Build a large payload that hits byte limits and exercises strip.
            let mut content = String::new();
            for _ in 0..200 {
                content
                    .push_str("lrwxrwxrwx 1 fxh fxh 24  7月 15 21:18 /home/fxh/.local/bin/grok\n");
                content.push('\u{7f}');
                content.push_str("ELF");
                content.push('\u{1b}');
                content.push('\u{30f}');
                content.push_str("binary-tail\n");
            }
            let presented = present_tool_output_with(
                &content,
                "bash",
                &dir,
                PreviewStyle::Tail,
                Some(ToolOutputLimits {
                    max_lines: 50,
                    max_bytes: 2048,
                }),
            );
            assert!(presented.truncated || !presented.text.is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn head_no_truncation() {
        let r = truncate_head("a\nb\nc", 10, 1000);
        assert!(!r.truncated);
        assert_eq!(r.content, "a\nb\nc");
    }

    #[test]
    fn head_by_lines() {
        let content = (0..50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
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
        let r = truncate_head(content, 100, 6);
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some("bytes"));
        assert_eq!(r.content, "aaaa");
    }

    #[test]
    fn tail_keeps_end() {
        let content = (0..20)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_tail(&content, 3, 10_000);
        assert!(r.truncated);
        assert_eq!(r.output_lines, 3);
        assert!(r.content.contains("L17"));
        assert!(r.content.contains("L19"));
        assert!(!r.content.contains("L0"));
    }

    #[test]
    fn notice_appended() {
        let content = (0..30)
            .map(|i| format!("{i}"))
            .collect::<Vec<_>>()
            .join("\n");
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

    /// Point spill root at a unique `/tmp` dir (writable under agent bwrap).
    fn with_temp_spill_root<T>(f: impl FnOnce(&Path) -> T) -> T {
        let root = std::env::temp_dir().join(format!(
            "one-spill-root-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&root);
        let prev = set_tool_outputs_root_override(Some(root.clone()));
        let out = f(&root);
        set_tool_outputs_root_override(prev);
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    #[test]
    fn spill_when_over_line_limit() {
        with_temp_spill_root(|root| {
            // cwd only affects the project slug under the spill root — not the
            // root itself. Without root override, spill goes to ~/.one/... and
            // fails under bwrap (Read-only file system).
            let dir = std::env::temp_dir().join(format!(
                "one-spill-cwd-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let _ = std::fs::create_dir_all(&dir);
            let big = (0..100)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n");
            let presented = present_tool_output_with(
                &big,
                "bash",
                &dir,
                PreviewStyle::Head,
                Some(ToolOutputLimits {
                    max_lines: 10,
                    max_bytes: 1_000_000,
                }),
            );

            assert!(presented.truncated);
            assert!(
                presented.spill_path.is_some(),
                "spill failed (root={root:?}): {}",
                presented.text
            );
            let path = presented.spill_path.unwrap();
            assert!(
                path.starts_with(root),
                "spill outside override root: {path:?}"
            );
            assert!(path.exists());
            let on_disk = std::fs::read_to_string(&path).unwrap();
            assert_eq!(on_disk, big.trim_end());
            assert!(presented.text.contains("Full output saved to:"));
            assert!(presented.text.contains("lines truncated"));
            assert!(presented.text.contains("line0"));
            assert!(!presented.text.contains("line99") || presented.text.contains("saved to"));
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn under_limit_no_spill() {
        with_temp_spill_root(|_| {
            let dir = std::env::temp_dir().join(format!("one-nospill-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let presented = present_tool_output_with(
                "a\nb\nc",
                "grep",
                &dir,
                PreviewStyle::Head,
                Some(ToolOutputLimits {
                    max_lines: 100,
                    max_bytes: 10_000,
                }),
            );
            assert!(!presented.truncated);
            assert!(presented.spill_path.is_none());
            assert_eq!(presented.text, "a\nb\nc");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn limits_resolve() {
        let l = ToolOutputLimits::resolve(Some(100), Some(2048));
        assert_eq!(l.max_lines, 100);
        assert_eq!(l.max_bytes, 2048);
        let d = ToolOutputLimits::resolve(None, None);
        assert_eq!(d, ToolOutputLimits::default());
    }

    #[test]
    fn cleanup_removes_files_before_cutoff() {
        let root = std::env::temp_dir().join(format!(
            "one-cleanup-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let proj = root.join("--proj--");
        std::fs::create_dir_all(&proj).unwrap();
        let file = proj.join("stale.txt");
        std::fs::write(&file, "stale-data").unwrap();
        // Cutoff in the future → every existing mtime is "old".
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        let report = cleanup_tool_outputs_before(&root, future);
        assert_eq!(report.removed_files, 1);
        assert!(!file.exists());
        // Empty project dir should be pruned.
        assert!(!proj.exists() || std::fs::read_dir(&proj).map(|d| d.count()).unwrap_or(0) == 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_keeps_files_after_cutoff() {
        let root = std::env::temp_dir().join(format!(
            "one-cleanup-keep-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let proj = root.join("--proj--");
        std::fs::create_dir_all(&proj).unwrap();
        let file = proj.join("fresh.txt");
        std::fs::write(&file, "fresh").unwrap();
        // Cutoff in the past → file is newer than cutoff, keep.
        let past = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(7 * 24 * 3600))
            .unwrap_or(UNIX_EPOCH);
        let report = cleanup_tool_outputs_before(&root, past);
        assert_eq!(report.removed_files, 0);
        assert!(file.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
