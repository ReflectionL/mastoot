//! Custom-emoji shortcodes (`:blobcat:`) in rendered text.
//!
//! A terminal can't paint the emoji image inline with text, so the
//! shortcode stays visible — but demoted to the tertiary tier so
//! `hello :blobcat: world` reads as prose with a quiet marker, not as
//! a broken token. Only shortcodes the server actually listed on the
//! status / account are touched; a stray `:` in prose is left alone.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Re-style every `:code:` run in `lines` whose `code` is in `codes`.
/// Spans are split around the match; every other byte keeps its style.
pub fn dim_shortcodes(lines: &mut [Line<'static>], codes: &[&str], dim: Style) {
    if codes.is_empty() {
        return;
    }
    for line in lines.iter_mut() {
        if !line.spans.iter().any(|s| s.content.contains(':')) {
            continue;
        }
        let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
        for span in std::mem::take(&mut line.spans) {
            split_span(span, codes, dim, &mut out);
        }
        line.spans = out;
    }
}

fn split_span(span: Span<'static>, codes: &[&str], dim: Style, out: &mut Vec<Span<'static>>) {
    let text: &str = &span.content;
    let style = span.style;
    let mut cursor = 0usize;
    let mut scan = 0usize;
    while let Some(rel) = text[scan..].find(':') {
        let i = scan + rel;
        let rest = &text[i + 1..];
        let hit = codes
            .iter()
            .find(|c| rest.starts_with(**c) && rest[c.len()..].starts_with(':'))
            .map(|c| c.len());
        match hit {
            Some(len) => {
                if i > cursor {
                    out.push(Span::styled(text[cursor..i].to_string(), style));
                }
                let end = i + 1 + len + 1;
                out.push(Span::styled(text[i..end].to_string(), style.patch(dim)));
                cursor = end;
                scan = end;
            }
            None => scan = i + 1,
        }
    }
    if cursor < text.len() {
        out.push(Span::styled(text[cursor..].to_string(), style));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn dump(l: &Line<'static>) -> Vec<(String, Option<Color>)> {
        l.spans
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect()
    }

    #[test]
    fn known_shortcode_is_split_and_dimmed() {
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines = vec![Line::from(Span::raw("hi :blobcat: there"))];
        dim_shortcodes(&mut lines, &["blobcat"], dim);
        assert_eq!(
            dump(&lines[0]),
            vec![
                ("hi ".to_string(), None),
                (":blobcat:".to_string(), Some(Color::DarkGray)),
                (" there".to_string(), None),
            ]
        );
    }

    #[test]
    fn unknown_or_partial_codes_are_untouched() {
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines = vec![Line::from(Span::raw("time 12:30 and :nope: and :blob"))];
        dim_shortcodes(&mut lines, &["blobcat"], dim);
        assert_eq!(lines[0].spans.len(), 1);
    }

    #[test]
    fn adjacent_codes_each_get_their_own_span() {
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines = vec![Line::from(Span::raw(":a::b:"))];
        dim_shortcodes(&mut lines, &["a", "b"], dim);
        assert_eq!(lines[0].spans.len(), 2);
    }
}
