//! Theme — a single source of truth for every color and emphasis level in
//! the TUI. Widgets never construct their own `ratatui::style::Style`
//! literals; they go through a [`Theme`] instance.

use ratatui::style::{Color, Modifier, Style};

use crate::config::ThemeConfig;

/// Whether the terminal background is dark or light.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

impl ThemeMode {
    /// Ask the terminal for its background colour (OSC 11), falling back
    /// to the `COLORFGBG` environment variable, then `None`. Must run
    /// before the TUI takes over stdin (it briefly toggles raw mode).
    #[must_use]
    pub fn detect() -> Option<Self> {
        use terminal_colorsaurus::{QueryOptions, ThemeMode as Probe, theme_mode};
        match theme_mode(QueryOptions::default()) {
            Ok(Probe::Light) => Some(Self::Light),
            Ok(Probe::Dark) => Some(Self::Dark),
            Err(e) => {
                tracing::debug!(?e, "OSC 11 background query failed; trying COLORFGBG");
                std::env::var("COLORFGBG")
                    .ok()
                    .and_then(|v| Self::from_colorfgbg(&v))
            }
        }
    }

    /// Parse `COLORFGBG` (`"fg;bg"` or `"fg;default;bg"`, ANSI indices).
    /// Background 7 / 15 are the light greys; 0-6 and 8 are dark.
    #[must_use]
    pub fn from_colorfgbg(v: &str) -> Option<Self> {
        let bg: u8 = v.rsplit(';').next()?.trim().parse().ok()?;
        match bg {
            0..=6 | 8 => Some(Self::Dark),
            7 | 9..=15 => Some(Self::Light),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// Light-terminal counterpart of `frost`: same cool, restrained
    /// hierarchy, flipped for a pale background. Body text keeps ≥7:1
    /// against white, every other foreground ≥4.5:1 (guarded by a test).
    #[must_use]
    pub fn dawn() -> Self {
        Self {
            fg_primary: Color::Rgb(36, 40, 54),
            fg_secondary: Color::Rgb(94, 100, 120),
            fg_tertiary: Color::Rgb(112, 118, 138),

            accent: Color::Rgb(48, 82, 196),
            mention: Color::Rgb(112, 78, 196),
            hashtag: Color::Rgb(18, 120, 100),
            boost: Color::Rgb(38, 128, 52),
            favorite: Color::Rgb(168, 98, 6),
            error: Color::Rgb(196, 48, 48),

            bg: Color::Reset,
        }
    }

    /// Look up a concrete theme by name. Unknown names (and `auto`,
    /// which [`Theme::resolve`] handles) fall back to `frost` silently.
    #[must_use]
    pub fn by_name(name: &str) -> Self {
        match name {
            "ember" => Self::ember(),
            "dawn" => Self::dawn(),
            _ => Self::frost(),
        }
    }

    /// Resolve the `[theme]` config section to a theme. `name = "auto"`
    /// picks `dark` or `light` (defaults `frost` / `dawn`) depending on
    /// `mode`; any other name is looked up directly.
    #[must_use]
    pub fn resolve(cfg: &ThemeConfig, mode: Option<ThemeMode>) -> Self {
        if cfg.name != "auto" {
            return Self::by_name(&cfg.name);
        }
        match mode.unwrap_or(ThemeMode::Dark) {
            ThemeMode::Dark => Self::by_name(cfg.dark.as_deref().unwrap_or("frost")),
            ThemeMode::Light => Self::by_name(cfg.light.as_deref().unwrap_or("dawn")),
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

    /// Inline `<code>` / `<pre>` runs. Secondary tier: visibly set
    /// apart from prose without competing with links or mentions.
    pub fn code(self) -> Style {
        self.secondary()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::many_single_char_names)]
    fn lum(c: Color) -> f64 {
        let Color::Rgb(r, g, b) = c else {
            panic!("rgb only")
        };
        let f = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
    }

    #[test]
    fn dawn_is_readable_on_white() {
        let t = Theme::dawn();
        let ratio = |c: Color| 1.05 / (lum(c) + 0.05);
        assert!(ratio(t.fg_primary) >= 7.0, "body text too light");
        for c in [
            t.fg_secondary,
            t.fg_tertiary,
            t.accent,
            t.mention,
            t.hashtag,
            t.boost,
            t.favorite,
            t.error,
        ] {
            let r = ratio(c);
            assert!(r >= 4.5, "{c:?} only {r:.2}:1 on white");
        }
    }

    #[test]
    fn default_theme_is_auto() {
        assert_eq!(ThemeConfig::default().name, "auto");
        assert!(ThemeConfig::default().is_default());
        assert!(!cfg("frost").is_default());
    }

    #[test]
    fn default_config_does_not_pin_theme() {
        let text = toml::to_string_pretty(&crate::config::Config::default()).unwrap();
        assert!(!text.contains("[theme]"), "{text}");
    }

    #[test]
    fn colorfgbg_parsing() {
        assert_eq!(ThemeMode::from_colorfgbg("15;0"), Some(ThemeMode::Dark));
        assert_eq!(ThemeMode::from_colorfgbg("0;15"), Some(ThemeMode::Light));
        assert_eq!(
            ThemeMode::from_colorfgbg("0;default;7"),
            Some(ThemeMode::Light)
        );
        assert_eq!(ThemeMode::from_colorfgbg("0;default"), None);
        assert_eq!(ThemeMode::from_colorfgbg(""), None);
    }

    fn cfg(name: &str) -> ThemeConfig {
        ThemeConfig {
            name: name.into(),
            ..ThemeConfig::default()
        }
    }

    #[test]
    fn auto_follows_mode_and_overrides() {
        let auto = cfg("auto");
        assert_eq!(Theme::resolve(&auto, Some(ThemeMode::Light)), Theme::dawn());
        assert_eq!(Theme::resolve(&auto, Some(ThemeMode::Dark)), Theme::frost());
        assert_eq!(Theme::resolve(&auto, None), Theme::frost());
        let custom = ThemeConfig {
            dark: Some("ember".into()),
            ..cfg("auto")
        };
        assert_eq!(
            Theme::resolve(&custom, Some(ThemeMode::Dark)),
            Theme::ember()
        );
        assert_eq!(
            Theme::resolve(&cfg("ember"), Some(ThemeMode::Light)),
            Theme::ember()
        );
        assert_eq!(Theme::resolve(&cfg("dawn"), None), Theme::dawn());
    }
}
