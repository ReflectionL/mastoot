//! Notifications screen — vertical list of notification cards with a
//! single-row filter strip on top.
//!
//! Filter is *client-side*: we always fetch the full mixed feed and
//! hide types the user isn't currently focused on. That keeps the
//! Mastodon API call simple (no `types[]=…` round-trips on Tab) and
//! lets the user flip filters instantly without waiting on the wire.
//!
//! Likes and boosts of the same post collapse into one row ("Alice, Bob
//! and 3 others favourited your post"). Grouping is a view concern —
//! `items` stays the flat server order — and the group sits where its
//! newest member is. Selection traverses *groups*; cycling the filter
//! resets the cursor so it always lands on something on-screen.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use std::collections::HashMap;

use crate::api::models::{Notification, NotificationId, NotificationType, Status};
use crate::state::{Action, TimelineKind};
use crate::ui::Theme;
use crate::ui::widgets::notification_card;
use crate::ui::widgets::status_card::RenderPrefs;

const LOAD_MORE_TRIGGER: usize = 5;

/// Filter chips along the top of the notifications screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationFilter {
    All,
    Mentions,
    Boosts,
    Favourites,
    Follows,
}

impl NotificationFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Mentions => "Mentions",
            Self::Boosts => "Boosts",
            Self::Favourites => "Favourites",
            Self::Follows => "Follows",
        }
    }

    fn matches(self, n: &Notification) -> bool {
        match self {
            Self::All => true,
            Self::Mentions => matches!(
                n.notification_type,
                NotificationType::Mention | NotificationType::Quote
            ),
            Self::Boosts => matches!(n.notification_type, NotificationType::Reblog),
            Self::Favourites => matches!(n.notification_type, NotificationType::Favourite),
            Self::Follows => matches!(
                n.notification_type,
                NotificationType::Follow | NotificationType::FollowRequest
            ),
        }
    }

    fn cycle_next(self) -> Self {
        match self {
            Self::All => Self::Mentions,
            Self::Mentions => Self::Boosts,
            Self::Boosts => Self::Favourites,
            Self::Favourites => Self::Follows,
            Self::Follows => Self::All,
        }
    }

    fn cycle_prev(self) -> Self {
        match self {
            Self::All => Self::Follows,
            Self::Mentions => Self::All,
            Self::Boosts => Self::Mentions,
            Self::Favourites => Self::Boosts,
            Self::Follows => Self::Favourites,
        }
    }
}

/// Result of a key in the notifications screen.
pub enum NotifOutcome {
    /// Stay here; key consumed (or no-op).
    Continue,
    /// Send an `Action` to the state task.
    Dispatch(Action),
    /// Open the linked status in detail mode. Carries a clone so the
    /// app layer can build a DetailState without a second lookup.
    OpenStatus(Status),
}

pub struct NotificationsScreen {
    pub selected: usize,
    pub scroll: u16,
    pub filter: NotificationFilter,
    last_g: bool,
    pub load_more_pending: bool,
    known_len: usize,
    pub exhausted: bool,
    /// Id of the newest notification in the selected group. Lets the
    /// cursor follow what the user is looking at when new items land
    /// above it (a fresh favourite may merge into an existing group
    /// instead of adding a row).
    anchor: Option<NotificationId>,
}

impl Default for NotificationsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            selected: 0,
            scroll: 0,
            filter: NotificationFilter::All,
            last_g: false,
            load_more_pending: false,
            known_len: 0,
            exhausted: false,
            anchor: None,
        }
    }

    /// Reset to first-launch defaults. Used by account switching.
    pub fn reset(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.filter = NotificationFilter::All;
        self.last_g = false;
        self.load_more_pending = false;
        self.known_len = 0;
        self.exhausted = false;
        self.anchor = None;
    }

    /// A `LoadMore` request errored out — allow a retry.
    pub fn on_load_more_failed(&mut self) {
        self.load_more_pending = false;
    }

    /// The list changed outside a page load (deletion).
    pub fn on_len_changed(&mut self, len: usize) {
        self.known_len = len;
    }

    /// Indexes (into the full `notifications` list) that pass the
    /// current filter. Cheap — a few dozen entries at most per page.
    pub fn visible_indices(&self, items: &[Notification]) -> Vec<usize> {
        items
            .iter()
            .enumerate()
            .filter(|(_, n)| self.filter.matches(n))
            .map(|(i, _)| i)
            .collect()
    }

    /// Visible notifications folded into display groups, newest first.
    /// Each group is a list of indexes into `items`, in server order;
    /// the first entry is the representative. Only favourites and
    /// boosts merge, and only when they target the same post.
    pub fn groups(&self, items: &[Notification]) -> Vec<Vec<usize>> {
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut by_key: HashMap<(NotificationType, String), usize> = HashMap::new();
        for i in self.visible_indices(items) {
            let n = &items[i];
            let mergeable = matches!(
                n.notification_type,
                NotificationType::Favourite | NotificationType::Reblog
            );
            let key = n
                .status
                .as_ref()
                .filter(|_| mergeable)
                .map(|st| (n.notification_type, st.id.to_string()));
            if let Some(k) = key {
                if let Some(&g) = by_key.get(&k) {
                    groups[g].push(i);
                } else {
                    by_key.insert(k, groups.len());
                    groups.push(vec![i]);
                }
            } else {
                groups.push(vec![i]);
            }
        }
        groups
    }

    pub fn on_items_changed(&mut self, len: usize, appended: bool) {
        if appended {
            if len == self.known_len {
                self.exhausted = true;
            }
        } else {
            self.selected = 0;
            self.scroll = 0;
            self.exhausted = false;
        }
        self.known_len = len;
        if self.selected >= len && len > 0 {
            self.selected = len - 1;
        }
        self.load_more_pending = false;
    }

    /// Streaming-style prepend: fresh notifications landed at the top
    /// of `items`. Re-find the group the user was looking at so the
    /// cursor sticks to it.
    pub fn on_prepended(&mut self, items: &[Notification]) {
        self.known_len = items.len();
        let Some(anchor) = self.anchor.clone() else {
            return;
        };
        if let Some(g) = self
            .groups(items)
            .iter()
            .position(|members| members.iter().any(|&i| items[i].id == anchor))
        {
            self.selected = g;
        }
    }

    /// Index into the full list of the selected group's representative
    /// (newest) notification, if any.
    #[must_use]
    pub fn selected_index(&self, items: &[Notification]) -> Option<usize> {
        self.groups(items)
            .get(self.selected)
            .and_then(|g| g.first())
            .copied()
    }

    fn remember_anchor(&mut self, items: &[Notification]) {
        self.anchor = self.selected_index(items).map(|i| items[i].id.clone());
    }

    /// Driver. `items` is the full notifications list.
    pub fn handle_key(&mut self, key: KeyEvent, items: &[Notification]) -> NotifOutcome {
        let groups = self.groups(items);
        let len = groups.len();
        let reset_g = !matches!(key.code, KeyCode::Char('g'));
        let outcome = match key.code {
            KeyCode::Tab => {
                self.filter = self.filter.cycle_next();
                self.selected = 0;
                self.scroll = 0;
                NotifOutcome::Continue
            }
            KeyCode::BackTab => {
                self.filter = self.filter.cycle_prev();
                self.selected = 0;
                self.scroll = 0;
                NotifOutcome::Continue
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < len {
                    self.selected += 1;
                }
                self.check_load_more(items.len(), self.selected, len)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                NotifOutcome::Continue
            }
            KeyCode::Char('g') => {
                if self.last_g {
                    self.selected = 0;
                    self.scroll = 0;
                }
                self.last_g = !self.last_g;
                NotifOutcome::Continue
            }
            KeyCode::Char('G') => {
                if len > 0 {
                    self.selected = len - 1;
                }
                self.check_load_more(items.len(), self.selected, len)
            }
            KeyCode::Char('R') => {
                NotifOutcome::Dispatch(Action::Refresh(TimelineKind::Notifications))
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                NotifOutcome::Dispatch(Action::Refresh(TimelineKind::Notifications))
            }
            KeyCode::Char('l') | KeyCode::Enter => {
                let real_idx = groups.get(self.selected).and_then(|g| g.first()).copied();
                match real_idx
                    .and_then(|i| items.get(i))
                    .and_then(|n| n.status.as_ref())
                {
                    Some(s) => NotifOutcome::OpenStatus(s.clone()),
                    None => NotifOutcome::Continue,
                }
            }
            _ => NotifOutcome::Continue,
        };
        if reset_g {
            self.last_g = false;
        }
        self.remember_anchor(items);
        outcome
    }

    /// Trigger a fetch of older entries if the cursor is within
    /// `LOAD_MORE_TRIGGER` of the last *visible* item AND we have
    /// not already asked for more since the last update.
    fn check_load_more(
        &mut self,
        total_len: usize,
        selected: usize,
        visible_len: usize,
    ) -> NotifOutcome {
        if !self.load_more_pending
            && !self.exhausted
            && total_len > 0
            && visible_len > 0
            && selected + LOAD_MORE_TRIGGER >= visible_len
        {
            self.load_more_pending = true;
            NotifOutcome::Dispatch(Action::LoadMore(TimelineKind::Notifications))
        } else {
            NotifOutcome::Continue
        }
    }

    /// Top filter strip: `All · Mentions · Boosts · Favourites · Follows`,
    /// active chip bold + primary, others dim.
    fn render_filter_strip(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        const ORDER: [NotificationFilter; 5] = [
            NotificationFilter::All,
            NotificationFilter::Mentions,
            NotificationFilter::Boosts,
            NotificationFilter::Favourites,
            NotificationFilter::Follows,
        ];
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(11);
        for (i, f) in ORDER.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ·  ", theme.tertiary()));
            }
            let style = if *f == self.filter {
                theme.primary().add_modifier(Modifier::BOLD)
            } else {
                theme.tertiary()
            };
            spans.push(Span::styled(f.label().to_string(), style));
        }
        // Right-aligned hint.
        let hint = Span::styled("Tab: cycle filter", theme.tertiary());
        // Compute used width and pad.
        let used: usize = spans
            .iter()
            .flat_map(|s| s.content.chars())
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        let pad = (area.width as usize).saturating_sub(used + hint.content.chars().count() + 2);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(hint);
        let p = Paragraph::new(Line::from(spans)).style(theme.primary());
        frame.render_widget(p, area);
    }

    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        items: &[Notification],
        theme: &Theme,
        prefs: RenderPrefs,
    ) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);
        self.render_filter_strip(frame, layout[0], theme);

        let body_area = layout[1];
        let groups = self.groups(items);
        self.selected = self.selected.min(groups.len().saturating_sub(1));
        self.remember_anchor(items);

        if groups.is_empty() {
            let msg = if items.is_empty() {
                "loading notifications…"
            } else {
                "no notifications match this filter"
            };
            let p = Paragraph::new(Line::from(msg))
                .style(theme.secondary())
                .block(Block::new().padding(Padding::uniform(1)));
            frame.render_widget(p, body_area);
            return;
        }

        const H_PAD: u16 = 1;
        let inner_width = body_area.width.saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut sel_range: (u16, u16) = (0, 0);
        for (i, members) in groups.iter().enumerate() {
            if i > 0 {
                for _ in 0..notification_card::INTER_NOTIFICATION_BLANK_LINES {
                    lines.push(Line::default());
                }
            }
            let others: Vec<&Notification> = members[1..].iter().map(|&j| &items[j]).collect();
            let card = notification_card::render_group(
                &items[members[0]],
                &others,
                theme,
                i == self.selected,
                prefs,
                inner_width,
            );
            let start = lines.len() as u16;
            let len = card.len() as u16;
            lines.extend(card);
            if i == self.selected {
                sel_range = (start, start + len);
            }
        }

        // Minus the Block's one-row top padding.
        let height = body_area.height.saturating_sub(1);
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
        frame.render_widget(p, body_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{Account, NotificationId};
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn fake(kind: NotificationType, id: &str) -> Notification {
        Notification {
            id: NotificationId::new(id),
            notification_type: kind,
            created_at: None,
            account: Account {
                acct: "alice@ex.com".into(),
                ..Default::default()
            },
            status: None,
            report: None,
        }
    }

    #[test]
    fn filter_matches_only_matching_kinds() {
        let items = vec![
            fake(NotificationType::Mention, "1"),
            fake(NotificationType::Reblog, "2"),
            fake(NotificationType::Favourite, "3"),
        ];
        let s = NotificationsScreen::new();
        assert_eq!(s.visible_indices(&items).len(), 3); // All

        let mut s2 = NotificationsScreen::new();
        s2.filter = NotificationFilter::Boosts;
        let v = s2.visible_indices(&items);
        assert_eq!(v, vec![1]);
    }

    #[test]
    fn tab_cycles_filter_and_resets_cursor() {
        let items = vec![
            fake(NotificationType::Mention, "1"),
            fake(NotificationType::Mention, "2"),
        ];
        let mut s = NotificationsScreen::new();
        s.selected = 1;
        s.handle_key(key(KeyCode::Tab, KeyModifiers::NONE), &items);
        assert_eq!(s.filter, NotificationFilter::Mentions);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn enter_with_no_status_is_no_op() {
        let items = vec![fake(NotificationType::Follow, "1")];
        let mut s = NotificationsScreen::new();
        match s.handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &items) {
            NotifOutcome::Continue => {}
            _ => panic!("expected Continue (follow has no status)"),
        }
    }

    fn fake_on(kind: NotificationType, id: &str, status: &str) -> Notification {
        let mut n = fake(kind, id);
        n.status = Some(Status {
            id: crate::api::models::StatusId::new(status),
            ..Default::default()
        });
        n
    }

    #[test]
    fn favourites_of_one_post_form_one_group() {
        let items = vec![
            fake_on(NotificationType::Favourite, "4", "p1"),
            fake_on(NotificationType::Favourite, "3", "p2"),
            fake_on(NotificationType::Favourite, "2", "p1"),
            fake_on(NotificationType::Reblog, "1", "p1"),
        ];
        let s = NotificationsScreen::new();
        // p1 favourites merge (even though p2 sits between them); the
        // boost of p1 is a different action and stays separate.
        assert_eq!(s.groups(&items), vec![vec![0, 2], vec![1], vec![3]]);
    }

    #[test]
    fn mentions_and_follows_never_merge() {
        let items = vec![
            fake_on(NotificationType::Mention, "3", "p1"),
            fake_on(NotificationType::Mention, "2", "p1"),
            fake(NotificationType::Follow, "1"),
        ];
        let s = NotificationsScreen::new();
        assert_eq!(s.groups(&items).len(), 3);
    }

    #[test]
    fn cursor_moves_by_group_and_enter_opens_the_post() {
        let items = vec![
            fake_on(NotificationType::Favourite, "3", "p1"),
            fake_on(NotificationType::Favourite, "2", "p1"),
            fake_on(NotificationType::Mention, "1", "p2"),
        ];
        let mut s = NotificationsScreen::new();
        s.handle_key(key(KeyCode::Char('j'), KeyModifiers::NONE), &items);
        assert_eq!(s.selected, 1);
        assert_eq!(s.selected_index(&items), Some(2));
        match s.handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &items) {
            NotifOutcome::OpenStatus(st) => assert_eq!(st.id.as_str(), "p2"),
            _ => panic!("expected OpenStatus"),
        }
    }

    #[test]
    fn prepend_that_merges_keeps_cursor_on_the_same_group() {
        let mut items = vec![
            fake_on(NotificationType::Mention, "2", "p2"),
            fake_on(NotificationType::Favourite, "1", "p1"),
        ];
        let mut s = NotificationsScreen::new();
        s.handle_key(key(KeyCode::Char('j'), KeyModifiers::NONE), &items);
        assert_eq!(s.selected, 1); // the favourite group

        // A new favourite of p1 lands on top: it joins that group, so
        // no row is added and the cursor must not move.
        items.insert(0, fake_on(NotificationType::Favourite, "3", "p1"));
        s.on_prepended(&items);
        assert_eq!(s.groups(&items), vec![vec![0, 2], vec![1]]);
        assert_eq!(s.selected, 0);
        assert_eq!(s.selected_index(&items), Some(0));

        // A brand-new mention adds a row above the group instead.
        items.insert(0, fake_on(NotificationType::Mention, "4", "p3"));
        s.on_prepended(&items);
        assert_eq!(s.selected, 1);
    }
}
