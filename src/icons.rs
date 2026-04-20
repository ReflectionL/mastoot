//! Nerd Font icon constants.
//!
//! Uses the Material Design Icons (`nf-md-*`) range, which is supported by
//! every Nerd Font patched set (JetBrainsMono Nerd Font, FiraCode Nerd Font,
//! Hack Nerd Font, etc.). Each constant carries both the Unicode codepoint
//! and an ASCII fallback used when `config.ui.nerd_font = false`.

pub const BOOST: &str = "\u{f01e6}"; // nf-md-repeat_variant
pub const BOOST_ASCII: &str = "[boost]";

pub const FAVORITE: &str = "\u{f04ce}"; // nf-md-star
pub const FAVORITE_ASCII: &str = "*";

pub const REPLY: &str = "\u{f0167}"; // nf-md-reply
pub const REPLY_ASCII: &str = "↪";

pub const BOOKMARK: &str = "\u{f00c0}"; // nf-md-bookmark
pub const BOOKMARK_ASCII: &str = "[bookmark]";

pub const IMAGE: &str = "\u{f0976}"; // nf-md-image
pub const IMAGE_ASCII: &str = "[img]";

pub const VIDEO: &str = "\u{f05a0}"; // nf-md-video
pub const VIDEO_ASCII: &str = "[video]";

pub const GIF: &str = "\u{f0a0f}"; // nf-md-file_gif_box
pub const GIF_ASCII: &str = "[gif]";

pub const LOCK: &str = "\u{f033e}"; // nf-md-lock
pub const LOCK_ASCII: &str = "[lock]";

pub const VERIFIED: &str = "\u{f05e1}"; // nf-md-check_decagram
pub const VERIFIED_ASCII: &str = "[verified]";

pub const LINK: &str = "\u{f0337}"; // nf-md-link_variant
pub const LINK_ASCII: &str = "[link]";

pub const WARNING: &str = "\u{f0026}"; // nf-md-alert
pub const WARNING_ASCII: &str = "[!]";

pub const NOTIFICATION: &str = "\u{f009a}"; // nf-md-bell
pub const NOTIFICATION_ASCII: &str = "[bell]";

/// Cursor glyph used to mark the selected row in timelines. Not a Nerd Font
/// codepoint — a regular Unicode box drawing character.
pub const CURSOR: &str = "\u{258F}"; // ▏

/// Picks between [`NF`] and [`ASCII`] depending on the runtime setting.
#[must_use]
pub fn pick(nerd_font: bool, nf: &'static str, ascii: &'static str) -> &'static str {
    if nerd_font { nf } else { ascii }
}
