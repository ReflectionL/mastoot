//! Search results (`/`). One page, three sections in Phanpy's order:
//! accounts, hashtags, posts. Sections that came back empty are
//! simply absent — no "no accounts" placeholders.
//!
//! The cursor walks a flat list across all three sections. `l` /
//! `Enter` opens whatever is under it: a profile, a hashtag timeline
//! (re-entering this screen with `#tag` as the query), or a thread.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::api::models::{Account, SearchResults, Status, StatusId, Tag};
use crate::api::music::MusicCache;
use crate::state::Action;
use crate::state::event::FailedAction;
use crate::ui::Theme;
use crate::ui::images::{self, ImageCache};
use crate::ui::screens::account_list::render_account_card;
use crate::ui::widgets::status_card::{self, ImageOverlay, RenderPrefs};

/// What a key press turned into.
pub enum SearchOutcome {
    Continue,
    Back,
    Dispatch(Action),
    OpenProfile(Account),
    OpenStatus(Status),
    /// Run a fresh search for `#name` (hashtag timeline).
    SearchTag(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Account(usize),
    Tag(usize),
    Status(usize),
}

pub struct SearchScreen {
    pub query: String,
    pub accounts: Vec<Account>,
    pub hashtags: Vec<Tag>,
    pub statuses: Vec<Status>,
    pub selected: usize,
    pub scroll: u16,
    last_g: bool,
    pub loading: bool,
    /// Status ids whose CW the user revealed here.
    pub revealed: HashSet<StatusId>,
}

impl SearchScreen {
    #[must_use]
    pub fn new(query: String) -> Self {
        Self {
            query,
            accounts: Vec::new(),
            hashtags: Vec::new(),
            statuses: Vec::new(),
            selected: 0,
            scroll: 0,
            last_g: false,
            loading: true,
            revealed: HashSet::new(),
        }
    }

    /// Install a `/api/v2/search` payload.
    pub fn on_results(&mut self, results: SearchResults) {
        self.accounts = results.accounts;
        self.hashtags = results.hashtags;
        self.statuses = results.statuses;
        self.selected = 0;
        self.scroll = 0;
        self.loading = false;
    }

    /// Hashtag timelines arrive as a plain status page.
    pub fn on_statuses(&mut self, statuses: Vec<Status>) {
        self.accounts.clear();
        self.hashtags.clear();
        self.statuses = statuses;
        self.selected = 0;
        self.scroll = 0;
        self.loading = false;
    }

    pub fn on_failed(&mut self) {
        self.loading = false;
    }

    fn items(&self) -> Vec<Item> {
        let mut v =
            Vec::with_capacity(self.accounts.len() + self.hashtags.len() + self.statuses.len());
        v.extend((0..self.accounts.len()).map(Item::Account));
        v.extend((0..self.hashtags.len()).map(Item::Tag));
        v.extend((0..self.statuses.len()).map(Item::Status));
        v
    }

    fn selected_item(&self) -> Option<Item> {
        self.items().get(self.selected).copied()
    }

    /// The status under the cursor (inner post for boosts), if the
    /// cursor is on a post at all.
    pub fn selected_target(&self) -> Option<&Status> {
        match self.selected_item()? {
            Item::Status(i) => {
                let outer = self.statuses.get(i)?;
                Some(outer.reblog.as_deref().unwrap_or(outer))
            }
            _ => None,
        }
    }

    pub fn selected_target_mut(&mut self) -> Option<&mut Status> {
        match self.selected_item()? {
            Item::Status(i) => {
                let outer = self.statuses.get_mut(i)?;
                if outer.reblog.is_some() {
                    outer.reblog.as_deref_mut()
                } else {
                    Some(outer)
                }
            }
            _ => None,
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

    /// Patch a status after a server round-trip.
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

    pub fn revert_action(&mut self, id: &StatusId, action: FailedAction) {
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

    pub fn on_status_deleted(&mut self, id: &StatusId) {
        self.statuses
            .retain(|s| s.id != *id && s.reblog.as_ref().is_none_or(|r| r.id != *id));
        let n = self.items().len();
        if self.selected >= n && n > 0 {
            self.selected = n - 1;
        }
    }

    /// Re-run whatever produced this page: a hashtag timeline for
    /// `#tag` queries, a text search otherwise.
    fn refresh_action(&self) -> Action {
        match self.query.strip_prefix('#') {
            Some(name) if !name.is_empty() && !name.contains(char::is_whitespace) => {
                Action::SearchTag {
                    name: name.to_string(),
                }
            }
            _ => Action::Search {
                query: self.query.clone(),
            },
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SearchOutcome {
        let n = self.items().len();
        let reset_g = !matches!(key.code, KeyCode::Char('g'));
        let outcome = match key.code {
            KeyCode::Char('h') | KeyCode::Esc | KeyCode::Backspace => SearchOutcome::Back,
            KeyCode::Char('R') => SearchOutcome::Dispatch(self.refresh_action()),
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                SearchOutcome::Dispatch(self.refresh_action())
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < n {
                    self.selected += 1;
                }
                SearchOutcome::Continue
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                SearchOutcome::Continue
            }
            KeyCode::Char('g') => {
                if self.last_g {
                    self.selected = 0;
                    self.scroll = 0;
                }
                self.last_g = !self.last_g;
                SearchOutcome::Continue
            }
            KeyCode::Char('G') => {
                if n > 0 {
                    self.selected = n - 1;
                }
                SearchOutcome::Continue
            }
            KeyCode::Char('l') | KeyCode::Enter => match self.selected_item() {
                Some(Item::Account(i)) => SearchOutcome::OpenProfile(self.accounts[i].clone()),
                Some(Item::Tag(i)) => SearchOutcome::SearchTag(self.hashtags[i].name.clone()),
                Some(Item::Status(_)) => match self.selected_target() {
                    Some(s) => SearchOutcome::OpenStatus(s.clone()),
                    None => SearchOutcome::Continue,
                },
                None => SearchOutcome::Continue,
            },
            KeyCode::Char('f') => match self.toggle_favourite_optimistic() {
                Some(a) => SearchOutcome::Dispatch(a),
                None => SearchOutcome::Continue,
            },
            KeyCode::Char('b') => match self.toggle_reblog_optimistic() {
                Some(a) => SearchOutcome::Dispatch(a),
                None => SearchOutcome::Continue,
            },
            KeyCode::Char('s') => {
                if let Some(t) = self.selected_target() {
                    let id = t.id.clone();
                    if !self.revealed.remove(&id) {
                        self.revealed.insert(id);
                    }
                }
                SearchOutcome::Continue
            }
            _ => SearchOutcome::Continue,
        };
        if reset_g {
            self.last_g = false;
        }
        outcome
    }

    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        prefs: RenderPrefs,
        music: &mut MusicCache,
        images_cache: &mut ImageCache,
    ) {
        const H_PAD: u16 = 1;
        let inner_width = area.width.saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut sel_range: (u16, u16) = (0, 0);
        let mut overlays: Vec<(u16, ImageOverlay)> = Vec::new();
        let items = self.items();

        if items.is_empty() {
            let msg = if self.loading {
                "searching…"
            } else {
                "nothing found"
            };
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(msg, theme.tertiary())));
        }

        let section = |lines: &mut Vec<Line<'static>>, label: &str| {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                label.to_string(),
                theme.tertiary(),
            )));
            lines.push(Line::default());
        };

        let mut last_kind: Option<u8> = None;
        for (idx, item) in items.iter().enumerate() {
            let kind = match item {
                Item::Account(_) => 0,
                Item::Tag(_) => 1,
                Item::Status(_) => 2,
            };
            if last_kind == Some(kind) {
                let gap = if kind == 2 {
                    prefs.inter_post_blank_lines
                } else {
                    1
                };
                for _ in 0..gap {
                    lines.push(Line::default());
                }
            } else {
                section(
                    &mut lines,
                    match kind {
                        0 => "accounts",
                        1 => "hashtags",
                        _ => "posts",
                    },
                );
                last_kind = Some(kind);
            }
            let selected = idx == self.selected;
            let start = lines.len() as u16;
            match *item {
                Item::Account(i) => {
                    lines.extend(render_account_card(
                        &self.accounts[i],
                        theme,
                        selected,
                        inner_width,
                    ));
                }
                Item::Tag(i) => {
                    lines.push(tag_line(&self.hashtags[i], theme, selected));
                }
                Item::Status(i) => {
                    let status = &self.statuses[i];
                    let inner_id = &status.reblog.as_deref().unwrap_or(status).id;
                    let opts = status_card::CardOpts {
                        selected,
                        cw_revealed: self.revealed.contains(inner_id),
                        show_images: images_cache.enabled(),
                        show_reply_hint: true,
                        ..prefs.card_opts()
                    };
                    let block = status_card::render_blocks(
                        status,
                        theme,
                        opts,
                        inner_width,
                        Some(&mut *music),
                    );
                    for ov in block.image_overlays {
                        overlays.push((start + ov.line_offset, ov));
                    }
                    lines.extend(block.lines);
                }
            }
            if selected {
                sel_range = (start, lines.len() as u16);
            }
        }

        // Minus the Block's one-row top padding.
        let height = area.height.saturating_sub(1);
        let (sel_start, sel_end) = sel_range;
        if !items.is_empty() {
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

        for (abs, ov) in &overlays {
            images::draw_overlay(frame, area, H_PAD, *abs, self.scroll, ov, images_cache);
        }
    }

    /// `← search "query" · h / Esc to go back`
    pub fn render_modal_header(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let line = Line::from(vec![
            Span::styled("← ", theme.tertiary()),
            Span::styled(format!("search \"{}\"", self.query), theme.secondary()),
            Span::styled("   ·   ", theme.tertiary()),
            Span::styled("h / Esc to go back", theme.tertiary()),
        ]);
        frame.render_widget(Paragraph::new(line).style(theme.primary()), area);
    }
}

/// `#tag  ·  12 posts this week` (history is the last 7 days).
fn tag_line(tag: &Tag, theme: &Theme, selected: bool) -> Line<'static> {
    let uses: u64 = tag
        .history
        .iter()
        .filter_map(|h| h.uses.parse::<u64>().ok())
        .sum();
    let mut spans = vec![if selected {
        Span::styled(format!("{} ", crate::icons::CURSOR), theme.cursor())
    } else {
        Span::raw("  ")
    }];
    spans.push(Span::styled(
        format!("#{}", tag.name),
        theme.hashtag_style(),
    ));
    if uses > 0 {
        spans.push(Span::styled(
            format!("  ·  {uses} posts this week"),
            theme.tertiary(),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{AccountId, TagHistory};
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn results() -> SearchResults {
        SearchResults {
            accounts: vec![Account {
                id: AccountId::new("a1"),
                acct: "alice".into(),
                ..Default::default()
            }],
            statuses: vec![Status {
                id: StatusId::new("s1"),
                ..Default::default()
            }],
            hashtags: vec![Tag {
                name: "rust".into(),
                url: String::new(),
                history: vec![TagHistory {
                    day: "0".into(),
                    uses: "7".into(),
                    accounts: "3".into(),
                }],
                following: None,
            }],
        }
    }

    #[test]
    fn cursor_walks_accounts_then_tags_then_posts() {
        let mut s = SearchScreen::new("q".into());
        s.on_results(results());
        assert!(matches!(
            s.handle_key(key('l')),
            SearchOutcome::OpenProfile(_)
        ));
        s.handle_key(key('j'));
        assert!(matches!(s.handle_key(key('l')), SearchOutcome::SearchTag(t) if t == "rust"));
        s.handle_key(key('j'));
        assert!(matches!(
            s.handle_key(key('l')),
            SearchOutcome::OpenStatus(_)
        ));
        s.handle_key(key('j')); // clamps
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn favourite_only_applies_on_a_post() {
        let mut s = SearchScreen::new("q".into());
        s.on_results(results());
        assert!(matches!(s.handle_key(key('f')), SearchOutcome::Continue));
        s.selected = 2;
        assert!(matches!(
            s.handle_key(key('f')),
            SearchOutcome::Dispatch(Action::Favourite(_))
        ));
    }
}
