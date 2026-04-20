//! Apple Music link enrichment.
//!
//! Mastodon posts contain bare `https://music.apple.com/…` URLs that
//! look like plain links in the default HTML render. We want to show
//! the user something prettier: artist + track typography and (in
//! spacious density mode) a rendered cover-art thumbnail.
//!
//! The URL itself only carries the slug (a URL-safe version of the
//! album or track name) and the numeric `id`. Real titles, artist
//! names, and artwork URLs live on the public iTunes Search / Lookup
//! API — **no auth required, no key, just HTTP GET**. This module
//! caches lookups per-process so repeated renders don't hit the net.
//!
//! Architecture mirrors [`crate::ui::images::ImageCache`]: a `pending`
//! set dedupes in-flight requests, a `failed` set prevents retry
//! storms, and completed `AppleMusicMeta` drops into `cache` via an
//! mpsc channel drained on the UI render tick.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

const COMPLETION_CAP: usize = 64;

/// Kinds of Apple Music links we care about. Playlist and Artist are
/// detected but don't get artwork in v1 (they'd need a separate
/// `search` call; straightforward follow-up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppleMusicKind {
    Song,
    Album,
    Playlist,
    Artist,
}

impl AppleMusicKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Song => "Song",
            Self::Album => "Album",
            Self::Playlist => "Playlist",
            Self::Artist => "Artist",
        }
    }
}

/// Parsed triple from an `https://music.apple.com/...` URL. `id` is
/// the *lookup* id: on a song URL that has `?i=NNNN`, we use the song
/// id (not the album id) so metadata describes the track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleMusicLink {
    pub kind: AppleMusicKind,
    pub id: String,
    /// Slug from the path. Fallback display when metadata hasn't
    /// arrived yet. Stays URL-safe (dash-separated words).
    pub slug: String,
}

/// Metadata pulled from iTunes Lookup, normalized to the handful of
/// fields the UI actually renders.
#[derive(Debug, Clone)]
pub struct AppleMusicMeta {
    pub kind: AppleMusicKind,
    pub title: String,
    pub artist: String,
    /// Only present for Song (from `collectionName`). Album entries
    /// leave this blank — the title *is* the album name.
    pub album: Option<String>,
    /// 4-digit year parsed from `releaseDate`.
    pub year: Option<u16>,
    /// High-resolution artwork URL (600x600) derived from the 100x100
    /// URL the API returns.
    pub artwork_url: Option<String>,
}

type Completion = (String, Result<AppleMusicMeta, String>);

pub struct MusicCache {
    cache: HashMap<String, AppleMusicMeta>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    tx: mpsc::Sender<Completion>,
    rx: mpsc::Receiver<Completion>,
}

impl Default for MusicCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MusicCache {
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(COMPLETION_CAP);
        Self {
            cache: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            tx,
            rx,
        }
    }

    /// Kick off a lookup for `id` if it's not already in-flight or
    /// known-failed. Cheap to call every render — the sets dedup.
    pub fn ensure_loaded(&mut self, link: &AppleMusicLink) {
        if self.cache.contains_key(&link.id)
            || self.pending.contains(&link.id)
            || self.failed.contains(&link.id)
        {
            return;
        }
        self.pending.insert(link.id.clone());
        let id = link.id.clone();
        let kind = link.kind;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = lookup(&id, kind).await;
            let _ = tx.send((id, res)).await;
        });
    }

    /// Drain newly arrived lookups from the channel into the cache.
    /// Called once per render tick — cheap when nothing arrived.
    pub fn drain(&mut self) {
        while let Ok((id, res)) = self.rx.try_recv() {
            self.pending.remove(&id);
            match res {
                Ok(meta) => {
                    debug!(%id, title = %meta.title, "music metadata cached");
                    self.cache.insert(id, meta);
                }
                Err(e) => {
                    warn!(%id, %e, "music lookup failed");
                    self.failed.insert(id);
                }
            }
        }
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&AppleMusicMeta> {
        self.cache.get(id)
    }
}

/// Parse a URL into an [`AppleMusicLink`]. Returns `None` for
/// non-Apple-Music or unrecognized path shapes.
///
/// Supported shapes (US store shown; any country code works):
/// - `music.apple.com/us/song/<slug>/<id>`
/// - `music.apple.com/us/album/<slug>/<id>` (album view)
/// - `music.apple.com/us/album/<slug>/<id>?i=<song-id>` (track deep-link)
/// - `music.apple.com/us/playlist/<slug>/pl.<id>`
/// - `music.apple.com/us/artist/<slug>/<id>`
#[must_use]
pub fn parse_url(url: &str) -> Option<AppleMusicLink> {
    let url = url.trim();
    // Accept bare host too. Normalize to a parseable URL.
    let normalized = if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with("music.apple.com/") {
        format!("https://{url}")
    } else {
        return None;
    };
    let parsed = url::Url::parse(&normalized).ok()?;
    if parsed.host_str()? != "music.apple.com" {
        return None;
    }
    let path: Vec<&str> = parsed
        .path_segments()
        .map(|p| p.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    // Expected: [country, kind, slug, id]. Playlist's id starts with
    // `pl.` which is fine to keep verbatim.
    if path.len() < 4 {
        return None;
    }
    let kind = match path[1] {
        "song" => AppleMusicKind::Song,
        "album" => AppleMusicKind::Album,
        "playlist" => AppleMusicKind::Playlist,
        "artist" => AppleMusicKind::Artist,
        _ => return None,
    };
    let slug = path[2].to_string();
    // A song deep-link is an album URL with `?i=<song-id>`. Use the
    // song id so lookup returns track metadata.
    let deep_link_song = parsed
        .query_pairs()
        .find(|(k, _)| k == "i")
        .map(|(_, v)| v.to_string());
    let (kind, id) = match (kind, deep_link_song) {
        (AppleMusicKind::Album, Some(song_id)) => (AppleMusicKind::Song, song_id),
        (k, _) => (k, path[3].to_string()),
    };
    Some(AppleMusicLink { kind, id, slug })
}

/// Slug → display-friendly fallback (`dont-look-back-in-anger` →
/// `Dont Look Back In Anger`). Not perfect — can't recover casing
/// correctly in all cases — but good enough while metadata is loading.
#[must_use]
pub fn humanize_slug(slug: &str) -> String {
    slug.split('-')
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Deserialize)]
struct LookupResponse {
    results: Vec<LookupResult>,
}

#[derive(Deserialize)]
struct LookupResult {
    #[serde(rename = "wrapperType")]
    wrapper_type: Option<String>,
    #[serde(rename = "trackName")]
    track_name: Option<String>,
    #[serde(rename = "collectionName")]
    collection_name: Option<String>,
    #[serde(rename = "artistName")]
    artist_name: Option<String>,
    #[serde(rename = "artworkUrl100")]
    artwork_url100: Option<String>,
    #[serde(rename = "releaseDate")]
    release_date: Option<String>,
}

async fn lookup(id: &str, kind: AppleMusicKind) -> Result<AppleMusicMeta, String> {
    // iTunes Lookup is a single GET with no auth. `country=us` nudges
    // the server toward English metadata; the caller's URL might be
    // for a different store but naming is identical cross-region.
    let url = format!("https://itunes.apple.com/lookup?id={id}&country=us");
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status()));
    }
    let body: LookupResponse = resp.json().await.map_err(|e| e.to_string())?;
    let first = body
        .results
        .into_iter()
        .next()
        .ok_or_else(|| "empty results".to_string())?;

    // iTunes sometimes returns a containing artist / collection entry
    // instead of the exact wrapper we expected (e.g. a playlist id
    // hits a collection wrapper). Accept whichever shape came back.
    let title = match kind {
        AppleMusicKind::Song => first
            .track_name
            .clone()
            .or(first.collection_name.clone())
            .ok_or_else(|| "no track/collection name".to_string())?,
        AppleMusicKind::Album | AppleMusicKind::Playlist => first
            .collection_name
            .clone()
            .or(first.track_name.clone())
            .ok_or_else(|| "no collection name".to_string())?,
        AppleMusicKind::Artist => first
            .artist_name
            .clone()
            .ok_or_else(|| "no artist name".to_string())?,
    };
    let album = match kind {
        AppleMusicKind::Song => first.collection_name.clone(),
        _ => None,
    };
    let year = first
        .release_date
        .as_ref()
        .and_then(|d| d.get(0..4).and_then(|y| y.parse().ok()));
    // The artwork URL the API hands back is 100x100. Swap the
    // dimensions in the URL for something closer to "card-friendly";
    // the CDN serves these sizes identically.
    let artwork_url = first.artwork_url100.map(|u| upscale_artwork(&u, 600));
    let wrapper = first.wrapper_type.unwrap_or_default();
    debug!(%id, %title, wrapper, "music lookup ok");
    Ok(AppleMusicMeta {
        kind,
        title,
        artist: first.artist_name.unwrap_or_default(),
        album,
        year,
        artwork_url,
    })
}

/// Rewrite iTunes' `100x100bb` size segment to a larger one. When the
/// URL shape is unexpected, return it unchanged.
fn upscale_artwork(url: &str, size: u16) -> String {
    let needle = "/100x100bb";
    if let Some(pos) = url.rfind(needle) {
        use std::fmt::Write;
        let mut out = String::with_capacity(url.len());
        out.push_str(&url[..pos]);
        let _ = write!(out, "/{size}x{size}bb");
        out.push_str(&url[pos + needle.len()..]);
        out
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_album_url() {
        let link = parse_url("https://music.apple.com/us/album/foo-bar/1440858346").unwrap();
        assert_eq!(link.kind, AppleMusicKind::Album);
        assert_eq!(link.id, "1440858346");
        assert_eq!(link.slug, "foo-bar");
    }

    #[test]
    fn parse_song_deep_link() {
        let link = parse_url(
            "https://music.apple.com/us/album/dont-look-back-in-anger/1440858346?i=1440859099",
        )
        .unwrap();
        assert_eq!(link.kind, AppleMusicKind::Song);
        assert_eq!(link.id, "1440859099");
        assert_eq!(link.slug, "dont-look-back-in-anger");
    }

    #[test]
    fn parse_playlist_url() {
        let link = parse_url("https://music.apple.com/us/playlist/todays-hits/pl.abc123").unwrap();
        assert_eq!(link.kind, AppleMusicKind::Playlist);
        assert_eq!(link.id, "pl.abc123");
    }

    #[test]
    fn parse_non_apple_music_returns_none() {
        assert!(parse_url("https://example.com/foo").is_none());
        assert!(parse_url("https://music.apple.com/us/album/").is_none()); // too short
    }

    #[test]
    fn humanize_slug_capitalizes_words() {
        assert_eq!(humanize_slug("dont-look-back"), "Dont Look Back");
        assert_eq!(humanize_slug(""), "");
    }

    #[test]
    fn upscale_rewrites_size_segment() {
        let before = "https://is1-ssl.mzstatic.com/image/thumb/.../abc/100x100bb.jpg";
        let after = upscale_artwork(before, 600);
        assert!(after.contains("600x600bb"));
    }
}
