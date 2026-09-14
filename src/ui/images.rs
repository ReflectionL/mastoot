//! Asynchronous image cache for ratatui-image.
//!
//! Mastodon timelines are dense with media. The TUI can't block on
//! HTTP downloads, so this module hands fetches off to background
//! tokio tasks and feeds finished bytes back through an mpsc channel.
//! `drain()` is called once per render tick to consume any newly
//! arrived downloads and decode them into per-image protocols.
//!
//! Protocol detection runs once at startup via [`Picker::from_query_stdio`].
//! When the host terminal doesn't support any of the inline-image
//! protocols (kitty / iTerm2 / Sixel / halfblocks), the picker falls
//! back to a coarse half-block renderer; if even that fails, the
//! whole module degrades gracefully — `ensure_loaded` becomes a
//! no-op and rendering callers see `None` from `get_mut`.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use image::ImageReader;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use std::sync::{Arc, OnceLock};

use tokio::sync::{Notify, Semaphore, mpsc};
use tracing::{debug, warn};

use crate::api::models::MediaId;
use crate::ui::widgets::status_card::ImageOverlay;

/// Channel back-pressure cap. We rarely have more than a dozen images
/// in flight, but reblogs and reload-bursts can queue more.
const COMPLETION_CAP: usize = 64;

/// Hard ceiling on bytes per image. Anything larger is dropped — kitty
/// and Sixel can choke on multi-MB JPEGs and the inline-rendering
/// pipeline is single-threaded per frame.
const MAX_BYTES: usize = 6 * 1024 * 1024;

/// Result of one fetch: a decoded image ready for the picker, or a
/// stringified error to log and forget. Decoding happens on the
/// blocking pool inside the download task, never on the render thread.
type Completion = (MediaId, Result<image::DynamicImage, String>);

/// Cap on simultaneous media downloads. A long timeline can reference
/// dozens of previews at once; four in flight keeps the API happy and
/// the first pictures appear sooner than with a thundering herd.
const MAX_CONCURRENT_DOWNLOADS: usize = 4;

fn download_gate() -> &'static Arc<Semaphore> {
    static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)))
}

pub struct ImageCache {
    picker: Option<Picker>,
    init_status: String,
    cache: HashMap<MediaId, StatefulProtocol>,
    pending: HashSet<MediaId>,
    /// Permanently failed downloads — don't retry on every render.
    failed: HashSet<MediaId>,
    tx: mpsc::Sender<Completion>,
    rx: mpsc::Receiver<Completion>,
    /// Pinged after every completed download so the render loop
    /// redraws right away instead of waiting for the next key press
    /// or the 30 s tick.
    notify: Arc<Notify>,
}

impl ImageCache {
    /// Initialize the cache. Probes the terminal for inline-image
    /// support; if the probe times out or fails we fall back to the
    /// universal halfblock renderer (every terminal can draw colored
    /// half-blocks). `enabled()` will still report true; rendering
    /// just looks blockier.
    ///
    /// `override_protocol`: explicit user / env override. Useful
    /// because `from_query_stdio` famously misdetects iTerm2 as
    /// kitty (both reply to the kitty graphics query, but iTerm2 only
    /// actually renders the proprietary OSC 1337 protocol). When None,
    /// we auto-detect iTerm2 via `TERM_PROGRAM` and force the right
    /// protocol type.
    #[must_use]
    pub fn new() -> Self {
        Self::with_override(detect_protocol_override())
    }

    /// Variant that lets the caller force a protocol — wired through
    /// `[ui] image_protocol = "iterm2|kitty|sixel|halfblocks"` in
    /// the config (Phase 4 polish).
    #[must_use]
    pub fn with_override(override_protocol: Option<ProtocolType>) -> Self {
        let (picker, status) = match Picker::from_query_stdio() {
            Ok(mut p) => {
                if let Some(forced) = override_protocol {
                    debug!(?forced, was = ?p.protocol_type(), "forcing image protocol");
                    p.set_protocol_type(forced);
                }
                debug!("ratatui-image picker initialized: {p:?}");
                let s = format!("images: {:?}", p.protocol_type());
                (Some(p), s)
            }
            Err(e) => {
                warn!(?e, "image picker probe failed; falling back to halfblocks");
                (
                    Some(Picker::halfblocks()),
                    "images: halfblocks fallback".into(),
                )
            }
        };
        let (tx, rx) = mpsc::channel(COMPLETION_CAP);
        Self {
            picker,
            init_status: status,
            cache: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            tx,
            rx,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Handle the render loop can `await` on to be woken when a
    /// download lands (see [`Notify::notified`]).
    #[must_use]
    pub fn wakeup(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// A cache that never renders anything — `[ui] media_render =
    /// "text_only"`. Every call site keeps working; `enabled()` is
    /// false so cards fall back to icon + alt-text lines.
    #[must_use]
    pub fn disabled() -> Self {
        let (tx, rx) = mpsc::channel(1);
        Self {
            picker: None,
            init_status: "images: off (config)".into(),
            cache: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            tx,
            rx,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Build from the user's `[ui]` config section.
    #[must_use]
    pub fn from_config(media: crate::config::MediaRender, protocol: Option<&str>) -> Self {
        if media == crate::config::MediaRender::TextOnly {
            return Self::disabled();
        }
        let forced = protocol.and_then(parse_protocol);
        if protocol.is_some() && forced.is_none() {
            warn!(
                ?protocol,
                "unknown image_protocol in config; auto-detecting"
            );
        }
        Self::with_override(forced.or_else(detect_protocol_override))
    }

    /// Whether image rendering is available at all on this terminal.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }

    /// Human-readable description of the picker init outcome — shown
    /// once at startup as an info toast so the user can tell at a
    /// glance whether they got the kitty / iTerm2 / Sixel protocol or
    /// fell back to halfblocks.
    #[must_use]
    pub fn init_status(&self) -> &str {
        &self.init_status
    }

    /// Kick off a download for `url` if not already cached, pending,
    /// or known to have failed. Cheap to call every render — internal
    /// sets dedup. Spawns a tokio task that POSTs the bytes back over
    /// the internal channel.
    pub fn ensure_loaded(&mut self, id: &MediaId, url: &str) {
        if self.picker.is_none()
            || self.cache.contains_key(id)
            || self.pending.contains(id)
            || self.failed.contains(id)
        {
            return;
        }
        self.pending.insert(id.clone());
        let id = id.clone();
        let url = url.to_string();
        let tx = self.tx.clone();
        let notify = Arc::clone(&self.notify);
        let gate = Arc::clone(download_gate());
        tokio::spawn(async move {
            let res = {
                let _permit = gate.acquire().await;
                match download(&url).await {
                    Ok(bytes) => tokio::task::spawn_blocking(move || decode(&bytes))
                        .await
                        .unwrap_or_else(|e| Err(format!("decode task failed: {e}"))),
                    Err(e) => Err(e),
                }
            };
            let _ = tx.send((id, res)).await;
            notify.notify_one();
        });
    }

    /// Drain any completed downloads from the channel into ratatui-image
    /// protocols ready for `render_stateful_widget`. Cheap: the pixels
    /// were already decoded on the blocking pool.
    pub fn drain(&mut self) {
        // try_recv loop — non-blocking, drains everything available.
        while let Ok((id, res)) = self.rx.try_recv() {
            self.pending.remove(&id);
            match res {
                Ok(img) => {
                    let Some(picker) = self.picker.as_mut() else {
                        continue;
                    };
                    let proto = picker.new_resize_protocol(img);
                    self.cache.insert(id, proto);
                }
                Err(e) => {
                    warn!(%id, %e, "image fetch failed");
                    self.failed.insert(id);
                }
            }
        }
    }

    /// Mutable access to a loaded protocol. ratatui-image needs `&mut`
    /// at render time so it can resize/encode lazily.
    pub fn get_mut(&mut self, id: &MediaId) -> Option<&mut StatefulProtocol> {
        self.cache.get_mut(id)
    }
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_protocol(name: &str) -> Option<ProtocolType> {
    match name.trim().to_ascii_lowercase().as_str() {
        "kitty" => Some(ProtocolType::Kitty),
        "iterm2" | "iterm" => Some(ProtocolType::Iterm2),
        "sixel" => Some(ProtocolType::Sixel),
        "halfblocks" | "halfblock" | "blocks" => Some(ProtocolType::Halfblocks),
        _ => None,
    }
}

/// Heuristic: pick a protocol override based on `TERM_PROGRAM` /
/// related env vars. Returns `None` when we trust ratatui-image's
/// auto-detection (e.g., kitty proper, alacritty, generic xterm).
fn detect_protocol_override() -> Option<ProtocolType> {
    // Most common false positive: iTerm2 advertises kitty graphics
    // support in its query response but only actually renders the
    // OSC-1337 (iTerm2) protocol.
    let term_program = std::env::var("TERM_PROGRAM").ok();
    match term_program.as_deref() {
        Some("iTerm.app") => Some(ProtocolType::Iterm2),
        // WezTerm supports both kitty and iTerm2; trust the query.
        // Ghostty supports kitty natively; trust the query.
        // Apple Terminal.app supports neither — fall through to
        // halfblocks via the auto-detection.
        _ => None,
    }
}

/// Draw a single [`ImageOverlay`] into the current frame. Shared by
/// every screen that renders status cards so the overlay → rect →
/// StatefulImage path is identical across timeline / detail /
/// profile (A3: "images everywhere" convergence).
///
/// `area` is the whole screen-content rect (outer, before the card's
/// Block padding). `h_pad` is the Block's horizontal padding. The
/// inner vertical padding is hard-coded to 1 (matches every status
/// list's Block). `scroll` is the row offset applied to the
/// Paragraph.
///
/// Triggers a background download via `ensure_loaded` if the overlay
/// isn't cached yet — cheap to call every render.
pub fn draw_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    h_pad: u16,
    abs_offset: u16,
    scroll: u16,
    ov: &ImageOverlay,
    cache: &mut ImageCache,
) {
    cache.ensure_loaded(&ov.media_id, &ov.url);

    let inner_x = area.x + h_pad;
    let inner_y = area.y + 1; // Block::padding top = 1
    let inner_w = area.width.saturating_sub(h_pad * 2);
    let inner_h = area.height.saturating_sub(1);

    let Some(start_row) = abs_offset.checked_sub(scroll) else {
        return;
    };
    if start_row >= inner_h {
        return;
    }
    if start_row + ov.height > inner_h {
        return;
    }
    let img_x = inner_x + 2 + ov.x_offset;
    let avail_w = inner_w.saturating_sub(2 + ov.x_offset);
    let img_w = ov.width_cols.unwrap_or(avail_w).min(avail_w);
    if img_w == 0 {
        return;
    }
    let rect = Rect {
        x: img_x,
        y: inner_y + start_row,
        width: img_w,
        height: ov.height,
    };
    if let Some(proto) = cache.get_mut(&ov.media_id) {
        let widget: StatefulImage<StatefulProtocol> =
            StatefulImage::default().resize(Resize::Fit(None));
        frame.render_stateful_widget(widget, rect, proto);
    }
}

/// HTTP GET with no auth (Mastodon media URLs are public CDN paths).
async fn download(url: &str) -> Result<Vec<u8>, String> {
    let resp = crate::api::client::shared_http()
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("oversize ({} bytes)", bytes.len()));
    }
    Ok(bytes.to_vec())
}

/// CPU-bound decode. Runs on tokio's blocking pool.
fn decode(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ensure_loaded_doesnt_block_or_insert_synchronously() {
        // Even with a working picker (halfblocks fallback under
        // `cargo test`), ensure_loaded spawns the download in the
        // background — the cache should not be populated by the time
        // it returns.
        let mut c = ImageCache::new();
        c.ensure_loaded(&MediaId::new("x"), "https://invalid.example/x.png");
        assert!(c.get_mut(&MediaId::new("x")).is_none());
    }
}
