//! Grok Build–style `/context` snapshot and categorical bar.
//!
//! Layout (wide):
//! ```text
//! Context
//!
//! 36.7k / 1.0m tokens (3.67%)
//! provider:model
//!
//! ◆ ◆ ◆ ◇ ◇ …   (100 cells: system / messages / overhead / free)
//!
//! ◆ System prompt     1.2k tokens  (0.1%)
//! ◆ Messages         29.9k tokens    (3%)
//! ◆ Reasoning           4.0k tokens  (0.4%)
//! ◆ Overhead            1.6k tokens  (0.2%)
//! ◇ Free              963k tokens   (96%)
//!
//! ◈ Tool definitions  5.6k tokens  (0.6%) · 12 tools
//! ◈ Skills            2.4k tokens  (0.2%) · 21 skills
//! ◈ MCP servers        320 tokens  (0.1%) ·  4 servers
//!
//! Auto-compact at 85% · ~812k tokens remaining
//!
//! Turns: 5 · Tool calls: 12 · Compactions: 0
//! ```
//!
//! Tool / skill / MCP rows are informational: they never enter the bar
//! (their tokens overlap system / messages / overhead).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

const SYSTEM_GLYPH: &str = "\u{25C6}"; // ◆
const MESSAGES_GLYPH: &str = "\u{25C6}";
const OVERHEAD_GLYPH: &str = "\u{25C6}";
const FREE_GLYPH: &str = "\u{25C7}"; // ◇
const INFO_GLYPH: &str = "\u{25C8}"; // ◈

/// Informational usage row (skills listing, MCP announcements).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUsageCategory {
    pub label: String,
    pub tokens: u64,
    pub detail: Option<String>,
}

impl TokenUsageCategory {
    pub fn skills(tokens: u64, count: u64) -> Self {
        Self {
            label: "Skills".into(),
            tokens,
            detail: Some(count_detail(count, "skill")),
        }
    }

    pub fn mcp_servers(tokens: u64, count: u64) -> Self {
        Self {
            label: "MCP servers".into(),
            tokens,
            detail: Some(count_detail(count, "server")),
        }
    }
}

/// `1 tool` / `12 tools` — count-then-noun, matching Grok's legend.
pub fn count_detail(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Snapshot of context-window usage at the moment `/context` ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSnapshot {
    pub used: u64,
    pub total: u64,
    pub system_prompt_tokens: u64,
    pub tool_definitions_count: u64,
    pub tool_definitions_tokens: u64,
    pub compaction_count: u64,
    pub turn_count: u64,
    pub tool_call_count: u64,
    pub message_count: u64,
    pub message_tokens: u64,
    /// Thinking / encrypted reasoning, scaled when API `used` is known.
    pub reasoning_tokens: u64,
    pub free_tokens: u64,
    pub usage_pct: u8,
    pub auto_compact_threshold_percent: u8,
    /// False when `used` is a local char/4 estimate (not last API prompt size).
    pub used_estimated: bool,
    /// Auto-compact is enabled (otherwise the ETA line says it's off).
    pub auto_compact_enabled: bool,
    pub model: String,
    pub usage_categories: Vec<TokenUsageCategory>,
}

impl ContextSnapshot {
    /// Build styled body lines for the given content width.
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        self.build_lines(BarLayout::for_width(width))
    }
}

/// 100-cell bar: wide 5×20 (~39 cols) or narrow 10×10 (~19 cols).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BarLayout {
    row_len: usize,
    rows: usize,
}

impl BarLayout {
    const WIDE: Self = Self {
        row_len: 20,
        rows: 5,
    };
    const NARROW: Self = Self {
        row_len: 10,
        rows: 10,
    };
    const NARROW_BREAKPOINT: u16 = 50;

    fn for_width(width: u16) -> Self {
        if width < Self::NARROW_BREAKPOINT {
            Self::NARROW
        } else {
            Self::WIDE
        }
    }

    const fn total(self) -> usize {
        self.row_len * self.rows
    }
}

struct LegendRow {
    glyph: &'static str,
    color: Color,
    label: String,
    tokens: u64,
    detail: Option<String>,
}

struct RowLayout {
    label_width: usize,
    tokens_width: usize,
    percent_width: usize,
    count_width: usize,
}

impl RowLayout {
    fn measure<'a>(rows: impl Iterator<Item = &'a LegendRow> + Clone, total: u64) -> Self {
        Self {
            label_width: rows
                .clone()
                .map(|r| r.label.chars().count())
                .max()
                .unwrap_or(0)
                + 1,
            tokens_width: rows
                .clone()
                .map(|r| fmt_tok(r.tokens).chars().count())
                .max()
                .unwrap_or(0),
            percent_width: rows
                .clone()
                .map(|r| Self::percent(r.tokens, total).chars().count())
                .max()
                .unwrap_or(0),
            count_width: rows
                .filter_map(|r| r.detail.as_deref())
                .filter_map(|d| d.split(' ').next().map(|n| n.chars().count()))
                .max()
                .unwrap_or(0),
        }
    }

    fn percent(tokens: u64, total: u64) -> String {
        format!("({})", percent_of_window(tokens, total))
    }

    fn cells(&self, tokens: u64, total: u64) -> String {
        format!(
            "{:>tokens_width$} tokens   {:>percent_width$}",
            fmt_tok(tokens),
            Self::percent(tokens, total),
            tokens_width = self.tokens_width,
            percent_width = self.percent_width,
        )
    }

    fn detail_suffix(&self, detail: &str) -> String {
        match detail.split_once(' ') {
            Some((count, rest)) => {
                format!(
                    " \u{00b7} {count:>count_width$} {rest}",
                    count_width = self.count_width
                )
            }
            None => format!(" \u{00b7} {detail}"),
        }
    }

    fn render(
        &self,
        row: &LegendRow,
        bar: BarLayout,
        total: u64,
        label_style: Style,
        muted: Style,
    ) -> Vec<Line<'static>> {
        let glyph = Span::styled(format!("{} ", row.glyph), Style::default().fg(row.color));
        let suffix = row.detail.as_deref().map(|d| self.detail_suffix(d));
        if bar == BarLayout::NARROW {
            let first = Line::from(vec![glyph, Span::styled(row.label.clone(), label_style)]);
            let mut second = vec![
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{} tokens   {}",
                        fmt_tok(row.tokens),
                        Self::percent(row.tokens, total)
                    ),
                    muted,
                ),
            ];
            if let Some(extra) = suffix {
                second.push(Span::styled(extra, muted));
            }
            vec![first, Line::from(second)]
        } else {
            let mut spans = vec![
                glyph,
                Span::styled(
                    format!(
                        "{:<label_width$}",
                        row.label,
                        label_width = self.label_width
                    ),
                    label_style,
                ),
                Span::raw(" "),
                Span::styled(self.cells(row.tokens, total), muted),
            ];
            if let Some(extra) = suffix {
                spans.push(Span::styled(extra, muted));
            }
            vec![Line::from(spans)]
        }
    }
}

impl ContextSnapshot {
    fn build_lines(&self, bar: BarLayout) -> Vec<Line<'static>> {
        let used = self.used;
        let total = self.total;
        let usage_pct = self.usage_pct;
        let system_tokens = self.system_prompt_tokens;
        let message_tokens = self.message_tokens;
        let reasoning_tokens = self.reasoning_tokens;
        let free_tokens = self.free_tokens;
        let leftover = used
            .saturating_sub(system_tokens)
            .saturating_sub(message_tokens)
            .saturating_sub(reasoning_tokens);

        let muted = Style::default().fg(Theme::MUTED).bg(Theme::PANEL);
        let label_style = Style::default().fg(Theme::MUTED).bg(Theme::PANEL);
        let primary = Style::default()
            .fg(Theme::FG)
            .bg(Theme::PANEL)
            .add_modifier(Modifier::BOLD);
        let secondary = Style::default().fg(Theme::FG).bg(Theme::PANEL);

        let system_color = Theme::MUTED;
        let messages_color = Theme::FG;
        let reasoning_color = Theme::ACCENT;
        let overhead_color = Theme::SECONDARY;
        let empty_color = Theme::BORDER;
        let tools_color = Theme::INFO;

        let total_cells = bar.total();
        let cells_for = |tokens: u64| -> usize {
            if total == 0 {
                0
            } else {
                ((tokens as f64 / total as f64) * total_cells as f64).round() as usize
            }
        };
        let used_cells = cells_for(used).min(total_cells);
        let system_cells = cells_for(system_tokens).min(used_cells);
        let messages_cells = cells_for(message_tokens).min(used_cells.saturating_sub(system_cells));
        let reasoning_cells = cells_for(reasoning_tokens)
            .min(used_cells.saturating_sub(system_cells + messages_cells));
        let overhead_cells = used_cells
            .saturating_sub(system_cells)
            .saturating_sub(messages_cells)
            .saturating_sub(reasoning_cells);
        let free_cells = total_cells.saturating_sub(used_cells);

        let mut cells: Vec<(&'static str, Color)> = Vec::with_capacity(total_cells);
        for _ in 0..system_cells {
            cells.push((SYSTEM_GLYPH, system_color));
        }
        for _ in 0..messages_cells {
            cells.push((MESSAGES_GLYPH, messages_color));
        }
        for _ in 0..reasoning_cells {
            cells.push((OVERHEAD_GLYPH, reasoning_color));
        }
        for _ in 0..overhead_cells {
            cells.push((OVERHEAD_GLYPH, overhead_color));
        }
        for _ in 0..free_cells {
            cells.push((FREE_GLYPH, empty_color));
        }
        debug_assert_eq!(cells.len(), total_cells);

        let mut bar_lines: Vec<Line<'static>> = Vec::with_capacity(bar.rows);
        for row_idx in 0..bar.rows {
            let start = row_idx * bar.row_len;
            let end = (start + bar.row_len).min(cells.len());
            let mut spans = Vec::with_capacity(bar.row_len * 2);
            for (i, (glyph, color)) in cells[start..end].iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.push(Span::styled(
                    (*glyph).to_string(),
                    Style::default().fg(*color).bg(Theme::PANEL),
                ));
            }
            bar_lines.push(Line::from(spans));
        }

        let mut legend_rows = vec![
            LegendRow {
                glyph: SYSTEM_GLYPH,
                color: system_color,
                label: "System prompt".into(),
                tokens: system_tokens,
                detail: None,
            },
            LegendRow {
                glyph: MESSAGES_GLYPH,
                color: messages_color,
                label: "Messages".into(),
                tokens: message_tokens,
                detail: None,
            },
        ];
        if reasoning_tokens > 0 {
            legend_rows.push(LegendRow {
                glyph: OVERHEAD_GLYPH,
                color: reasoning_color,
                label: "Reasoning".into(),
                tokens: reasoning_tokens,
                detail: None,
            });
        }
        if leftover > 0 {
            legend_rows.push(LegendRow {
                glyph: OVERHEAD_GLYPH,
                color: overhead_color,
                label: "Overhead".into(),
                tokens: leftover,
                detail: None,
            });
        }
        legend_rows.push(LegendRow {
            glyph: FREE_GLYPH,
            color: empty_color,
            label: "Free".into(),
            tokens: free_tokens,
            detail: None,
        });

        let info_rows: Vec<LegendRow> = std::iter::once(LegendRow {
            glyph: INFO_GLYPH,
            color: tools_color,
            label: "Tool definitions".into(),
            tokens: self.tool_definitions_tokens,
            detail: Some(count_detail(self.tool_definitions_count, "tool")),
        })
        .chain(self.usage_categories.iter().map(|c| LegendRow {
            glyph: INFO_GLYPH,
            color: tools_color,
            label: c.label.clone(),
            tokens: c.tokens,
            detail: c.detail.clone(),
        }))
        .collect();

        let layout = RowLayout::measure(legend_rows.iter().chain(info_rows.iter()), total);

        let approx = if self.used_estimated { "~" } else { "" };
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled("Context", primary)),
            Line::from(""),
            Line::from(Span::styled(
                format!(
                    "{approx}{} / {} tokens ({:.2}%)",
                    fmt_tok_big(used),
                    fmt_tok_big(total),
                    precise_usage_percent(used, total),
                ),
                secondary,
            )),
            Line::from(Span::styled(
                self.model.clone(),
                Style::default().fg(Theme::MUTED).bg(Theme::PANEL),
            )),
            Line::from(""),
        ];
        lines.extend(bar_lines);
        lines.push(Line::from(""));
        for row in &legend_rows {
            lines.extend(layout.render(row, bar, total, label_style, muted));
        }
        lines.push(Line::from(""));
        for row in &info_rows {
            lines.extend(layout.render(row, bar, total, label_style, muted));
        }
        lines.push(Line::from(""));

        if total > 0 {
            let threshold_percent = self.auto_compact_threshold_percent;
            let (text, style) = if !self.auto_compact_enabled {
                (
                    "Auto-compact off \u{00b7} /compact to reclaim space".to_string(),
                    muted,
                )
            } else {
                let threshold_tokens = total.saturating_mul(threshold_percent as u64).div_ceil(100);
                let remaining = threshold_tokens.saturating_sub(used);
                if usage_pct >= threshold_percent {
                    (
                        format!("Auto-compact triggers next turn (at {threshold_percent}%)"),
                        Style::default().fg(Theme::WARNING).bg(Theme::PANEL),
                    )
                } else {
                    (
                        format!(
                            "Auto-compact at {threshold_percent}% \u{00b7} ~{} tokens remaining",
                            fmt_tok_big(remaining)
                        ),
                        muted,
                    )
                }
            };
            lines.push(Line::from(Span::styled(text, style)));
            lines.push(Line::from(""));
        }

        lines.push(Line::from(Span::styled(
            format!(
                "Turns: {} \u{00b7} Tool calls: {} \u{00b7} Compactions: {}",
                self.turn_count, self.tool_call_count, self.compaction_count
            ),
            muted,
        )));

        if self.auto_compact_enabled
            && (80..self.auto_compact_threshold_percent).contains(&usage_pct)
        {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Tip: run /compact to free up context space.".to_string(),
                Style::default().fg(Theme::WARNING).bg(Theme::PANEL),
            )));
        }

        lines
    }
}

/// Compact token count (`123`, `1.2k`, `100k`).
pub fn fmt_tok(n: u64) -> String {
    if n >= 99_500 {
        format!("{}k", (n + 500) / 1000)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{n}")
    }
}

fn precise_usage_percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64) * 100.0
    }
}

/// Like [`fmt_tok`] but rolls over to `1.0m` at one million.
pub fn fmt_tok_big(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else {
        fmt_tok(n)
    }
}

fn percent_of_window(part: u64, total: u64) -> String {
    if total == 0 {
        return "-".to_string();
    }
    let p = ((part as f64 / total as f64) * 100.0).max(if part > 0 { 0.1 } else { 0.0 });
    if p < 10.0 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ContextSnapshot {
        ContextSnapshot {
            used: 36_700,
            total: 1_000_000,
            system_prompt_tokens: 1_200,
            tool_definitions_count: 12,
            tool_definitions_tokens: 5_600,
            compaction_count: 0,
            turn_count: 5,
            tool_call_count: 12,
            message_count: 8,
            message_tokens: 29_900,
            reasoning_tokens: 0,
            free_tokens: 963_300,
            usage_pct: 4,
            auto_compact_threshold_percent: 85,
            used_estimated: false,
            auto_compact_enabled: true,
            model: "grok-4".into(),
            usage_categories: vec![],
        }
    }

    fn line_text(lines: &[Line<'static>], idx: usize) -> String {
        lines[idx]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn all_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()).chain(["\n"]))
            .collect()
    }

    fn count_bar_glyphs(lines: &[Line<'static>], layout: BarLayout) -> (usize, usize, usize) {
        let mut diamonds = 0usize;
        let mut info = 0usize;
        let mut free = 0usize;
        let bar_start = 5;
        let bar_end = bar_start + layout.rows;
        for line in &lines[bar_start..bar_end] {
            for span in &line.spans {
                let c = span.content.as_ref();
                if c == SYSTEM_GLYPH || c == MESSAGES_GLYPH {
                    diamonds += 1;
                } else if c == INFO_GLYPH {
                    info += 1;
                } else if c == FREE_GLYPH {
                    free += 1;
                }
            }
        }
        (diamonds, info, free)
    }

    #[test]
    fn header_tokens_and_model() {
        let lines = snapshot().build_lines(BarLayout::WIDE);
        assert_eq!(line_text(&lines, 0), "Context");
        assert_eq!(line_text(&lines, 1), "");
        let l2 = line_text(&lines, 2);
        assert!(l2.contains("tokens"), "got {l2:?}");
        assert!(l2.contains("(3.67%)"), "got {l2:?}");
        assert_eq!(line_text(&lines, 3), "grok-4");
    }

    #[test]
    fn estimated_used_is_marked() {
        let mut snap = snapshot();
        snap.used_estimated = true;
        let lines = snap.build_lines(BarLayout::WIDE);
        let l2 = line_text(&lines, 2);
        assert!(l2.starts_with('~'), "got {l2:?}");
    }

    #[test]
    fn auto_compact_eta() {
        let all = all_text(&snapshot().build_lines(BarLayout::WIDE));
        assert!(
            all.contains("Auto-compact at 85%") && all.contains("~813k tokens remaining"),
            "got:\n{all}"
        );
    }

    #[test]
    fn auto_compact_off() {
        let mut snap = snapshot();
        snap.auto_compact_enabled = false;
        let all = all_text(&snap.build_lines(BarLayout::WIDE));
        assert!(all.contains("Auto-compact off"), "got:\n{all}");
        assert!(!all.contains("/compact to free"));
    }

    #[test]
    fn tip_in_warning_band() {
        let mut snap = snapshot();
        snap.usage_pct = 80;
        let all = all_text(&snap.build_lines(BarLayout::WIDE));
        assert!(all.contains("/compact"), "got:\n{all}");
    }

    #[test]
    fn tip_omitted_at_threshold() {
        let mut snap = snapshot();
        snap.usage_pct = 85;
        let all = all_text(&snap.build_lines(BarLayout::WIDE));
        assert!(all.contains("Auto-compact triggers next turn"));
        assert!(!all.contains("Tip: run /compact"));
    }

    #[test]
    fn bar_is_one_hundred_cells() {
        let (diamonds, info, free) =
            count_bar_glyphs(&snapshot().build_lines(BarLayout::WIDE), BarLayout::WIDE);
        assert_eq!(diamonds + info + free, 100);
        assert_eq!(info, 0, "informational glyphs must never enter the bar");
        assert_eq!(diamonds, 4); // 36700/1e6 ≈ 4%
        assert_eq!(free, 96);
    }

    #[test]
    fn bar_does_not_overshoot_when_estimates_exceed_used() {
        let mut snap = snapshot();
        snap.total = 100_000;
        snap.used = 10_000;
        snap.system_prompt_tokens = 8_000;
        snap.message_tokens = 5_000;
        snap.tool_definitions_tokens = 0;
        snap.free_tokens = 90_000;
        snap.usage_pct = 10;
        let (diamonds, info, free) =
            count_bar_glyphs(&snap.build_lines(BarLayout::WIDE), BarLayout::WIDE);
        assert_eq!(info, 0);
        assert_eq!(diamonds, 10);
        assert_eq!(free, 90);
    }

    #[test]
    fn overhead_row_excludes_tools() {
        let snap = ContextSnapshot {
            used: 100_000,
            total: 500_000,
            system_prompt_tokens: 5_000,
            tool_definitions_count: 190,
            tool_definitions_tokens: 75_000,
            compaction_count: 0,
            turn_count: 1,
            tool_call_count: 0,
            message_count: 4,
            message_tokens: 25_000,
            reasoning_tokens: 0,
            free_tokens: 400_000,
            usage_pct: 20,
            auto_compact_threshold_percent: 65,
            used_estimated: false,
            auto_compact_enabled: true,
            model: "one".into(),
            usage_categories: vec![],
        };
        let all = all_text(&snap.build_lines(BarLayout::WIDE));
        assert!(all.contains("Overhead") && all.contains("70.0k"));
        assert!(all.contains("Tool definitions") && all.contains("190 tools"));
        let (diamonds, info, free) =
            count_bar_glyphs(&snap.build_lines(BarLayout::WIDE), BarLayout::WIDE);
        assert_eq!(info, 0);
        assert_eq!(diamonds, 20);
        assert_eq!(free, 80);
    }

    #[test]
    fn reasoning_and_overhead_are_separate_rows() {
        let mut snap = snapshot();
        snap.used = 100_000;
        snap.total = 200_000;
        snap.system_prompt_tokens = 20_000;
        snap.message_tokens = 50_000;
        snap.reasoning_tokens = 10_000;
        snap.tool_definitions_tokens = 20_000;
        snap.free_tokens = 100_000;
        snap.usage_pct = 50;
        let all = all_text(&snap.build_lines(BarLayout::WIDE));
        assert!(
            all.contains("Reasoning") && all.contains("10.0k"),
            "got:\n{all}"
        );
        assert!(
            all.contains("Overhead") && all.contains("20.0k"),
            "got:\n{all}"
        );
        assert!(!all.contains("Reasoning/overhead"));
    }

    #[test]
    fn usage_categories_render_aligned() {
        let mut snap = snapshot();
        snap.usage_categories = vec![
            TokenUsageCategory::skills(2_400, 21),
            TokenUsageCategory::mcp_servers(320, 4),
        ];
        let all = all_text(&snap.build_lines(BarLayout::WIDE));
        assert!(all.contains("Skills") && all.contains("21 skills"));
        assert!(all.contains("MCP servers") && all.contains("4 servers"));
        assert!(
            all.contains("\u{00b7}  4 servers"),
            "single-digit count pad:\n{all}"
        );
        let (_, info, _) = count_bar_glyphs(&snap.build_lines(BarLayout::WIDE), BarLayout::WIDE);
        assert_eq!(info, 0);
    }

    #[test]
    fn fmt_tok_boundaries() {
        assert_eq!(fmt_tok(0), "0");
        assert_eq!(fmt_tok(999), "999");
        assert_eq!(fmt_tok(1_000), "1.0k");
        assert_eq!(fmt_tok(99_499), "99.5k");
        assert_eq!(fmt_tok(99_500), "100k");
        assert_eq!(fmt_tok_big(1_000_000), "1.0m");
        assert_eq!(fmt_tok_big(1_500_000), "1.5m");
    }

    #[test]
    fn percent_formatting() {
        assert_eq!(percent_of_window(0, 0), "-");
        assert_eq!(percent_of_window(1, 1_000_000), "0.1%");
        assert_eq!(percent_of_window(50_000, 1_000_000), "5.0%");
        assert_eq!(percent_of_window(500_000, 1_000_000), "50%");
    }

    #[test]
    fn narrow_bar_still_100_cells() {
        let (d, i, f) = count_bar_glyphs(
            &snapshot().build_lines(BarLayout::NARROW),
            BarLayout::NARROW,
        );
        assert_eq!(d + i + f, 100);
    }

    #[test]
    fn footer_stats() {
        let all = all_text(&snapshot().build_lines(BarLayout::WIDE));
        assert!(all.contains("Turns: 5"));
        assert!(all.contains("Tool calls: 12"));
        assert!(all.contains("Compactions: 0"));
    }
}
