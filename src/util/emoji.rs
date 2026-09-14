//! Emoji-aware terminal width normalization.
//!
//! Terminals render every emoji grapheme cluster as 2 cells, but
//! `unicode-width` (which ratatui uses to lay out cells) disagrees for
//! several common shapes:
//!
//! | Cluster                  | `unicode-width` | terminal |
//! |--------------------------|-----------------|----------|
//! | `🤔`                     | 2               | 2        |
//! | `❤️` (text char + VS16)  | 1               | 2        |
//! | `👨‍👩‍👧` (ZWJ sequence)  | 6               | 2        |
//! | `🇯🇵` (regional indicator)| 4               | 2        |
//! | `1️⃣` (keycap)            | 1               | 2        |
//!
//! Under-width clusters cause neighbouring characters to overlap the
//! emoji's trailing cell. Over-width clusters cause gaps. Both look bad
//! when emojis are consecutive.
//!
//! [`normalize`] walks each grapheme cluster, detects emoji shapes via
//! [`unicode-properties`] plus VS16 / ZWJ / regional-indicator
//! heuristics, and forces every emoji cluster to width 2 by padding
//! under-wide clusters with trailing spaces. Over-width clusters are
//! left as-is (no good fix without losing characters) but flagged in
//! tests for awareness.

use std::borrow::Cow;

use unicode_properties::UnicodeEmoji;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const VS16: char = '\u{FE0F}';
const ZWJ: char = '\u{200D}';

/// Pad every emoji grapheme cluster in `s` so its reported width
/// matches what the terminal actually draws (2 cells).
///
/// Non-emoji text passes through unchanged. The output retains the
/// original characters in order; only trailing ASCII spaces are
/// appended after emoji clusters whose [`UnicodeWidthStr::width`] is
/// less than 2.
#[must_use]
pub fn normalize(s: &str) -> Cow<'_, str> {
    if s.is_empty() || !needs_normalization(s) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 4);
    for cluster in s.graphemes(true) {
        out.push_str(cluster);
        if is_emoji_cluster(cluster) {
            let w = UnicodeWidthStr::width(cluster);
            if w < 2 {
                for _ in w..2 {
                    out.push(' ');
                }
            }
        }
    }
    Cow::Owned(out)
}

/// Owned convenience for call sites that build the input from a
/// temporary (`format!` / `if … else` expressions) and need a
/// `String` to move into a `Span<'static>`.
#[must_use]
pub fn normalize_owned(s: &str) -> String {
    normalize(s).into_owned()
}

/// Fast pre-check: skip the grapheme walk if `s` clearly contains no
/// emoji code points. Lets the hot path (ASCII status content) stay
/// allocation-free.
fn needs_normalization(s: &str) -> bool {
    s.chars().any(|c| {
        c == VS16
            || c == ZWJ
            || is_regional_indicator(c)
            || (c.is_emoji_char() && !is_text_default(c))
    })
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

/// True when a grapheme cluster will render as a 2-cell emoji in a
/// modern terminal, regardless of what `unicode-width` claims.
fn is_emoji_cluster(cluster: &str) -> bool {
    let mut chars = cluster.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    // VS16 anywhere in the cluster forces emoji presentation.
    if cluster.contains(VS16) {
        return true;
    }
    // ZWJ sequences are emoji by construction in any well-formed text.
    if cluster.contains(ZWJ) {
        return true;
    }
    // Two regional indicators = flag.
    if is_regional_indicator(first) {
        return true;
    }
    // Default-emoji-presentation code points (Emoji_Presentation=Yes).
    if first.is_emoji_char() && !is_text_default(first) {
        return true;
    }
    false
}

/// A few code points have Emoji=Yes but Emoji_Presentation=No, i.e.
/// they render as text glyphs unless followed by VS16. We treat those
/// as non-emoji here; the VS16 branch above catches the emoji form.
fn is_text_default(c: char) -> bool {
    matches!(
        c as u32,
        0x0023 | 0x002A | 0x0030..=0x0039 // # * 0-9 (keycap bases)
            | 0x00A9 | 0x00AE                // © ®
            | 0x2122 | 0x2139                // ™ ℹ
            | 0x2194..=0x2199                // arrows
            | 0x21A9..=0x21AA
            | 0x2328 | 0x23CF
            | 0x2600..=0x2604
            | 0x260E | 0x2611
            | 0x2614..=0x2615
            | 0x2618 | 0x261D | 0x2620
            | 0x2622..=0x2623
            | 0x2626 | 0x262A
            | 0x262E..=0x262F
            | 0x2638..=0x263A
            | 0x2640 | 0x2642
            | 0x265F | 0x2660 | 0x2663 | 0x2665..=0x2666 | 0x2668
            | 0x267B | 0x267E
            | 0x2692..=0x2697
            | 0x2699 | 0x269B..=0x269C
            | 0x26A0
            | 0x26B0..=0x26B1
            | 0x26C8 | 0x26CF
            | 0x26D1 | 0x26D3..=0x26D4
            | 0x26E9..=0x26EA
            | 0x26F0..=0x26F5
            | 0x26F7..=0x26FA
            | 0x2702 | 0x2708..=0x2709
            | 0x270C..=0x270D
            | 0x270F | 0x2712 | 0x2714 | 0x2716 | 0x271D
            | 0x2721 | 0x2733..=0x2734 | 0x2744 | 0x2747
            | 0x2763..=0x2764             // ❤
            | 0x27A1
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured_width(s: &str) -> usize {
        UnicodeWidthStr::width(s)
    }

    #[test]
    fn ascii_passes_through() {
        let s = "hello world";
        assert_eq!(normalize(s), s);
    }

    #[test]
    fn plain_emoji_unchanged() {
        // 🤔 already width 2 — no pad needed.
        let s = "🤔";
        assert_eq!(normalize(s), s);
        assert_eq!(measured_width(&normalize(s)), 2);
    }

    #[test]
    fn vs16_heart_is_padded_to_width_two() {
        // ❤ + VS16 = ❤️. Unicode-width says 1; terminal draws 2.
        let s = "\u{2764}\u{FE0F}";
        let out = normalize(s);
        assert!(out.starts_with(s));
        assert_eq!(measured_width(&out), 2);
    }

    #[test]
    fn keycap_one_is_padded_to_width_two() {
        // 1 + VS16 + COMBINING ENCLOSING KEYCAP = 1️⃣
        let s = "1\u{FE0F}\u{20E3}";
        let out = normalize(s);
        assert!(out.starts_with(s));
        assert_eq!(measured_width(&out), 2);
    }

    #[test]
    fn consecutive_vs16_emoji_no_longer_overlap() {
        // Three hearts in a row used to total width 3, terminal drew 6
        // cells → overlap. Normalized should total width 6.
        let s = "\u{2764}\u{FE0F}\u{2764}\u{FE0F}\u{2764}\u{FE0F}";
        let out = normalize(s);
        assert_eq!(measured_width(&out), 6);
    }

    #[test]
    fn flag_cluster_is_detected_as_emoji() {
        // 🇯🇵 = regional indicator pair. Unicode-width sums to 4 but
        // terminal renders as 2. We can't shrink it, but is_emoji_cluster
        // must say yes (so future Span-splitting can opt in).
        let s = "\u{1F1EF}\u{1F1F5}";
        assert!(is_emoji_cluster(s));
    }

    #[test]
    fn mixed_text_and_emoji() {
        // "hi ❤️ there" — pad only the heart.
        let s = "hi \u{2764}\u{FE0F} there";
        let out = normalize(s);
        // Visible chars match: "hi ❤️" + " there" with one extra pad.
        assert!(out.contains("\u{2764}\u{FE0F} "));
        assert_eq!(measured_width(&out), measured_width("hi xx there"));
    }

    #[test]
    fn nerd_font_glyphs_pass_through() {
        // Material Design Icons in PUA — these are 1 cell wide and
        // should NOT be flagged as emoji or padded.
        let s = "\u{F01E6}\u{F04CE}"; // BOOST, FAVORITE from icons.rs
        let out = normalize(s);
        assert_eq!(out, s);
    }

    #[test]
    fn no_allocation_when_no_emoji() {
        // Sanity: needs_normalization should be cheap-false here.
        assert!(!needs_normalization("plain ASCII status text"));
        assert!(!needs_normalization("@user@example.com posted a #thing"));
    }
}
