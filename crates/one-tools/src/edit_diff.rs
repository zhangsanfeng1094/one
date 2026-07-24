//! Shared edit matching / diff helpers (Pi fuzzy + OpenCode multi-strategy + CRLF).
//!
//! Pipeline:
//! 1. Normalize line endings to LF for matching; restore original ending on write.
//! 2. Strip read-tool line-number prefixes from old_string when present (`12|…`).
//! 3. Strategy waterfall (OpenCode-inspired): exact → trailing/unicode fuzzy →
//!    line-trim → whitespace-normalize → indent-flexible → block-anchor.
//! 4. Emit a real line-oriented unified diff of before/after content.

use std::fmt;

/// Detected newline style of the on-disk file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStrategy {
    Exact,
    /// Trailing whitespace + smart quotes / dashes / special spaces (Pi).
    Fuzzy,
    /// Per-line full trim (OpenCode LineTrimmedReplacer).
    LineTrimmed,
    /// Collapse internal whitespace runs (OpenCode WhitespaceNormalizedReplacer).
    WhitespaceNormalized,
    /// Ignore common leading indent (OpenCode IndentationFlexibleReplacer).
    IndentationFlexible,
    /// First/last line anchors + middle Levenshtein (OpenCode BlockAnchorReplacer).
    BlockAnchor,
}

impl MatchStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchStrategy::Exact => "exact",
            MatchStrategy::Fuzzy => "fuzzy",
            MatchStrategy::LineTrimmed => "line_trimmed",
            MatchStrategy::WhitespaceNormalized => "whitespace",
            MatchStrategy::IndentationFlexible => "indent",
            MatchStrategy::BlockAnchor => "block_anchor",
        }
    }

    pub fn is_relaxed(self) -> bool {
        !matches!(self, MatchStrategy::Exact)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchSpan {
    /// Byte offset in LF-normalized content (inclusive start).
    pub start: usize,
    /// Byte offset in LF-normalized content (exclusive end).
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditApplyError {
    EmptyOldString,
    NoChange,
    NotFound,
    Multiple {
        count: usize,
    },
    /// Matched span is far larger than old_string (OpenCode safety guard).
    Disproportionate,
}

impl fmt::Display for EditApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl EditApplyError {
    /// Actionable message for the model (OpenCode / Pi style).
    pub fn user_message(&self) -> String {
        match self {
            EditApplyError::EmptyOldString => {
                "old_string cannot be empty when editing an existing file. \
                 Provide the exact text to replace, or use `write` for a full-file replacement."
                    .to_string()
            }
            EditApplyError::NoChange => {
                "No changes to apply: old_string and new_string are identical \
                 (after line-ending normalization)."
                    .to_string()
            }
            EditApplyError::NotFound => {
                "Could not find old_string in the file (exact and relaxed match strategies failed). \
                 Tips: re-read the file; do not include read-tool line-number prefixes \
                 (e.g. `12|`); provide more surrounding context; check indentation, \
                 whitespace, and quotes."
                    .to_string()
            }
            EditApplyError::Multiple { count } => {
                format!(
                    "old_string matched {count} times; must be unique. \
                     Provide more surrounding context to make the match unique, \
                     or set replace_all=true to change every occurrence."
                )
            }
            EditApplyError::Disproportionate => {
                "Refusing replacement because the matched span is much larger than old_string. \
                 Re-read the file and provide the full exact old_string for the intended replacement."
                    .to_string()
            }
        }
    }
}

/// Result of a successful in-memory apply.
#[derive(Debug, Clone)]
pub struct AppliedEdit {
    pub content_lf: String,
    pub replacements: usize,
    pub strategy: MatchStrategy,
}

pub fn detect_line_ending(text: &str) -> LineEnding {
    if text.contains("\r\n") {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

/// Normalize CRLF → LF for matching. Lone `\r` also becomes `\n`.
pub fn normalize_to_lf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Restore LF content to the file's original newline style.
pub fn apply_line_ending(text_lf: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text_lf.to_string(),
        LineEnding::Crlf => text_lf.replace('\n', "\r\n"),
    }
}

/// Pi-style fuzzy normalization: trailing WS, smart quotes/dashes, special spaces.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed_end = line.trim_end();
        for ch in trimmed_end.chars() {
            out.push(normalize_fuzzy_char(ch));
        }
    }
    out
}

fn normalize_fuzzy_char(ch: char) -> char {
    match ch {
        // Smart single quotes → '
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        // Smart double quotes → "
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        // Dashes / minus variants → -
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        // Special spaces → regular space
        '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
        | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
        | '\u{3000}' => ' ',
        _ => ch,
    }
}

/// Find all non-overlapping exact matches of `needle` in `content` (both LF).
pub fn find_exact_matches(content: &str, needle: &str) -> Vec<MatchSpan> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(rel) = content[start..].find(needle) {
        let abs = start + rel;
        out.push(MatchSpan {
            start: abs,
            end: abs + needle.len(),
        });
        start = abs + needle.len();
    }
    out
}

/// Drop trailing empty line from needle line list (OpenCode/Pi).
fn needle_content_lines(needle: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = needle.split('\n').collect();
    if lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Map a window of content lines to a byte span in `content`.
fn span_for_window(
    content: &str,
    content_lines: &[LineSpan<'_>],
    start_line: usize,
    line_count: usize,
    needle_ends_with_nl: bool,
) -> MatchSpan {
    let start = content_lines[start_line].start;
    let last = &content_lines[start_line + line_count - 1];
    let end = if needle_ends_with_nl {
        if last.end < content.len() && content.as_bytes().get(last.end) == Some(&b'\n') {
            last.end + 1
        } else {
            last.end
        }
    } else {
        last.end
    };
    MatchSpan { start, end }
}

/// Line-aligned fuzzy matches (trailing WS + unicode quotes/dashes/spaces).
pub fn find_fuzzy_matches(content: &str, needle: &str) -> Vec<MatchSpan> {
    find_line_window_matches(content, needle, |a, b| {
        normalize_for_fuzzy_match(a) == normalize_for_fuzzy_match(b)
    })
}

/// OpenCode LineTrimmedReplacer: compare lines after trim().
pub fn find_line_trimmed_matches(content: &str, needle: &str) -> Vec<MatchSpan> {
    find_line_window_matches(content, needle, |a, b| a.trim() == b.trim())
}

/// OpenCode IndentationFlexibleReplacer: strip common leading indent per block.
pub fn find_indentation_flexible_matches(content: &str, needle: &str) -> Vec<MatchSpan> {
    let needle_lines = needle_content_lines(needle);
    if needle_lines.is_empty() {
        return Vec::new();
    }
    let needle_dedent = dedent_lines(&needle_lines);
    let content_lines = line_spans(content);
    if content_lines.len() < needle_lines.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let max_start = content_lines.len() - needle_lines.len();
    for i in 0..=max_start {
        let window: Vec<&str> = content_lines[i..i + needle_lines.len()]
            .iter()
            .map(|l| l.text)
            .collect();
        if dedent_lines(&window) == needle_dedent {
            out.push(span_for_window(
                content,
                &content_lines,
                i,
                needle_lines.len(),
                needle.ends_with('\n'),
            ));
        }
    }
    out
}

/// OpenCode WhitespaceNormalizedReplacer: collapse `\s+` → single space.
pub fn find_whitespace_normalized_matches(content: &str, needle: &str) -> Vec<MatchSpan> {
    if needle.is_empty() {
        return Vec::new();
    }
    let needle_ws = collapse_whitespace(needle);
    let content_lines = line_spans(content);
    let needle_lines = needle_content_lines(needle);
    let mut out = Vec::new();

    // Single-line: match whole lines or unique substring via collapsed form.
    if needle_lines.len() == 1 {
        for line in &content_lines {
            if collapse_whitespace(line.text) == needle_ws {
                out.push(MatchSpan {
                    start: line.start,
                    end: line.end,
                });
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    // Multi-line block: collapse the joined block.
    if needle_lines.len() > 1 && content_lines.len() >= needle_lines.len() {
        let max_start = content_lines.len() - needle_lines.len();
        for i in 0..=max_start {
            let block: String = content_lines[i..i + needle_lines.len()]
                .iter()
                .map(|l| l.text)
                .collect::<Vec<_>>()
                .join("\n");
            if collapse_whitespace(&block) == needle_ws {
                out.push(span_for_window(
                    content,
                    &content_lines,
                    i,
                    needle_lines.len(),
                    needle.ends_with('\n'),
                ));
            }
        }
    }
    out
}

const BLOCK_ANCHOR_SIMILARITY: f64 = 0.65;

/// OpenCode BlockAnchorReplacer: first/last line anchors + middle similarity.
pub fn find_block_anchor_matches(content: &str, needle: &str) -> Vec<MatchSpan> {
    let needle_lines = needle_content_lines(needle);
    if needle_lines.len() < 3 {
        return Vec::new();
    }
    let content_lines = line_spans(content);
    if content_lines.len() < 3 {
        return Vec::new();
    }

    let first = needle_lines[0].trim();
    let last = needle_lines[needle_lines.len() - 1].trim();
    let search_block = needle_lines.len();
    let max_delta = (search_block as f64 * 0.25).floor().max(1.0) as usize;

    // Collect candidates where first and last anchors match.
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for i in 0..content_lines.len() {
        if content_lines[i].text.trim() != first {
            continue;
        }
        for j in (i + 2)..content_lines.len() {
            if content_lines[j].text.trim() == last {
                let actual = j - i + 1;
                if actual.abs_diff(search_block) <= max_delta {
                    candidates.push((i, j));
                }
                break;
            }
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    let score = |start: usize, end: usize| -> f64 {
        let actual = end - start + 1;
        let middle_search = search_block.saturating_sub(2);
        let middle_actual = actual.saturating_sub(2);
        let lines_to_check = middle_search.min(middle_actual);
        if lines_to_check == 0 {
            return 1.0;
        }
        let mut sim = 0.0;
        for k in 1..=lines_to_check {
            let a = content_lines[start + k].text.trim();
            let b = needle_lines[k].trim();
            let max_len = a.chars().count().max(b.chars().count());
            if max_len == 0 {
                continue;
            }
            let dist = levenshtein(a, b);
            sim += 1.0 - (dist as f64 / max_len as f64);
        }
        sim / lines_to_check as f64
    };

    let mut out = Vec::new();
    if candidates.len() == 1 {
        let (s, e) = candidates[0];
        if score(s, e) >= BLOCK_ANCHOR_SIMILARITY {
            out.push(span_for_window(
                content,
                &content_lines,
                s,
                e - s + 1,
                needle.ends_with('\n'),
            ));
        }
        return out;
    }

    let mut best: Option<(usize, usize, f64)> = None;
    for (s, e) in candidates {
        let sc = score(s, e);
        if best.map(|(_, _, b)| sc > b).unwrap_or(true) {
            best = Some((s, e, sc));
        }
    }
    if let Some((s, e, sc)) = best {
        if sc >= BLOCK_ANCHOR_SIMILARITY {
            out.push(span_for_window(
                content,
                &content_lines,
                s,
                e - s + 1,
                needle.ends_with('\n'),
            ));
        }
    }
    out
}

fn find_line_window_matches(
    content: &str,
    needle: &str,
    line_eq: impl Fn(&str, &str) -> bool,
) -> Vec<MatchSpan> {
    if needle.is_empty() {
        return Vec::new();
    }
    let content_lines = line_spans(content);
    let needle_lines = needle_content_lines(needle);
    if needle_lines.is_empty() || content_lines.len() < needle_lines.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let max_start = content_lines.len() - needle_lines.len();
    for i in 0..=max_start {
        let mut ok = true;
        for (j, n) in needle_lines.iter().enumerate() {
            if !line_eq(content_lines[i + j].text, n) {
                ok = false;
                break;
            }
        }
        if ok {
            out.push(span_for_window(
                content,
                &content_lines,
                i,
                needle_lines.len(),
                needle.ends_with('\n'),
            ));
        }
    }
    out
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

fn dedent_lines(lines: &[&str]) -> String {
    let non_empty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let min_indent = non_empty
        .iter()
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                (*l).to_string()
            } else {
                l.chars().skip(min_indent).collect::<String>()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// OpenCode guard: refuse when fuzzy span is much larger than the model’s old_string.
pub fn is_disproportionate_match(search: &str, old_string: &str) -> bool {
    let old_lines = old_string.lines().count().max(1);
    let search_lines = search.lines().count().max(1);
    if search_lines >= old_lines.saturating_add(3).max(old_lines.saturating_mul(2)) {
        return true;
    }
    if old_lines == 1 {
        return false;
    }
    let old_len = old_string.trim().len();
    let search_len = search.trim().len();
    search_len > (old_len + 500).max(old_len.saturating_mul(4))
}

/// If every non-empty line looks like `12|…` or `12: …` (read-tool prefixes), strip them.
pub fn strip_line_number_prefixes(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let non_empty: Vec<&str> = lines.iter().copied().filter(|l| !l.is_empty()).collect();
    if non_empty.is_empty() {
        return text.to_string();
    }
    let mut stripped: Vec<String> = Vec::with_capacity(lines.len());
    for line in &lines {
        if line.is_empty() {
            stripped.push(String::new());
            continue;
        }
        if let Some(rest) = strip_one_line_prefix(line) {
            stripped.push(rest.to_string());
        } else {
            // Not a consistent numbered dump — leave original.
            return text.to_string();
        }
    }
    stripped.join("\n")
}

fn strip_one_line_prefix(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut i = 0;
    if i >= bytes.len() || !bytes[i].is_ascii_digit() {
        return None;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    match bytes[i] {
        b'|' => Some(&line[i + 1..]),
        b':' => {
            let rest = &line[i + 1..];
            Some(rest.strip_prefix(' ').unwrap_or(rest))
        }
        _ => None,
    }
}

#[derive(Debug)]
struct LineSpan<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn line_spans(content: &str) -> Vec<LineSpan<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = content.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            lines.push(LineSpan {
                start,
                end: i,
                text: &content[start..i],
            });
            start = i + 1;
        }
    }
    if start < content.len() || content.is_empty() || !content.ends_with('\n') {
        if start <= content.len() && !(content.is_empty() && !lines.is_empty()) {
            lines.push(LineSpan {
                start,
                end: content.len(),
                text: &content[start..],
            });
        }
    }
    lines
}

/// Apply `old` → `new` on LF content with multi-strategy matching.
pub fn apply_edit_lf(
    content_lf: &str,
    old_lf: &str,
    new_lf: &str,
    replace_all: bool,
) -> Result<AppliedEdit, EditApplyError> {
    // Models often paste read-tool output including `12|` prefixes.
    let old_stripped = strip_line_number_prefixes(old_lf);
    let old_use = if old_stripped != old_lf {
        old_stripped.as_str()
    } else {
        old_lf
    };
    // Also try stripping new_string prefixes for symmetry when both were copied.
    let new_stripped = strip_line_number_prefixes(new_lf);
    let new_use = if new_stripped != new_lf && old_stripped != old_lf {
        new_stripped.as_str()
    } else {
        new_lf
    };

    if old_use.is_empty() {
        return Err(EditApplyError::EmptyOldString);
    }
    if old_use == new_use {
        return Err(EditApplyError::NoChange);
    }

    let strategies: &[(MatchStrategy, fn(&str, &str) -> Vec<MatchSpan>)] = &[
        (MatchStrategy::Exact, find_exact_matches),
        (MatchStrategy::Fuzzy, find_fuzzy_matches),
        (MatchStrategy::LineTrimmed, find_line_trimmed_matches),
        (
            MatchStrategy::WhitespaceNormalized,
            find_whitespace_normalized_matches,
        ),
        (
            MatchStrategy::IndentationFlexible,
            find_indentation_flexible_matches,
        ),
        (MatchStrategy::BlockAnchor, find_block_anchor_matches),
    ];

    let mut multi_count = 0usize;
    let mut saw_disproportionate = false;

    for &(strategy, finder) in strategies {
        let matches = finder(content_lf, old_use);
        if matches.is_empty() {
            continue;
        }

        // Filter disproportionate spans (mostly relevant for block-anchor).
        let mut safe: Vec<MatchSpan> = Vec::new();
        for m in matches {
            let span_text = &content_lf[m.start..m.end];
            if is_disproportionate_match(span_text, old_use) {
                saw_disproportionate = true;
                continue;
            }
            safe.push(m);
        }
        if safe.is_empty() {
            continue;
        }

        if !replace_all && safe.len() > 1 {
            multi_count = multi_count.max(safe.len());
            // OpenCode: try next strategy for a unique match.
            continue;
        }

        return apply_spans(content_lf, &safe, new_use, replace_all, strategy);
    }

    if multi_count > 1 {
        return Err(EditApplyError::Multiple { count: multi_count });
    }
    if saw_disproportionate {
        return Err(EditApplyError::Disproportionate);
    }
    Err(EditApplyError::NotFound)
}

fn apply_spans(
    content_lf: &str,
    matches: &[MatchSpan],
    new_lf: &str,
    replace_all: bool,
    strategy: MatchStrategy,
) -> Result<AppliedEdit, EditApplyError> {
    let replacements = if replace_all { matches.len() } else { 1 };
    let use_matches: &[MatchSpan] = if replace_all { matches } else { &matches[..1] };

    let mut content = content_lf.to_string();
    for m in use_matches.iter().rev() {
        content.replace_range(m.start..m.end, new_lf);
    }

    if content == content_lf {
        return Err(EditApplyError::NoChange);
    }

    Ok(AppliedEdit {
        content_lf: content,
        replacements,
        strategy,
    })
}

/// Soft cap for `details.patch` (UI-only). Larger patches are omitted from details;
/// the model never sees the patch body.
pub const MAX_DETAILS_PATCH_BYTES: usize = 100 * 1024;

/// Model-facing summary + UI-only unified patch after a successful edit.
///
/// **`summary`** is what enters LLM context (short ack, Codex/OpenCode style).
/// **`patch`** is optional UI metadata (`ToolOutput.details`) and must not be
/// copied into `ToolResultMessage.content`.
#[derive(Debug, Clone)]
pub struct EditSuccess {
    pub summary: String,
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
}

/// Line-oriented unified diff of full before/after (LF). Single or multi-hunk.
pub fn unified_diff(path: &str, before_lf: &str, after_lf: &str, context: usize) -> String {
    let before_lines: Vec<&str> = split_lines_no_trailing_empty(before_lf);
    let after_lines: Vec<&str> = split_lines_no_trailing_empty(after_lf);

    let mut out = String::new();
    out.push_str("--- a/");
    out.push_str(path);
    out.push('\n');
    out.push_str("+++ b/");
    out.push_str(path);
    out.push('\n');

    if before_lines == after_lines {
        out.push_str("@@ -0,0 +0,0 @@\n");
        return trim_trailing_nl(out);
    }

    // Prefix/suffix-stripped LCS; surgical edits on large files stay O(change²).
    let edits = line_diff(&before_lines, &after_lines);
    let hunks = group_hunks(&edits, before_lines.len(), after_lines.len(), context);

    if hunks.is_empty() {
        // Should be rare after line_diff; emit a compact mid-only dump rather than
        // duplicating equal lines.
        out.push_str(&format!(
            "@@ -1,{} +1,{} @@\n",
            before_lines.len().max(1),
            after_lines.len().max(1)
        ));
        for l in &before_lines {
            out.push('-');
            out.push_str(l);
            out.push('\n');
        }
        for l in &after_lines {
            out.push('+');
            out.push_str(l);
            out.push('\n');
        }
        return trim_trailing_nl(out);
    }

    for h in hunks {
        let old_count = h.old_end.saturating_sub(h.old_start);
        let new_count = h.new_end.saturating_sub(h.new_start);
        let old_start = if old_count == 0 {
            h.old_start
        } else {
            h.old_start + 1
        };
        let new_start = if new_count == 0 {
            h.new_start
        } else {
            h.new_start + 1
        };
        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for line in &h.lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    trim_trailing_nl(out)
}

fn count_line_ops(before_lf: &str, after_lf: &str) -> (usize, usize) {
    let before_lines = split_lines_no_trailing_empty(before_lf);
    let after_lines = split_lines_no_trailing_empty(after_lf);
    let edits = line_diff(&before_lines, &after_lines);
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for e in edits {
        match e.op {
            DiffOp::Insert => additions += 1,
            DiffOp::Delete => deletions += 1,
            DiffOp::Equal => {}
        }
    }
    (additions, deletions)
}

/// Build model summary + UI patch (OpenCode/Codex: model sees ack only).
pub fn format_edit_success(
    path: &str,
    before_lf: &str,
    after_lf: &str,
    replacements: usize,
    strategy: MatchStrategy,
) -> EditSuccess {
    let (additions, deletions) = count_line_ops(before_lf, after_lf);
    let patch = unified_diff(path, before_lf, after_lf, 3);

    let mut summary = String::new();
    let relaxed = strategy.is_relaxed();
    if replacements > 1 {
        summary.push_str(&format!("Updated {path} ({replacements} replacements"));
        if relaxed {
            summary.push_str(&format!(", {} match", strategy.as_str()));
        }
        summary.push(')');
    } else {
        summary.push_str(&format!("Updated {path}"));
        if relaxed {
            summary.push_str(&format!(" ({} match)", strategy.as_str()));
        }
    }
    if additions > 0 || deletions > 0 {
        summary.push_str(&format!(" (+{additions} −{deletions})"));
    }

    EditSuccess {
        summary,
        patch,
        additions,
        deletions,
    }
}

/// Whether `patch` is small enough to attach on `ToolOutput.details`.
pub fn patch_for_details(patch: &str) -> Option<String> {
    if patch.is_empty() {
        return None;
    }
    if patch.len() > MAX_DETAILS_PATCH_BYTES {
        return None;
    }
    Some(patch.to_string())
}

fn split_lines_no_trailing_empty(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = s.split('\n').collect();
    if s.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn trim_trailing_nl(mut s: String) -> String {
    while s.ends_with('\n') {
        s.pop();
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffOp {
    Equal,
    Delete,
    Insert,
}

#[derive(Debug, Clone)]
struct DiffEdit {
    op: DiffOp,
    /// Line index in before (for Equal/Delete) or after-only insert position hint.
    old_idx: usize,
    new_idx: usize,
    text: String,
}

/// Hunt–Szymanski / simple LCS DP for line diffs.
///
/// Strips common prefix/suffix first so a surgical edit on a multi-thousand-line
/// file does not hit the cell cap and fall back to a whole-file delete+insert
/// (which used to flood model context when the patch was inlined into content).
fn line_diff(a: &[&str], b: &[&str]) -> Vec<DiffEdit> {
    let n = a.len();
    let m = b.len();
    if n == 0 && m == 0 {
        return Vec::new();
    }

    // Common prefix.
    let mut prefix = 0usize;
    while prefix < n && prefix < m && a[prefix] == b[prefix] {
        prefix += 1;
    }
    // Common suffix (do not overlap prefix).
    let mut suffix = 0usize;
    while suffix < n - prefix && suffix < m - prefix && a[n - 1 - suffix] == b[m - 1 - suffix] {
        suffix += 1;
    }

    let a_mid = &a[prefix..n - suffix];
    let b_mid = &b[prefix..m - suffix];
    let mid_edits = line_diff_middle(a_mid, b_mid, prefix);

    let mut edits = Vec::with_capacity(prefix + mid_edits.len() + suffix);
    for i in 0..prefix {
        edits.push(DiffEdit {
            op: DiffOp::Equal,
            old_idx: i,
            new_idx: i,
            text: a[i].to_string(),
        });
    }
    edits.extend(mid_edits);
    for s in 0..suffix {
        let old_idx = n - suffix + s;
        let new_idx = m - suffix + s;
        edits.push(DiffEdit {
            op: DiffOp::Equal,
            old_idx,
            new_idx,
            text: a[old_idx].to_string(),
        });
    }
    edits
}

/// LCS (or delete-all+insert-all) for the non-common middle only.
fn line_diff_middle(a: &[&str], b: &[&str], old_base: usize) -> Vec<DiffEdit> {
    let n = a.len();
    let m = b.len();
    // Cap pathological cost on the *middle* only. Full-file rewrites still hit
    // this; surgical edits on large files usually do not.
    const MAX_CELLS: usize = 2_000_000;
    if n.saturating_mul(m) > MAX_CELLS {
        let mut edits = Vec::with_capacity(n + m);
        for (i, t) in a.iter().enumerate() {
            edits.push(DiffEdit {
                op: DiffOp::Delete,
                old_idx: old_base + i,
                new_idx: old_base, // approximate; group_hunks uses op stream
                text: (*t).to_string(),
            });
        }
        for (j, t) in b.iter().enumerate() {
            edits.push(DiffEdit {
                op: DiffOp::Insert,
                old_idx: old_base + n,
                new_idx: old_base + j,
                text: (*t).to_string(),
            });
        }
        return edits;
    }

    if n == 0 && m == 0 {
        return Vec::new();
    }

    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            if a[i] == b[j] {
                dp[i + 1][j + 1] = dp[i][j] + 1;
            } else {
                dp[i + 1][j + 1] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    let mut edits_rev = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            edits_rev.push(DiffEdit {
                op: DiffOp::Equal,
                old_idx: old_base + i - 1,
                new_idx: old_base + j - 1,
                text: a[i - 1].to_string(),
            });
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            edits_rev.push(DiffEdit {
                op: DiffOp::Insert,
                old_idx: old_base + i,
                new_idx: old_base + j - 1,
                text: b[j - 1].to_string(),
            });
            j -= 1;
        } else {
            edits_rev.push(DiffEdit {
                op: DiffOp::Delete,
                old_idx: old_base + i - 1,
                new_idx: old_base + j,
                text: a[i - 1].to_string(),
            });
            i -= 1;
        }
    }
    edits_rev.reverse();
    edits_rev
}

struct Hunk {
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
    lines: Vec<String>,
}

fn group_hunks(edits: &[DiffEdit], _old_len: usize, _new_len: usize, context: usize) -> Vec<Hunk> {
    // Collect indices of non-equal ops
    let change_idxs: Vec<usize> = edits
        .iter()
        .enumerate()
        .filter(|(_, e)| e.op != DiffOp::Equal)
        .map(|(i, _)| i)
        .collect();
    if change_idxs.is_empty() {
        return Vec::new();
    }

    // Merge change regions with context padding
    let mut regions: Vec<(usize, usize)> = Vec::new();
    let mut r_start = change_idxs[0];
    let mut r_end = change_idxs[0];
    for &idx in &change_idxs[1..] {
        if idx <= r_end + context * 2 + 1 {
            r_end = idx;
        } else {
            regions.push((r_start, r_end));
            r_start = idx;
            r_end = idx;
        }
    }
    regions.push((r_start, r_end));

    let mut hunks = Vec::new();
    for (cs, ce) in regions {
        let start = cs.saturating_sub(context);
        let end = (ce + context + 1).min(edits.len());

        let mut lines = Vec::new();
        let mut old_start = None;
        let mut old_end = 0usize;
        let mut new_start = None;
        let mut new_end = 0usize;

        for e in &edits[start..end] {
            match e.op {
                DiffOp::Equal => {
                    if old_start.is_none() {
                        old_start = Some(e.old_idx);
                    }
                    if new_start.is_none() {
                        new_start = Some(e.new_idx);
                    }
                    old_end = e.old_idx + 1;
                    new_end = e.new_idx + 1;
                    lines.push(format!(" {}", e.text));
                }
                DiffOp::Delete => {
                    if old_start.is_none() {
                        old_start = Some(e.old_idx);
                    }
                    old_end = e.old_idx + 1;
                    // new position stays
                    if new_start.is_none() {
                        new_start = Some(e.new_idx);
                    }
                    new_end = e.new_idx;
                    lines.push(format!("-{}", e.text));
                }
                DiffOp::Insert => {
                    if new_start.is_none() {
                        new_start = Some(e.new_idx);
                    }
                    new_end = e.new_idx + 1;
                    if old_start.is_none() {
                        old_start = Some(e.old_idx);
                    }
                    old_end = e.old_idx;
                    lines.push(format!("+{}", e.text));
                }
            }
        }

        hunks.push(Hunk {
            old_start: old_start.unwrap_or(0),
            old_end,
            new_start: new_start.unwrap_or(0),
            new_end,
            lines,
        });
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_roundtrip() {
        let raw = "a\r\nb\r\n";
        assert_eq!(detect_line_ending(raw), LineEnding::Crlf);
        let lf = normalize_to_lf(raw);
        assert_eq!(lf, "a\nb\n");
        assert_eq!(apply_line_ending(&lf, LineEnding::Crlf), raw);
    }

    #[test]
    fn exact_replace() {
        let r = apply_edit_lf("hello world\n", "world", "one", false).unwrap();
        assert_eq!(r.content_lf, "hello one\n");
        assert_eq!(r.strategy, MatchStrategy::Exact);
        assert_eq!(r.replacements, 1);
    }

    #[test]
    fn trailing_whitespace_fuzzy() {
        // File line has trailing spaces; model omits them.
        let content = "fn main() {  \n    ok\n}\n";
        let old = "fn main() {\n    ok\n}";
        let new = "fn main() {\n    ok();\n}";
        let r = apply_edit_lf(content, old, new, false).unwrap();
        assert_eq!(r.strategy, MatchStrategy::Fuzzy);
        assert!(r.content_lf.contains("ok();"));
        assert!(!r.content_lf.contains("ok\n}"));
    }

    #[test]
    fn smart_quotes_fuzzy() {
        let content = "msg = \u{201C}hello\u{201D}\n";
        let old = "msg = \"hello\"";
        let new = "msg = \"hi\"";
        let r = apply_edit_lf(content, old, new, false).unwrap();
        assert_eq!(r.strategy, MatchStrategy::Fuzzy);
        assert_eq!(r.content_lf, "msg = \"hi\"\n");
    }

    #[test]
    fn multiple_requires_replace_all() {
        let err = apply_edit_lf("foo\nfoo\n", "foo", "bar", false).unwrap_err();
        assert!(matches!(err, EditApplyError::Multiple { count: 2 }));
        let r = apply_edit_lf("foo\nfoo\n", "foo", "bar", true).unwrap();
        assert_eq!(r.replacements, 2);
        assert_eq!(r.content_lf, "bar\nbar\n");
    }

    #[test]
    fn not_found() {
        let err = apply_edit_lf("abc\n", "zzz", "y", false).unwrap_err();
        assert!(matches!(err, EditApplyError::NotFound));
        assert!(err.user_message().contains("re-read"));
    }

    #[test]
    fn unified_diff_shows_change() {
        let d = unified_diff("a.rs", "a\nb\nc\n", "a\nB\nc\n", 1);
        assert!(d.contains("--- a/a.rs"));
        assert!(d.contains("-b"));
        assert!(d.contains("+B"));
    }

    #[test]
    fn large_file_surgical_edit_is_compact_hunk() {
        let mut before = String::new();
        for i in 0..4000 {
            before.push_str(&format!("line {i}\n"));
        }
        before.push_str("CHANGE_ME\n");
        for i in 0..4000 {
            before.push_str(&format!("tail {i}\n"));
        }
        let after = before.replace("CHANGE_ME", "CHANGED");
        let d = unified_diff("big.rs", &before, &after, 3);
        assert!(d.contains("-CHANGE_ME"), "{d}");
        assert!(d.contains("+CHANGED"), "{d}");
        // Must not dump ~16k dual-copy lines.
        assert!(d.lines().count() < 40, "lines={}", d.lines().count());
        let success = format_edit_success("big.rs", &before, &after, 1, MatchStrategy::Exact);
        assert!(success.summary.contains("Updated big.rs"));
        assert!(success.summary.contains("+1"));
        assert!(!success.summary.contains("--- a/"));
        assert_eq!(success.additions, 1);
        assert_eq!(success.deletions, 1);
        assert!(patch_for_details(&success.patch).is_some());
    }

    #[test]
    fn line_trimmed_ignores_indent_spaces() {
        let content = "    fn foo() {\n        bar();\n    }\n";
        // Model used different indent on the block.
        let old = "fn foo() {\n    bar();\n}";
        let new = "fn foo() {\n    bar();\n    baz();\n}";
        let r = apply_edit_lf(content, old, new, false).unwrap();
        assert!(
            matches!(
                r.strategy,
                MatchStrategy::LineTrimmed
                    | MatchStrategy::IndentationFlexible
                    | MatchStrategy::Fuzzy
            ),
            "strategy={:?}",
            r.strategy
        );
        assert!(r.content_lf.contains("baz()"), "{}", r.content_lf);
    }

    #[test]
    fn whitespace_normalized_collapses_spaces() {
        let content = "return   x   +   y;\n";
        let old = "return x + y;";
        let new = "return x + y + z;";
        let r = apply_edit_lf(content, old, new, false).unwrap();
        assert!(
            matches!(
                r.strategy,
                MatchStrategy::WhitespaceNormalized
                    | MatchStrategy::LineTrimmed
                    | MatchStrategy::Fuzzy
            ),
            "{:?}",
            r.strategy
        );
        assert!(r.content_lf.contains("x + y + z"));
    }

    #[test]
    fn indentation_flexible() {
        let content = "\t\titem: 1,\n\t\titem: 2,\n";
        let old = "item: 1,\nitem: 2,";
        let new = "item: 1,\nitem: 2,\nitem: 3,";
        let r = apply_edit_lf(content, old, new, false).unwrap();
        assert!(
            matches!(
                r.strategy,
                MatchStrategy::IndentationFlexible
                    | MatchStrategy::LineTrimmed
                    | MatchStrategy::WhitespaceNormalized
            ),
            "{:?}",
            r.strategy
        );
        assert!(r.content_lf.contains("item: 3"));
    }

    #[test]
    fn block_anchor_tolerates_middle_typo() {
        let content = "fn run() {\n    let x = 1;\n    let y = 2;\n    done();\n}\n";
        // Middle line slightly wrong (model typo) but anchors match.
        let old = "fn run() {\n    let x = 999;\n    let y = 2;\n    done();\n}";
        let new = "fn run() {\n    let x = 1;\n    let y = 2;\n    done();\n    ok();\n}";
        let r = apply_edit_lf(content, old, new, false).unwrap();
        assert_eq!(r.strategy, MatchStrategy::BlockAnchor);
        assert!(r.content_lf.contains("ok();"), "{}", r.content_lf);
    }

    #[test]
    fn strips_read_line_number_prefixes() {
        let content = "hello world\n";
        let old = "1|hello world";
        let new = "1|hello one";
        let r = apply_edit_lf(content, old, new, false).unwrap();
        assert_eq!(r.content_lf, "hello one\n");
    }

    #[test]
    fn strip_line_prefix_helper() {
        assert_eq!(
            strip_line_number_prefixes("10|fn main() {\n11|    ok\n12|}"),
            "fn main() {\n    ok\n}"
        );
        // Not numbered → unchanged
        assert_eq!(strip_line_number_prefixes("fn main() {}"), "fn main() {}");
    }
}
