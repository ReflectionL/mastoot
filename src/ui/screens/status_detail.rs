//! Status detail screen — focal post in the middle of its reply tree.
//!
//! Layout (top to bottom): ancestors (oldest first) → focal → descendants
//! (flat in v1; tree-aware indentation is a Phase 4 polish). The focal
//! post is delineated only by whitespace and the cursor convention used
//! everywhere else; no boxes, no rules.
//!
//! Selection traverses the entire combined list so `f` / `b` / `r` can
//! act on any post in the thread, not just the focal.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::api::models::{Status, StatusId};
use crate::api::music::MusicCache;
use crate::state::Action;
use crate::state::event::FailedAction;
use crate::ui::Theme;
use crate::ui::images::{self, ImageCache};
use crate::ui::widgets::status_card::{self, CardOpts, ImageOverlay};

/// Result of a key press inside the detail screen.
pub enum DetailOutcome {
    /// Key was consumed; stay here.
    Continue,
    /// Pop back to the timeline.
    Back,
    /// Forward an [`Action`] to the state task (used for f / b / B / R).
    Dispatch(Action),
}

/// State for one open detail page. Owns its own copy of the focal /
/// ancestors / descendants so we can apply optimistic updates locally
/// without racing the timeline cache.
pub struct DetailState {
    focal_id: StatusId,
    /// The post the user picked. Always present.
    focal: Status,
    /// Older posts up the reply chain, oldest first.
    ancestors: Vec<Status>,
    /// Replies (flat for now), ordered as the server returned them.
    descendants: Vec<Status>,
    /// Cursor index over `[ancestors, focal, descendants]`.
    pub selected: usize,
    pub scroll: u16,
    last_g: bool,
    /// True between mode entry and the first `StatusContext` event.
    pub loading: bool,
    /// Status ids whose CW the user has revealed in this detail view.
    pub revealed: HashSet<StatusId>,
}

impl DetailState {
    /// Build a detail page for `focal`, with no thread loaded yet.
    /// The cursor starts on the focal itself so reading flows naturally
    /// from "what I tapped" outward.
    #[must_use]
    pub fn new(focal: Status) -> Self {
        Self {
            focal_id: focal.id.clone(),
            focal,
            ancestors: Vec::new(),
            descendants: Vec::new(),
            selected: 0,
            scroll: 0,
            last_g: false,
            loading: true,
            revealed: HashSet::new(),
        }
    }

    #[must_use]
    pub fn focal_id(&self) -> &StatusId {
        &self.focal_id
    }

    /// Apply a fresh `/context` payload. If the cursor was on the focal
    /// (the common case while loading), keep it on the focal once we
    /// know how many ancestors there are.
    pub fn on_context_loaded(&mut self, ancestors: Vec<Status>, descendants: Vec<Status>) {
        self.ancestors = ancestors;
        self.descendants = descendants;
        self.loading = false;
        self.selected = self.ancestors.len(); // focal
        self.scroll = 0;
    }

    /// Patch a status that just changed server-side into whichever slot
    /// holds it (focal, ancestor, descendant, or the inner reblog of any
    /// of those).
    pub fn on_status_updated(&mut self, status: &Status) {
        merge(&mut self.focal, status);
        for s in &mut self.ancestors {
            merge(s, status);
        }
        for s in &mut self.descendants {
            merge(s, status);
        }
    }

    /// Reverse an optimistic flip on whichever slot carries `id`.
    pub fn revert_action(&mut self, id: &StatusId, action: FailedAction) {
        let undo = |s: &mut Status| {
            if s.id == *id {
                crate::ui::app::apply_revert(s, action);
            } else if let Some(inner) = s.reblog.as_deref_mut()
                && inner.id == *id
            {
                crate::ui::app::apply_revert(inner, action);
            }
        };
        undo(&mut self.focal);
        for s in &mut self.ancestors {
            undo(s);
        }
        for s in &mut self.descendants {
            undo(s);
        }
    }

    fn total(&self) -> usize {
        self.ancestors.len() + 1 + self.descendants.len()
    }

    /// Borrow the outer status the cursor points at.
    fn selected_outer(&self) -> Option<&Status> {
        let n = self.ancestors.len();
        match self.selected.cmp(&n) {
            std::cmp::Ordering::Less => self.ancestors.get(self.selected),
            std::cmp::Ordering::Equal => Some(&self.focal),
            std::cmp::Ordering::Greater => self.descendants.get(self.selected - n - 1),
        }
    }

    fn selected_outer_mut(&mut self) -> Option<&mut Status> {
        let n = self.ancestors.len();
        match self.selected.cmp(&n) {
            std::cmp::Ordering::Less => self.ancestors.get_mut(self.selected),
            std::cmp::Ordering::Equal => Some(&mut self.focal),
            std::cmp::Ordering::Greater => self.descendants.get_mut(self.selected - n - 1),
        }
    }

    /// The post the action keys should target — for a boost, the inner
    /// status (Mastodon convention).
    pub fn selected_target(&self) -> Option<&Status> {
        let outer = self.selected_outer()?;
        Some(outer.reblog.as_deref().unwrap_or(outer))
    }

    pub fn selected_target_mut(&mut self) -> Option<&mut Status> {
        let outer = self.selected_outer_mut()?;
        if outer.reblog.is_some() {
            outer.reblog.as_deref_mut()
        } else {
            Some(outer)
        }
    }

    /// Optimistically flip favourite on the cursor's target and return
    /// the action to dispatch; `None` when nothing is selectable.
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

    /// Translate a key. Reply (`r`) and back (`h` / `Esc`) are returned
    /// up to App so it can build a ComposeState or pop the mode.
    pub fn handle_key(&mut self, key: KeyEvent) -> DetailOutcome {
        // gg sequence handling.
        let reset_g = !matches!(key.code, KeyCode::Char('g'));
        let outcome = match key.code {
            KeyCode::Char('h') | KeyCode::Esc | KeyCode::Backspace => DetailOutcome::Back,
            KeyCode::Char('R') => {
                DetailOutcome::Dispatch(Action::OpenStatus(self.focal_id.clone()))
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                DetailOutcome::Dispatch(Action::OpenStatus(self.focal_id.clone()))
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let n = self.total();
                if n > 0 && self.selected + 1 < n {
                    self.selected += 1;
                }
                DetailOutcome::Continue
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                DetailOutcome::Continue
            }
            KeyCode::Char('g') => {
                if self.last_g {
                    self.selected = 0;
                    self.scroll = 0;
                }
                self.last_g = !self.last_g;
                DetailOutcome::Continue
            }
            KeyCode::Char('G') => {
                let n = self.total();
                if n > 0 {
                    self.selected = n - 1;
                }
                DetailOutcome::Continue
            }
            KeyCode::Char('f') => match self.toggle_favourite_optimistic() {
                Some(a) => DetailOutcome::Dispatch(a),
                None => DetailOutcome::Continue,
            },
            KeyCode::Char('b') => match self.toggle_reblog_optimistic() {
                Some(a) => DetailOutcome::Dispatch(a),
                None => DetailOutcome::Continue,
            },
            KeyCode::Char('B') => match self.force_unreblog_optimistic() {
                Some(a) => DetailOutcome::Dispatch(a),
                None => DetailOutcome::Continue,
            },
            KeyCode::Char('s') => {
                if let Some(t) = self.selected_target() {
                    let id = t.id.clone();
                    if !self.revealed.remove(&id) {
                        self.revealed.insert(id);
                    }
                }
                DetailOutcome::Continue
            }
            _ => DetailOutcome::Continue,
        };
        if reset_g {
            self.last_g = false;
        }
        outcome
    }

    /// Render the thread. Identical card visuals to the timeline; the
    /// focal post does not get any special chrome, only its position
    /// and (initially) the cursor mark it out. Inline images on the
    /// focal post are overlaid on top of the text Paragraph after it
    /// renders — placeholder rows in the card body reserve the space.
    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        nerd_font: bool,
        images: &mut ImageCache,
        music: &mut MusicCache,
    ) {
        if self.loading && self.ancestors.is_empty() && self.descendants.is_empty() {
            // Show the focal alone while the context is in flight.
            // Same renderer, with a dim "loading thread…" hint above.
            const H_PAD: u16 = 1;
            let inner_width = area.width.saturating_sub(H_PAD * 2);
            let mut lines: Vec<Line<'static>> = Vec::new();
            lines.push(Line::from(ratatui::text::Span::styled(
                "  loading thread…",
                theme.tertiary(),
            )));
            for _ in 0..status_card::inter_post_blank_lines() {
                lines.push(Line::default());
            }
            let focal_inner_id = &self.focal.reblog.as_deref().unwrap_or(&self.focal).id;
            let opts = CardOpts {
                selected: true,
                nerd_font,
                show_metrics: true,
                cw_revealed: self.revealed.contains(focal_inner_id),
                show_images: images.enabled(),
            };
            let block =
                status_card::render_blocks(&self.focal, theme, opts, inner_width, Some(music));
            let preface = lines.len() as u16;
            lines.extend(block.lines);
            let p = Paragraph::new(lines)
                .style(Style::default().fg(theme.fg_primary).bg(theme.bg))
                .block(Block::new().padding(Padding::new(H_PAD, H_PAD, 1, 0)));
            frame.render_widget(p, area);
            // Image overlays on top — no scroll in loading mode (scroll = 0).
            for ov in &block.image_overlays {
                images::draw_overlay(frame, area, H_PAD, preface + ov.line_offset, 0, ov, images);
            }
            return;
        }

        const H_PAD: u16 = 1;
        let inner_width = area.width.saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut sel_range: (u16, u16) = (0, 0);
        let mut overlays: Vec<(u16, ImageOverlay)> = Vec::new();
        let n_anc = self.ancestors.len();
        let total = n_anc + 1 + self.descendants.len();
        let images_enabled = images.enabled();
        for idx in 0..total {
            let status: &Status = match idx.cmp(&n_anc) {
                std::cmp::Ordering::Less => &self.ancestors[idx],
                std::cmp::Ordering::Equal => &self.focal,
                std::cmp::Ordering::Greater => &self.descendants[idx - n_anc - 1],
            };
            if !lines.is_empty() {
                for _ in 0..status_card::inter_post_blank_lines() {
                    lines.push(Line::default());
                }
            }
            let inner_id = &status.reblog.as_deref().unwrap_or(status).id;
            let is_focal = idx == n_anc;
            let opts = CardOpts {
                selected: idx == self.selected,
                nerd_font,
                show_metrics: is_focal,
                cw_revealed: self.revealed.contains(inner_id),
                show_images: images_enabled,
            };
            // Every status in the thread gets the music cache so
            // Apple Music links render compactly (`󰝚 Artist · Title`)
            // regardless of whether they're the focal post. Artwork
            // / spacious cards still only render when `show_images`
            // is true (focal-only for now), so non-focal cards never
            // trigger artwork downloads.
            let block =
                status_card::render_blocks(status, theme, opts, inner_width, Some(&mut *music));
            let card_start = lines.len() as u16;
            let card_len = block.lines.len() as u16;
            for ov in block.image_overlays {
                overlays.push((card_start + ov.line_offset, ov));
            }
            lines.extend(block.lines);
            if idx == self.selected {
                sel_range = (card_start, card_start + card_len);
            }
        }

        // Keep the selected card inside `area` by adjusting scroll.
        let height = area.height;
        let (sel_start, sel_end) = sel_range;
        if sel_start < self.scroll {
            self.scroll = sel_start;
        } else if sel_end > self.scroll + height {
            self.scroll = sel_end.saturating_sub(height);
        }

        let p = Paragraph::new(lines)
            .style(Style::default().fg(theme.fg_primary).bg(theme.bg))
            .scroll((self.scroll, 0))
            .block(Block::new().padding(Padding::new(H_PAD, H_PAD, 1, 0)));
        frame.render_widget(p, area);

        // Image overlays — drawn after the Paragraph so they cover the
        // placeholder rows. Skipped when partially scrolled off the
        // top or bottom; user can scroll into view to see them.
        for (abs_offset, ov) in &overlays {
            images::draw_overlay(frame, area, H_PAD, *abs_offset, self.scroll, ov, images);
        }
    }
}

/// Replace `slot` with `incoming` if their ids match; if `slot` is a
/// reblog whose inner post matches, replace the inner post instead.
fn merge(slot: &mut Status, incoming: &Status) {
    if slot.id == incoming.id {
        *slot = incoming.clone();
    } else if let Some(inner) = slot.reblog.as_deref_mut()
        && inner.id == incoming.id
    {
        *inner = incoming.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{Account, Visibility};
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn fake(id: &str) -> Status {
        Status {
            id: StatusId::new(id),
            account: Account {
                acct: "alice@ex.com".into(),
                display_name: "Alice".into(),
                ..Default::default()
            },
            content: format!("<p>body of {id}</p>"),
            visibility: Visibility::Public,
            ..Default::default()
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn cursor_lands_on_focal_after_load() {
        let mut d = DetailState::new(fake("focal"));
        d.on_context_loaded(vec![fake("a1"), fake("a2")], vec![fake("d1")]);
        assert_eq!(d.selected, 2); // 0,1 = ancestors; 2 = focal
        assert_eq!(d.selected_target().unwrap().id, StatusId::new("focal"));
    }

    #[test]
    fn jk_navigate_across_thread() {
        let mut d = DetailState::new(fake("focal"));
        d.on_context_loaded(vec![fake("a1")], vec![fake("d1"), fake("d2")]);
        // selected starts at focal (idx 1)
        d.handle_key(key('j'));
        assert_eq!(d.selected, 2);
        d.handle_key(key('j'));
        assert_eq!(d.selected, 3);
        d.handle_key(key('j')); // clamp
        assert_eq!(d.selected, 3);
        d.handle_key(key('k'));
        d.handle_key(key('k'));
        d.handle_key(key('k'));
        assert_eq!(d.selected, 0);
    }

    #[test]
    fn favourite_targets_inner_of_a_boost() {
        let mut outer = fake("outer");
        let inner = fake("inner");
        outer.reblog = Some(Box::new(inner));
        let mut d = DetailState::new(outer);
        d.on_context_loaded(vec![], vec![]);
        let action = d.toggle_favourite_optimistic().unwrap();
        match action {
            Action::Favourite(id) => assert_eq!(id, StatusId::new("inner")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn revert_undoes_optimistic_favourite() {
        let mut s = fake("focal");
        s.favourited = Some(true);
        s.favourites_count = 1;
        let mut d = DetailState::new(s);
        d.revert_action(&StatusId::new("focal"), FailedAction::Favourite);
        let f = d.selected_target().unwrap();
        assert_eq!(f.favourited, Some(false));
        assert_eq!(f.favourites_count, 0);
    }

    #[test]
    fn h_and_esc_return_back() {
        let mut d = DetailState::new(fake("focal"));
        match d.handle_key(key('h')) {
            DetailOutcome::Back => {}
            _ => panic!("expected Back"),
        }
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        match d.handle_key(esc) {
            DetailOutcome::Back => {}
            _ => panic!("expected Back on Esc"),
        }
    }
}
