//! The key reference, as data.
//!
//! One table feeds both the in-app help overlay (`?`) and the key table
//! in `README.md`, so the two cannot drift apart. The actual dispatch
//! lives in `ui::app::input` and the screens; when a binding changes
//! there, change it here too — the README test fails until the table
//! and the README agree, and prints the block to paste.

/// One row of the reference: the keys, and what they do.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    /// Keys as typed, alternatives separated by ` / `.
    pub keys: &'static str,
    /// Short description, lowercase, no trailing period.
    pub action: &'static str,
}

/// A titled group of bindings.
#[derive(Debug, Clone, Copy)]
pub struct Section {
    pub title: &'static str,
    pub bindings: &'static [Binding],
}

const fn b(keys: &'static str, action: &'static str) -> Binding {
    Binding { keys, action }
}

/// Every documented binding, grouped for display.
pub const SECTIONS: &[Section] = &[
    Section {
        title: "Anywhere",
        bindings: &[
            b("?", "toggle this help"),
            b("Esc", "back; at the top level asks before quitting"),
            b("Ctrl+C", "quit (asks first over an unsaved draft)"),
            b("1 / 2 / 3 / 4", "Home / Local / Federated / Notifications"),
            b("5", "your profile"),
            b("6 / 7", "your favourites / bookmarks"),
            b("/", "search accounts · hashtags · posts"),
            b("A", "switch account"),
            b("S", "cycle live updates: streaming · polling · off"),
            b("D", "toggle inter-post density (1 ↔ 2 blank lines)"),
        ],
    },
    Section {
        title: "Moving around",
        bindings: &[
            b("j / ↓", "next post"),
            b("k / ↑", "previous post"),
            b("gg / G", "top / bottom"),
            b("R", "refresh"),
            b("l / Enter", "open thread"),
            b("h / Esc", "back"),
            b("Q", "open the quoted post"),
            b("s", "reveal / hide a content warning"),
            b("Tab / Shift+Tab", "cycle the notification filter"),
        ],
    },
    Section {
        title: "On a post",
        bindings: &[
            b("f", "favourite / unfavourite"),
            b("b", "boost / unboost"),
            b("B", "force un-boost"),
            b("c", "new post"),
            b("r", "reply"),
            b("q", "quote (Mastodon 4.5+ native quote)"),
            b("e", "edit your own post"),
            b("d", "delete your own post (Enter confirms)"),
            b("u", "author's profile"),
            b("o", "open in browser"),
            b("y", "copy link"),
        ],
    },
    Section {
        title: "In a profile",
        bindings: &[
            b("F", "follow / unfollow"),
            b("o / O", "followers / following"),
        ],
    },
    Section {
        title: "Composing",
        bindings: &[
            b("Ctrl+Enter", "send (Alt+Enter and Ctrl+D also work)"),
            b("Esc", "cancel (asks first if there is a draft)"),
            b("Ctrl+V", "cycle visibility"),
            b("Ctrl+S", "toggle content warning"),
            b("Ctrl+O", "attach a file (path, then optional alt text)"),
            b("Ctrl+X", "remove the last attachment"),
            b("Tab", "switch between body and content warning"),
            b(
                "@… / #…",
                "suggest accounts / hashtags: Tab takes, ↑↓ choose, Esc hides",
            ),
        ],
    },
    Section {
        title: "Editing text (compose, search, prompts)",
        bindings: &[
            b("⌥⌫ / Ctrl+W", "delete the word before the cursor"),
            b("⌘⌫ / Ctrl+U", "delete to the start of the line"),
            b("⌥⌦ / Alt+D", "delete the word after the cursor"),
            b("⌘⌦ / Ctrl+K", "delete to the end of the line"),
            b("⌥← / ⌥→", "move by word (also Alt+B / Alt+F, Ctrl+← / →)"),
            b("⌘← / ⌘→", "start / end of line (also Ctrl+A / Ctrl+E)"),
            b("⌘↑ / ⌘↓", "start / end of the text"),
            b("Ctrl+Z / ⌘Z", "undo (also Ctrl+_)"),
            b("Ctrl+Y / ⇧⌘Z", "redo"),
        ],
    },
];

/// Marker comments that fence the generated table in `README.md`.
pub const README_START: &str = "<!-- keys:start (generated from src/ui/keymap.rs) -->";
/// Closing fence; see [`README_START`].
pub const README_END: &str = "<!-- keys:end -->";

/// Lines for the help overlay, section titles included. The closing
/// hint is left to the overlay, which knows whether it has to scroll.
#[must_use]
pub fn help_lines() -> Vec<String> {
    let key_col = SECTIONS
        .iter()
        .flat_map(|s| s.bindings)
        .map(|bd| display_width(bd.keys))
        .max()
        .unwrap_or(0);
    let mut out = vec!["mastoot — keys".to_string(), String::new()];
    for section in SECTIONS {
        out.push(format!("  {}", section.title));
        for bd in section.bindings {
            let pad = key_col - display_width(bd.keys);
            out.push(format!("    {}{}  {}", bd.keys, " ".repeat(pad), bd.action));
        }
        out.push(String::new());
    }
    out
}

/// The Markdown that belongs between the README fences.
#[must_use]
pub fn readme_table() -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for section in SECTIONS {
        let _ = write!(
            out,
            "**{}**\n\n| Key | Action |\n| --- | --- |\n",
            section.title
        );
        for bd in section.bindings {
            let keys = bd
                .keys
                .split(" / ")
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(" ");
            let _ = writeln!(out, "| {keys} | {} |", bd.action);
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readme_key_table_matches_keymap() {
        let readme = include_str!("../../README.md");
        let start = readme
            .find(README_START)
            .expect("README is missing the keys:start fence");
        let end = readme
            .find(README_END)
            .expect("README is missing the keys:end fence");
        let actual = readme[start + README_START.len()..end].trim();
        let expected = readme_table();
        assert!(
            actual == expected,
            "README key table is out of date. Replace the block between the keys fences with:\n\n{expected}\n"
        );
    }

    #[test]
    fn help_overlay_lists_every_binding() {
        let help = help_lines().join("\n");
        for bd in SECTIONS.iter().flat_map(|s| s.bindings) {
            assert!(help.contains(bd.keys), "help is missing {}", bd.keys);
            assert!(help.contains(bd.action), "help is missing {}", bd.action);
        }
    }
}
