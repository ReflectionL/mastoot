//! Profile screen — header (display name + handle + bio + counts)
//! followed by the user's recent statuses.
//!
//! Same screen renders both:
//! - tab 5 self-profile (`is_self == true`), and
//! - modal other-user profile invoked via `u` (`is_self == false`).
//!
//! Action keys (`f` / `b` / `B` / `r`) inside this screen target the
//! cursor's selected status; `l` / `Enter` opens the status detail.
//! `h` / `Esc` returns to the previous mode (App-level concern).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::api::html;
use crate::api::models::{Account, AccountId, Relationship, Status};
use crate::api::music::MusicCache;
use crate::state::Action;
use crate::ui::Theme;
use crate::ui::images::{self, ImageCache};
use crate::ui::widgets::status_card::{self, CardOpts, ImageOverlay};

const LOAD_MORE_TRIGGER: usize = 5;

/// Result of a key in the profile screen.
pub enum ProfileOutcome {
    Continue,
    /// Pop back to whatever the user was looking at before.
    Back,
    /// Forward an [`Action`] to the state task.
    Dispatch(Action),
    /// Open the cursor's selected status in detail mode.
    OpenStatus(Status),
}

pub struct ProfileScreen {
    pub account_id: AccountId,
    pub account: Option<Account>,
    pub statuses: Vec<Status>,
    pub selected: usize,
    pub scroll: u16,
    pub is_self: bool,
    last_g: bool,
    pub load_more_pending: bool,
    pub loading: bool,
    /// Viewer ↔ this-account relationship. `None` until
    /// [`Action::LoadRelationship`] returns; `Some` once known.
    /// Always `None` for self-profile (we never fetch our own).
    pub relationship: Option<Relationship>,
}

impl ProfileScreen {
    /// Build a screen seeded with whatever Account we already have on
    /// hand (status author, AppState.me, …) so the header can render
    /// instantly while statuses are still in flight.
    #[must_use]
    pub fn new(account: Account, is_self: bool) -> Self {
        Self {
            account_id: account.id.clone(),
            account: Some(account),
            statuses: Vec::new(),
            selected: 0,
            scroll: 0,
            is_self,
            last_g: false,
            load_more_pending: false,
            loading: true,
            relationship: None,
        }
    }

    /// Apply a freshly fetched [`Relationship`] (from `LoadRelationship`
    /// or as the side-effect of a successful follow / unfollow). Only
    /// installs when the id matches; stale replies are ignored.
    pub fn on_relationship_loaded(&mut self, rel: Relationship) {
        if rel.id == self.account_id {
            self.relationship = Some(rel);
        }
    }

    /// Optimistically toggle follow state. Returns the action to
    /// dispatch; `None` for self-profile (you can't follow yourself)
    /// or before the relationship has loaded.
    pub fn toggle_follow_optimistic(&mut self) -> Option<Action> {
        if self.is_self {
            return None;
        }
        let rel = self.relationship.as_mut()?;
        let was_following = rel.following;
        rel.following = !was_following;
        // Mastodon transitions: requesting flips on for follows of
        // locked accounts. We don't know if locked here, so just keep
        // requested as-is and let the server reply correct it.
        Some(if was_following {
            Action::Unfollow(self.account_id.clone())
        } else {
            Action::Follow(self.account_id.clone())
        })
    }

    /// Reverse an optimistic follow flip when the server rejects it.
    pub fn revert_follow_action(&mut self, attempted_follow: bool) {
        if let Some(rel) = self.relationship.as_mut() {
            // Restore to the *opposite* of what we attempted.
            rel.following = !attempted_follow;
        }
    }

    /// Apply a fresh `ProfileLoaded` payload.
    pub fn on_loaded(&mut self, account: Account, statuses: Vec<Status>, appended: bool) {
        // Pagination calls don't carry a real account — only the id.
        // Don't overwrite the cached header with a stub.
        if appended {
            let known: std::collections::HashSet<_> =
                self.statuses.iter().map(|s| s.id.clone()).collect();
            for s in statuses {
                if !known.contains(&s.id) {
                    self.statuses.push(s);
                }
            }
        } else {
            self.account = Some(account);
            self.statuses = statuses;
            self.selected = 0;
            self.scroll = 0;
        }
        self.loading = false;
        self.load_more_pending = false;
    }

    /// Patch a status in place (after a successful favourite / boost
    /// reply round-trip).
    pub fn on_status_updated(&mut self, status: &Status) {
        for slot in &mut self.statuses {
            if slot.id == status.id {
                *slot = status.clone();
            } else if let Some(inner) = slot.reblog.as_deref_mut()
                && inner.id == status.id
            {
                *inner = status.clone();
            }
        }
    }

    /// Reverse an optimistic action when the server rejects it.
    pub fn revert_action(
        &mut self,
        id: &crate::api::models::StatusId,
        action: crate::state::event::FailedAction,
    ) {
        for slot in &mut self.statuses {
            if slot.id == *id {
                crate::ui::app::apply_revert(slot, action);
            } else if let Some(inner) = slot.reblog.as_deref_mut()
                && inner.id == *id
            {
                crate::ui::app::apply_revert(inner, action);
            }
        }
    }

    /// The status the cursor points at. Honors boost convention —
    /// returns the inner reblog when the outer is a boost.
    pub fn selected_target(&self) -> Option<&Status> {
        let outer = self.statuses.get(self.selected)?;
        Some(outer.reblog.as_deref().unwrap_or(outer))
    }

    pub fn selected_target_mut(&mut self) -> Option<&mut Status> {
        let outer = self.statuses.get_mut(self.selected)?;
        if outer.reblog.is_some() {
            outer.reblog.as_deref_mut()
        } else {
            Some(outer)
        }
    }

    pub fn toggle_favourite_optimistic(&mut self) -> Option<Action> {
        let target = self.selected_target_mut()?;
        let now = !target.favourited.unwrap_or(false);
        target.favourited = Some(now);
        target.favourites_count = if now {
            target.favourites_count.saturating_add(1)
        } else {
            target.favourites_count.saturating_sub(1)
        };
        let id = target.id.clone();
        Some(if now {
            Action::Favourite(id)
        } else {
            Action::Unfavourite(id)
        })
    }

    pub fn toggle_reblog_optimistic(&mut self) -> Option<Action> {
        let target = self.selected_target_mut()?;
        let now = !target.reblogged.unwrap_or(false);
        target.reblogged = Some(now);
        target.reblogs_count = if now {
            target.reblogs_count.saturating_add(1)
        } else {
            target.reblogs_count.saturating_sub(1)
        };
        let id = target.id.clone();
        Some(if now {
            Action::Reblog(id)
        } else {
            Action::Unreblog(id)
        })
    }

    pub fn force_unreblog_optimistic(&mut self) -> Option<Action> {
        let target = self.selected_target_mut()?;
        if !target.reblogged.unwrap_or(false) {
            return None;
        }
        target.reblogged = Some(false);
        target.reblogs_count = target.reblogs_count.saturating_sub(1);
        let id = target.id.clone();
        Some(Action::Unreblog(id))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ProfileOutcome {
        let len = self.statuses.len();
        let reset_g = !matches!(key.code, KeyCode::Char('g'));
        let outcome = match key.code {
            KeyCode::Char('h') | KeyCode::Esc | KeyCode::Backspace => ProfileOutcome::Back,
            KeyCode::Char('R') => ProfileOutcome::Dispatch(Action::LoadProfile {
                id: self.account_id.clone(),
                max_id: None,
            }),
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ProfileOutcome::Dispatch(Action::LoadProfile {
                    id: self.account_id.clone(),
                    max_id: None,
                })
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < len {
                    self.selected += 1;
                }
                self.check_load_more(len)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                ProfileOutcome::Continue
            }
            KeyCode::Char('g') => {
                if self.last_g {
                    self.selected = 0;
                    self.scroll = 0;
                }
                self.last_g = !self.last_g;
                ProfileOutcome::Continue
            }
            KeyCode::Char('G') => {
                if len > 0 {
                    self.selected = len - 1;
                }
                self.check_load_more(len)
            }
            KeyCode::Char('l') | KeyCode::Enter => match self.selected_target() {
                Some(s) => ProfileOutcome::OpenStatus(s.clone()),
                None => ProfileOutcome::Continue,
            },
            KeyCode::Char('f') => match self.toggle_favourite_optimistic() {
                Some(a) => ProfileOutcome::Dispatch(a),
                None => ProfileOutcome::Continue,
            },
            KeyCode::Char('b') => match self.toggle_reblog_optimistic() {
                Some(a) => ProfileOutcome::Dispatch(a),
                None => ProfileOutcome::Continue,
            },
            KeyCode::Char('B') => match self.force_unreblog_optimistic() {
                Some(a) => ProfileOutcome::Dispatch(a),
                None => ProfileOutcome::Continue,
            },
            KeyCode::Char('F') => match self.toggle_follow_optimistic() {
                Some(a) => ProfileOutcome::Dispatch(a),
                None => ProfileOutcome::Continue,
            },
            _ => ProfileOutcome::Continue,
        };
        if reset_g {
            self.last_g = false;
        }
        outcome
    }

    fn check_load_more(&mut self, len: usize) -> ProfileOutcome {
        if !self.load_more_pending && len > 0 && self.selected + LOAD_MORE_TRIGGER >= len {
            self.load_more_pending = true;
            let max_id = self.statuses.last().map(|s| s.id.0.clone());
            ProfileOutcome::Dispatch(Action::LoadProfile {
                id: self.account_id.clone(),
                max_id,
            })
        } else {
            ProfileOutcome::Continue
        }
    }

    /// Header lines: display name (bold) + handle (secondary), counts
    /// row, and the bio (HTML-rendered, dim).
    fn header_lines(&self, theme: &Theme, inner_width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let Some(acc) = self.account.as_ref() else {
            out.push(Line::from(Span::styled(
                "loading profile…".to_string(),
                theme.tertiary(),
            )));
            return out;
        };

        let display = if acc.display_name.is_empty() {
            acc.username.clone()
        } else {
            acc.display_name.clone()
        };
        let handle = format!("@{}", acc.acct);
        let mut header = vec![
            Span::styled(display, theme.display_name()),
            Span::raw("  "),
            Span::styled(handle, theme.handle()),
        ];
        // Relationship chip on other-user profiles. Combinations:
        //   following && followed_by → "mutual"
        //   following                → "following"
        //   followed_by              → "follows you"
        //   requested                → "requested"
        //   else                     → no chip
        if !self.is_self
            && let Some(rel) = &self.relationship
        {
            let label = if rel.following && rel.followed_by {
                Some("mutual")
            } else if rel.following {
                Some("following")
            } else if rel.requested {
                Some("requested")
            } else if rel.followed_by {
                Some("follows you")
            } else {
                None
            };
            if let Some(label) = label {
                header.push(Span::styled("  ·  ", theme.tertiary()));
                header.push(Span::styled(label.to_string(), theme.secondary()));
            }
        }
        out.push(Line::from(header));

        // Counts: posts · followers · following.
        let counts = format!(
            "{} posts   ·   {} followers   ·   {} following",
            acc.statuses_count, acc.followers_count, acc.following_count,
        );
        out.push(Line::from(Span::styled(counts, theme.tertiary())));

        // Bio. HTML-rendered, dimmed, wrapped to inner_width.
        if !acc.note.is_empty() {
            out.push(Line::default());
            let bio = html::render(&acc.note, theme);
            let mut wrapped = crate::ui::widgets::wrap::wrap_lines(&bio, inner_width);
            for line in &mut wrapped {
                for span in &mut line.spans {
                    if span.style.fg.is_none() {
                        span.style = span.style.patch(theme.secondary());
                    }
                }
            }
            out.extend(wrapped);
        }

        out
    }

    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        nerd_font: bool,
        music: &mut MusicCache,
        images_cache: &mut ImageCache,
    ) {
        const H_PAD: u16 = 1;
        let inner_width = area.width.saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.extend(self.header_lines(theme, inner_width));
        // Visual gap between header and post list.
        lines.push(Line::default());
        lines.push(Line::default());

        if self.statuses.is_empty() {
            let msg = if self.loading {
                "loading posts…"
            } else {
                "no posts"
            };
            lines.push(Line::from(Span::styled(msg, theme.tertiary())));
        }

        let mut sel_range: (u16, u16) = (0, 0);
        let mut image_overlays: Vec<(u16, ImageOverlay)> = Vec::new();
        for (i, status) in self.statuses.iter().enumerate() {
            if i > 0 {
                for _ in 0..status_card::inter_post_blank_lines() {
                    lines.push(Line::default());
                }
            }
            let opts = CardOpts {
                selected: i == self.selected,
                nerd_font,
                show_metrics: false,
                cw_revealed: false,
                show_images: images_cache.enabled(),
            };
            let block =
                status_card::render_blocks(status, theme, opts, inner_width, Some(&mut *music));
            let start = lines.len() as u16;
            let len = block.lines.len() as u16;
            for ov in block.image_overlays {
                image_overlays.push((start + ov.line_offset, ov));
            }
            lines.extend(block.lines);
            if i == self.selected {
                sel_range = (start, start + len);
            }
        }

        let height = area.height;
        let (sel_start, sel_end) = sel_range;
        if !self.statuses.is_empty() {
            if sel_start < self.scroll {
                self.scroll = sel_start;
            } else if sel_end > self.scroll + height {
                self.scroll = sel_end.saturating_sub(height);
            }
        }

        let p = Paragraph::new(lines)
            .style(Style::default().fg(theme.fg_primary).bg(theme.bg))
            .scroll((self.scroll, 0))
            .block(Block::new().padding(Padding::new(H_PAD, H_PAD, 1, 0)));
        frame.render_widget(p, area);

        // Image overlays (spacious-mode Apple Music cover art).
        for (abs, ov) in &image_overlays {
            images::draw_overlay(frame, area, H_PAD, *abs, self.scroll, ov, images_cache);
        }
    }

    /// Header row shown at the top of the profile page when invoked
    /// modally — mirrors the detail page's `← thread` strip.
    pub fn render_modal_header(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let line = Line::from(vec![
            Span::styled("← ", theme.tertiary()),
            Span::styled("profile", theme.secondary()),
            Span::styled("   ·   ", theme.tertiary()),
            Span::styled("h / Esc to go back", theme.tertiary()),
        ]);
        frame.render_widget(
            Paragraph::new(line).style(theme.primary().add_modifier(Modifier::empty())),
            area,
        );
    }
}
