//! Timeline screen. Keeps a cursor over a `Vec<Status>`, renders the
//! cards top-down into a scrollable viewport, and maps vim-ish keys
//! (`j` / `k` / `gg` / `G` / `R`) to [`Action`]s.

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::api::models::{Status, StatusId};
use crate::api::music::MusicCache;
use crate::state::{Action, TimelineKind};
use crate::ui::Theme;
use crate::ui::images::{self, ImageCache};
use crate::ui::widgets::status_card::{self, ImageOverlay, RenderPrefs};

/// Threshold at which we auto-request more posts. When the cursor is
/// within this many items of the end we fire [`Action::LoadMore`].
const LOAD_MORE_TRIGGER: usize = 5;

/// State scoped to one timeline tab.
pub struct TimelineScreen {
    pub kind: TimelineKind,
    pub selected: usize,
    pub scroll: u16,
    pub last_g: bool,
    pub load_more_pending: bool,
    /// Length of the backing list as of the last update we were told
    /// about. Lets `on_items_changed` detect an empty "older" page.
    known_len: usize,
    /// The server returned nothing older — stop asking until the next
    /// full refresh. Without this, every `j` at the bottom of a
    /// finished timeline re-fires `LoadMore`.
    pub exhausted: bool,
    /// Status ids whose CW the user has explicitly revealed in this
    /// session. Lives per-tab on purpose: revealing a CW in Home does
    /// not reveal it in Federated. Resets on tab refresh.
    pub revealed: HashSet<StatusId>,
}

impl TimelineScreen {
    #[must_use]
    pub fn new(kind: TimelineKind) -> Self {
        Self {
            kind,
            selected: 0,
            scroll: 0,
            last_g: false,
            load_more_pending: false,
            known_len: 0,
            exhausted: false,
            revealed: HashSet::new(),
        }
    }

    /// Reset all per-screen state to first-launch defaults. Used by
    /// account switching — the incoming account has its own timeline
    /// cursor, CW-reveal choices, etc.
    pub fn reset(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.last_g = false;
        self.load_more_pending = false;
        self.known_len = 0;
        self.exhausted = false;
        self.revealed.clear();
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

    /// The backing list shrank or grew outside of a page load (a
    /// deletion, a stream prepend). Keeps the exhaustion heuristic
    /// honest and clamps the cursor.
    pub fn on_len_changed(&mut self, len: usize) {
        self.known_len = len;
        if self.selected >= len && len > 0 {
            self.selected = len - 1;
        }
    }

    /// A `LoadMore` request errored out — allow the next scroll to
    /// the bottom to try again.
    pub fn on_load_more_failed(&mut self) {
        self.load_more_pending = false;
    }

    /// Called after a streaming update prepended `count` fresh items at
    /// index 0. Shifts the cursor down by `count` so it keeps pointing
    /// at whatever the user was actually reading — the new items scroll
    /// into the top of the viewport without stealing focus. `new_len`
    /// is the timeline length *after* the prepend.
    pub fn on_prepended(&mut self, count: usize, new_len: usize) {
        if count == 0 || new_len == 0 {
            return;
        }
        // Bound-clamp is cheap insurance against races: if a Replace
        // raced with a prepend, the store may be shorter than our
        // bumped index.
        let bumped = self.selected.saturating_add(count);
        self.selected = bumped.min(new_len - 1);
        self.known_len = new_len;
    }

    /// Translate a key into an optional [`Action`]. `items` is the
    /// timeline's current backing slice — needed by `s` so it can find
    /// the selected status' id without going back through App.
    pub fn handle_key(&mut self, key: KeyEvent, items: &[Status]) -> Option<Action> {
        let len = items.len();
        let reset_g = !matches!(key.code, KeyCode::Char('g'));
        let action = match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < len {
                    self.selected += 1;
                }
                self.check_load_more(len)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Char('g') => {
                if self.last_g {
                    self.selected = 0;
                    self.scroll = 0;
                }
                self.last_g = !self.last_g;
                None
            }
            KeyCode::Char('G') => {
                if len > 0 {
                    self.selected = len - 1;
                }
                self.check_load_more(len)
            }
            KeyCode::Char('R') => Some(Action::Refresh(self.kind)),
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::Refresh(self.kind))
            }
            KeyCode::Char('s') => {
                if let Some(s) = items.get(self.selected) {
                    let id = s.reblog.as_deref().unwrap_or(s).id.clone();
                    if !self.revealed.remove(&id) {
                        self.revealed.insert(id);
                    }
                }
                None
            }
            _ => None,
        };
        if reset_g {
            self.last_g = false;
        }
        action
    }

    fn check_load_more(&mut self, len: usize) -> Option<Action> {
        if !self.load_more_pending
            && !self.exhausted
            && len > 0
            && self.selected + LOAD_MORE_TRIGGER >= len
        {
            self.load_more_pending = true;
            Some(Action::LoadMore(self.kind))
        } else {
            None
        }
    }

    /// Build the full paragraph representing this timeline.
    ///
    /// Each card is pre-wrapped by `status_card::render` to the inner
    /// width. Adjacent cards are separated by pure whitespace — two
    /// blank rows — so inter-post spacing is visibly larger than the
    /// single blank row between paragraphs inside a post. No
    /// horizontal rule, no borders; the hierarchy comes from the
    /// rhythm alone.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        items: &[Status],
        theme: &Theme,
        prefs: RenderPrefs,
        parents: &HashMap<StatusId, Status>,
        music: &mut MusicCache,
        images_cache: &mut ImageCache,
    ) {
        if items.is_empty() {
            let msg = Paragraph::new(Line::from("loading timeline…"))
                .style(theme.secondary())
                .block(Block::new().padding(Padding::uniform(1)));
            frame.render_widget(msg, area);
            return;
        }

        // 1 column of padding on each side.
        const H_PAD: u16 = 1;
        let inner_width = area.width.saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut selected_line_range = (0u16, 0u16);
        let mut image_overlays: Vec<(u16, ImageOverlay)> = Vec::new();
        for (i, status) in items.iter().enumerate() {
            if i > 0 {
                for _ in 0..prefs.inter_post_blank_lines {
                    lines.push(Line::default());
                }
            }
            let inner_id = &status.reblog.as_deref().unwrap_or(status).id;
            let opts = status_card::CardOpts {
                selected: i == self.selected,
                cw_revealed: self.revealed.contains(inner_id),
                show_images: images_cache.enabled(),
                show_reply_hint: true,
                ..prefs.card_opts()
            };
            let parent = status_card::find_parent(status, items, parents);
            let block = status_card::render_blocks(
                status,
                theme,
                opts,
                inner_width,
                Some(&mut *music),
                parent,
            );
            let start = lines.len() as u16;
            let len = block.lines.len() as u16;
            for ov in block.image_overlays {
                image_overlays.push((start + ov.line_offset, ov));
            }
            lines.extend(block.lines);
            if i == self.selected {
                selected_line_range = (start, start + len);
            }
        }

        // Keep the selected card inside `area` by adjusting scroll.
        // The Block below pads one row at the top, so the visible
        // window is one shorter than `area.height`.
        let height = area.height.saturating_sub(1);
        let (sel_start, sel_end) = selected_line_range;
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

        // Image overlays — cover art from spacious Apple Music cards.
        // (Media attachments stay text in timeline density; media
        // attachment inline images are Phase 4 / A3 polish.)
        for (abs, ov) in &image_overlays {
            images::draw_overlay(frame, area, H_PAD, *abs, self.scroll, ov, images_cache);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn fake_items(n: usize) -> Vec<Status> {
        (0..n)
            .map(|i| Status {
                id: StatusId::new(format!("s{i}")),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn j_advances_cursor_and_caps_at_end() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        let items = fake_items(3);
        s.handle_key(key('j'), &items);
        s.handle_key(key('j'), &items);
        s.handle_key(key('j'), &items);
        s.handle_key(key('j'), &items);
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn k_goes_up_and_floors_at_zero() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        s.selected = 1;
        let items = fake_items(3);
        s.handle_key(key('k'), &items);
        s.handle_key(key('k'), &items);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn gg_jumps_to_top() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        s.selected = 4;
        s.scroll = 20;
        let items = fake_items(5);
        s.handle_key(key('g'), &items);
        s.handle_key(key('g'), &items);
        assert_eq!(s.selected, 0);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn capital_g_jumps_to_bottom() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        let items = fake_items(10);
        let action = s.handle_key(key('G'), &items);
        assert_eq!(s.selected, 9);
        assert!(matches!(action, Some(Action::LoadMore(TimelineKind::Home))));
    }

    #[test]
    fn load_more_fires_near_end() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        s.selected = 5;
        // len=10, threshold=5 → selected+5 >= 10 → fire
        let items = fake_items(10);
        let action = s.handle_key(key('j'), &items);
        assert!(matches!(action, Some(Action::LoadMore(TimelineKind::Home))));
    }

    #[test]
    fn load_more_does_not_refire_until_updated() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        s.selected = 5;
        let items10 = fake_items(10);
        let _ = s.handle_key(key('j'), &items10);
        let again = s.handle_key(key('j'), &items10);
        assert!(again.is_none());
        s.on_items_changed(20, true);
        let items20 = fake_items(20);
        let third = s.handle_key(key('j'), &items20);
        assert!(third.is_none()); // cursor at 7, threshold 5 → 12 < 20, no fire
    }

    #[test]
    fn empty_older_page_marks_exhausted_until_refresh() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        s.on_items_changed(10, false);
        s.selected = 8;
        let items = fake_items(10);
        assert!(matches!(
            s.handle_key(key('j'), &items),
            Some(Action::LoadMore(_))
        ));
        // Server says: nothing older.
        s.on_items_changed(10, true);
        assert!(s.exhausted);
        assert!(s.handle_key(key('j'), &items).is_none());
        // A refresh clears the flag.
        s.on_items_changed(10, false);
        assert!(!s.exhausted);
    }

    #[test]
    fn load_more_failure_unwedges_pagination() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        s.on_items_changed(10, false);
        s.selected = 8;
        let items = fake_items(10);
        assert!(s.handle_key(key('j'), &items).is_some());
        assert!(s.handle_key(key('j'), &items).is_none());
        s.on_load_more_failed();
        assert!(s.handle_key(key('j'), &items).is_some());
    }

    #[test]
    fn s_toggles_reveal_for_inner_id() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        let items = fake_items(2);
        let id0 = items[0].id.clone();
        s.handle_key(key('s'), &items);
        assert!(s.revealed.contains(&id0));
        s.handle_key(key('s'), &items);
        assert!(!s.revealed.contains(&id0));
    }

    #[test]
    fn prepend_bumps_cursor_to_keep_same_item() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        s.selected = 3;
        s.on_prepended(1, 11);
        assert_eq!(
            s.selected, 4,
            "cursor should follow its item when one is prepended"
        );
    }

    #[test]
    fn prepend_clamps_if_new_len_shrank() {
        // Paranoid path — races shouldn't overshoot the end.
        let mut s = TimelineScreen::new(TimelineKind::Home);
        s.selected = 5;
        s.on_prepended(10, 4);
        assert!(s.selected < 4);
    }

    #[test]
    fn prepend_noop_when_count_zero() {
        let mut s = TimelineScreen::new(TimelineKind::Home);
        s.selected = 2;
        s.on_prepended(0, 10);
        assert_eq!(s.selected, 2);
    }
}
