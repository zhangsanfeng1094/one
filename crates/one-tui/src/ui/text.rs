//! Shared display-width helpers for TUI paint (truncate / wrap / pad).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) fn truncate_mid(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let w = UnicodeWidthStr::width(s);
    if w <= max {
        return s.to_string();
    }
    if max <= 3 {
        return "…".to_string();
    }
    let keep = max - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + cw > keep {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

pub(super) fn pad_or_truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let w = s.width();
    if w == width {
        return s.to_string();
    }
    if w < width {
        return format!("{s}{}", " ".repeat(width - w));
    }
    // Truncate by display width, reserve 1 for ellipsis.
    if width == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > width - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    // If ellipsis made us short (wide chars edge), pad.
    let final_w = out.width();
    if final_w < width {
        out.push_str(&" ".repeat(width - final_w));
    }
    out
}

pub(super) fn wrap_styled_segments(
    segments: &[(String, bool)],
    width: usize,
) -> Vec<Vec<(String, bool)>> {
    if width == 0 {
        return vec![segments.to_vec()];
    }
    let mut rows: Vec<Vec<(String, bool)>> = Vec::new();
    let mut cur: Vec<(String, bool)> = Vec::new();
    let mut col = 0usize;

    let push_chunk = |cur: &mut Vec<(String, bool)>, text: String, emp: bool| {
        if text.is_empty() {
            return;
        }
        if let Some(last) = cur.last_mut() {
            if last.1 == emp {
                last.0.push_str(&text);
                return;
            }
        }
        cur.push((text, emp));
    };

    for (text, emp) in segments {
        let mut rest = text.as_str();
        while !rest.is_empty() {
            if col >= width {
                rows.push(std::mem::take(&mut cur));
                col = 0;
            }
            let room = width.saturating_sub(col).max(1);
            let (take, advance) = take_prefix_cols(rest, room);
            if take.is_empty() {
                // Can't fit even one char — force break.
                rows.push(std::mem::take(&mut cur));
                col = 0;
                continue;
            }
            push_chunk(&mut cur, take.to_string(), *emp);
            col = col.saturating_add(advance);
            rest = &rest[take.len()..];
            if col >= width && !rest.is_empty() {
                rows.push(std::mem::take(&mut cur));
                col = 0;
            }
        }
    }
    if !cur.is_empty() || rows.is_empty() {
        rows.push(cur);
    }
    rows
}

pub(super) fn wrap_styled_spans(
    spans: &[ratatui::text::Span<'static>],
    width: usize,
) -> Vec<Vec<ratatui::text::Span<'static>>> {
    if width == 0 {
        return vec![spans.to_vec()];
    }
    let mut rows: Vec<Vec<ratatui::text::Span<'static>>> = Vec::new();
    let mut cur: Vec<ratatui::text::Span<'static>> = Vec::new();
    let mut col = 0usize;

    for span in spans {
        let style = span.style;
        let mut rest = span.content.as_ref();
        while !rest.is_empty() {
            if col >= width {
                rows.push(std::mem::take(&mut cur));
                col = 0;
            }
            let room = width.saturating_sub(col).max(1);
            let (take, advance) = take_prefix_cols(rest, room);
            if take.is_empty() {
                rows.push(std::mem::take(&mut cur));
                col = 0;
                continue;
            }
            if let Some(last) = cur.last_mut() {
                if last.style == style {
                    let mut s = last.content.to_string();
                    s.push_str(take);
                    *last = ratatui::text::Span::styled(s, style);
                } else {
                    cur.push(ratatui::text::Span::styled(take.to_string(), style));
                }
            } else {
                cur.push(ratatui::text::Span::styled(take.to_string(), style));
            }
            col = col.saturating_add(advance);
            rest = &rest[take.len()..];
            if col >= width && !rest.is_empty() {
                rows.push(std::mem::take(&mut cur));
                col = 0;
            }
        }
    }
    if !cur.is_empty() || rows.is_empty() {
        rows.push(cur);
    }
    rows
}

/// Take a prefix of `s` whose display width is ≤ `max_cols`. Returns (prefix, width).
pub(super) fn take_prefix_cols(s: &str, max_cols: usize) -> (&str, usize) {
    if max_cols == 0 {
        return ("", 0);
    }
    let mut w = 0usize;
    let mut end = 0usize;
    for (i, ch) in s.char_indices() {
        let cw = char_width(ch);
        if w + cw > max_cols {
            break;
        }
        w += cw;
        end = i + ch.len_utf8();
    }
    (&s[..end], w)
}

pub(super) fn truncate_display(s: &str, max_cols: usize) -> String {
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

/// Middle-truncate by **display width**, keeping head + tail.
///
/// Critical for tool rows: absolute/relative paths are long at the front;
/// the useful bit is usually the destination filename at the end.
///
/// ```text
/// cd ./benches/out/tb-regex-checker/file.rs
/// cd ./benches…/tb-regex-checker/file.rs
/// ```
pub(super) fn truncate_display_middle(s: &str, max_cols: usize) -> String {
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    if max_cols <= 1 {
        return "…".into();
    }
    if max_cols <= 3 {
        return truncate_display(s, max_cols);
    }
    let ellipsis_w = 1; // …
    let inner = max_cols - ellipsis_w;
    // ~40% head (cmd / leading dirs), ~60% tail (filename / destination).
    let head_budget = (inner * 2) / 5;
    let tail_budget = inner - head_budget;

    let mut head = String::new();
    let mut hw = 0usize;
    for ch in s.chars() {
        let cw = char_width(ch);
        if hw + cw > head_budget {
            break;
        }
        head.push(ch);
        hw += cw;
    }

    let mut tail_chars: Vec<char> = Vec::new();
    let mut tw = 0usize;
    for ch in s.chars().rev() {
        let cw = char_width(ch);
        if tw + cw > tail_budget {
            break;
        }
        tail_chars.push(ch);
        tw += cw;
    }
    tail_chars.reverse();
    let tail: String = tail_chars.into_iter().collect();
    format!("{head}…{tail}")
}

pub(super) fn wrap_paragraphs(content: &str, width: usize) -> Vec<String> {
    if content.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for (pi, para) in content.split('\n').enumerate() {
        if pi > 0 && para.is_empty() {
            out.push(String::new());
            continue;
        }
        let wrapped = wrap_str(para, width);
        if wrapped.is_empty() {
            out.push(String::new());
        } else {
            out.extend(wrapped);
        }
    }
    out
}

/// Soft-wrap by **terminal columns** (CJK = 2). Never split mid-grapheme.
pub(super) fn wrap_str(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    if text.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut cur_w = 0usize;

    // Prefer breaking on spaces when possible.
    for word in text.split_inclusive(' ') {
        let ww = display_width(word);
        if cur_w > 0 && cur_w + ww > width {
            out.push(std::mem::take(&mut current));
            cur_w = 0;
        }
        if ww > width {
            // Hard-split overlong token by columns.
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
                cur_w = 0;
            }
            for ch in word.chars() {
                let cw = char_width(ch);
                if cur_w > 0 && cur_w + cw > width {
                    out.push(std::mem::take(&mut current));
                    cur_w = 0;
                }
                current.push(ch);
                cur_w += cw;
            }
        } else {
            current.push_str(word);
            cur_w += ww;
        }
    }
    if !current.is_empty() {
        // Trim trailing spaces from visual lines for cleaner look.
        out.push(current.trim_end().to_string());
    }
    out
}

pub(super) fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub(super) fn char_width(ch: char) -> usize {
    UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4])).max(1)
}

/// Terminal columns occupied by `s` (fullwidth / CJK = 2) as u16 for layout.
pub(super) fn display_cols(s: &str) -> u16 {
    display_width(s).min(u16::MAX as usize) as u16
}

/// Map (total, viewport, offset) → (thumb_row, thumb_height) in track coords.
pub(super) fn scrollbar_thumb_geometry(
    total: usize,
    viewport: usize,
    offset: usize,
    track_h: usize,
) -> (usize, usize) {
    if track_h == 0 {
        return (0, 0);
    }
    if total <= viewport {
        return (0, track_h);
    }
    // Thumb size ∝ visible fraction; at least 1 row.
    let thumb_h = ((viewport * track_h) / total).max(1).min(track_h);
    let max_off = total.saturating_sub(viewport);
    let travel = track_h.saturating_sub(thumb_h);
    let thumb_start = if max_off == 0 {
        0
    } else {
        (offset * travel) / max_off
    };
    (thumb_start.min(travel), thumb_h)
}
