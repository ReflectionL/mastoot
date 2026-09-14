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
use crate::util::emoji;
use crate::util::time::relative;

/// User-level rendering preferences shared by every list screen:
/// icon set, timestamp style and inter-post spacing. Owned by
/// `ui::app::App` (the `D` key flips density at runtime) and passed
/// by value into each screen's `render`.
#[derive(Debug, Clone, Copy)]
pub struct RenderPrefs {
    /// Use Nerd Font glyphs (vs ASCII fallbacks).
    pub nerd_font: bool,
    /// `Jan 15 14:32` instead of `2h`.
    pub absolute_time: bool,
    /// Blank rows between adjacent cards (1 = dense, 2 = spacious).
    pub inter_post_blank_lines: usize,
}

impl RenderPrefs {
    /// Spacious density unlocks the taller presentations (Apple Music
    /// cover-art cards).
    #[must_use]
    pub fn spacious(self) -> bool {
        self.inter_post_blank_lines > 1
    }

    /// `CardOpts` seeded from these prefs; callers then set the
    /// per-card bits (`selected`, `cw_revealed`, …).
    #[must_use]
    pub fn card_opts(self) -> CardOpts {
        CardOpts {
            nerd_font: self.nerd_font,
            absolute_time: self.absolute_time,
            spacious: self.spacious(),
            ..CardOpts::default()
        }
    }
}

impl Default for RenderPrefs {
    fn default() -> Self {
        Self {
            nerd_font: true,
            absolute_time: false,
            inter_post_blank_lines: 1,
        }
    }
}

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
    /// Add a dim `↪ replying to @someone` line under the header when
    /// the post is a reply. Timeline / profile turn this on; the
    /// detail page doesn't (the thread itself is the context).
    pub show_reply_hint: bool,
    /// Render the timestamp as `Jan 15 14:32` instead of `2h`
    /// (`[ui] show_relative_time = false`).
    pub absolute_time: bool,
    /// Spacious density (2 blank rows between posts): Apple Music links
    /// expand into cover-art cards instead of a compact inline line.
    pub spacious: bool,
}

/// Fallback rows for an inline image whose dimensions the server
/// didn't report. When `meta.original` carries width / height the box
/// is sized to the picture's aspect instead — see [`image_box`].
pub const IMAGE_PLACEHOLDER_HEIGHT: u16 = 10;
/// Tallest inline image; portrait pictures get narrowed to fit.
const IMAGE_MAX_ROWS: u16 = 16;
/// Shortest inline image; a panorama still gets a readable strip.
const IMAGE_MIN_ROWS: u16 = 3;
/// A terminal cell is roughly twice as tall as it is wide, so `w`
/// columns × `h` rows show a `w : 2h` pixel box.
const CELL_ASPECT: f64 = 2.0;

/// `(rows, width_cols)` for an attachment rendered into a card of
/// `avail_cols` columns, preserving the picture's aspect so
/// ratatui-image's fit-to-area doesn't letterbox. Landscape images use
/// the full width and as many rows as the aspect needs; portrait ones
/// are capped at [`IMAGE_MAX_ROWS`] and narrowed instead.
fn image_box(m: &MediaAttachment, avail_cols: u16) -> (u16, Option<u16>) {
    let Some((w, h)) = media_dimensions(m) else {
        return (IMAGE_PLACEHOLDER_HEIGHT, None);
    };
    if avail_cols == 0 {
        return (IMAGE_PLACEHOLDER_HEIGHT, None);
    }
    let ratio = h / w; // pixel height per pixel width
    let rows_full = (f64::from(avail_cols) * ratio / CELL_ASPECT).round();
    if rows_full <= f64::from(IMAGE_MAX_ROWS) {
        let rows = (rows_full as u16).clamp(IMAGE_MIN_ROWS, IMAGE_MAX_ROWS);
        (rows, None)
    } else {
        let cols = (f64::from(IMAGE_MAX_ROWS) * CELL_ASPECT / ratio).round() as u16;
        (IMAGE_MAX_ROWS, Some(cols.clamp(4, avail_cols)))
    }
}

/// Pixel `(width, height)` from `meta.original` (or `meta.small`), if
/// the server sent them.
fn media_dimensions(m: &MediaAttachment) -> Option<(f64, f64)> {
    let meta = m.meta.as_ref()?;
    for key in ["original", "small"] {
        let dims = &meta[key];
        if let (Some(w), Some(h)) = (dims["width"].as_f64(), dims["height"].as_f64())
            && w > 0.0
            && h > 0.0
        {
            return Some((w, h));
        }
        if let Some(aspect) = dims["aspect"].as_f64()
            && aspect > 0.0
        {
            return Some((aspect, 1.0));
        }
    }
    None
}

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
    render_blocks(status, theme, opts, width, None, None).lines
}

impl CardRender {
    /// Shift the card right by `cols` (after the 2-column gutter) —
    /// used for nested replies on the thread page. Image overlays
    /// move with the text.
    pub fn indent(&mut self, cols: u16) {
        if cols == 0 {
            return;
        }
        let pad = " ".repeat(cols as usize);
        for line in &mut self.lines {
            if line.spans.is_empty() {
                continue;
            }
            line.spans.insert(1, Span::raw(pad.clone()));
        }
        for ov in &mut self.image_overlays {
            ov.x_offset += cols;
        }
    }
}

/// Structured render: returns wrapped, gutter-aligned lines plus a
/// list of image overlay slots the caller should fill with actual
/// `StatefulImage` widgets.
///
/// `music` is an optional Apple Music metadata cache. Passing `Some`
/// enables link enrichment — compact inline rewrites in dense density
/// mode, multi-line music cards with cover-art overlays in spacious
/// mode. Passing `None` leaves the raw Mastodon HTML link untouched.
///
/// `parent` is the post this one replies to, when the caller has it
/// on hand; with `opts.show_reply_hint` it turns the bare
/// `↪ replying to @…` line into `↪ @…: "excerpt"`.
pub fn render_blocks(
    status: &Status,
    theme: &Theme,
    opts: CardOpts,
    width: u16,
    mut music: Option<&mut crate::api::music::MusicCache>,
    parent: Option<&Status>,
) -> CardRender {
    let wrap_w = width.saturating_sub(2); // gutter takes 2 columns
    let (mut pre_lines, body_links) = build_lines(status, theme, opts, parent);

    // Apple Music enrichment. Runs pre-wrap so the inline link
    // replacement lets the body flow naturally. In spacious density
    // mode we also append a music card block just below the link's
    // line — its rows are short enough to pass through wrap_lines
    // untouched, and the overlay offset is mapped post-wrap via
    // `wrap_lines_with_map`.
    let music_enrichments = if let Some(ref mut cache) = music {
        enrich_apple_music(
            &mut pre_lines,
            &body_links,
            cache,
            opts.nerd_font,
            opts.spacious,
            theme,
        )
    } else {
        Vec::new()
    };

    let shown = status.reblog.as_deref().unwrap_or(status);

    // Custom emoji: `:blobcat:` stays as text but drops to the
    // tertiary tier. Runs after link enrichment (which needs the
    // original span indices) and before wrap.
    {
        let codes: Vec<&str> = shown
            .emojis
            .iter()
            .chain(shown.account.emojis.iter())
            .chain(status.account.emojis.iter())
            .map(|e| e.shortcode.as_str())
            .collect();
        crate::ui::widgets::shortcode::dim_shortcodes(&mut pre_lines, &codes, theme.tertiary());
    }

    let (mut wrapped, line_map) = wrap::wrap_lines_with_map(&pre_lines, wrap_w);
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
                // Reserve placeholder rows sized to the picture; the
                // screen overlays the actual image on top after the
                // Paragraph renders.
                let (rows, width_cols) = image_box(m, wrap_w);
                for _ in 0..rows {
                    wrapped.push(Line::default());
                }
                image_overlays.push(ImageOverlay {
                    line_offset: start,
                    height: rows,
                    media_id: m.id.clone(),
                    url,
                    x_offset: 0,
                    width_cols,
                });
                // Alt-text caption below the image, dim italic. Hidden
                // when the uploader didn't bother — most posts.
                let alt = m.description.as_deref().unwrap_or("").trim();
                if !alt.is_empty() {
                    let caption = Line::from(Span::styled(
                        emoji::normalize_owned(&format!("  {alt}")),
                        theme.tertiary().add_modifier(Modifier::ITALIC),
                    ));
                    wrapped.extend(wrap::wrap_lines(&[caption], wrap_w));
                }
            } else {
                wrapped.push(media_line(m, theme, opts.nerd_font));
            }
        }
    }

    // Poll. Rendered as a compact bar list — results are visible
    // whether or not the viewer has voted (voting itself is a
    // non-goal for now; the bars are the point).
    if !cw_hidden && let Some(poll) = &shown.poll {
        wrapped.push(Line::default());
        wrapped.extend(poll_lines(poll, theme, wrap_w));
    }

    // Link preview card. One dim line: `󰌷 Title · provider`. Skipped
    // when the post already carries media (Mastodon doesn't attach a
    // card then anyway) or when the link is an Apple Music URL that
    // got its own enrichment above.
    if !cw_hidden
        && shown.media_attachments.is_empty()
        && let Some(card) = &shown.card
        && let Some(line) = card_line(card, theme, opts.nerd_font)
    {
        wrapped.push(Line::default());
        wrapped.extend(wrap::wrap_lines(&[line], wrap_w));
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
    parent: Option<&Status>,
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
            Span::styled(
                emoji::normalize_owned(&format!("@{} boosted", booster.acct)),
                theme.secondary(),
            ),
        ]));
    }

    // Header: display_name (bold) · @handle (secondary) · timestamp (tertiary).
    let display = emoji::normalize_owned(&if shown.account.display_name.is_empty() {
        shown.account.username.clone()
    } else {
        shown.account.display_name.clone()
    });
    let handle = emoji::normalize_owned(&format!("@{}", shown.account.acct));
    let time = shown
        .created_at
        .map(|ts| format_time(ts, opts.absolute_time))
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

    // Reply preview. With the parent on hand: `↪ @acct: "excerpt"`
    // (CLAUDE.md §7.1). Otherwise a cheaper hint resolved from the
    // `mentions` list the server already sent.
    if opts.show_reply_hint && shown.in_reply_to_id.is_some() {
        if let Some(p) = parent {
            let excerpt = reply_excerpt(p, REPLY_EXCERPT_CHARS);
            let mut spans = vec![
                Span::styled("↪ ", theme.tertiary()),
                Span::styled(
                    emoji::normalize_owned(&format!("@{}", p.account.acct)),
                    theme.secondary(),
                ),
            ];
            if !excerpt.is_empty() {
                spans.push(Span::styled(": ", theme.tertiary()));
                spans.push(Span::styled(
                    emoji::normalize_owned(&format!("\u{201c}{excerpt}\u{201d}")),
                    theme.tertiary().add_modifier(Modifier::ITALIC),
                ));
            }
            out.push(Line::from(spans));
        } else if let Some(hint) = reply_hint(shown) {
            out.push(Line::from(vec![
                Span::styled("↪ ", theme.tertiary()),
                Span::styled(hint, theme.tertiary()),
            ]));
        }
    }

    // Content-warning banner.
    let cw_present = !shown.spoiler_text.is_empty();
    if cw_present {
        let warn = icons::pick(opts.nerd_font, icons::WARNING, icons::WARNING_ASCII);
        out.push(Line::from(vec![
            Span::styled(format!("{warn} "), theme.favorite_style()),
            Span::styled(
                emoji::normalize_owned(&format!("CW: {}", shown.spoiler_text)),
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
    spacious: bool,
    theme: &Theme,
) -> Vec<MusicEnrichment> {
    let icon = if nerd_font {
        ICON_MUSIC
    } else {
        ICON_MUSIC_ASCII
    };
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
            let compact_text = emoji::normalize_owned(&match &meta {
                Some(m) if !m.artist.is_empty() => {
                    format!("{icon} · {} · {}", m.title, m.artist)
                }
                Some(m) => format!("{icon} · {}", m.title),
                None => format!(
                    "{icon} · Apple Music · {}",
                    crate::api::music::humanize_slug(&ml.slug)
                ),
            });
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
    // Goes through the shared span-aware wrapper so CJK / emoji
    // widths and break rules match the post body.
    let wrap = |text: &str, style: Style| -> Vec<Line<'static>> {
        let logical = Line::from(Span::styled(text.to_string(), style));
        wrap::wrap_lines(&[logical], text_avail)
            .into_iter()
            .map(|mut chunk| {
                chunk.spans.insert(0, Span::raw(indent_str.clone()));
                chunk
            })
            .collect()
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    // Top padding — lets the artwork's square frame visually align
    // mid-card rather than flush to the top.
    out.push(Line::default());
    // Title — primary + bold.
    out.extend(wrap(
        &emoji::normalize_owned(&meta.title),
        theme.primary().add_modifier(Modifier::BOLD),
    ));
    // Artist — secondary.
    if !meta.artist.is_empty() {
        out.extend(wrap(
            &emoji::normalize_owned(&meta.artist),
            theme.secondary(),
        ));
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
        out.extend(wrap(&emoji::normalize_owned(&album_line), theme.tertiary()));
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
    let display = emoji::normalize_owned(&if quoted.account.display_name.is_empty() {
        quoted.account.username.clone()
    } else {
        quoted.account.display_name.clone()
    });
    let handle = emoji::normalize_owned(&format!("@{}", quoted.account.acct));
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
            emoji::normalize_owned(&format!("CW: {}", quoted.spoiler_text)),
            theme.tertiary().add_modifier(Modifier::ITALIC),
        )));
    }
    logical.extend(html::render(&quoted.content, theme));

    {
        let codes: Vec<&str> = quoted
            .emojis
            .iter()
            .chain(quoted.account.emojis.iter())
            .map(|e| e.shortcode.as_str())
            .collect();
        crate::ui::widgets::shortcode::dim_shortcodes(&mut logical, &codes, theme.tertiary());
    }

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

fn format_time(ts: chrono::DateTime<Utc>, absolute: bool) -> String {
    if absolute {
        crate::util::time::absolute(Utc::now(), ts)
    } else {
        relative(Utc::now(), ts)
    }
}

/// Locate the post `status` (or its boosted inner post) replies to:
/// first among `siblings` (the same list — threads often land on one
/// page together), then in the app-level `parents` cache.
#[must_use]
pub fn find_parent<'a, S: std::hash::BuildHasher>(
    status: &Status,
    siblings: &'a [Status],
    parents: &'a std::collections::HashMap<crate::api::models::StatusId, Status, S>,
) -> Option<&'a Status> {
    let shown = status.reblog.as_deref().unwrap_or(status);
    let pid = shown.in_reply_to_id.as_ref()?;
    siblings
        .iter()
        .map(|s| s.reblog.as_deref().unwrap_or(s))
        .find(|s| s.id == *pid)
        .or_else(|| parents.get(pid))
}

/// Characters of the parent's body shown in a reply preview.
const REPLY_EXCERPT_CHARS: usize = 72;

/// First `max` chars of the parent's plain text (CW text when the
/// body is behind a warning), collapsed to one line.
fn reply_excerpt(p: &Status, max: usize) -> String {
    let source = if p.spoiler_text.is_empty() {
        html::to_plain_text(&p.content)
    } else {
        format!("CW: {}", p.spoiler_text)
    };
    let one_line: String = source.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut it = one_line.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{}…", head.trim_end())
    } else {
        head
    }
}

/// `replying to @acct`, `in a thread` (self-reply), or `reply` when the
/// server didn't tell us who. `None` only when the status isn't a
/// reply at all.
fn reply_hint(s: &Status) -> Option<String> {
    let target = s.in_reply_to_account_id.as_ref()?;
    if *target == s.account.id {
        return Some("in a thread".to_string());
    }
    let acct = s
        .mentions
        .iter()
        .find(|m| m.id == *target)
        .map(|m| m.acct.as_str());
    Some(match acct {
        Some(a) => emoji::normalize_owned(&format!("replying to @{a}")),
        None => "reply".to_string(),
    })
}

/// Width of the poll result bar in cells.
const POLL_BAR_WIDTH: usize = 10;

/// One line per option (`▰▰▰▱▱ 42%  Title`) plus a dim footer with the
/// vote count and expiry. Own votes get a trailing check.
fn poll_lines(poll: &crate::api::models::Poll, theme: &Theme, wrap_w: u16) -> Vec<Line<'static>> {
    let total = poll
        .voters_count
        .filter(|_| poll.multiple)
        .unwrap_or(poll.votes_count)
        .max(1);
    let own: Vec<u32> = poll.own_votes.clone().unwrap_or_default();
    let mut logical: Vec<Line<'static>> = Vec::new();
    for (i, opt) in poll.options.iter().enumerate() {
        let votes = opt.votes_count.unwrap_or(0);
        let pct = ((votes as f64 / total as f64) * 100.0).round() as usize;
        let filled = (pct * POLL_BAR_WIDTH).div_ceil(100).min(POLL_BAR_WIDTH);
        let mine = own.contains(&(i as u32));
        let bar_style = if mine {
            theme.link()
        } else {
            theme.secondary()
        };
        let mut spans = vec![
            Span::styled("▰".repeat(filled), bar_style),
            Span::styled("▱".repeat(POLL_BAR_WIDTH - filled), theme.tertiary()),
            Span::styled(format!(" {pct:>3}%  "), theme.tertiary()),
            Span::styled(emoji::normalize_owned(&opt.title), theme.primary()),
        ];
        if mine {
            spans.push(Span::styled("  ✓", theme.link()));
        }
        logical.push(Line::from(spans));
    }
    let votes_label = if poll.multiple {
        format!("{} voters", poll.voters_count.unwrap_or(poll.votes_count))
    } else {
        format!("{} votes", poll.votes_count)
    };
    let when = if poll.expired {
        "final".to_string()
    } else if let Some(exp) = poll.expires_at {
        let left = exp.signed_duration_since(Utc::now());
        if left.num_seconds() <= 0 {
            "final".to_string()
        } else if left.num_hours() >= 48 {
            format!("ends in {}d", left.num_days())
        } else if left.num_minutes() >= 90 {
            format!("ends in {}h", left.num_hours())
        } else {
            format!("ends in {}m", left.num_minutes().max(1))
        }
    } else {
        "open".to_string()
    };
    logical.push(Line::from(Span::styled(
        format!("{votes_label}  ·  {when}"),
        theme.tertiary(),
    )));
    wrap::wrap_lines(&logical, wrap_w)
}

/// `󰌷 Title · provider-or-host`, or `None` when the card carries no
/// title or points at an Apple Music URL (handled by the enrichment
/// path instead).
fn card_line(
    card: &crate::api::models::Card,
    theme: &Theme,
    nerd_font: bool,
) -> Option<Line<'static>> {
    let title = card.title.trim();
    if title.is_empty() || crate::api::music::parse_url(&card.url).is_some() {
        return None;
    }
    let source = card
        .provider_name
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| {
            url::Url::parse(&card.url).ok().and_then(|u| {
                u.host_str()
                    .map(|h| h.trim_start_matches("www.").to_string())
            })
        });
    let icon = icons::pick(nerd_font, icons::LINK, icons::LINK_ASCII);
    let mut spans = vec![
        Span::styled(format!("{icon} "), theme.tertiary()),
        Span::styled(emoji::normalize_owned(title), theme.secondary()),
    ];
    if let Some(src) = source {
        spans.push(Span::styled(format!("  ·  {src}"), theme.tertiary()));
    }
    Some(Line::from(spans))
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
        emoji::normalize_owned(&format!("{icon}  {alt}"))
    };
    Line::from(vec![Span::styled(text, theme.secondary())])
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
    fn reply_hint_names_the_mentioned_account() {
        use crate::api::models::{AccountId, Mention};
        let theme = Theme::frost();
        let mut s = fake_status("1", "<p>yes</p>");
        s.in_reply_to_id = Some(StatusId::new("0"));
        s.in_reply_to_account_id = Some(AccountId::new("bob-id"));
        s.mentions.push(Mention {
            id: AccountId::new("bob-id"),
            username: "bob".into(),
            url: String::new(),
            acct: "bob@ex.com".into(),
        });
        let opts = CardOpts {
            show_reply_hint: true,
            ..opts_plain()
        };
        let text: String = render(&s, &theme, opts, 80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(text.contains("replying to @bob@ex.com"), "{text}");
        // Off by default (detail page).
        let text2: String = render(&s, &theme, opts_plain(), 80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(!text2.contains("replying to"));
    }

    #[test]
    fn reply_preview_quotes_the_parent_when_known() {
        let theme = Theme::frost();
        let mut parent = fake_status("0", "<p>What about the <b>other</b> thing?</p>");
        parent.account.acct = "bob@ex.com".into();
        let mut s = fake_status("1", "<p>yes</p>");
        s.in_reply_to_id = Some(StatusId::new("0"));
        let opts = CardOpts {
            show_reply_hint: true,
            ..opts_plain()
        };
        let text: String = render_blocks(&s, &theme, opts, 80, None, Some(&parent))
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(
            text.contains("↪ @bob@ex.com: \u{201c}What about the other thing?\u{201d}"),
            "{text}"
        );
    }

    #[test]
    fn indent_shifts_text_and_overlays() {
        let theme = Theme::frost();
        let s = fake_status("1", "<p>nested</p>");
        let mut block = render_blocks(&s, &theme, opts_plain(), 40, None, None);
        block.indent(4);
        let first = &block.lines[0];
        assert_eq!(first.spans[1].content.as_ref(), "    ");
    }

    #[test]
    fn poll_renders_bars_and_footer() {
        use crate::api::models::{Poll, PollId, PollOption};
        let theme = Theme::frost();
        let mut s = fake_status("1", "<p>vote!</p>");
        s.poll = Some(Poll {
            id: PollId::new("p"),
            expires_at: None,
            expired: true,
            multiple: false,
            votes_count: 4,
            voters_count: None,
            options: vec![
                PollOption {
                    title: "tabs".into(),
                    votes_count: Some(3),
                },
                PollOption {
                    title: "spaces".into(),
                    votes_count: Some(1),
                },
            ],
            emojis: vec![],
            voted: Some(true),
            own_votes: Some(vec![0]),
        });
        let text: String = render(&s, &theme, opts_plain(), 80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(text.contains("75%"), "{text}");
        assert!(text.contains("tabs"));
        assert!(text.contains("✓"));
        assert!(text.contains("4 votes"));
        assert!(text.contains("final"));
    }

    #[test]
    fn link_card_shows_title_and_host() {
        use crate::api::models::Card;
        let theme = Theme::frost();
        let mut s = fake_status("1", "<p>read this</p>");
        s.card = Some(Card {
            url: "https://www.example.org/post/1".into(),
            title: "A Fine Article".into(),
            description: String::new(),
            card_type: crate::api::models::CardType::default(),
            author_name: None,
            author_url: None,
            provider_name: None,
            provider_url: None,
            html: None,
            width: None,
            height: None,
            image: None,
            embed_url: None,
            blurhash: None,
        });
        let text: String = render(&s, &theme, opts_plain(), 80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|sp| sp.content.as_ref()))
            .collect();
        assert!(text.contains("A Fine Article"), "{text}");
        assert!(text.contains("example.org"), "{text}");
    }

    #[test]
    fn custom_emoji_shortcode_is_dimmed_in_body_and_name() {
        use crate::api::models::CustomEmoji;
        let theme = Theme::frost();
        let mut s = fake_status("1", "<p>hello :blobcat: world</p>");
        s.account.display_name = "Alice :verified:".into();
        let mk = |c: &str| CustomEmoji {
            shortcode: c.into(),
            url: String::new(),
            static_url: String::new(),
            visible_in_picker: true,
            category: None,
        };
        s.emojis.push(mk("blobcat"));
        s.account.emojis.push(mk("verified"));
        let lines = render(&s, &theme, opts_plain(), 80);
        let dimmed: Vec<String> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|sp| sp.style.fg == Some(theme.fg_tertiary))
            .map(|sp| sp.content.to_string())
            .collect();
        assert!(dimmed.iter().any(|t| t == ":blobcat:"), "{dimmed:?}");
        assert!(dimmed.iter().any(|t| t == ":verified:"), "{dimmed:?}");
    }

    #[test]
    fn image_box_follows_aspect_ratio() {
        use crate::api::models::{MediaAttachment, MediaId, MediaType};
        let mk = |w: u64, h: u64| MediaAttachment {
            id: MediaId::new("m"),
            media_type: MediaType::Image,
            url: None,
            preview_url: None,
            remote_url: None,
            description: None,
            blurhash: None,
            meta: Some(serde_json::json!({"original": {"width": w, "height": h}})),
            preview_remote_url: None,
            text_url: None,
        };
        // 2:1 panorama in 40 cols → 40 * 0.5 / 2 = 10 rows, full width.
        assert_eq!(image_box(&mk(2000, 1000), 40), (10, None));
        // Square in 40 cols → 20 rows > cap → 16 rows, narrowed to 32 cols.
        assert_eq!(image_box(&mk(1000, 1000), 40), (16, Some(32)));
        // Extreme panorama still gets the minimum strip.
        assert_eq!(image_box(&mk(4000, 200), 40), (IMAGE_MIN_ROWS, None));
        // No meta → legacy fallback.
        let mut m = mk(1, 1);
        m.meta = None;
        assert_eq!(image_box(&m, 40), (IMAGE_PLACEHOLDER_HEIGHT, None));
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
