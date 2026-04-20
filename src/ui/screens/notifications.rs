//! Notifications screen — vertical list of notification cards with a
//! single-row filter strip on top.
//!
//! Filter is *client-side*: we always fetch the full mixed feed and
//! hide types the user isn't currently focused on. That keeps the
//! Mastodon API call simple (no `types[]=…` round-trips on Tab) and
//! lets the user flip filters instantly without waiting on the wire.
//!
//! Selection traverses *visible* notifications only; cycling the
//! filter resets the cursor so it always lands on something on-screen.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::api::models::{Notification, NotificationType, Status};
use crate::state::{Action, TimelineKind};
use crate::ui::Theme;
use crate::ui::widgets::notification_card;

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
        }
    }

    /// Reset to first-launch defaults. Used by account switching.
    pub fn reset(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.filter = NotificationFilter::All;
        self.last_g = false;
        self.load_more_pending = false;
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

    pub fn on_items_changed(&mut self, len: usize, appended: bool) {
        if !appended {
            self.selected = 0;
            self.scroll = 0;
        }
        if self.selected >= len && len > 0 {
            self.selected = len - 1;
        }
        self.load_more_pending = false;
    }

    /// Streaming-style prepend: `count` fresh notifications landed at
    /// the top. Bump the selected index so the cursor sticks to the
    /// thing the user was looking at.
    pub fn on_prepended(&mut self, count: usize, new_len: usize) {
        if count == 0 || new_len == 0 {
            return;
        }
        let bumped = self.selected.saturating_add(count);
        self.selected = bumped.min(new_len - 1);
    }

    /// Driver. `items` is the full notifications list.
    pub fn handle_key(&mut self, key: KeyEvent, items: &[Notification]) -> NotifOutcome {
        let visible = self.visible_indices(items);
        let len = visible.len();
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
                let real_idx = visible.get(self.selected).copied();
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
        nerd_font: bool,
    ) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);
        self.render_filter_strip(frame, layout[0], theme);

        let body_area = layout[1];
        let visible = self.visible_indices(items);

        if visible.is_empty() {
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
        for (i, idx) in visible.iter().enumerate() {
            if i > 0 {
                for _ in 0..notification_card::INTER_NOTIFICATION_BLANK_LINES {
                    lines.push(Line::default());
                }
            }
            let card = notification_card::render(
                &items[*idx],
                theme,
                i == self.selected,
                nerd_font,
                inner_width,
            );
            let start = lines.len() as u16;
            let len = card.len() as u16;
            lines.extend(card);
            if i == self.selected {
                sel_range = (start, start + len);
            }
        }

        let height = body_area.height;
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
}
