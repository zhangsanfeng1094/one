//! Markdown → ratatui [`Line`]s (OpenCode-style terminal chrome).
//!
//! Supports CommonMark + GFM tables / strikethrough / task lists:
//! headings, paragraphs, emphasis, inline code, fenced code, lists,
//! blockquotes, rules, and pipe tables.

use pulldown_cmark::{
    Alignment, CodeBlockKind, CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;
use crate::ui::text::expand_tabs;

/// Render markdown into display lines that fit `width` terminal columns.
///
/// Leading indent (`  `) is applied by the caller for assistant messages.
pub fn render(content: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let content = rewrite_ascii_tables(content);
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(&content, opts);
    let mut w = Writer::new(width);
    w.run(parser);
    w.finish()
}

// ── writer ──────────────────────────────────────────────────────────────────

struct Writer {
    width: usize,
    lines: Vec<Line<'static>>,

    /// Styled runs for the current block (paragraph / heading / item / cell).
    runs: Vec<(String, Style)>,
    style: Style,

    list_stack: Vec<ListState>,
    /// Pending marker text when starting a list item (e.g. `"• "` or `"1. "`).
    item_prefix: Option<String>,
    blockquote_depth: usize,

    in_code_block: bool,
    code_lang: String,
    code_lines: Vec<String>,

    /// Active table accumulation.
    table: Option<TableBuild>,
    /// Cell text while inside a table cell.
    cell_buf: String,
    in_table_cell: bool,
}

struct ListState {
    ordered: bool,
    next_index: u64,
}

struct TableBuild {
    alignments: Vec<Alignment>,
    /// rows of cells (strings, already plain text)
    header: Vec<String>,
    body: Vec<Vec<String>>,
    current_row: Vec<String>,
}

impl Writer {
    fn new(width: usize) -> Self {
        Self {
            width,
            lines: Vec::new(),
            runs: Vec::new(),
            style: Theme::assistant_body(),
            list_stack: Vec::new(),
            item_prefix: None,
            blockquote_depth: 0,
            in_code_block: false,
            code_lang: String::new(),
            code_lines: Vec::new(),
            table: None,
            cell_buf: String::new(),
            in_table_cell: false,
        }
    }

    fn run<'a, I: Iterator<Item = Event<'a>>>(&mut self, iter: I) {
        for ev in iter {
            match ev {
                Event::Start(tag) => self.start_tag(tag),
                Event::End(tag) => self.end_tag(tag),
                Event::Text(text) => self.text(text),
                Event::Code(code) => self.code(code),
                Event::SoftBreak => self.soft_break(),
                Event::HardBreak => self.hard_break(),
                Event::Rule => self.rule(),
                Event::TaskListMarker(checked) => self.task_marker(checked),
                Event::Html(_) | Event::InlineHtml(_) | Event::FootnoteReference(_) => {}
                Event::InlineMath(s) | Event::DisplayMath(s) => {
                    self.push_run(s.to_string(), Theme::code());
                }
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        // Flush any trailing open block (incomplete stream).
        if self.in_code_block {
            self.flush_code_block();
        }
        if self.table.is_some() {
            self.flush_table();
        }
        if !self.runs.is_empty() {
            self.flush_paragraph();
        }
        self.lines
    }

    // ── tags ────────────────────────────────────────────────────────────

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.runs.clear();
                self.style = if self.blockquote_depth > 0 {
                    Theme::blockquote()
                } else {
                    Theme::assistant_body()
                };
            }
            Tag::Heading { level, .. } => {
                self.runs.clear();
                self.style = heading_style(level);
            }
            Tag::BlockQuote(_) => {
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.in_code_block = true;
                self.code_lines.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Tag::List(start) => {
                // Nested lists arrive before the parent Item ends; flush the
                // parent marker + text so the child's `runs.clear()` cannot
                // drop it.
                if self.item_prefix.is_some() {
                    self.flush_list_item();
                }
                let ordered = start.is_some();
                self.list_stack.push(ListState {
                    ordered,
                    next_index: start.unwrap_or(1),
                });
            }
            Tag::Item => {
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = if let Some(list) = self.list_stack.last_mut() {
                    if list.ordered {
                        let n = list.next_index;
                        list.next_index += 1;
                        format!("{indent}{n}. ")
                    } else {
                        let bullet = if depth == 0 { "• " } else { "◦ " };
                        format!("{indent}{bullet}")
                    }
                } else {
                    format!("{indent}• ")
                };
                self.item_prefix = Some(marker);
                self.runs.clear();
                self.style = Theme::assistant_body();
            }
            Tag::Emphasis => {
                self.style = Theme::emphasis();
            }
            Tag::Strong => {
                self.style = Theme::strong();
            }
            Tag::Strikethrough => {
                self.style = self.style.add_modifier(Modifier::CROSSED_OUT);
            }
            Tag::Link { .. } => {
                self.style = Theme::link();
            }
            Tag::Image { .. } => {
                self.push_run("🖼 ".into(), Theme::meta());
            }
            Tag::Table(alignments) => {
                self.table = Some(TableBuild {
                    alignments: alignments.to_vec(),
                    header: Vec::new(),
                    body: Vec::new(),
                    current_row: Vec::new(),
                });
            }
            Tag::TableHead => {}
            Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.current_row.clear();
                }
            }
            Tag::TableCell => {
                self.in_table_cell = true;
                self.cell_buf.clear();
            }
            Tag::HtmlBlock
            | Tag::MetadataBlock(_)
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_paragraph();
            }
            TagEnd::Heading(_) => {
                self.flush_paragraph();
                // breathing room after headings
                self.lines.push(Line::from(""));
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.flush_code_block();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.lines.push(Line::from(""));
                }
            }
            TagEnd::Item => {
                self.flush_list_item();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.style = if self.blockquote_depth > 0 {
                    Theme::blockquote()
                } else {
                    Theme::assistant_body()
                };
            }
            TagEnd::Table => {
                self.flush_table();
            }
            TagEnd::TableHead => {
                // GFM: header cells sit directly under TableHead (no TableRow wrapper).
                if let Some(t) = self.table.as_mut() {
                    t.header = std::mem::take(&mut t.current_row);
                }
            }
            TagEnd::TableRow => {
                // Body rows only (header has no TableRow events).
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.current_row);
                    t.body.push(row);
                }
            }
            TagEnd::TableCell => {
                self.in_table_cell = false;
                if let Some(t) = self.table.as_mut() {
                    t.current_row.push(std::mem::take(&mut self.cell_buf));
                }
            }
            TagEnd::Image => {}
            TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_)
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    // ── content ─────────────────────────────────────────────────────────

    fn text(&mut self, text: CowStr<'_>) {
        if self.in_code_block {
            // Code blocks may include trailing newlines as separate SoftBreak
            // or embedded \n in Text events.
            for (i, part) in text.split('\n').enumerate() {
                if i > 0 {
                    self.code_lines.push(String::new());
                }
                if let Some(last) = self.code_lines.last_mut() {
                    last.push_str(part);
                } else {
                    self.code_lines.push(part.to_string());
                }
            }
            return;
        }
        if self.in_table_cell {
            self.cell_buf.push_str(&text);
            return;
        }
        self.push_text_with_safe_badges(&text);
    }

    /// Rewrite checkbox / circled-digit glyphs that often tofu into solid chips.
    ///
    /// Terminal fonts frequently break ☑☐✅①②③ and keycap emoji (the "symbols
    /// with background" users expect). Map them to reverse-video ASCII badges.
    fn push_text_with_safe_badges(&mut self, text: &str) {
        let mut buf = String::new();
        let flush = |w: &mut Self, buf: &mut String| {
            if !buf.is_empty() {
                w.push_run(std::mem::take(buf), w.style);
            }
        };

        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            // Keycap digits: '1' + VS16? + combining enclosing keycap U+20E3
            // or emoji presentation of circled numbers.
            if ch.is_ascii_digit() {
                let mut look = chars.clone();
                // Optional variation selector-16
                if look.peek() == Some(&'\u{FE0F}') {
                    look.next();
                }
                if look.peek() == Some(&'\u{20E3}') {
                    // consume from real iterator
                    if chars.peek() == Some(&'\u{FE0F}') {
                        chars.next();
                    }
                    if chars.peek() == Some(&'\u{20E3}') {
                        chars.next();
                    }
                    flush(self, &mut buf);
                    self.push_run(format!(" {ch} "), Theme::badge_primary());
                    // drop trailing space if next is space (we already pad)
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }
                    self.push_run(" ".into(), self.style);
                    continue;
                }
            }

            match ch {
                // Checked boxes / heavy checks — solid success chip
                '☑' | '✅' | '✓' | '✔' | '🗹' | '🅥' => {
                    flush(self, &mut buf);
                    self.push_run("[x]".into(), Theme::badge_success());
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }
                    self.push_run(" ".into(), self.style);
                }
                // Empty / ballot boxes
                '☐' | '⬜' | '◻' | '▢' | '□' => {
                    flush(self, &mut buf);
                    self.push_run("[ ]".into(), Theme::badge_muted());
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }
                    self.push_run(" ".into(), self.style);
                }
                // Circled digits ①..⑨  (and ⑩)
                '①'..='⑨' => {
                    let n = (ch as u32 - '①' as u32) + 1;
                    flush(self, &mut buf);
                    self.push_run(format!(" {n} "), Theme::badge_primary());
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }
                    self.push_run(" ".into(), self.style);
                }
                '⑩' => {
                    flush(self, &mut buf);
                    self.push_run(" 10 ".into(), Theme::badge_primary());
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }
                    self.push_run(" ".into(), self.style);
                }
                // Negative circled ❶..❾
                '❶'..='❾' => {
                    let n = (ch as u32 - '❶' as u32) + 1;
                    flush(self, &mut buf);
                    self.push_run(format!(" {n} "), Theme::badge_primary());
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }
                    self.push_run(" ".into(), self.style);
                }
                other => buf.push(other),
            }
        }
        flush(self, &mut buf);
    }

    fn code(&mut self, code: CowStr<'_>) {
        if self.in_table_cell {
            self.cell_buf.push_str(&code);
            return;
        }
        self.push_run(code.to_string(), Theme::code());
    }

    fn soft_break(&mut self) {
        if self.in_code_block {
            self.code_lines.push(String::new());
            return;
        }
        if self.in_table_cell {
            self.cell_buf.push(' ');
            return;
        }
        self.push_run(" ".into(), self.style);
    }

    fn hard_break(&mut self) {
        if self.in_code_block {
            self.code_lines.push(String::new());
            return;
        }
        if self.in_table_cell {
            self.cell_buf.push(' ');
            return;
        }
        // Force a line break inside the current paragraph.
        self.flush_paragraph_keep_prefix();
    }

    fn rule(&mut self) {
        let w = self.width.max(8);
        self.lines
            .push(Line::from(Span::styled("─".repeat(w), Theme::meta())));
        self.lines.push(Line::from(""));
    }

    fn task_marker(&mut self, checked: bool) {
        // Avoid ☑/☐ — many terminal fonts render them as broken dual-cell tofu.
        // Solid reverse-video chips are single-width ASCII and always legible.
        if checked {
            self.push_run("[x]".into(), Theme::badge_success());
        } else {
            self.push_run("[ ]".into(), Theme::badge_muted());
        }
        self.push_run(" ".into(), Theme::assistant_body());
    }

    fn push_run(&mut self, text: String, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.runs.last_mut() {
            if last.1 == style {
                last.0.push_str(&text);
                return;
            }
        }
        self.runs.push((text, style));
    }

    // ── flush helpers ───────────────────────────────────────────────────

    fn blockquote_prefix(&self) -> String {
        if self.blockquote_depth == 0 {
            String::new()
        } else {
            "▎ ".repeat(self.blockquote_depth)
        }
    }

    fn flush_paragraph(&mut self) {
        self.flush_runs_wrapped(None);
        // blank line after paragraphs when not nested in a list
        if self.list_stack.is_empty() && self.blockquote_depth == 0 {
            self.lines.push(Line::from(""));
        }
    }

    fn flush_paragraph_keep_prefix(&mut self) {
        // hard break mid-paragraph
        let prefix = self.blockquote_prefix();
        let budget = self.width.saturating_sub(display_width(&prefix)).max(4);
        let wrapped = wrap_runs(&self.runs, budget);
        self.runs.clear();
        if wrapped.is_empty() {
            self.lines.push(prefix_line(&prefix, Vec::new()));
            return;
        }
        for row in wrapped {
            self.lines.push(prefix_line(&prefix, row));
        }
    }

    fn flush_list_item(&mut self) {
        if self.item_prefix.is_none() && self.runs.is_empty() {
            return;
        }
        let marker = self.item_prefix.take().unwrap_or_else(|| "• ".into());
        let bq = self.blockquote_prefix();
        let full_prefix = format!("{bq}{marker}");
        let budget = self
            .width
            .saturating_sub(display_width(&full_prefix))
            .max(4);
        let wrapped = wrap_runs(&self.runs, budget);
        self.runs.clear();
        if wrapped.is_empty() {
            self.lines
                .push(Line::from(vec![Span::styled(full_prefix, Theme::meta())]));
            return;
        }
        let hang = " ".repeat(display_width(&full_prefix));
        for (i, row) in wrapped.into_iter().enumerate() {
            let pre = if i == 0 {
                Span::styled(full_prefix.clone(), Theme::meta())
            } else {
                Span::raw(hang.clone())
            };
            let mut spans = vec![pre];
            for (text, style) in row {
                spans.push(Span::styled(text, style));
            }
            self.lines.push(Line::from(spans));
        }
    }

    fn flush_runs_wrapped(&mut self, force_prefix: Option<String>) {
        let bq = force_prefix.unwrap_or_else(|| self.blockquote_prefix());
        let budget = self.width.saturating_sub(display_width(&bq)).max(4);
        let wrapped = wrap_runs(&self.runs, budget);
        self.runs.clear();
        if wrapped.is_empty() {
            return;
        }
        for row in wrapped {
            self.lines.push(prefix_line(&bq, row));
        }
    }

    fn flush_code_block(&mut self) {
        self.in_code_block = false;
        // Drop a single trailing empty line from fenced blocks.
        if self.code_lines.last().is_some_and(|l| l.is_empty()) {
            self.code_lines.pop();
        }

        let bq = self.blockquote_prefix();
        let inner_w = self.width.saturating_sub(display_width(&bq) + 2).max(4);

        if !self.code_lang.is_empty() {
            self.lines.push(Line::from(vec![
                Span::raw(bq.clone()),
                Span::styled(format!(" {} ", self.code_lang), Theme::code_lang()),
            ]));
        }

        if self.code_lines.is_empty() {
            self.lines.push(Line::from(vec![
                Span::raw(bq),
                Span::styled("  ".to_string(), Theme::code_block()),
            ]));
        } else {
            for line in self.code_lines.drain(..) {
                let clean = expand_tabs(&line, 4);
                let shown = truncate_display(&clean, inner_w);
                let pad = inner_w.saturating_sub(display_width(&shown));
                self.lines.push(Line::from(vec![
                    Span::raw(bq.clone()),
                    Span::styled(" ", Theme::code_block()),
                    Span::styled(shown, Theme::code_block()),
                    Span::styled(" ".repeat(pad), Theme::code_block()),
                    Span::styled(" ", Theme::code_block()),
                ]));
            }
        }
        self.code_lang.clear();
        self.lines.push(Line::from(""));
    }

    fn flush_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let bq = self.blockquote_prefix();
        let avail = self.width.saturating_sub(display_width(&bq)).max(8);
        let rendered = render_table(&table, avail);
        for line in rendered {
            if bq.is_empty() {
                self.lines.push(line);
            } else {
                let mut spans = vec![Span::raw(bq.clone())];
                spans.extend(line.spans);
                self.lines.push(Line::from(spans));
            }
        }
        self.lines.push(Line::from(""));
    }
}

// ── table rendering ─────────────────────────────────────────────────────────

fn render_table(table: &TableBuild, avail: usize) -> Vec<Line<'static>> {
    let cols = table
        .alignments
        .len()
        .max(table.header.len())
        .max(table.body.iter().map(|r| r.len()).max().unwrap_or(0))
        .max(1);

    // Normalize rows to `cols` cells.
    let mut header = table.header.clone();
    header.resize(cols, String::new());
    let body: Vec<Vec<String>> = table
        .body
        .iter()
        .map(|r| {
            let mut row = r.clone();
            row.resize(cols, String::new());
            row
        })
        .collect();

    let aligns = effective_alignments(&table.alignments, &header, &body, cols);

    // Natural width per column (content + 2 padding), capped so long CJK
    // cells wrap instead of stretching the box past `avail`.
    let mut natural: Vec<usize> = vec![3; cols];
    for (i, cell) in header.iter().enumerate() {
        natural[i] = natural[i].max(display_width(cell.trim()) + 2);
    }
    for row in &body {
        for (i, cell) in row.iter().enumerate() {
            natural[i] = natural[i].max(display_width(cell.trim()) + 2);
        }
    }
    // borders: left + between* + right = cols + 1
    let border_cost = cols + 1;
    let max_col = avail.saturating_sub(border_cost).max(12);
    for w in natural.iter_mut() {
        *w = (*w).min(max_col);
    }
    let max_inner = avail.saturating_sub(border_cost).max(cols * 3);
    let widths = fit_columns(&natural, max_inner, cols);

    let mut out = Vec::new();
    out.push(box_line(&widths, BoxRow::Top));
    out.extend(table_data_rows(&header, &widths, &aligns, true));
    out.push(box_line(&widths, BoxRow::Mid));
    for row in &body {
        out.extend(table_data_rows(row, &widths, &aligns, false));
    }
    out.push(box_line(&widths, BoxRow::Bot));
    out
}

/// GFM alignment, with a numeric-column fallback (right) when unspecified.
fn effective_alignments(
    declared: &[Alignment],
    header: &[String],
    body: &[Vec<String>],
    cols: usize,
) -> Vec<Alignment> {
    (0..cols)
        .map(|i| {
            let given = declared.get(i).copied().unwrap_or(Alignment::None);
            if !matches!(given, Alignment::None) {
                return given;
            }
            let cells = body.iter().filter_map(|r| r.get(i).map(|s| s.trim()));
            let mut saw = false;
            for cell in cells {
                if cell.is_empty() {
                    continue;
                }
                saw = true;
                if !looks_numeric(cell) {
                    return Alignment::Left;
                }
            }
            if saw {
                Alignment::Right
            } else if header
                .get(i)
                .is_some_and(|h| looks_numeric(h.trim()) && !h.trim().is_empty())
            {
                Alignment::Right
            } else {
                Alignment::Left
            }
        })
        .collect()
}

fn looks_numeric(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    let t = t.trim_end_matches('%').trim();
    if t.parse::<f64>().is_ok() {
        return true;
    }
    t.chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '-' | '+' | ':' | '/'))
        && t.chars().any(|c| c.is_ascii_digit())
}

enum BoxRow {
    Top,
    Mid,
    Bot,
}

fn box_line(widths: &[usize], kind: BoxRow) -> Line<'static> {
    let (l, m, r, h) = match kind {
        BoxRow::Top => ('┌', '┬', '┐', '─'),
        BoxRow::Mid => ('├', '┼', '┤', '─'),
        BoxRow::Bot => ('└', '┴', '┘', '─'),
    };
    let mut s = String::new();
    s.push(l);
    for (i, &w) in widths.iter().enumerate() {
        s.extend(std::iter::repeat(h).take(w));
        s.push(if i + 1 == widths.len() { r } else { m });
    }
    Line::from(Span::styled(s, Theme::table_border()))
}

fn table_data_rows(
    cells: &[String],
    widths: &[usize],
    aligns: &[Alignment],
    header: bool,
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<String>> = widths
        .iter()
        .enumerate()
        .map(|(i, &w)| {
            let raw = cells.get(i).map(|s| s.trim()).unwrap_or("");
            let content_w = w.saturating_sub(2).max(1);
            wrap_plain(raw, content_w)
        })
        .collect();
    let height = wrapped.iter().map(|c| c.len().max(1)).max().unwrap_or(1);
    let style = if header {
        Theme::table_header()
    } else {
        Theme::table_cell()
    };

    (0..height)
        .map(|row_i| {
            let mut spans = vec![Span::styled("│", Theme::table_border())];
            for (i, &w) in widths.iter().enumerate() {
                let content_w = w.saturating_sub(2).max(1);
                let text = wrapped
                    .get(i)
                    .and_then(|c| c.get(row_i))
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let pad = content_w.saturating_sub(display_width(text));
                let align = aligns.get(i).copied().unwrap_or(Alignment::None);
                let (left, right) = match align {
                    Alignment::Center => (pad / 2, pad - pad / 2),
                    Alignment::Right => (pad, 0),
                    Alignment::Left | Alignment::None => (0, pad),
                };
                spans.push(Span::styled(
                    format!(" {}{}{} ", " ".repeat(left), text, " ".repeat(right)),
                    style,
                ));
                spans.push(Span::styled("│", Theme::table_border()));
            }
            Line::from(spans)
        })
        .collect()
}

/// Wrap plain text to `width` display columns (CJK = 2). Never splits a grapheme.
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let t = text.trim();
    if t.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut last_space: Option<usize> = None;
    for ch in t.chars() {
        let cw = char_width(ch);
        if cur_w + cw > width && !cur.is_empty() {
            if let Some(sp) = last_space {
                let rest = cur.split_off(sp);
                let rest = rest.trim_start().to_string();
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                cur = rest;
                cur_w = display_width(&cur);
                last_space = None;
            } else {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
                last_space = None;
            }
        }
        if ch == ' ' {
            last_space = Some(cur.len());
        }
        cur.push(ch);
        cur_w += cw;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Shrink column widths so sum(widths) ≤ budget, preserving min 3 per col.
fn fit_columns(natural: &[usize], budget: usize, cols: usize) -> Vec<usize> {
    let min_each = 3usize;
    let min_total = min_each * cols;
    if budget <= min_total {
        return vec![min_each; cols];
    }
    let mut widths = natural.to_vec();
    let mut total: usize = widths.iter().sum();
    if total <= budget {
        return widths;
    }
    // Iteratively steal 1 col from the widest until we fit.
    while total > budget {
        if let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > min_each)
            .max_by_key(|(_, w)| *w)
        {
            widths[idx] -= 1;
            total -= 1;
        } else {
            break;
        }
    }
    widths
}

// ── wrap / measure ──────────────────────────────────────────────────────────

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Theme::heading(),
        _ => Theme::heading_sub(),
    }
}

fn prefix_line(prefix: &str, row: Vec<(String, Style)>) -> Line<'static> {
    let mut spans = Vec::new();
    if !prefix.is_empty() {
        spans.push(Span::styled(prefix.to_string(), Theme::blockquote_bar()));
    }
    if row.is_empty() {
        return Line::from(spans);
    }
    for (text, style) in row {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// Word-wrap styled runs into lines of total display width ≤ `width`.
fn wrap_runs(runs: &[(String, Style)], width: usize) -> Vec<Vec<(String, Style)>> {
    if width == 0 {
        return vec![runs.to_vec()];
    }
    // Flatten to (char, style) then rebuild — simple & correct for CJK.
    let mut chars: Vec<(char, Style)> = Vec::new();
    for (text, style) in runs {
        for ch in text.chars() {
            chars.push((ch, *style));
        }
    }
    if chars.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0usize;
    // Track last space for soft break.
    let mut last_space: Option<usize> = None;

    for (ch, style) in chars {
        let cw = char_width(ch);
        if ch == ' ' {
            // allow trailing consideration
        }
        if cur_w + cw > width && !cur.is_empty() {
            // Try break at last space.
            if let Some(sp) = last_space {
                let (left, right) = cur.split_at(sp);
                let left_line = chars_to_runs(left);
                // drop the space at break
                let right_chars: Vec<(char, Style)> = right
                    .iter()
                    .skip_while(|(c, _)| *c == ' ')
                    .cloned()
                    .collect();
                lines.push(left_line);
                cur = right_chars;
                cur_w = cur.iter().map(|(c, _)| char_width(*c)).sum();
                last_space = None;
            } else {
                lines.push(chars_to_runs(&cur));
                cur.clear();
                cur_w = 0;
                last_space = None;
            }
        }
        if ch == ' ' {
            last_space = Some(cur.len());
        }
        cur.push((ch, style));
        cur_w += cw;
    }
    if !cur.is_empty() {
        // trim trailing spaces on visual line
        while cur.last().is_some_and(|(c, _)| *c == ' ') {
            cur.pop();
        }
        if !cur.is_empty() {
            lines.push(chars_to_runs(&cur));
        }
    }
    lines
}

fn chars_to_runs(chars: &[(char, Style)]) -> Vec<(String, Style)> {
    let mut runs = Vec::new();
    for &(ch, style) in chars {
        if let Some(last) = runs.last_mut() {
            let (ref mut s, ref st): &mut (String, Style) = last;
            if *st == style {
                s.push(ch);
                continue;
            }
        }
        runs.push((ch.to_string(), style));
    }
    runs
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn char_width(ch: char) -> usize {
    UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4])).max(1)
}

/// Rewrite model-drawn box-diagram "tables" into GFM pipe tables so the
/// renderer can re-flow them with display-width columns. Fenced code is left
/// untouched. Non-tabular ASCII art is kept as-is.
fn rewrite_ascii_tables(src: &str) -> String {
    if !src.contains('┌') && !src.contains('│') && !src.contains('├') {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len());
    let mut in_fence = false;
    let mut buf: Vec<&str> = Vec::new();
    let flush = |buf: &mut Vec<&str>, out: &mut String| {
        if buf.is_empty() {
            return;
        }
        if let Some(gfm) = box_block_to_gfm(buf) {
            out.push_str(&gfm);
            if !gfm.ends_with('\n') {
                out.push('\n');
            }
        } else {
            for (i, line) in buf.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str(line);
            }
            out.push('\n');
        }
        buf.clear();
    };
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            flush(&mut buf, &mut out);
            in_fence = !in_fence;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if !in_fence && is_box_table_line(line) {
            buf.push(line);
        } else {
            flush(&mut buf, &mut out);
            out.push_str(line);
            out.push('\n');
        }
    }
    flush(&mut buf, &mut out);
    if src.ends_with('\n') {
        out
    } else {
        out.trim_end_matches('\n').to_string()
    }
}

fn is_box_table_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    t.contains('│')
        || t.contains('┌')
        || t.contains('└')
        || t.contains('├')
        || t.contains('┤')
        || t.contains('┬')
        || t.contains('┴')
        || t.contains('┼')
}

fn is_box_separator(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars().all(|c| {
            matches!(
                c,
                '─' | '━'
                    | '┌'
                    | '┐'
                    | '└'
                    | '┘'
                    | '├'
                    | '┤'
                    | '┬'
                    | '┴'
                    | '┼'
                    | '│'
                    | '+'
                    | '-'
                    | '|'
                    | ' '
            )
        })
        && t.chars()
            .any(|c| matches!(c, '─' | '━' | '-' | '┼' | '┬' | '┴'))
}

fn box_block_to_gfm(lines: &[&str]) -> Option<String> {
    if lines.len() < 3 {
        return None;
    }
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in lines {
        if is_box_separator(line) {
            continue;
        }
        let cells = split_box_cells(line);
        if cells.len() < 2 {
            return None;
        }
        rows.push(cells);
    }
    if rows.len() < 2 {
        return None;
    }
    let cols = rows.iter().map(|r| r.len()).max()?;
    for row in rows.iter_mut() {
        row.resize(cols, String::new());
        for cell in row.iter_mut() {
            *cell = cell.replace('|', "\\|");
        }
    }
    let mut gfm = String::new();
    gfm.push('|');
    for cell in &rows[0] {
        gfm.push(' ');
        gfm.push_str(cell);
        gfm.push_str(" |");
    }
    gfm.push('\n');
    gfm.push('|');
    for _ in 0..cols {
        gfm.push_str(" --- |");
    }
    gfm.push('\n');
    for row in rows.iter().skip(1) {
        gfm.push('|');
        for cell in row {
            gfm.push(' ');
            gfm.push_str(cell);
            gfm.push_str(" |");
        }
        gfm.push('\n');
    }
    Some(gfm)
}

fn split_box_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let parts: Vec<&str> = if t.contains('│') {
        t.split('│').collect()
    } else {
        t.split('|').collect()
    };
    let mut cells: Vec<String> = parts.iter().map(|s| s.trim().to_string()).collect();
    if cells.first().is_some_and(|s| s.is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|s| s.is_empty()) {
        cells.pop();
    }
    cells
}

fn truncate_display(s: &str, max_cols: usize) -> String {
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    let limit = max_cols.saturating_sub(1);
    for ch in s.chars() {
        let cw = char_width(ch);
        if w + cw > limit {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn heading_and_bold() {
        let md = "# Title\n\nHello **world** and `code`.";
        let lines = render(md, 40);
        let s = flat(&lines);
        assert!(s.contains("Title"), "{s}");
        assert!(s.contains("world"), "{s}");
        assert!(s.contains("code"), "{s}");
        // hashes stripped
        assert!(!s.contains("# Title"), "{s}");
    }

    #[test]
    fn unordered_list() {
        let md = "- alpha\n- beta\n- gamma";
        let lines = render(md, 40);
        let s = flat(&lines);
        assert!(s.contains("• alpha"), "{s}");
        assert!(s.contains("• beta"), "{s}");
        assert!(s.contains("• gamma"), "{s}");
    }

    #[test]
    fn ordered_list() {
        let md = "1. one\n2. two";
        let lines = render(md, 40);
        let s = flat(&lines);
        assert!(s.contains("1. one"), "{s}");
        assert!(s.contains("2. two"), "{s}");
    }

    #[test]
    fn task_list_uses_ascii_badges_not_unicode_boxes() {
        let md = "- [x] done item\n- [ ] todo item";
        let lines = render(md, 40);
        let s = flat(&lines);
        assert!(s.contains("[x]"), "checked badge missing: {s}");
        assert!(s.contains("[ ]"), "unchecked badge missing: {s}");
        assert!(s.contains("done item"), "{s}");
        assert!(s.contains("todo item"), "{s}");
        // Fragile dual-cell glyphs that tofu on many terminals.
        assert!(!s.contains('☑'), "{s}");
        assert!(!s.contains('☐'), "{s}");
        // Badge styles: checked = success chip, unchecked = muted chip.
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|sp| sp.content.contains('['))
            .map(|sp| format!("{:?}:{}", sp.style.bg, sp.content))
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            joined.contains("[x]") && joined.contains("[ ]"),
            "expected both badges in spans: {joined}"
        );
    }

    #[test]
    fn unicode_checkboxes_and_circled_digits_become_badges() {
        let md = "☑ done\n☐ todo\n① first\n2️⃣ second";
        let lines = render(md, 48);
        let s = flat(&lines);
        assert!(s.contains("[x]"), "checked rewrite: {s}");
        assert!(s.contains("[ ]"), "unchecked rewrite: {s}");
        assert!(s.contains("1"), "circled digit rewrite: {s}");
        assert!(s.contains("2"), "keycap digit rewrite: {s}");
        assert!(
            s.contains("first") && s.contains("second") && s.contains("done"),
            "{s}"
        );
        assert!(
            !s.contains('☑') && !s.contains('☐') && !s.contains('①'),
            "{s}"
        );
    }

    #[test]
    fn fenced_code_block() {
        let md = "```rust\nfn main() {}\n```";
        let lines = render(md, 40);
        let s = flat(&lines);
        assert!(s.contains("rust"), "{s}");
        assert!(s.contains("fn main()"), "{s}");
    }

    #[test]
    fn gfm_table() {
        let md = "\
| Name | Age |
|------|-----|
| Ada  | 36  |
| Lin  | 42  |
";
        let lines = render(md, 40);
        let s = flat(&lines);
        assert!(s.contains("Name"), "{s}");
        assert!(s.contains("Age"), "{s}");
        assert!(s.contains("Ada"), "{s}");
        assert!(s.contains("Lin"), "{s}");
        // box drawing
        assert!(
            s.contains('┌') || s.contains('│') || s.contains('├'),
            "expected box borders, got:\n{s}"
        );
    }

    #[test]
    fn blockquote() {
        let md = "> quoted text";
        let lines = render(md, 40);
        let s = flat(&lines);
        assert!(s.contains("quoted text"), "{s}");
        assert!(s.contains('▎') || s.contains('│'), "{s}");
    }

    #[test]
    fn nested_list_uses_hollow_bullet() {
        let md = "- alpha\n    - nested";
        let lines = render(md, 40);
        let s = flat(&lines);
        assert!(s.contains("• alpha"), "{s}");
        assert!(s.contains("◦ nested"), "{s}");
    }

    #[test]
    fn cjk_table_columns_share_display_width() {
        let md = "\
| 层 | 职责 |
|----|------|
| 表现层 | UI |
| API | 网关 |
";
        let lines = render(md, 48);
        let widths: Vec<usize> = lines
            .iter()
            .filter(|l| {
                let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                t.contains('│') || t.contains('┌') || t.contains('├') || t.contains('└')
            })
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| display_width(s.content.as_ref()))
                    .sum()
            })
            .collect();
        assert!(widths.len() >= 3, "expected a boxed table");
        let first = widths[0];
        assert!(
            widths.iter().all(|&w| w == first),
            "CJK table rows must share display width, got {widths:?}\n{}",
            flat(&lines)
        );
    }

    #[test]
    fn ascii_box_table_is_reparsed() {
        let md = "\
┌──────┬──────┐
│ 项目 │ 值   │
├──────┼──────┤
│ 中文 │ 12   │
│ ASCII│ 3    │
└──────┴──────┘
";
        let lines = render(md, 40);
        let s = flat(&lines);
        assert!(s.contains("项目") && s.contains("中文"), "{s}");
        let widths: Vec<usize> = lines
            .iter()
            .filter(|l| {
                let t: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
                t.contains('│') || t.contains('┌')
            })
            .map(|l| {
                l.spans
                    .iter()
                    .map(|sp| display_width(sp.content.as_ref()))
                    .sum()
            })
            .collect();
        if let Some(&first) = widths.first() {
            assert!(
                widths.iter().all(|&w| w == first),
                "reparsed box must align, got {widths:?}\n{s}"
            );
        }
    }

    #[test]
    fn numeric_column_right_aligns() {
        let md = "\
| Name | Age |
|------|-----|
| Ada  | 36  |
| Lin  | 4   |
";
        let lines = render(md, 40);
        let body = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .find(|s| s.contains("Lin"))
            .unwrap_or_default();
        // Right-aligned 4 sits closer to the right pipe than a left-aligned digit.
        assert!(
            body.contains("  4 ") || body.contains(" 4 │") || body.contains("  4│"),
            "expected numeric right-align, got {body:?}"
        );
    }

    #[test]
    fn horizontal_rule() {
        let md = "above\n\n---\n\nbelow";
        let lines = render(md, 40);
        let s = flat(&lines);
        assert!(s.contains('─'), "{s}");
        assert!(s.contains("above"), "{s}");
        assert!(s.contains("below"), "{s}");
    }

    #[test]
    fn table_fits_narrow_width() {
        let md = "\
| A Very Long Header Name | B |
|-------------------------|---|
| short | x |
";
        let lines = render(md, 24);
        for line in &lines {
            let w: usize = line
                .spans
                .iter()
                .map(|s| display_width(s.content.as_ref()))
                .sum();
            assert!(w <= 24, "line too wide ({w}): {:?}", line);
        }
    }
}

#[cfg(test)]
mod debug_plain {
    use super::*;
    #[test]
    fn plain_paragraph_not_empty() {
        let lines = render("Hello world, this is a plain reply.", 40);
        let s: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
            .collect();
        eprintln!("PLAIN=>{s:?}");
        assert!(!s.trim().is_empty(), "got empty: {lines:?}");
        assert!(s.contains("Hello"), "{s}");
    }

    #[test]
    fn streaming_partial_not_empty() {
        // incomplete markdown mid-stream
        let lines = render("Here is some **bold and more text without close", 40);
        let s: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
            .collect();
        eprintln!("PARTIAL=>{s:?}");
        assert!(s.contains("Here") || s.contains("bold"), "{s}");
    }

    #[test]
    fn chinese_not_empty() {
        let lines = render("你好，这是一段中文回复。", 40);
        let s: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
            .collect();
        eprintln!("ZH=>{s:?}");
        assert!(s.contains("你好"), "{s}");
    }
}
