//! Status card — the visual core of the entire application.
//!
//! Produces a `Vec<Line<'static>>` describing a single post, already
//! wrapped to the viewport width and prefixed with a 2-column gutter
//! (carrying either a cursor bar or two spaces). The timeline renderer
//! inserts breathing room and dividers *between* cards; this module
//! does not emit trailing blank lines.
//!
//! Layout (from CLAUDE.md §7.1):
//!
//! ```text
//! ▏ Display Name  @handle@instance  ·  2h
//!   Body paragraph …
//!   (wrapped to viewport width)
//!
//!   󰋩  Alt text for attached image
//!
//!   󰑖 @booster boosted         ← only when the parent is a reblog
//! ```
//!
//! Rules:
//! - Selected posts get a left-column `▏` in accent color; unselected
//!   get two spaces. The body column therefore never shifts when the
//!   cursor moves.
//! - Boost headers render *above* the inner status, and the inner
//!   status' own account is used in the header row (not the booster).
//! - Media attachments collapse to one icon + alt-text line each.

use chrono::Utc;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::api::html;
use crate::api::models::{MediaAttachment, MediaType, Status};
use crate::icons;
use crate::ui::Theme;
use crate::ui::widgets::wrap;
use crate::util::time::relative;

/// Per-render flags. Bundled into a struct rather than passed as
/// individual booleans so call sites read `opts.cw_revealed = true`
/// instead of "the fourth `true` from the right".
#[derive(Debug, Clone, Copy, Default)]
pub struct CardOpts {
    /// Cursor sits on this card; render with the gutter bar.
    pub selected: bool,
    /// Use Nerd Font glyphs (vs ASCII fallbacks).
    pub nerd_font: bool,
    /// Append a dim line of reply / boost / favourite counts. Only the
    /// focal post on a detail page sets this.
    pub show_metrics: bool,
    /// User has explicitly revealed the body of a CW'd post. When
    /// `false` and a `spoiler_text` is present, body + media are
    /// suppressed in favour of a "press s to reveal" hint.
    pub cw_revealed: bool,
    /// Reserve rows for inline image rendering (Phase 4 / ratatui-image).
    /// Off by default — only screens that drive an [`ImageCache`] turn
    /// this on. When false, image media collapses to a single text
    /// caption line (the legacy behavior).
    pub show_images: bool,
}

/// Number of terminal rows reserved per inline image. Wide enough to
/// look meaningful, short enough that 4-image grids still fit on a
/// single screen. ratatui-image fits-to-area, so the actual aspect is
/// preserved within this budget.
pub const IMAGE_PLACEHOLDER_HEIGHT: u16 = 10;

/// One inline-image hint emitted by [`render_blocks`]. The caller is
/// responsible for actually drawing the image on top of the placeholder
/// rows by computing an absolute Rect and calling
/// `frame.render_stateful_widget(StatefulImage::default(), rect, &mut
/// protocol)`. Offsets are *post-wrap*, so the caller can map them
/// directly to terminal rows after applying its own scroll.
///
/// `x_offset` / `width_cols` let a single card carry sub-rect images
/// side-by-side with text — used for Apple Music cover art which
/// lives in a narrow left column while title / artist typography
/// flows to the right.
#[derive(Debug, Clone)]
pub struct ImageOverlay {
    pub line_offset: u16,
    pub height: u16,
    pub media_id: crate::api::models::MediaId,
    pub url: String,
    /// Column offset within the card's content area (after gutter).
    /// 0 for full-width media attachments.
    pub x_offset: u16,
    /// Width cap in cols. `None` means "use full card width minus
    /// gutter" — the default for media attachments.
    pub width_cols: Option<u16>,
}

/// Structured render output: text lines for the card body plus
/// optional image overlay metadata. Old callers can keep using
/// [`render`] which discards the overlays.
pub struct CardRender {
    pub lines: Vec<Line<'static>>,
    pub image_overlays: Vec<ImageOverlay>,
}

/// Legacy convenience: render a status as a flat list of visual lines.
/// Discards the image overlay metadata — callers that want inline
/// image rendering (and Apple Music enrichment) should use
/// [`render_blocks`] instead.
#[must_use]
pub fn render(status: &Status, theme: &Theme, opts: CardOpts, width: u16) -> Vec<Line<'static>> {
    render_blocks(status, theme, opts, width, None).lines
}

/// Structured render: returns wrapped, gutter-aligned lines plus a
/// list of image overlay slots the caller should fill with actual
/// `StatefulImage` widgets.
///
/// `music` is an optional Apple Music metadata cache. Passing `Some`
/// enables link enrichment — compact inline rewrites in dense density
/// mode, multi-line music cards with cover-art overlays in spacious
/// mode. Passing `None` leaves the raw Mastodon HTML link untouched.
pub fn render_blocks(
    status: &Status,
    theme: &Theme,
    opts: CardOpts,
    width: u16,
    mut music: Option<&mut crate::api::music::MusicCache>,
) -> CardRender {
    let wrap_w = width.saturating_sub(2); // gutter takes 2 columns
    let (mut pre_lines, body_links) = build_lines(status, theme, opts);

    // Apple Music enrichment. Runs pre-wrap so the inline link
    // replacement lets the body flow naturally. In spacious density
    // mode we also append a music card block just below the link's
    // line — its rows are short enough to pass through wrap_lines
    // untouched, and the overlay offset is mapped post-wrap via
    // `wrap_lines_with_map`.
    let music_enrichments = if let Some(ref mut cache) = music {
        enrich_apple_music(&mut pre_lines, &body_links, cache, opts.nerd_font, theme)
    } else {
        Vec::new()
    };

    let (mut wrapped, line_map) = wrap::wrap_lines_with_map(&pre_lines, wrap_w);

    let shown = status.reblog.as_deref().unwrap_or(status);
    let cw_hidden = !shown.spoiler_text.is_empty() && !opts.cw_revealed;

    let mut image_overlays: Vec<ImageOverlay> = Vec::new();

    // Spacious mode: splice a full music card (cover + typography)
    // just after the pre-wrap line that carried the Apple Music URL.
    // Only enrichments flagged as `card_ready` qualify (metadata
    // arrived + artwork URL present); unready ones already got a
    // compact fallback span above. Each insertion shifts later rows
    // down; `inserts_so_far` tracks the cumulative shift so each new
    // card's post-wrap line index lands in the right place, and we
    // bump any already-registered link_overlay line offsets whose
    // row got pushed by the insertion.
    if !cw_hidden && music_enrichments.iter().any(|e| e.card_ready) {
        let mut enrichments = music_enrichments;
        enrichments.sort_by_key(|e| e.pre_wrap_line_index);
        let mut inserts_so_far = 0usize;
        for enr in enrichments {
            if !enr.card_ready {
                continue;
            }
            let Some(meta) = &enr.meta else {
                continue;
            };
            let Some(artwork_url) = meta.artwork_url.as_deref() else {
                continue;
            };
            let post_end = if enr.pre_wrap_line_index + 1 < line_map.len() {
                line_map[enr.pre_wrap_line_index + 1]
            } else {
                wrapped.len().saturating_sub(inserts_so_far)
            };
            let insert_at = post_end + inserts_so_far;
            let rows = music_card_rows(meta, theme, wrap_w);
            let rows_len = rows.len();
            for (i, row) in rows.into_iter().enumerate() {
                wrapped.insert(insert_at + i, row);
            }
            image_overlays.push(ImageOverlay {
                line_offset: insert_at as u16,
                height: rows_len as u16,
                media_id: crate::api::models::MediaId::new(format!("music:{artwork_url}")),
                url: artwork_url.to_string(),
                x_offset: 0,
                width_cols: Some(MUSIC_ARTWORK_WIDTH),
            });
            inserts_so_far += rows_len;
        }
    }
    if !cw_hidden && !shown.media_attachments.is_empty() {
        wrapped.push(Line::default());
        for m in &shown.media_attachments {
            let is_image = matches!(m.media_type, MediaType::Image | MediaType::Gifv);
            let url = m.preview_url.as_deref().or(m.url.as_deref());
            if opts.show_images
                && is_image
                && let Some(url) = url
            {
                let url = url.to_string();
                let start = wrapped.len() as u16;
                // Reserve placeholder rows; the screen overlays the
                // actual image on top after the Paragraph renders.
                for _ in 0..IMAGE_PLACEHOLDER_HEIGHT {
                    wrapped.push(Line::default());
                }
                image_overlays.push(ImageOverlay {
                    line_offset: start,
                    height: IMAGE_PLACEHOLDER_HEIGHT,
                    media_id: m.id.clone(),
                    url,
                    x_offset: 0,
                    width_cols: None,
                });
                // Alt-text caption below the image, dim italic. Hidden
                // when the uploader didn't bother — most posts.
                let alt = m.description.as_deref().unwrap_or("").trim();
                if !alt.is_empty() {
                    let caption = Line::from(Span::styled(
                        format!("  {alt}"),
                        theme.tertiary().add_modifier(Modifier::ITALIC),
                    ));
                    wrapped.extend(wrap::wrap_lines(&[caption], wrap_w));
                }
            } else {
                wrapped.push(media_line(m, theme, opts.nerd_font));
            }
        }
    }

    // Quoted post. Renders below body+media but above metric line.
    // Pre-wrapped at (wrap_w - 2) so the 2-col indent survives the
    // outer wrap pass.
    if let Some(q) = &shown.quote {
        let q_lines = quote_block(q, theme, opts.nerd_font, wrap_w);
        if !q_lines.is_empty() {
            wrapped.push(Line::default());
            wrapped.extend(q_lines);
        }
    }

    if opts.show_metrics {
        wrapped.push(Line::default());
        wrapped.push(metric_line(shown, theme, opts.nerd_font));
    }

    let lines = wrapped
        .into_iter()
        .map(|l| with_gutter(l, theme, opts.selected))
        .collect();
    CardRender {
        lines,
        image_overlays,
    }
}

/// Width of the cover-art column in a spacious-mode Apple Music card.
const MUSIC_ARTWORK_WIDTH: u16 = 14;
/// Column where the card's right-hand text begins. Artwork occupies
/// `0..MUSIC_ARTWORK_WIDTH`, a 2-col gap, then text.
const MUSIC_TEXT_INDENT: u16 = MUSIC_ARTWORK_WIDTH + 2;
/// Height (rows) of a spacious-mode Apple Music card.
const MUSIC_CARD_HEIGHT: u16 = 6;

/// Nerd Font music note glyph (`nf-md-music`) used to mark enriched
/// Apple Music links in both compact and spacious modes.
const ICON_MUSIC: &str = "\u{f075a}";
const ICON_MUSIC_ASCII: &str = "[music]";

/// Side-record from [`enrich_apple_music`] used to insert spacious
/// music cards *after* wrap, so cover-art overlay offsets and text
/// rows survive wrap's potential line expansion of earlier body
/// content.
struct MusicEnrichment {
    /// Index into `pre_lines` (i.e., pre-wrap). Translated via the
    /// line_map returned from `wrap_lines_with_map`.
    pre_wrap_line_index: usize,
    meta: Option<crate::api::music::AppleMusicMeta>,
    /// True when the enrichment drained the original link's spans
    /// in favor of a full music card; false when it fell back to
    /// compact inline text (spacious-but-unready, or density 1).
    card_ready: bool,
}

/// Compose the un-wrapped logical lines plus link locations. The link
/// list covers only the `<a>` tags inside the body content — header
/// bits (boost, display name, CW banner) don't emit links. Each
/// LinkRef's `line_index` is rebased to absolute coordinates in the
/// returned Vec.
fn build_lines(
    status: &Status,
    theme: &Theme,
    opts: CardOpts,
) -> (Vec<Line<'static>>, Vec<html::LinkRef>) {
    let mut out: Vec<Line<'static>> = Vec::new();

    // Boost detection: if this status is a reblog, the visible content
    // is the inner status; the outer account is the booster.
    let boost_header = status.reblog.as_ref().map(|_| &status.account);
    let shown = status.reblog.as_deref().unwrap_or(status);

    if let Some(booster) = boost_header {
        let icon = icons::pick(opts.nerd_font, icons::BOOST, icons::BOOST_ASCII);
        out.push(Line::from(vec![
            Span::styled(format!("{icon} "), theme.boost_style()),
            Span::styled(format!("@{} boosted", booster.acct), theme.secondary()),
        ]));
    }

    // Header: display_name (bold) · @handle (secondary) · timestamp (tertiary).
    let display = if shown.account.display_name.is_empty() {
        shown.account.username.clone()
    } else {
        shown.account.display_name.clone()
    };
    let handle = format!("@{}", shown.account.acct);
    let time = shown
        .created_at
        .map(|ts| relative(Utc::now(), ts))
        .unwrap_or_default();

    let mut header = vec![
        Span::styled(display, theme.display_name()),
        Span::raw("  "),
        Span::styled(handle, theme.handle()),
    ];
    if !time.is_empty() {
        header.push(Span::styled("  ·  ", theme.timestamp()));
        header.push(Span::styled(time, theme.timestamp()));
    }
    if shown.account.bot {
        header.push(Span::styled(
            "  [bot]",
            Style::default().fg(theme.fg_tertiary).bg(theme.bg),
        ));
    }
    if matches!(shown.visibility, crate::api::models::Visibility::Private) {
        let lock = icons::pick(opts.nerd_font, icons::LOCK, icons::LOCK_ASCII);
        header.push(Span::styled(format!("  {lock}"), theme.secondary()));
    }

    // Viewer-state markers. Tiny, no counts — just a dim acknowledgement
    // that you've acted on this post. Appear only when the flag is set.
    if shown.favourited.unwrap_or(false) {
        let icon = icons::pick(opts.nerd_font, icons::FAVORITE, icons::FAVORITE_ASCII);
        header.push(Span::styled(format!("  {icon}"), theme.favorite_style()));
    }
    if shown.reblogged.unwrap_or(false) {
        let icon = icons::pick(opts.nerd_font, icons::BOOST, icons::BOOST_ASCII);
        header.push(Span::styled(format!("  {icon}"), theme.boost_style()));
    }
    if shown.bookmarked.unwrap_or(false) {
        let icon = icons::pick(opts.nerd_font, icons::BOOKMARK, icons::BOOKMARK_ASCII);
        header.push(Span::styled(format!("  {icon}"), theme.link()));
    }
    out.push(Line::from(header));

    // Content-warning banner.
    let cw_present = !shown.spoiler_text.is_empty();
    if cw_present {
        let warn = icons::pick(opts.nerd_font, icons::WARNING, icons::WARNING_ASCII);
        out.push(Line::from(vec![
            Span::styled(format!("{warn} "), theme.favorite_style()),
            Span::styled(
                format!("CW: {}", shown.spoiler_text),
                theme.secondary().add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    // Body. CW-hidden posts collapse to a single hint line — the
    // media block is handled separately in `render_blocks` and is
    // also suppressed there when the CW is up.
    let mut links_out: Vec<html::LinkRef> = Vec::new();
    if cw_present && !opts.cw_revealed {
        out.push(Line::from(Span::styled(
            "press s to reveal".to_string(),
            theme.tertiary(),
        )));
    } else {
        let (mut body, mut body_links) = html::render_with_links(&shown.content, theme);
        // Fedibird / Misskey forks prepend a visible `RE: <link>` to
        // the status body when it's a quote post. We already render
        // the quoted post inline below; the duplicate is noise.
        if let Some(quoted) = shown
            .quote
            .as_ref()
            .and_then(|q| q.quoted_status.as_deref())
        {
            strip_re_reference(&mut body, &mut body_links, quoted);
        }
        let body_start = out.len();
        out.extend(body);
        for mut link in body_links {
            link.line_index += body_start;
            links_out.push(link);
        }
    }

    (out, links_out)
}

/// Remove a leading `RE: <link>` reference that points at the quoted
/// post. The line is only stripped when its visible content is *just*
/// that reference (optional leading / trailing whitespace) — we don't
/// touch lines where "RE:" happens to appear inside real body text.
///
/// After dropping matching lines, any blank lines that now sit at the
/// top of `body` are also removed so the remaining body doesn't start
/// with a gap where the reference used to be.
fn strip_re_reference(
    body: &mut Vec<Line<'static>>,
    links: &mut Vec<html::LinkRef>,
    quoted: &Status,
) {
    // Build the set of URLs we'd recognize as "pointing at the quoted
    // post": the public url, the ActivityPub uri, and any mention of
    // the quoted status' id in a URL tail. We compare case-sensitively
    // — Mastodon URLs are.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(url) = quoted.url.as_deref() {
        candidates.push(url.to_string());
    }
    if !quoted.uri.is_empty() {
        candidates.push(quoted.uri.clone());
    }

    let lines_to_drop: std::collections::BTreeSet<usize> = links
        .iter()
        .filter(|lr| url_matches_quoted(&lr.href, &candidates, &quoted.id.0))
        .filter_map(|lr| is_re_only_line(body, lr).then_some(lr.line_index))
        .collect();

    if lines_to_drop.is_empty() {
        return;
    }

    // Drop matching lines back-to-front so earlier indices stay valid.
    for idx in lines_to_drop.iter().rev() {
        if *idx < body.len() {
            body.remove(*idx);
        }
    }

    // Adjust / drop any link records tied to those lines.
    links.retain(|lr| !lines_to_drop.contains(&lr.line_index));
    for lr in links.iter_mut() {
        let shift = lines_to_drop.iter().filter(|i| **i < lr.line_index).count();
        lr.line_index -= shift;
    }

    // Strip leading blank lines left behind by the removal.
    while body.first().is_some_and(|l| l.spans.is_empty()) {
        body.remove(0);
        for lr in links.iter_mut() {
            lr.line_index = lr.line_index.saturating_sub(1);
        }
    }
}

fn url_matches_quoted(href: &str, candidates: &[String], quoted_id: &str) -> bool {
    if candidates.iter().any(|c| c == href) {
        return true;
    }
    // Fallback: the URL ends in `/<quoted_id>` — covers federation
    // redirect URLs that don't byte-match the canonical `quoted.url`
    // (e.g. the local instance rewrites a remote post's URL). Guard on
    // a reasonable id length so this doesn't accidentally match short
    // numeric paths.
    !quoted_id.is_empty()
        && quoted_id.len() >= 6
        && (href.ends_with(&format!("/{quoted_id}"))
            || href.contains(&format!("/{quoted_id}?"))
            || href.contains(&format!("/{quoted_id}#")))
}

/// True when the link's host line is effectively `RE: <link>` — the
/// only other content is whitespace, a `RE:` / `QT:` prefix, or a
/// trailing colon variant.
fn is_re_only_line(body: &[Line<'static>], lr: &html::LinkRef) -> bool {
    let Some(line) = body.get(lr.line_index) else {
        return false;
    };
    if lr.span_range.end > line.spans.len() {
        return false;
    }
    let pre: String = line.spans[..lr.span_range.start]
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    let post: String = line.spans[lr.span_range.end..]
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    let pre_trim = pre.trim();
    let post_trim = post.trim();
    if !post_trim.is_empty() {
        return false;
    }
    // Accept "RE:" / "QT:" (and their fullwidth-colon variants), case
    // insensitive. Leave lines that carry actual prose untouched.
    let normalized = pre_trim.to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "re:" | "re：" | "qt:" | "qt：" | "re" | "qt" | ""
    )
}

/// Scan pre-wrap body for Apple Music links. The `D` density key
/// picks the display:
///
/// - **compact (density 1)** — replace the link spans with a single
///   pretty `󰝚 Artist · Title` span. Compact only.
/// - **spacious (density 2)** — *drain* the link spans (leaving just
///   the surrounding text on that line) and flag the line for a
///   full music card insertion post-wrap. No compact text — the
///   card is the display.
///
/// Returns one [`MusicEnrichment`] per link so the post-wrap phase
/// can look up the URL / metadata and stitch the card block in at
/// the right row. Both replacement styles carry the original URL in
/// the enrichment record so the click-overlay pass can make them
/// clickable.
fn enrich_apple_music(
    pre_lines: &mut Vec<Line<'static>>,
    links: &[html::LinkRef],
    music: &mut crate::api::music::MusicCache,
    nerd_font: bool,
    theme: &Theme,
) -> Vec<MusicEnrichment> {
    let icon = if nerd_font {
        ICON_MUSIC
    } else {
        ICON_MUSIC_ASCII
    };
    let spacious = inter_post_blank_lines() > 1;
    let mut enrichments: Vec<MusicEnrichment> = Vec::new();

    // Collect matching links. Process later spans on each line first
    // so in-place splice on that line doesn't invalidate earlier
    // span_range indices.
    let mut apple: Vec<(&html::LinkRef, crate::api::music::AppleMusicLink)> = links
        .iter()
        .filter_map(|lr| crate::api::music::parse_url(&lr.href).map(|ml| (lr, ml)))
        .collect();
    apple.sort_by(|a, b| {
        b.0.line_index
            .cmp(&a.0.line_index)
            .then(b.0.span_range.end.cmp(&a.0.span_range.end))
    });

    for (lr, ml) in apple {
        music.ensure_loaded(&ml);
        let meta = music.get(&ml.id).cloned();

        let Some(line) = pre_lines.get_mut(lr.line_index) else {
            continue;
        };
        if lr.span_range.end > line.spans.len() {
            continue;
        }

        // Card-ready means: spacious density + metadata arrived +
        // artwork URL is in the metadata. Only then do we drain the
        // link's spans; otherwise we fall back to the compact inline
        // rewrite so the user never sees a "blank" row while the
        // lookup is still in flight.
        let card_ready = spacious && meta.as_ref().is_some_and(|m| m.artwork_url.is_some());

        if card_ready {
            line.spans.drain(lr.span_range.clone());
        } else {
            // Compact inline replacement. UNDERLINED modifier here
            // is load-bearing: it is the post-render marker used by
            // the click-overlay pass to find these cells and wrap
            // them in OSC 8 hyperlinks.
            // Compact text: `󰝚 · Title · Artist`. Dot separators on
            // both sides of the title for visual rhythm — matches
            // the title↔artist separator the user has been seeing.
            let compact_text = match &meta {
                Some(m) if !m.artist.is_empty() => {
                    format!("{icon} · {} · {}", m.title, m.artist)
                }
                Some(m) => format!("{icon} · {}", m.title),
                None => format!(
                    "{icon} · Apple Music · {}",
                    crate::api::music::humanize_slug(&ml.slug)
                ),
            };
            line.spans.splice(
                lr.span_range.clone(),
                [Span::styled(
                    compact_text,
                    theme.mention_style().add_modifier(Modifier::BOLD),
                )],
            );
        }

        enrichments.push(MusicEnrichment {
            pre_wrap_line_index: lr.line_index,
            meta,
            card_ready,
        });
    }

    enrichments
}

/// Build the spacious-mode Apple Music card body. Cover art is
/// handled by an [`ImageOverlay`] the caller registers; these rows
/// carry the typography that lives to the right of the cover.
///
/// Height adapts to the wrapped text — long titles / artists / album
/// names expand the card. The caller must size the artwork overlay
/// to match (returned by [`Vec::len`]) so the cover doesn't spill
/// over into the next card or under-fill the reserved rows.
fn music_card_rows(
    meta: &crate::api::music::AppleMusicMeta,
    theme: &Theme,
    wrap_w: u16,
) -> Vec<Line<'static>> {
    let indent_cols = MUSIC_TEXT_INDENT;
    let text_avail = wrap_w.saturating_sub(indent_cols);
    let indent_str: String = " ".repeat(indent_cols as usize);

    // Build a row with `indent` + `text` for each wrapped chunk.
    // `wrap_text` respects char widths so CJK / emoji work.
    let wrap = |text: &str, style: Style| -> Vec<Line<'static>> {
        wrap_text(text, text_avail)
            .into_iter()
            .map(|chunk| {
                Line::from(vec![
                    Span::raw(indent_str.clone()),
                    Span::styled(chunk, style),
                ])
            })
            .collect()
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    // Top padding — lets the artwork's square frame visually align
    // mid-card rather than flush to the top.
    out.push(Line::default());
    // Title — primary + bold.
    out.extend(wrap(
        &meta.title,
        theme.primary().add_modifier(Modifier::BOLD),
    ));
    // Artist — secondary.
    if !meta.artist.is_empty() {
        out.extend(wrap(&meta.artist, theme.secondary()));
    }
    // Album · Year — tertiary.
    let album_line = {
        let mut parts = Vec::new();
        if let Some(album) = meta.album.as_deref()
            && !album.is_empty()
        {
            parts.push(album.to_string());
        }
        if let Some(year) = meta.year {
            parts.push(year.to_string());
        }
        parts.join("  ·  ")
    };
    if !album_line.is_empty() {
        out.extend(wrap(&album_line, theme.tertiary()));
    }
    // Kind label — very dim.
    out.extend(wrap(
        &format!("Apple Music · {}", meta.kind.label()),
        theme.tertiary(),
    ));
    // Minimum floor so even a bare-metadata card keeps its breathing
    // rhythm.
    while out.len() < MUSIC_CARD_HEIGHT as usize {
        out.push(Line::default());
    }
    // Bottom padding — one blank row below the last text line when
    // the card expanded past the floor.
    if out.last().is_some_and(|l| !l.spans.is_empty()) {
        out.push(Line::default());
    }
    out
}

/// Simple visible-width aware wrap for a plain text run (no spans,
/// no styles). Breaks on whitespace when possible; hard-breaks
/// mid-word when the next token alone would overflow. Mirrors
/// [`crate::ui::widgets::wrap`] policy but operates on owned
/// strings.
fn wrap_text(text: &str, width: u16) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![text.to_string()];
    }
    let width = width as usize;
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    let mut pending = String::new();
    let mut pending_w = 0usize;

    let flush_pending = |current: &mut String,
                         current_w: &mut usize,
                         pending: &mut String,
                         pending_w: &mut usize,
                         out: &mut Vec<String>| {
        if *pending_w == 0 {
            return;
        }
        if *current_w + *pending_w > width {
            if *current_w > 0 {
                out.push(std::mem::take(current));
                *current_w = 0;
            }
            // Pending alone overflows — hard-break.
            if *pending_w > width {
                let mut chunk = String::new();
                let mut chunk_w = 0usize;
                for c in pending.chars() {
                    let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    if chunk_w + w > width && !chunk.is_empty() {
                        out.push(std::mem::take(&mut chunk));
                        chunk_w = 0;
                    }
                    chunk.push(c);
                    chunk_w += w;
                }
                *current = chunk;
                *current_w = chunk_w;
            } else {
                *current = std::mem::take(pending);
                *current_w = *pending_w;
            }
        } else {
            current.push_str(pending);
            *current_w += *pending_w;
        }
        pending.clear();
        *pending_w = 0;
    };

    for c in text.chars() {
        if c.is_whitespace() {
            flush_pending(
                &mut current,
                &mut current_w,
                &mut pending,
                &mut pending_w,
                &mut out,
            );
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if current_w + w > width && current_w > 0 {
                out.push(std::mem::take(&mut current));
                current_w = 0;
                continue;
            }
            if current_w > 0 || !current.is_empty() {
                current.push(c);
                current_w += w;
            }
        } else {
            pending.push(c);
            pending_w += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    flush_pending(
        &mut current,
        &mut current_w,
        &mut pending,
        &mut pending_w,
        &mut out,
    );
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Render a quoted-post inset: header + body, dimmed and indented two
/// columns, already wrapped to `outer_wrap` (the cards's content width
/// without the gutter). When the quote state is anything other than
/// `accepted` we drop in a one-line placeholder instead.
fn quote_block(
    q: &crate::api::models::QuoteData,
    theme: &Theme,
    nerd_font: bool,
    outer_wrap: u16,
) -> Vec<Line<'static>> {
    let inner_wrap = outer_wrap.saturating_sub(2);
    let mut out: Vec<Line<'static>> = Vec::new();

    let Some(quoted) = q.quoted_status.as_deref() else {
        // Quote field present but no payload (revoked / deleted / etc.)
        let label = match q.state.as_deref() {
            Some("revoked") => "[quote revoked]",
            Some("deleted") => "[quoted post deleted]",
            Some("rejected") => "[quote not approved]",
            Some("pending") => "[quote pending approval]",
            _ => "[quoted post unavailable]",
        };
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(label.to_string(), theme.tertiary()),
        ]));
        return out;
    };

    // Quoted header. Dim italic display name + plain @handle + time.
    let display = if quoted.account.display_name.is_empty() {
        quoted.account.username.clone()
    } else {
        quoted.account.display_name.clone()
    };
    let handle = format!("@{}", quoted.account.acct);
    let time = quoted
        .created_at
        .map(|ts| relative(Utc::now(), ts))
        .unwrap_or_default();
    let quote_glyph = if nerd_font { "❝ " } else { "> " };
    let mut header = vec![
        Span::styled(quote_glyph.to_string(), theme.tertiary()),
        Span::styled(display, theme.tertiary().add_modifier(Modifier::ITALIC)),
        Span::raw(" "),
        Span::styled(handle, theme.tertiary()),
    ];
    if !time.is_empty() {
        header.push(Span::styled("  ·  ", theme.tertiary()));
        header.push(Span::styled(time, theme.tertiary()));
    }
    header.push(Span::styled("  ·  ", theme.tertiary()));
    header.push(Span::styled("Q: open", theme.tertiary()));

    let mut logical = vec![Line::from(header)];
    // Body content of the quoted post — collapsed to a few lines
    // visually via wrapping, but no hard line cap (Phanpy / Ice Cubes
    // both show the full quoted body since it's the *point* of a quote
    // post). CW is honored by the *quoted* post's own spoiler_text:
    // for now show the body always; CW-respect inside quote can be a
    // Phase 4 polish if it turns out to bite.
    if !quoted.spoiler_text.is_empty() {
        logical.push(Line::from(Span::styled(
            format!("CW: {}", quoted.spoiler_text),
            theme.tertiary().add_modifier(Modifier::ITALIC),
        )));
    }
    logical.extend(html::render(&quoted.content, theme));

    // Dim the body so it visibly recedes from the host post.
    for line in &mut logical {
        for span in &mut line.spans {
            // Only patch fg if the span's style hasn't already set one
            // (mention / hashtag colours keep their accent).
            if span.style.fg.is_none() {
                span.style = span.style.patch(theme.tertiary());
            }
        }
    }

    let wrapped = wrap::wrap_lines(&logical, inner_wrap);
    for line in wrapped {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    out
}

fn metric_line(s: &Status, theme: &Theme, nerd_font: bool) -> Line<'static> {
    let reply_i = icons::pick(nerd_font, icons::REPLY, icons::REPLY_ASCII);
    let boost_i = icons::pick(nerd_font, icons::BOOST, icons::BOOST_ASCII);
    let fav_i = icons::pick(nerd_font, icons::FAVORITE, icons::FAVORITE_ASCII);
    let dim = theme.tertiary();
    Line::from(vec![
        Span::styled(format!("{reply_i} {}", s.replies_count), dim),
        Span::raw("   "),
        Span::styled(format!("{boost_i} {}", s.reblogs_count), dim),
        Span::raw("   "),
        Span::styled(format!("{fav_i} {}", s.favourites_count), dim),
    ])
}

/// Pad the line with a 2-column left gutter. Selected rows paint the
/// first column with a thin cursor bar.
fn with_gutter(line: Line<'static>, theme: &Theme, selected: bool) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    if selected {
        spans.push(Span::styled(format!("{} ", icons::CURSOR), theme.cursor()));
    } else {
        spans.push(Span::raw("  "));
    }
    spans.extend(line.spans);
    Line::from(spans)
}

fn media_line(m: &MediaAttachment, theme: &Theme, nerd_font: bool) -> Line<'static> {
    let icon = match m.media_type {
        MediaType::Image => icons::pick(nerd_font, icons::IMAGE, icons::IMAGE_ASCII),
        MediaType::Video => icons::pick(nerd_font, icons::VIDEO, icons::VIDEO_ASCII),
        MediaType::Gifv => icons::pick(nerd_font, icons::GIF, icons::GIF_ASCII),
        MediaType::Audio | MediaType::Unknown => {
            icons::pick(nerd_font, icons::LINK, icons::LINK_ASCII)
        }
    };
    let alt = m.description.as_deref().unwrap_or("").trim();
    let text = if alt.is_empty() {
        format!("{icon}  [{:?}]", m.media_type)
    } else {
        format!("{icon}  {alt}")
    };
    Line::from(vec![Span::styled(text, theme.secondary())])
}

/// Inter-post blank-line count. Runtime-toggleable via `D` so the
/// user can A/B between density (1, default) and breathing room (2)
/// without restarting. Stored in an atomic so render code can read
/// it without threading a context through every screen.
static INTER_POST_BLANK_LINES_CELL: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(1);

/// Read the current inter-post blank-line count.
#[must_use]
pub fn inter_post_blank_lines() -> usize {
    INTER_POST_BLANK_LINES_CELL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Set the inter-post blank-line count, clamped to `[1, 3]`.
pub fn set_inter_post_blank_lines(n: usize) {
    INTER_POST_BLANK_LINES_CELL.store(n.clamp(1, 3), std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{Account, StatusId, Visibility};

    fn fake_status(id: &str, body_html: &str) -> Status {
        Status {
            id: StatusId::new(id),
            account: Account {
                display_name: "Alice".into(),
                acct: "alice@ex.com".into(),
                ..Default::default()
            },
            content: body_html.to_string(),
            created_at: Some(Utc::now() - chrono::Duration::hours(2)),
            visibility: Visibility::Public,
            ..Default::default()
        }
    }

    fn opts_plain() -> CardOpts {
        CardOpts {
            nerd_font: true,
            cw_revealed: true,
            ..Default::default()
        }
    }

    #[test]
    fn render_emits_non_empty_lines() {
        let theme = Theme::frost();
        let s = fake_status("1", "<p>hello world</p>");
        let lines = render(&s, &theme, opts_plain(), 80);
        assert!(lines.iter().any(|l| !l.spans.is_empty()));
    }

    #[test]
    fn selected_line_starts_with_cursor_glyph() {
        let theme = Theme::frost();
        let s = fake_status("1", "<p>hi</p>");
        let opts = CardOpts {
            selected: true,
            ..opts_plain()
        };
        let lines = render(&s, &theme, opts, 80);
        let first = lines.first().unwrap();
        let first_span = first.spans.first().unwrap();
        assert!(first_span.content.starts_with('\u{258F}'));
    }

    #[test]
    fn boost_gets_header_line() {
        let theme = Theme::frost();
        let mut outer = fake_status("2", "");
        outer.account.acct = "booster@ex.com".into();
        let inner = fake_status("1", "<p>original</p>");
        outer.reblog = Some(Box::new(inner));
        let lines = render(&outer, &theme, opts_plain(), 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("booster@ex.com"));
        assert!(text.contains("original"));
    }

    #[test]
    fn long_content_wraps_within_width() {
        let theme = Theme::frost();
        let s = fake_status(
            "1",
            "<p>this is a reasonably long sentence that should definitely wrap at narrow width</p>",
        );
        // 20 cols - 2 gutter = 18 effective. Every visual line ≤ 20.
        let lines = render(&s, &theme, opts_plain(), 20);
        for line in &lines {
            let total: usize = line
                .spans
                .iter()
                .flat_map(|s| s.content.chars())
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            assert!(total <= 20, "line too wide: {total}");
        }
    }

    #[test]
    fn metric_line_renders_when_requested() {
        let theme = Theme::frost();
        let mut s = fake_status("1", "<p>hi</p>");
        s.replies_count = 3;
        s.reblogs_count = 5;
        s.favourites_count = 12;
        let opts = CardOpts {
            show_metrics: true,
            ..opts_plain()
        };
        let lines = render(&s, &theme, opts, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(text.contains(" 3"));
        assert!(text.contains(" 5"));
        assert!(text.contains(" 12"));
    }

    #[test]
    fn metric_line_omitted_by_default() {
        let theme = Theme::frost();
        let mut s = fake_status("1", "<p>hi</p>");
        s.favourites_count = 999;
        let lines = render(&s, &theme, opts_plain(), 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(!text.contains("999"));
    }

    #[test]
    fn cw_collapsed_hides_body() {
        let theme = Theme::frost();
        let mut s = fake_status("1", "<p>secret-spoilery-content-XYZ</p>");
        s.spoiler_text = "topic warning".into();
        let opts = CardOpts {
            nerd_font: true,
            ..Default::default() // cw_revealed: false
        };
        let lines = render(&s, &theme, opts, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(text.contains("CW: topic warning"));
        assert!(text.contains("press s to reveal"));
        assert!(!text.contains("secret-spoilery-content-XYZ"));
    }

    #[test]
    fn quote_renders_inline_card() {
        use crate::api::models::QuoteData;
        let theme = Theme::frost();
        let mut host = fake_status("host", "<p>my hot take on this</p>");
        let mut quoted = fake_status("inner", "<p>QUOTED-CONTENT-MARK</p>");
        quoted.account.acct = "bob@ex.com".into();
        host.quote = Some(QuoteData {
            state: Some("accepted".into()),
            quoted_status: Some(Box::new(quoted)),
        });
        let lines = render(&host, &theme, opts_plain(), 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(text.contains("my hot take"));
        assert!(text.contains("@bob@ex.com"));
        assert!(text.contains("QUOTED-CONTENT-MARK"));
    }

    #[test]
    fn quote_strips_re_reference_line() {
        use crate::api::models::QuoteData;
        let theme = Theme::frost();
        let quoted_url = "https://ex.com/@bob/123456";
        let host_html = format!(
            "<p>RE: <a href=\"{quoted_url}\"><span class=\"invisible\">https://</span>ex.com/@bob/123456</a></p><p>my actual take</p>"
        );
        let mut host = fake_status("host", &host_html);
        let mut quoted = fake_status("123456", "<p>QUOTED-CONTENT-MARK</p>");
        quoted.account.acct = "bob@ex.com".into();
        quoted.url = Some(quoted_url.to_string());
        host.quote = Some(QuoteData {
            state: Some("accepted".into()),
            quoted_status: Some(Box::new(quoted)),
        });
        let lines = render(&host, &theme, opts_plain(), 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(text.contains("my actual take"));
        assert!(text.contains("QUOTED-CONTENT-MARK"));
        // The RE: prefix + dedupe URL should be gone.
        assert!(
            !text.contains("RE:"),
            "body still contains RE: reference: {text}"
        );
    }

    #[test]
    fn quote_keeps_re_inside_prose() {
        // "RE:" inside actual prose + unrelated link → must NOT strip.
        use crate::api::models::QuoteData;
        let theme = Theme::frost();
        let html = "<p>context: RE: the prior <a href=\"https://ex.com/other\">discussion</a></p>";
        let mut host = fake_status("host", html);
        let mut quoted = fake_status("other_id", "<p>q</p>");
        quoted.url = Some("https://ex.com/@bob/NONMATCH".to_string());
        host.quote = Some(QuoteData {
            state: Some("accepted".into()),
            quoted_status: Some(Box::new(quoted)),
        });
        let lines = render(&host, &theme, opts_plain(), 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(
            text.contains("RE: the prior"),
            "prose RE: was wrongly stripped: {text}"
        );
        assert!(text.contains("discussion"));
    }

    #[test]
    fn quote_revoked_shows_placeholder() {
        use crate::api::models::QuoteData;
        let theme = Theme::frost();
        let mut host = fake_status("host", "<p>hi</p>");
        host.quote = Some(QuoteData {
            state: Some("revoked".into()),
            quoted_status: None,
        });
        let lines = render(&host, &theme, opts_plain(), 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(text.contains("[quote revoked]"));
    }

    #[test]
    fn cw_revealed_shows_body() {
        let theme = Theme::frost();
        let mut s = fake_status("1", "<p>secret-spoilery-content-XYZ</p>");
        s.spoiler_text = "topic warning".into();
        let lines = render(&s, &theme, opts_plain(), 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(text.contains("CW: topic warning"));
        assert!(text.contains("secret-spoilery-content-XYZ"));
        assert!(!text.contains("press s to reveal"));
    }
}
