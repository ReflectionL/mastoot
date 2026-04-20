//! Theme — a single source of truth for every color and emphasis level in
//! the TUI. Widgets never construct their own `ratatui::style::Style`
//! literals; they go through a [`Theme`] instance.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // Three-level foreground hierarchy.
    pub fg_primary: Color,
    pub fg_secondary: Color,
    pub fg_tertiary: Color,

    // Functional accents.
    pub accent: Color,
    pub mention: Color,
    pub hashtag: Color,
    pub boost: Color,
    pub favorite: Color,
    pub error: Color,

    pub bg: Color,
}

impl Theme {
    /// Cool, restrained default — the reference look for the project.
    #[must_use]
    pub fn frost() -> Self {
        Self {
            fg_primary: Color::Rgb(225, 227, 235),
            fg_secondary: Color::Rgb(148, 154, 172),
            fg_tertiary: Color::Rgb(95, 100, 120),

            accent: Color::Rgb(124, 158, 255),
            mention: Color::Rgb(184, 166, 255),
            hashtag: Color::Rgb(143, 207, 191),
            boost: Color::Rgb(143, 207, 143),
            favorite: Color::Rgb(255, 196, 120),
            error: Color::Rgb(255, 120, 120),

            bg: Color::Reset,
        }
    }

    /// Warm alternate. "Ember" evokes smoldering parchment — copper as
    /// the signature accent, warm-grey hierarchy, and functional hues
    /// that stay in the warm family but are far enough apart in hue to
    /// stay legible against one another (the original palette had
    /// mention/accent both copper, boost/hashtag both olive — fixed).
    #[must_use]
    pub fn ember() -> Self {
        Self {
            // Warm three-level hierarchy. Tertiary bumped up slightly so
            // `h / Esc to go back` style hints stay readable on very
            // dark terminal backgrounds.
            fg_primary: Color::Rgb(236, 227, 218),
            fg_secondary: Color::Rgb(183, 160, 134),
            fg_tertiary: Color::Rgb(125, 104, 84),

            // Copper — signature. Used for cursor, links, focus cues.
            accent: Color::Rgb(206, 128, 82),
            // Dusty rose — one hue-step toward magenta so `@handle`
            // reads as a *different* kind of emphasis than a link,
            // while staying inside the warm envelope.
            mention: Color::Rgb(220, 150, 162),
            // Amber-gold. Yellower than before so it's visually
            // separated from `boost` at the same brightness.
            hashtag: Color::Rgb(198, 176, 110),
            // Sage green. Pushed further toward green (less yellow) so
            // a reblog marker doesn't look like a hashtag.
            boost: Color::Rgb(150, 182, 122),
            // Saturated amber — the "star" color, distinct from copper.
            favorite: Color::Rgb(240, 183, 108),
            // Warm rust red — clearly negative, still in the warm family.
            error: Color::Rgb(220, 110, 96),

            bg: Color::Reset,
        }
    }

    /// Look up by config name. Unknown names fall back to `frost` silently.
    #[must_use]
    pub fn by_name(name: &str) -> Self {
        match name {
            "ember" => Self::ember(),
            _ => Self::frost(),
        }
    }

    // ---- style helpers ----

    pub fn primary(self) -> Style {
        Style::default().fg(self.fg_primary).bg(self.bg)
    }

    pub fn secondary(self) -> Style {
        Style::default().fg(self.fg_secondary).bg(self.bg)
    }

    pub fn tertiary(self) -> Style {
        Style::default().fg(self.fg_tertiary).bg(self.bg)
    }

    pub fn display_name(self) -> Style {
        self.primary().add_modifier(Modifier::BOLD)
    }

    pub fn handle(self) -> Style {
        self.secondary()
    }

    pub fn timestamp(self) -> Style {
        self.tertiary()
    }

    pub fn link(self) -> Style {
        Style::default().fg(self.accent).bg(self.bg)
    }

    pub fn mention_style(self) -> Style {
        Style::default().fg(self.mention).bg(self.bg)
    }

    pub fn hashtag_style(self) -> Style {
        Style::default().fg(self.hashtag).bg(self.bg)
    }

    pub fn boost_style(self) -> Style {
        Style::default().fg(self.boost).bg(self.bg)
    }

    pub fn favorite_style(self) -> Style {
        Style::default().fg(self.favorite).bg(self.bg)
    }

    pub fn error_style(self) -> Style {
        Style::default().fg(self.error).bg(self.bg)
    }

    pub fn cursor(self) -> Style {
        Style::default().fg(self.accent).bg(self.bg)
    }
}
