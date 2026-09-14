//! Turn Mastodon's restricted HTML `status.content` into a
//! `Vec<ratatui::text::Line<'static>>`.
//!
//! Vanilla Mastodon emits a small, fixed subset of HTML:
//!
//! - `<p>`      — paragraph (one blank line between paragraphs)
//! - `<br>`     — hard line break
//! - `<a>`      — link; for URL links Mastodon wraps the display form in
//!   `<span class="invisible">…` and `<span class="ellipsis">…` siblings,
//!   which we collapse.
//! - `<span class="mention">` — `@user@instance` reference
//! - `<span class="hashtag">` — `#tag` reference
//! - `<span class="invisible">` — rendered dim (usually `https://`)
//! - `<span class="ellipsis">`  — rendered normally (trailing `…` appended)
//!
//! Forks with Markdown (glitch-soc, Akkoma, Misskey, Mastodon ≥ 4.3
//! with formatting enabled) additionally send block and inline
//! formatting, which we render structurally so a list doesn't collapse
//! into one run-on line:
//!
//! - `<ul>` / `<ol>` / `<li>` — one item per line with a `•` / `1.` prefix
//! - `<blockquote>`         — each line prefixed with a dim `▎`
//! - `<pre>`                — block, secondary colour
//! - `<h1>`–`<h6>`          — own line, bold
//! - `<code>` `<strong>` `<b>` `<em>` `<i>` `<del>` `<s>` `<u>` `<small>`
//!   — inline modifiers
//!
//! Everything unknown is rendered as plain text (fail safe). Unicode
//! entities and numeric references are decoded by the parser.
//!
//! **Caching.** Parsing with html5ever costs tens of microseconds per
//! status, and every screen re-renders every visible card on every
//! frame. Since `status.content` never changes for a given status
//! (edits arrive as a new `Status` value), the parsed result is
//! memoised per thread keyed by the HTML text and the theme. See
//! [`render_with_links`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use scraper::{ElementRef, Html, Node};

use crate::ui::Theme;
use crate::util::emoji;

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

type Rendered = (Vec<Line<'static>>, Vec<LinkRef>);

/// Upper bound on memoised bodies per thread. When exceeded the whole
/// map is dropped — a timeline session rarely touches more than a few
/// hundred distinct statuses, so this is a safety valve, not an LRU.
const CACHE_CAP: usize = 4096;

thread_local! {
    static CACHE: RefCell<HashMap<u64, Rendered>> = RefCell::new(HashMap::new());
}

fn cache_key(html: &str, theme: &Theme) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    html.hash(&mut h);
    theme.hash(&mut h);
    h.finish()
}

/// Render HTML into lines plus a list of link locations. This is the
/// full-information version of [`render`] — [`render`] just discards
/// the link list.
///
/// Results are memoised per thread (see module docs); repeated calls
/// with the same `html` + `theme` are a hash lookup and a clone.
#[must_use]
pub fn render_with_links(html: &str, theme: &Theme) -> Rendered {
    if html.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }
    let key = cache_key(html, theme);
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let rendered = render_uncached(html, theme);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_CAP {
            c.clear();
        }
        c.insert(key, rendered.clone());
    });
    rendered
}

fn render_uncached(html: &str, theme: &Theme) -> Rendered {
    let doc = Html::parse_fragment(html);
    let mut w = Walker {
        theme,
        lines: vec![Line::default()],
        links: Vec::new(),
        lists: Vec::new(),
        pending_marker: false,
    };
    for child in doc.root_element().children() {
        w.walk(child, Style::default(), false);
    }
    let Walker {
        mut lines, links, ..
    } = w;
    while lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    (lines, links)
}

/// One list nesting level: `None` for `<ul>`, `Some(next_number)` for
/// `<ol>`.
type ListLevel = Option<usize>;

struct Walker<'t> {
    theme: &'t Theme,
    lines: Vec<Line<'static>>,
    links: Vec<LinkRef>,
    lists: Vec<ListLevel>,
    /// A list marker (`• ` / `1. `) was just emitted and no text has
    /// followed it yet. Block children of the `<li>` (Markdown
    /// renderers love `<li><p>…</p></li>`) must not break away from
    /// it.
    pending_marker: bool,
}

impl Walker<'_> {
    fn last_is_empty(&self) -> bool {
        self.lines.last().is_none_or(|l| l.spans.is_empty())
    }

    /// Make sure the next text lands at the start of a fresh line.
    fn fresh_line(&mut self) {
        if self.pending_marker {
            self.pending_marker = false;
            return;
        }
        if !self.last_is_empty() {
            self.lines.push(Line::default());
        }
    }

    /// Like [`Self::fresh_line`] but also guarantees one blank line
    /// above — paragraph rhythm.
    fn paragraph_break(&mut self) {
        if self.pending_marker {
            self.pending_marker = false;
            return;
        }
        if !self.last_is_empty() {
            self.lines.push(Line::default());
            self.lines.push(Line::default());
        }
    }

    fn walk(&mut self, node: ego_tree::NodeRef<'_, Node>, inherited: Style, inside_link: bool) {
        match node.value() {
            Node::Text(text) => {
                if !text.trim().is_empty() {
                    self.pending_marker = false;
                }
                push_text(&mut self.lines, text.to_string(), inherited);
            }
            Node::Element(_) => {
                let Some(elem) = ElementRef::wrap(node) else {
                    return;
                };
                self.element(elem, inherited, inside_link);
            }
            _ => {}
        }
    }

    fn walk_children(&mut self, elem: ElementRef<'_>, style: Style, inside_link: bool) {
        for c in elem.children() {
            self.walk(c, style, inside_link);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn element(&mut self, elem: ElementRef<'_>, inherited: Style, inside_link: bool) {
        let name = elem.value().name();
        let class = elem.value().attr("class").unwrap_or("");
        let theme = self.theme;

        match name {
            "p" => {
                self.paragraph_break();
                self.walk_children(elem, inherited, inside_link);
            }
            "br" => {
                self.lines.push(Line::default());
            }
            "a" => {
                let is_mention = class.contains("mention");
                let is_hashtag = class.contains("hashtag");
                let style = if is_mention {
                    theme.mention_style()
                } else if is_hashtag {
                    theme.hashtag_style()
                } else {
                    theme.link()
                };
                // Snapshot position BEFORE walking children so we can
                // tell which spans belong to this link.
                let href = elem.value().attr("href").unwrap_or("").to_string();
                let start_line = self.lines.len() - 1;
                let start_span = self.lines.last().map_or(0, |l| l.spans.len());
                self.walk_children(elem, style, true);
                // Close the link. Only record when both ends land on
                // the same line — skip multi-line links.
                let end_line = self.lines.len() - 1;
                if end_line == start_line && !href.is_empty() {
                    let end_span = self.lines.last().map_or(0, |l| l.spans.len());
                    if end_span > start_span {
                        self.links.push(LinkRef {
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
                self.walk_children(elem, style, inside_link);
            }

            // ---- block formatting (Markdown-enabled forks) ----------
            "ul" | "ol" => {
                // Top-level lists sit as a block; nested ones continue
                // the parent item's rhythm without a gap.
                if self.lists.is_empty() {
                    self.paragraph_break();
                } else {
                    self.fresh_line();
                }
                self.lists.push(if name == "ol" { Some(1) } else { None });
                self.walk_children(elem, inherited, inside_link);
                self.lists.pop();
                self.fresh_line();
            }
            "li" => {
                self.fresh_line();
                let depth = self.lists.len().max(1);
                let indent = "  ".repeat(depth - 1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{indent}{n}. ");
                        *n += 1;
                        m
                    }
                    _ => format!("{indent}• "),
                };
                if let Some(line) = self.lines.last_mut() {
                    line.spans.push(Span::styled(marker, theme.tertiary()));
                }
                self.pending_marker = true;
                self.walk_children(elem, inherited, inside_link);
                self.pending_marker = false;
            }
            "blockquote" => {
                self.paragraph_break();
                let start = self.lines.len() - 1;
                self.walk_children(elem, inherited, inside_link);
                let quote_style = theme.tertiary();
                for line in &mut self.lines[start..] {
                    if !line.spans.is_empty() {
                        line.spans.insert(0, Span::styled("▎ ", quote_style));
                    }
                }
                // Prefix any blank line *inside* the quote too, so the
                // bar reads as one continuous block.
                let end = self.lines.len();
                for line in &mut self.lines[start..end] {
                    if line.spans.is_empty() {
                        line.spans.push(Span::styled("▎", quote_style));
                    }
                }
                self.paragraph_break();
            }
            "pre" => {
                self.paragraph_break();
                self.walk_children(elem, theme.code(), inside_link);
                self.paragraph_break();
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.paragraph_break();
                self.walk_children(elem, inherited.add_modifier(Modifier::BOLD), inside_link);
                self.paragraph_break();
            }
            "hr" => {
                // No ornament — a rule is just a paragraph gap here.
                self.paragraph_break();
            }

            // ---- inline formatting --------------------------------
            "code" => {
                self.walk_children(elem, inherited.patch(theme.code()), inside_link);
            }
            "strong" | "b" => {
                self.walk_children(elem, inherited.add_modifier(Modifier::BOLD), inside_link);
            }
            "em" | "i" => {
                self.walk_children(elem, inherited.add_modifier(Modifier::ITALIC), inside_link);
            }
            "del" | "s" | "strike" => {
                self.walk_children(
                    elem,
                    inherited.add_modifier(Modifier::CROSSED_OUT),
                    inside_link,
                );
            }
            "u" => {
                self.walk_children(
                    elem,
                    inherited.add_modifier(Modifier::UNDERLINED),
                    inside_link,
                );
            }
            "small" | "sub" | "sup" => {
                self.walk_children(elem, inherited.patch(theme.tertiary()), inside_link);
            }
            _ => {
                // Unknown element: walk children as if the wrapper
                // weren't there.
                self.walk_children(elem, inherited, inside_link);
            }
        }
    }
}

fn push_text(lines: &mut Vec<Line<'static>>, text: String, style: Style) {
    if text.is_empty() {
        return;
    }
    // Mastodon's HTML never contains `\n` in text nodes (but `<pre>`
    // blocks on forks do), so split defensively.
    let mut first = true;
    for chunk in text.split('\n') {
        if !first {
            lines.push(Line::default());
        }
        first = false;
        if chunk.is_empty() {
            continue;
        }
        let span = Span::styled(emoji::normalize(chunk).into_owned(), style);
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
                    "p" | "li" | "blockquote" | "pre" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
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

    fn dump(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

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
    fn paragraphs_are_separated_by_one_blank_line() {
        let theme = Theme::frost();
        let lines = render("<p>a</p><p>b</p>", &theme);
        assert_eq!(dump(&lines), vec!["a", "", "b"]);
    }

    #[test]
    fn br_is_a_hard_break_without_blank_line() {
        let theme = Theme::frost();
        let lines = render("<p>a<br>b</p>", &theme);
        assert_eq!(dump(&lines), vec!["a", "b"]);
    }

    #[test]
    fn list_items_each_get_their_own_line() {
        let theme = Theme::frost();
        let lines = render("<p>list:</p><ul><li>one</li><li>two</li></ul>", &theme);
        assert_eq!(dump(&lines), vec!["list:", "", "• one", "• two"]);
    }

    #[test]
    fn list_item_wrapping_a_paragraph_stays_on_the_marker_line() {
        let theme = Theme::frost();
        let lines = render("<ul><li><p>one</p></li><li><p>two</p></li></ul>", &theme);
        assert_eq!(dump(&lines), vec!["• one", "• two"]);
    }

    #[test]
    fn ordered_list_numbers_items() {
        let theme = Theme::frost();
        let lines = render("<ol><li>a</li><li>b</li></ol>", &theme);
        assert_eq!(dump(&lines), vec!["1. a", "2. b"]);
    }

    #[test]
    fn blockquote_prefixes_every_line() {
        let theme = Theme::frost();
        let lines = render(
            "<p>said:</p><blockquote><p>x</p><p>y</p></blockquote><p>end</p>",
            &theme,
        );
        let d = dump(&lines);
        assert_eq!(d[0], "said:");
        assert!(d.iter().any(|l| l == "▎ x"), "{d:?}");
        assert!(d.iter().any(|l| l == "▎ y"), "{d:?}");
        assert_eq!(d.last().unwrap(), "end");
    }

    #[test]
    fn strong_and_em_set_modifiers() {
        let theme = Theme::frost();
        let lines = render("<p><strong>bold</strong> <em>it</em></p>", &theme);
        let spans = &lines[0].spans;
        let bold = spans.iter().find(|s| s.content == "bold").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let it = spans.iter().find(|s| s.content == "it").unwrap();
        assert!(it.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn pre_block_keeps_newlines() {
        let theme = Theme::frost();
        let lines = render("<p>code:</p><pre><code>a\nb</code></pre>", &theme);
        assert_eq!(dump(&lines), vec!["code:", "", "a", "b"]);
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

    #[test]
    fn cache_returns_identical_output() {
        let theme = Theme::frost();
        let html = "<p>cached <a href=\"https://e.com\">link</a></p>";
        let a = render_with_links(html, &theme);
        let b = render_with_links(html, &theme);
        assert_eq!(dump(&a.0), dump(&b.0));
        assert_eq!(a.1.len(), b.1.len());
        // A different theme must not collide.
        let c = render_with_links(html, &Theme::ember());
        assert_eq!(dump(&c.0), dump(&a.0));
        assert_ne!(
            c.0[0].spans.last().unwrap().style.fg,
            a.0[0].spans.last().unwrap().style.fg
        );
    }
}
