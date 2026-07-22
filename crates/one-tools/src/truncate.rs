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

/// Root directory for all spill files: `~/.one/agent/tool-outputs/`.
pub fn tool_outputs_root() -> PathBuf {
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
    let content = content.trim_end();
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
    let hint = format!(
        "The tool call succeeded but the output was truncated. Full output saved to: {path_disp}\n\
         Use Grep to search the full content or Read with offset/limit to view specific sections."
    );

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

    #[test]
    fn spill_when_over_line_limit() {
        let dir = std::env::temp_dir().join(format!(
            "one-spill-test-{}-{}",
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
        assert!(presented.spill_path.is_some());
        let path = presented.spill_path.unwrap();
        assert!(path.exists());
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, big.trim_end());
        assert!(presented.text.contains("Full output saved to:"));
        assert!(presented.text.contains("lines truncated"));
        assert!(presented.text.contains("line0"));
        assert!(!presented.text.contains("line99") || presented.text.contains("saved to"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn under_limit_no_spill() {
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
