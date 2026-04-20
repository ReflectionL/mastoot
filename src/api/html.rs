//! Turn Mastodon's restricted HTML `status.content` into a
//! `Vec<ratatui::text::Line<'static>>`.
//!
//! Mastodon emits a small, fixed subset of HTML:
//!
//! - `<p>`      — paragraph (separator)
//! - `<br>`     — hard line break
//! - `<a>`      — link; for URL links Mastodon wraps the display form in
//!   `<span class="invisible">…` and `<span class="ellipsis">…` siblings,
//!   which we collapse.
//! - `<span class="mention">` — `@user@instance` reference
//! - `<span class="hashtag">` — `#tag` reference
//! - `<span class="invisible">` — rendered dim (usually `https://`)
//! - `<span class="ellipsis">`  — rendered normally (trailing `…` appended)
//!
//! Everything unknown is rendered as plain text (fail safe). Unicode
//! entities and numeric references are decoded by the parser.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use scraper::{ElementRef, Html, Node};

use crate::ui::Theme;

/// Parse Mastodon status HTML into styled lines.
///
/// The output is owned (`'static`) so it can be passed across thread
/// boundaries and cached.
#[must_use]
pub fn render(html: &str, theme: &Theme) -> Vec<Line<'static>> {
    render_with_links(html, theme).0
}

/// Location of a single `<a href="...">` inside the rendered body. The
/// caller uses `line_index` / `span_range` to find the exact spans
/// that represent the link's visible text, and `href` to decide
/// whether to replace them (e.g., Apple Music enrichment).
///
/// Multi-line links (rare — Mastodon emits URL links as one span on
/// one pre-wrap line) are *not* captured; they'd need a richer range
/// type, and post-wrap they'd need remapping anyway.
#[derive(Debug, Clone)]
pub struct LinkRef {
    pub href: String,
    pub line_index: usize,
    pub span_range: std::ops::Range<usize>,
}

/// Render HTML into lines plus a list of link locations. This is the
/// full-information version of [`render`] — [`render`] just discards
/// the link list.
#[must_use]
pub fn render_with_links(html: &str, theme: &Theme) -> (Vec<Line<'static>>, Vec<LinkRef>) {
    if html.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }
    let doc = Html::parse_fragment(html);
    let mut lines: Vec<Line<'static>> = vec![Line::default()];
    let mut links: Vec<LinkRef> = Vec::new();

    for child in doc.root_element().children() {
        walk(
            child,
            &mut lines,
            &mut links,
            theme,
            Style::default(),
            false,
        );
    }

    while lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    (lines, links)
}

fn walk(
    node: ego_tree::NodeRef<'_, Node>,
    lines: &mut Vec<Line<'static>>,
    links: &mut Vec<LinkRef>,
    theme: &Theme,
    inherited: Style,
    inside_link: bool,
) {
    match node.value() {
        Node::Text(text) => {
            push_text(lines, text.to_string(), inherited);
        }
        Node::Element(_) => {
            let Some(elem) = ElementRef::wrap(node) else {
                return;
            };
            let name = elem.value().name();
            let class = elem.value().attr("class").unwrap_or("");

            match name {
                "p" => {
                    // A single blank line between paragraphs. Inter-post
                    // breathing room comes from the timeline renderer,
                    // not from here.
                    if lines.last().is_some_and(|l| !l.spans.is_empty()) {
                        lines.push(Line::default());
                    }
                    for c in elem.children() {
                        walk(c, lines, links, theme, inherited, inside_link);
                    }
                }
                "br" => {
                    lines.push(Line::default());
                }
                "a" => {
                    let class_attr = elem.value().attr("class").unwrap_or("");
                    let is_mention = class_attr.contains("mention");
                    let is_hashtag = class_attr.contains("hashtag");
                    let style = if is_mention {
                        theme.mention_style()
                    } else if is_hashtag {
                        theme.hashtag_style()
                    } else {
                        theme.link()
                    };
                    // Snapshot position BEFORE walking children so we
                    // can tell which spans belong to this link.
                    let href = elem.value().attr("href").unwrap_or("").to_string();
                    let start_line = lines.len() - 1;
                    let start_span = lines.last().map_or(0, |l| l.spans.len());
                    for c in elem.children() {
                        walk(c, lines, links, theme, style, true);
                    }
                    // Close the link. Only record when both ends land
                    // on the same line — skip multi-line links.
                    let end_line = lines.len() - 1;
                    if end_line == start_line && !href.is_empty() {
                        let end_span = lines.last().map_or(0, |l| l.spans.len());
                        if end_span > start_span {
                            links.push(LinkRef {
                                href,
                                line_index: start_line,
                                span_range: start_span..end_span,
                            });
                        }
                    }
                }
                "span" => {
                    let style = if class.contains("invisible") {
                        theme.tertiary()
                    } else if class.contains("mention") && !inside_link {
                        theme.mention_style()
                    } else if class.contains("hashtag") && !inside_link {
                        theme.hashtag_style()
                    } else {
                        inherited
                    };
                    for c in elem.children() {
                        walk(c, lines, links, theme, style, inside_link);
                    }
                }
                _ => {
                    // Unknown element: walk children as if the wrapper
                    // weren't there.
                    for c in elem.children() {
                        walk(c, lines, links, theme, inherited, inside_link);
                    }
                }
            }
        }
        _ => {}
    }
}

fn push_text(lines: &mut Vec<Line<'static>>, text: String, style: Style) {
    if text.is_empty() {
        return;
    }
    // Mastodon's HTML never contains `\n` in text nodes, but we defend.
    let mut first = true;
    for chunk in text.split('\n') {
        if !first {
            lines.push(Line::default());
        }
        first = false;
        if chunk.is_empty() {
            continue;
        }
        let span = Span::styled(chunk.to_string(), style);
        if let Some(line) = lines.last_mut() {
            line.spans.push(span);
        } else {
            lines.push(Line::from(span));
        }
    }
}

/// Return every `href` on `<a>` tags in the HTML, in document order.
/// Used by the `o` keybinding to find a URL to open in the browser
/// without rerunning the full styled render pass.
#[must_use]
pub fn extract_links(html: &str) -> Vec<String> {
    if html.trim().is_empty() {
        return Vec::new();
    }
    let doc = Html::parse_fragment(html);
    let mut out = Vec::new();
    fn walk_hrefs(node: ego_tree::NodeRef<'_, Node>, out: &mut Vec<String>) {
        if let Node::Element(_) = node.value()
            && let Some(e) = ElementRef::wrap(node)
        {
            if e.value().name() == "a"
                && let Some(href) = e.value().attr("href")
                && !href.is_empty()
            {
                out.push(href.to_string());
            }
            for c in e.children() {
                walk_hrefs(c, out);
            }
        }
    }
    for c in doc.root_element().children() {
        walk_hrefs(c, &mut out);
    }
    out
}

/// Collapse styled lines back to a plain string. Useful for logging or
/// `examples/fetch_home.rs` in phase-1 when ratatui isn't running yet.
#[must_use]
pub fn to_plain_text(html: &str) -> String {
    let doc = Html::parse_fragment(html);
    let mut out = String::new();
    fn walk(node: ego_tree::NodeRef<'_, Node>, out: &mut String) {
        match node.value() {
            Node::Text(t) => out.push_str(&t.text),
            Node::Element(_) => {
                let Some(e) = ElementRef::wrap(node) else {
                    return;
                };
                match e.value().name() {
                    "br" => out.push('\n'),
                    "p" => {
                        if !out.is_empty() && !out.ends_with('\n') {
                            out.push('\n');
                        }
                        for c in e.children() {
                            walk(c, out);
                        }
                        out.push('\n');
                    }
                    _ => {
                        for c in e.children() {
                            walk(c, out);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for c in doc.root_element().children() {
        walk(c, &mut out);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_strips_paragraphs_and_breaks() {
        let html = "<p>hello</p><p>world<br>again</p>";
        assert_eq!(to_plain_text(html), "hello\nworld\nagain");
    }

    #[test]
    fn plain_text_decodes_entities() {
        let html = "<p>Tom &amp; Jerry</p>";
        assert_eq!(to_plain_text(html), "Tom & Jerry");
    }

    #[test]
    fn plain_text_handles_links() {
        let html = r#"<p>see <a href="https://example.com"><span class="invisible">https://</span>example.com</a></p>"#;
        assert_eq!(to_plain_text(html), "see https://example.com");
    }

    #[test]
    fn render_produces_non_empty_lines() {
        let theme = Theme::frost();
        let html = "<p>hi <span class=\"mention\">@user</span></p>";
        let lines = render(html, &theme);
        assert!(!lines.is_empty());
        assert!(lines.iter().any(|l| !l.spans.is_empty()));
    }

    #[test]
    fn render_with_links_captures_href_and_span_range() {
        let theme = Theme::frost();
        let html = r#"<p>see <a href="https://example.com/x"><span class="invisible">https://</span>example.com/x</a> today</p>"#;
        let (lines, links) = render_with_links(html, &theme);
        assert_eq!(links.len(), 1);
        let link = &links[0];
        assert_eq!(link.href, "https://example.com/x");
        // The link's spans should live on one line, covering at least
        // one span.
        assert!(!link.span_range.is_empty());
        let line = &lines[link.line_index];
        assert!(line.spans.len() >= link.span_range.end);
    }
}
