//! Compose screen — new post or reply.
//!
//! Modal: when active, the timeline view is replaced entirely (rather
//! than overlaid) — composing is a focused task and the split-screen
//! feel would be noisy.
//!
//! Layout:
//!
//! ```text
//! Compose                                              42 / 500
//!
//! Replying to @author
//! > …first lines of parent status…
//!
//! CW: (Ctrl-S to toggle)                           (hidden if off)
//! …spoiler text…
//!
//! …body…
//! █
//!
//! visibility: public          Ctrl+Enter send · Esc cancel
//!                             Ctrl+W visibility · Ctrl+S CW
//! ```
//!
//! Keys handled:
//! - `Ctrl+Enter`, `Alt+Enter`, `Ctrl+D` → submit
//! - `Esc` → cancel (the app layer decides whether to show a confirm)
//! - `Ctrl+W` → cycle visibility
//! - `Ctrl+S` → toggle CW field (and focus it on toggle-on)
//! - `Tab` / `Shift+Tab` → switch focus body ↔ spoiler
//! - everything else routed to the focused text area

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::api::html;
use crate::api::models::StatusId;
use crate::icons;
use crate::state::Visibility;
use crate::ui::Theme;
use crate::ui::widgets::textarea::TextArea;

/// Default Mastodon character limit. Phase 3.5 will fetch the real
/// number from `/api/v2/instance`.
pub const DEFAULT_MAX_CHARS: usize = 500;

#[derive(Debug, Clone)]
pub struct ReplyContext {
    pub id: StatusId,
    pub author_acct: String,
    /// Plain-text excerpt of the parent body, already HTML-decoded.
    pub excerpt: String,
}

/// Target for a native-quote compose. Shape mirrors [`ReplyContext`]
/// so the render path can share truncation logic.
#[derive(Debug, Clone)]
pub struct QuoteContext {
    pub id: StatusId,
    pub author_acct: String,
    pub excerpt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Body,
    Spoiler,
}

/// State for the compose screen.
pub struct ComposeState {
    body: TextArea,
    spoiler: TextArea,
    cw_enabled: bool,
    visibility: Visibility,
    reply: Option<ReplyContext>,
    /// Populated by [`ComposeState::quote`]. Mutually exclusive with
    /// `reply` in practice — quoting + replying in the same post isn't
    /// a pattern we support yet.
    quote: Option<QuoteContext>,
    focus: Focus,
    max_chars: usize,
}

/// Return value of [`ComposeState::handle_key`].
pub enum ComposeOutcome {
    /// Compose continues; UI should re-render but not transition.
    Continue,
    /// User cancelled. The app layer may prompt if body is non-empty
    /// (this method signals intent; it does not confirm).
    Cancel,
    /// User hit submit. Contains the full draft payload.
    Submit(ComposeDraft),
}

/// Plain-data form of what we'll send to the API. Translated to
/// `Action::Compose` by the app layer.
pub struct ComposeDraft {
    pub text: String,
    pub in_reply_to_id: Option<StatusId>,
    pub quote_id: Option<StatusId>,
    pub content_warning: Option<String>,
    pub sensitive: bool,
    pub visibility: Visibility,
}

impl ComposeState {
    /// Fresh top-level post. `max_chars` should come from
    /// `/api/v2/instance.configuration.statuses.max_characters`; pass
    /// [`DEFAULT_MAX_CHARS`] only if the instance hasn't replied yet.
    #[must_use]
    pub fn blank(max_chars: usize) -> Self {
        Self::with(None, None, None, Visibility::Public, max_chars)
    }

    /// Reply to `reply.id`. Body is pre-filled with `@author ` so the
    /// mention is first-class. Visibility copies the parent's — required
    /// by Mastodon for DM replies to stay DMs.
    #[must_use]
    pub fn reply(reply: ReplyContext, parent_visibility: Visibility, max_chars: usize) -> Self {
        let prefix = format!("@{} ", reply.author_acct);
        Self::with(
            Some(reply),
            None,
            Some(prefix),
            parent_visibility,
            max_chars,
        )
    }

    /// Quote `quote.id` (Mastodon 4.5 native quote). Body starts
    /// empty — quote posts typically don't auto-@-mention the
    /// original author; the quote card is the reference. Visibility
    /// defaults to `Public` (user can cycle with Ctrl+W).
    #[must_use]
    pub fn quote(quote: QuoteContext, max_chars: usize) -> Self {
        Self::with(None, Some(quote), None, Visibility::Public, max_chars)
    }

    fn with(
        reply: Option<ReplyContext>,
        quote: Option<QuoteContext>,
        prefill: Option<String>,
        vis: Visibility,
        max_chars: usize,
    ) -> Self {
        let mut body = prefill.map(|s| TextArea::from_text(&s)).unwrap_or_default();
        // Park the cursor at end-of-prefill so typing starts after it.
        body.move_end();
        Self {
            body,
            spoiler: TextArea::new(),
            cw_enabled: false,
            visibility: vis,
            reply,
            quote,
            focus: Focus::Body,
            max_chars,
        }
    }

    pub fn is_body_empty(&self) -> bool {
        self.body.is_empty()
    }

    fn char_count(&self) -> usize {
        let base = self.body.char_count();
        if self.cw_enabled {
            base + self.spoiler.char_count()
        } else {
            base
        }
    }

    fn over_limit(&self) -> bool {
        self.char_count() > self.max_chars
    }

    fn submit(&self) -> Option<ComposeDraft> {
        let text = self.body.text();
        if text.trim().is_empty() || self.over_limit() {
            return None;
        }
        let spoiler = if self.cw_enabled {
            let s = self.spoiler.text();
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        } else {
            None
        };
        Some(ComposeDraft {
            text,
            in_reply_to_id: self.reply.as_ref().map(|r| r.id.clone()),
            quote_id: self.quote.as_ref().map(|q| q.id.clone()),
            sensitive: spoiler.is_some(),
            content_warning: spoiler,
            visibility: self.visibility,
        })
    }

    fn cycle_visibility(&mut self) {
        self.visibility = match self.visibility {
            Visibility::Public => Visibility::Unlisted,
            Visibility::Unlisted => Visibility::Private,
            Visibility::Private => Visibility::Direct,
            Visibility::Direct => Visibility::Public,
        };
    }

    fn toggle_cw(&mut self) {
        self.cw_enabled = !self.cw_enabled;
        // Focus always falls back to Body on toggle — never trap the
        // user inside the CW field. If they want to enter CW text,
        // Tab moves focus explicitly.
        if !self.cw_enabled || self.focus == Focus::Spoiler {
            self.focus = Focus::Body;
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Body => {
                if self.cw_enabled {
                    Focus::Spoiler
                } else {
                    Focus::Body
                }
            }
            Focus::Spoiler => Focus::Body,
        };
    }

    /// Keyboard driver. See file-level docs for the bindings table.
    pub fn handle_key(&mut self, key: KeyEvent) -> ComposeOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Submit: Ctrl+Enter / Alt+Enter / Ctrl+D.
        if matches!(key.code, KeyCode::Enter) && (ctrl || alt) {
            return match self.submit() {
                Some(draft) => ComposeOutcome::Submit(draft),
                None => ComposeOutcome::Continue,
            };
        }
        if matches!(key.code, KeyCode::Char('d')) && ctrl {
            return match self.submit() {
                Some(draft) => ComposeOutcome::Submit(draft),
                None => ComposeOutcome::Continue,
            };
        }
        // Cancel.
        if matches!(key.code, KeyCode::Esc) {
            return ComposeOutcome::Cancel;
        }
        // Modal toggles.
        if ctrl && matches!(key.code, KeyCode::Char('w')) {
            self.cycle_visibility();
            return ComposeOutcome::Continue;
        }
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            self.toggle_cw();
            return ComposeOutcome::Continue;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.toggle_focus();
            return ComposeOutcome::Continue;
        }

        // CW is semantically single-line — swallow plain Enter so the
        // textarea doesn't scroll previous content off the one-row
        // viewport.
        if self.focus == Focus::Spoiler && matches!(key.code, KeyCode::Enter) {
            return ComposeOutcome::Continue;
        }

        // Route to focused textarea.
        let active = match self.focus {
            Focus::Body => &mut self.body,
            Focus::Spoiler => &mut self.spoiler,
        };
        active.handle_key(key);
        ComposeOutcome::Continue
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, nerd_font: bool) {
        // Vertical stack: title · reply/quote preview · CW field · body · footer.
        let preview_height: u16 = if self.reply.is_some() || self.quote.is_some() {
            3
        } else {
            0
        };
        let cw_height: u16 = if self.cw_enabled { 2 } else { 0 };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(2),              // title
                Constraint::Length(preview_height), // reply / quote preview
                Constraint::Length(cw_height),      // CW
                Constraint::Min(3),                 // body
                Constraint::Length(2),              // footer
            ])
            .split(area);

        self.render_title(frame, chunks[0], theme);
        if preview_height > 0 {
            self.render_reference_preview(frame, chunks[1], theme);
        }
        if cw_height > 0 {
            self.render_cw(frame, chunks[2], theme);
        }
        self.render_body(frame, chunks[3], theme);
        self.render_footer(frame, chunks[4], theme, nerd_font);
    }

    fn render_title(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let label = if self.reply.is_some() {
            "Reply"
        } else if self.quote.is_some() {
            "Quote"
        } else {
            "Compose"
        };
        let count_style = if self.over_limit() {
            theme.error_style()
        } else if self.char_count() * 10 > self.max_chars * 9 {
            theme.favorite_style()
        } else {
            theme.tertiary()
        };
        let count_text = format!("{} / {}", self.char_count(), self.max_chars);
        // Build a line: title on left, counter on right. Padding with
        // spaces is cheaper than a 2-column layout for a single line.
        let pad = area
            .width
            .saturating_sub(label.len() as u16 + count_text.len() as u16);
        let line = Line::from(vec![
            Span::styled(label.to_string(), theme.display_name()),
            Span::raw(" ".repeat(pad as usize)),
            Span::styled(count_text, count_style),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    /// Unified reply / quote preview — identical shape, different
    /// header verb. Reply wins if both are somehow set.
    fn render_reference_preview(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let (header, excerpt) = if let Some(r) = &self.reply {
            (
                format!("Replying to @{}", r.author_acct),
                r.excerpt.as_str(),
            )
        } else if let Some(q) = &self.quote {
            (format!("Quoting @{}", q.author_acct), q.excerpt.as_str())
        } else {
            return;
        };
        let excerpt = truncate_chars(excerpt, area.width.saturating_sub(2) as usize, "…");
        let glyph = if self.quote.is_some() { "❝ " } else { "> " };
        let lines = vec![
            Line::from(Span::styled(header, theme.secondary())),
            Line::from(vec![
                Span::styled(glyph.to_string(), theme.tertiary()),
                Span::styled(excerpt, theme.tertiary().add_modifier(Modifier::ITALIC)),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_cw(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        // Label row + one editable row.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);
        let focused = self.focus == Focus::Spoiler;
        let (label, label_style) = if focused {
            ("● CW", theme.display_name())
        } else {
            ("  CW  (Tab to focus)", theme.tertiary())
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(label, label_style))),
            chunks[0],
        );
        let style = if focused {
            theme.primary()
        } else {
            theme.secondary()
        };
        self.spoiler.render(frame, chunks[1], style, focused);
    }

    fn render_body(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let focused = self.focus == Focus::Body;
        let style = if focused {
            theme.primary()
        } else {
            theme.secondary()
        };
        self.body.render(frame, area, style, focused);
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, nerd_font: bool) {
        let vis_icon = if matches!(self.visibility, Visibility::Private | Visibility::Direct) {
            icons::pick(nerd_font, icons::LOCK, icons::LOCK_ASCII)
        } else {
            ""
        };
        let vis_label = match self.visibility {
            Visibility::Public => "public",
            Visibility::Unlisted => "unlisted",
            Visibility::Private => "followers-only",
            Visibility::Direct => "direct message",
        };
        let vis_prefix = if vis_icon.is_empty() {
            String::new()
        } else {
            format!("{vis_icon} ")
        };
        let line1 = Line::from(vec![
            Span::styled("visibility: ", theme.tertiary()),
            Span::styled(
                format!("{vis_prefix}{vis_label}"),
                theme.secondary().add_modifier(Modifier::BOLD),
            ),
        ]);
        let line2 = Line::from(Span::styled(
            "Ctrl+Enter send · Esc cancel · Ctrl+W visibility · Ctrl+S CW · Tab focus",
            theme.tertiary(),
        ));
        frame.render_widget(Paragraph::new(vec![line1, line2]), area);
    }
}

/// Build a ReplyContext from a Status, capturing the first N chars of
/// plain-text content as the excerpt.
#[must_use]
pub fn reply_context_from(
    status: &crate::api::models::Status,
    excerpt_chars: usize,
) -> ReplyContext {
    let plain = html::to_plain_text(&status.content);
    let excerpt = truncate_chars(&plain, excerpt_chars, "…");
    ReplyContext {
        id: status.id.clone(),
        author_acct: status.account.acct.clone(),
        excerpt,
    }
}

/// Build a QuoteContext from a Status — same shape as ReplyContext but
/// kept as a separate type so mis-wirings at call sites are caught by
/// the type checker.
#[must_use]
pub fn quote_context_from(
    status: &crate::api::models::Status,
    excerpt_chars: usize,
) -> QuoteContext {
    let plain = html::to_plain_text(&status.content);
    let excerpt = truncate_chars(&plain, excerpt_chars, "…");
    QuoteContext {
        id: status.id.clone(),
        author_acct: status.account.acct.clone(),
        excerpt,
    }
}

fn truncate_chars(s: &str, max: usize, ellipsis: &str) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s
        .chars()
        .take(max.saturating_sub(ellipsis.chars().count()))
        .collect();
    out.push_str(ellipsis);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_starts_empty() {
        let c = ComposeState::blank(DEFAULT_MAX_CHARS);
        assert!(c.is_body_empty());
        assert_eq!(c.char_count(), 0);
    }

    #[test]
    fn reply_prefills_mention() {
        let r = ReplyContext {
            id: StatusId::new("1"),
            author_acct: "alice@ex.com".into(),
            excerpt: "hi".into(),
        };
        let c = ComposeState::reply(r, Visibility::Public, DEFAULT_MAX_CHARS);
        assert_eq!(c.body.text(), "@alice@ex.com ");
    }

    #[test]
    fn ctrl_w_cycles_visibility() {
        let mut c = ComposeState::blank(DEFAULT_MAX_CHARS);
        let start = c.visibility;
        c.handle_key(KeyEvent {
            code: KeyCode::Char('w'),
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert_ne!(c.visibility, start);
    }

    #[test]
    fn ctrl_enter_submits_when_non_empty() {
        let mut c = ComposeState::blank(DEFAULT_MAX_CHARS);
        c.body = TextArea::from_text("hello");
        let out = c.handle_key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        match out {
            ComposeOutcome::Submit(d) => assert_eq!(d.text, "hello"),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn ctrl_enter_noops_when_empty() {
        let mut c = ComposeState::blank(DEFAULT_MAX_CHARS);
        let out = c.handle_key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(matches!(out, ComposeOutcome::Continue));
    }

    #[test]
    fn quote_starts_empty_and_carries_quote_id() {
        let q = QuoteContext {
            id: StatusId::new("42"),
            author_acct: "bob@ex.com".into(),
            excerpt: "original body".into(),
        };
        let mut c = ComposeState::quote(q, DEFAULT_MAX_CHARS);
        assert!(
            c.is_body_empty(),
            "quote compose should start with an empty body"
        );
        c.body = TextArea::from_text("my take");
        let out = c.handle_key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        match out {
            ComposeOutcome::Submit(d) => {
                assert_eq!(d.text, "my take");
                assert_eq!(d.quote_id, Some(StatusId::new("42")));
                assert!(d.in_reply_to_id.is_none());
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut c = ComposeState::blank(DEFAULT_MAX_CHARS);
        let out = c.handle_key(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(matches!(out, ComposeOutcome::Cancel));
    }

    #[test]
    fn over_limit_blocks_submit() {
        let mut c = ComposeState::blank(DEFAULT_MAX_CHARS);
        c.body = TextArea::from_text(&"a".repeat(DEFAULT_MAX_CHARS + 1));
        let out = c.handle_key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(matches!(out, ComposeOutcome::Continue));
    }
}
