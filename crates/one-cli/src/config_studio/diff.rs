//! Bounded, dependency-free unified diff for the studio's change preview.
//!
//! The preview must be trustworthy enough to approve a write, so this produces
//! real unified hunks (not a "whole file replaced" summary) while staying inside
//! a hard work budget: common prefix/suffix are trimmed first and an LCS table is
//! only built for the remaining middle section. Extremely large mid-sections fall
//! back to a block replace, which is honest about what changed.

/// One line of diff output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// One of `' '` (context), `'-'` (removed), `'+'` (added).
    pub kind: char,
    /// Line content without the trailing newline.
    pub text: String,
}

impl DiffLine {
    /// Render the line in unified-diff form (prefix + content).
    pub fn render(&self) -> String {
        format!("{}{}", self.kind, self.text)
    }
}

/// A contiguous change region with surrounding context.
#[derive(Debug, Clone)]
pub struct Hunk {
    /// 1-based first line of the hunk in the original file.
    pub old_start: usize,
    /// Number of original lines covered.
    pub old_len: usize,
    /// 1-based first line of the hunk in the new file.
    pub new_start: usize,
    /// Number of new lines covered.
    pub new_len: usize,
    /// Lines in the hunk.
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// `@@ -a,b +c,d @@` header.
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_len, self.new_start, self.new_len
        )
    }
}

/// Number of unchanged lines kept around each change.
const CONTEXT: usize = 3;

/// Largest mid-section (per side) that will be diffed exactly.
const MAX_EXACT_LINES: usize = 3000;

/// Result of diffing two texts.
#[derive(Debug, Clone)]
pub struct Diff {
    /// Grouped hunks; empty when the texts are identical.
    pub hunks: Vec<Hunk>,
    /// Lines added across all hunks.
    pub added: usize,
    /// Lines removed across all hunks.
    pub removed: usize,
    /// True when the mid-section exceeded [`MAX_EXACT_LINES`] and was replaced
    /// wholesale instead of being diffed exactly.
    pub approximate: bool,
}

impl Diff {
    /// Whether the two texts were identical.
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// Render a complete unified diff, or an empty string when unchanged.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for hunk in &self.hunks {
            out.push_str(&hunk.header());
            out.push('\n');
            for line in &hunk.lines {
                out.push_str(&line.render());
                out.push('\n');
            }
        }
        out
    }
}

/// Split into lines, ignoring a single trailing newline.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n').collect()
}

/// Compute a unified diff between `old` and `new`.
pub fn unified_diff(old: &str, new: &str) -> Diff {
    if old == new {
        return Diff {
            hunks: Vec::new(),
            added: 0,
            removed: 0,
            approximate: false,
        };
    }

    let a = split_lines(old);
    let b = split_lines(new);

    // Trim the common prefix and suffix so the LCS table stays small.
    let mut prefix = 0;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < a.len() - prefix
        && suffix < b.len() - prefix
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let a_mid = &a[prefix..a.len() - suffix];
    let b_mid = &b[prefix..b.len() - suffix];

    let approximate = a_mid.len() > MAX_EXACT_LINES || b_mid.len() > MAX_EXACT_LINES;
    let ops: Vec<DiffLine> = if approximate {
        let mut ops = Vec::with_capacity(a_mid.len() + b_mid.len());
        ops.extend(a_mid.iter().map(|t| DiffLine {
            kind: '-',
            text: (*t).to_string(),
        }));
        ops.extend(b_mid.iter().map(|t| DiffLine {
            kind: '+',
            text: (*t).to_string(),
        }));
        ops
    } else {
        lcs_ops(a_mid, b_mid)
    };

    if ops.is_empty() {
        return Diff {
            hunks: Vec::new(),
            added: 0,
            removed: 0,
            approximate,
        };
    }

    // Re-attach context and index the merged line stream.
    let mut merged: Vec<(char, String)> = Vec::with_capacity(a.len().max(b.len()) + 8);
    for line in &a[..prefix] {
        merged.push((' ', (*line).to_string()));
    }
    for op in &ops {
        merged.push((op.kind, op.text.clone()));
    }
    for line in &b[b.len() - suffix..] {
        merged.push((' ', (*line).to_string()));
    }

    let added = ops.iter().filter(|o| o.kind == '+').count();
    let removed = ops.iter().filter(|o| o.kind == '-').count();

    Diff {
        hunks: group_hunks(&merged, prefix),
        added,
        removed,
        approximate,
    }
}

/// Longest-common-subsequence diff of the trimmed middle sections.
fn lcs_ops(a: &[&str], b: &[&str]) -> Vec<DiffLine> {
    let n = a.len();
    let m = b.len();
    if n == 0 || m == 0 {
        let mut ops = Vec::with_capacity(n + m);
        ops.extend(a.iter().map(|t| DiffLine {
            kind: '-',
            text: (*t).to_string(),
        }));
        ops.extend(b.iter().map(|t| DiffLine {
            kind: '+',
            text: (*t).to_string(),
        }));
        return ops;
    }

    // (n+1) x (m+1) table of LCS lengths. Bounded by MAX_EXACT_LINES.
    let mut table = vec![0u32; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[idx(i, j)] = if a[i] == b[j] {
                table[idx(i + 1, j + 1)] + 1
            } else {
                table[idx(i + 1, j)].max(table[idx(i, j + 1)])
            };
        }
    }

    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(DiffLine {
                kind: ' ',
                text: a[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if table[idx(i + 1, j)] >= table[idx(i, j + 1)] {
            ops.push(DiffLine {
                kind: '-',
                text: a[i].to_string(),
            });
            i += 1;
        } else {
            ops.push(DiffLine {
                kind: '+',
                text: b[j].to_string(),
            });
            j += 1;
        }
    }
    while i < n {
        ops.push(DiffLine {
            kind: '-',
            text: a[i].to_string(),
        });
        i += 1;
    }
    while j < m {
        ops.push(DiffLine {
            kind: '+',
            text: b[j].to_string(),
        });
        j += 1;
    }
    ops
}

/// Group a merged line stream into hunks with `CONTEXT` lines of padding.
///
/// `leading_context` is how many unchanged lines were trimmed from the front, so
/// hunk headers can be numbered against the original files.
fn group_hunks(merged: &[(char, String)], leading_context: usize) -> Vec<Hunk> {
    // Indexes of changed lines.
    let changed: Vec<usize> = merged
        .iter()
        .enumerate()
        .filter(|(_, (kind, _))| *kind != ' ')
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }

    // Build [start, end) ranges around change clusters.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = changed[0].saturating_sub(CONTEXT);
    let mut end = (changed[0] + CONTEXT + 1).min(merged.len());
    for &c in &changed[1..] {
        let c_start = c.saturating_sub(CONTEXT);
        if c_start <= end {
            end = (c + CONTEXT + 1).min(merged.len());
        } else {
            ranges.push((start, end));
            start = c_start;
            end = (c + CONTEXT + 1).min(merged.len());
        }
    }
    ranges.push((start, end));

    // Track the line number in each side as we walk.
    let mut hunks = Vec::new();
    for (rs, re) in ranges {
        let mut old_line = leading_context + 1;
        let mut new_line = leading_context + 1;
        for (kind, _) in &merged[..rs] {
            match kind {
                ' ' => {
                    old_line += 1;
                    new_line += 1;
                }
                '-' => old_line += 1,
                '+' => new_line += 1,
                _ => {}
            }
        }

        let mut old_len = 0;
        let mut new_len = 0;
        let mut lines = Vec::with_capacity(re - rs);
        for (kind, text) in &merged[rs..re] {
            match kind {
                ' ' => {
                    old_len += 1;
                    new_len += 1;
                }
                '-' => old_len += 1,
                '+' => new_len += 1,
                _ => {}
            }
            lines.push(DiffLine {
                kind: *kind,
                text: text.clone(),
            });
        }

        hunks.push(Hunk {
            old_start: old_line,
            old_len,
            new_start: new_line,
            new_len,
            lines,
        });
    }

    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_has_no_hunks() {
        let d = unified_diff("{}\n", "{}\n");
        assert!(d.is_empty());
        assert_eq!(d.added, 0);
        assert_eq!(d.removed, 0);
    }

    #[test]
    fn reports_single_line_change_with_context() {
        let old = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
        let new = "{\n  \"a\": 1,\n  \"b\": 3\n}\n";
        let d = unified_diff(old, new);
        assert_eq!(d.added, 1);
        assert_eq!(d.removed, 1);
        assert_eq!(d.hunks.len(), 1);
        let rendered = d.render();
        assert!(rendered.contains("-  \"b\": 2"));
        assert!(rendered.contains("+  \"b\": 3"));
        // Context on both sides is retained.
        assert!(rendered.contains("   \"a\": 1"));
        assert!(rendered.contains(" }"));
    }

    #[test]
    fn handles_added_and_removed_lines() {
        let old = "one\ntwo\nthree\n";
        let new = "one\nthree\nfour\n";
        let d = unified_diff(old, new);
        assert_eq!(d.removed, 1);
        assert_eq!(d.added, 1);
        assert!(d.render().contains("-two"));
        assert!(d.render().contains("+four"));
    }

    #[test]
    fn empty_to_content_is_all_added() {
        let d = unified_diff("", "hello\n");
        assert_eq!(d.added, 1);
        assert_eq!(d.removed, 0);
        assert!(d.render().contains("+hello"));
    }

    #[test]
    fn large_mid_section_falls_back_to_block_replace() {
        let old = (0..MAX_EXACT_LINES + 50)
            .map(|i| format!("old-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..MAX_EXACT_LINES + 50)
            .map(|i| format!("new-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = unified_diff(&old, &new);
        assert!(d.approximate);
        assert!(!d.is_empty());
    }
}
