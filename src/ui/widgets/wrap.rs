//! Terminal-width line wrapping that preserves ratatui [`Span`] styles.
//!
//! Why we do this ourselves instead of using [`ratatui::widgets::Wrap`]:
//! ratatui's built-in wrap is applied **after** scroll offset is
//! resolved — when combined with `.scroll((offset, 0))` the offset
//! counts pre-wrap rows, so our cursor-follow math can't tell where the
//! selected card ends up on screen. Pre-wrapping lets the rest of the
//! timeline renderer treat one `Line` as one screen row.
//!
//! Wrap points:
//! - Whenever the next character would overflow `width` columns
//!   (measured with `unicode-width::UnicodeWidthChar`), prefer to break
//!   at the most recent **whitespace** or **CJK character**.
//! - If neither is available (e.g. a long URL), hard-break at the
//!   character boundary — better a visual seam than a truncated row.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// Wrap a sequence of logical lines to `width` terminal columns. Empty
/// input lines are preserved (they're how the caller indicates blank
/// rows between paragraphs). Styles on each span are carried onto every
/// resulting visual line.
#[must_use]
pub fn wrap_lines(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    wrap_lines_with_map(lines, width).0
}

/// Like [`wrap_lines`] but also returns a per-input-line map into the
/// wrapped output: `map[i]` is the output row where input line `i`
/// starts. Used by overlay placement (image / music card) so that
/// pre-wrap line offsets can be translated to post-wrap rows without
/// losing track when earlier lines wrapped into multiple rows.
#[must_use]
pub fn wrap_lines_with_map(
    lines: &[Line<'static>],
    width: u16,
) -> (Vec<Line<'static>>, Vec<usize>) {
    if width == 0 {
        let map: Vec<usize> = (0..lines.len()).collect();
        return (lines.to_vec(), map);
    }
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut map: Vec<usize> = Vec::with_capacity(lines.len());
    for line in lines {
        map.push(out.len());
        if line.spans.is_empty() {
            out.push(Line::default());
        } else {
            out.extend(wrap_one(line, width));
        }
    }
    (out, map)
}

fn wrap_one(line: &Line<'static>, width: u16) -> Vec<Line<'static>> {
    // Flatten to (char, style) pairs so we can walk freely.
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        for c in span.content.chars() {
            chars.push((c, span.style));
        }
    }
    if chars.is_empty() {
        return vec![Line::default()];
    }

    let width = width as usize;
    let mut out = Vec::new();
    // We iterate through `chars`, maintaining a pending slice
    // [start_idx .. cur_idx) and its displayed width.
    let mut start = 0usize;
    let mut cur_w = 0usize;
    let mut last_break: Option<usize> = None;

    let mut i = 0;
    while i < chars.len() {
        let (c, _) = chars[i];
        let w = UnicodeWidthChar::width(c).unwrap_or(0);

        // Would appending overflow? If we haven't placed any char yet,
        // always accept (even if the single char is wider than width).
        if cur_w + w > width && i > start {
            // Pick a break point.
            let break_at = last_break.unwrap_or(i);
            // emit [start .. break_at)
            out.push(build_line(&chars[start..break_at]));
            // advance past trailing spaces so the next visual line
            // doesn't begin with a leading gap.
            let mut resume = break_at;
            while resume < chars.len() && chars[resume].0 == ' ' && resume < i {
                resume += 1;
            }
            start = resume;
            cur_w = display_width(&chars[start..i]);
            last_break = None;
            // Re-process `i` without advancing so it joins the new line.
            continue;
        }

        cur_w += w;
        // Mark this position as a good break point *after* consuming c
        // if c is whitespace or a CJK-class character.
        if c == ' ' || c == '\t' || is_cjk(c) {
            last_break = Some(i + 1);
        }
        i += 1;
    }
    if start < chars.len() {
        out.push(build_line(&chars[start..]));
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

fn display_width(chars: &[(char, Style)]) -> usize {
    chars
        .iter()
        .map(|(c, _)| UnicodeWidthChar::width(*c).unwrap_or(0))
        .sum()
}

/// Rebuild a styled line from a slice of (char, style) pairs,
/// coalescing runs of the same style into one span.
fn build_line(chars: &[(char, Style)]) -> Line<'static> {
    if chars.is_empty() {
        return Line::default();
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run_style = chars[0].1;
    let mut run_text = String::new();
    for (c, style) in chars {
        if *style != run_style && !run_text.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run_text), run_style));
            run_style = *style;
        }
        run_text.push(*c);
    }
    if !run_text.is_empty() {
        spans.push(Span::styled(run_text, run_style));
    }
    Line::from(spans)
}

/// Rough "CJK character" predicate — anything in these ranges is a
/// reasonable place to break a line. We err on the side of over-
/// reporting; missing a CJK codepoint means a suboptimal break, not a
/// bug.
fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{11FF}'     // Hangul Jamo
        | '\u{2E80}'..='\u{2EFF}'   // CJK Radicals Supplement
        | '\u{2F00}'..='\u{2FDF}'   // Kangxi Radicals
        | '\u{3000}'..='\u{303F}'   // CJK Symbols and Punctuation
        | '\u{3040}'..='\u{309F}'   // Hiragana
        | '\u{30A0}'..='\u{30FF}'   // Katakana
        | '\u{3400}'..='\u{4DBF}'   // CJK Extension A
        | '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{A000}'..='\u{A4CF}'   // Yi Syllables
        | '\u{AC00}'..='\u{D7AF}'   // Hangul Syllables
        | '\u{F900}'..='\u{FAFF}'   // CJK Compatibility Ideographs
        | '\u{FF00}'..='\u{FFEF}'   // Halfwidth and Fullwidth Forms
        | '\u{20000}'..='\u{2FFFF}' // CJK Extensions B–F
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn plain(s: &str) -> Line<'static> {
        Line::from(Span::raw(s.to_string()))
    }

    #[test]
    fn passes_through_if_width_zero() {
        let input = vec![plain("anything")];
        let out = wrap_lines(&input, 0);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn english_breaks_on_spaces() {
        // "hello world again" is 17 chars; at width 12, greedy wrap
        // gives "hello world" (11) + "again" (5).
        let out = wrap_lines(&[plain("hello world again")], 12);
        assert_eq!(out.len(), 2);
        let first: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let second: String = out[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(first.trim_end(), "hello world");
        assert_eq!(second, "again");
    }

    #[test]
    fn long_word_hard_breaks() {
        let out = wrap_lines(&[plain("abcdefghij")], 4);
        assert!(out.len() >= 2);
        // Each line should be ≤ 4 wide.
        for line in &out {
            let total: usize = line
                .spans
                .iter()
                .flat_map(|s| s.content.chars())
                .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            assert!(total <= 4, "line too wide: {total}");
        }
    }

    #[test]
    fn cjk_breaks_between_characters() {
        // Each Chinese char is width 2; at width=6 we fit 3 per line.
        let out = wrap_lines(&[plain("今天天气真的很好")], 6);
        assert!(out.len() >= 2);
        for line in &out {
            let total: usize = line
                .spans
                .iter()
                .flat_map(|s| s.content.chars())
                .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            assert!(total <= 6, "line too wide: {total}");
        }
    }

    #[test]
    fn preserves_styles_across_wrap() {
        let red = Style::default().fg(Color::Red);
        let line = Line::from(vec![
            Span::styled("hello ", red),
            Span::raw("world again please"),
        ]);
        let out = wrap_lines(&[line], 12);
        assert!(out.len() >= 2);
        // "hello " should still be styled red on the first line.
        let first = &out[0];
        let hello_span = first
            .spans
            .iter()
            .find(|s| s.content.as_ref().contains("hello"))
            .expect("expected hello span");
        assert_eq!(hello_span.style.fg, Some(Color::Red));
    }

    #[test]
    fn preserves_empty_lines() {
        let input = vec![plain("a"), Line::default(), plain("b")];
        let out = wrap_lines(&input, 10);
        assert_eq!(out.len(), 3);
        assert!(out[1].spans.is_empty());
    }
}
