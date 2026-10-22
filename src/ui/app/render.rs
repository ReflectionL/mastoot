//! Frame rendering: tabs, body, status line, toasts, modals, help.

use super::{
    AccountSwitcherScreen, ApiHealth, App, Constraint, Direction, Layout, Line, Mode, Paragraph,
    ProfileScreen, Rect, Span, Status, StreamMode, StreamState, Style, TimelineKind, ToastLevel,
};

impl App {
    pub(super) fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        // Decode anything the image-download workers finished since the
        // last frame. Cheap when nothing arrived; never blocks.
        self.images.drain();
        self.music.drain();
        self.expire_toasts();
        // Copied out first: the per-mode arms below hold `&mut self.mode`.
        let prefs = self.prefs();

        let size = frame.area();

        if self.splash {
            self.render_splash(frame, size);
            return;
        }

        match &mut self.mode {
            Mode::Compose(state) => {
                state.render(frame, size, &self.theme, self.nerd_font);
            }
            Mode::ComposeConfirmDiscard(state) => {
                state.render(frame, size, &self.theme, self.nerd_font);
                self.render_discard_confirm(frame, size);
            }
            Mode::DeleteConfirm(_) => {
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ])
                    .split(size);
                self.render_tabs(frame, layout[0]);
                self.render_status_line(frame, layout[2]);
                self.render_delete_confirm(frame, size);
            }
            Mode::QuitConfirm => {
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ])
                    .split(size);
                self.render_tabs(frame, layout[0]);
                self.render_body(frame, layout[1]);
                self.render_status_line(frame, layout[2]);
                self.render_quit_confirm(frame, size);
            }
            Mode::StatusDetail(state) => {
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ])
                    .split(size);
                // body first so the &mut state borrow ends before we
                // call &self render helpers below.
                state.render(
                    frame,
                    layout[1],
                    &self.theme,
                    prefs,
                    &mut self.images,
                    &mut self.music,
                );
                self.render_detail_header(frame, layout[0]);
                self.render_status_line(frame, layout[2]);
            }
            Mode::Profile(state) => {
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ])
                    .split(size);
                let is_self = state.is_self;
                state.render(
                    frame,
                    layout[1],
                    &self.theme,
                    prefs,
                    &self.parents,
                    &mut self.music,
                    &mut self.images,
                );
                if is_self {
                    self.render_tabs(frame, layout[0]);
                } else {
                    ProfileScreen::render_modal_header(frame, layout[0], &self.theme);
                }
                self.render_status_line(frame, layout[2]);
            }
            Mode::Search(state) => {
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ])
                    .split(size);
                state.render(
                    frame,
                    layout[1],
                    &self.theme,
                    prefs,
                    &mut self.music,
                    &mut self.images,
                );
                state.render_modal_header(frame, layout[0], &self.theme);
                self.render_status_line(frame, layout[2]);
            }
            Mode::AccountList(state) => {
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ])
                    .split(size);
                state.render(frame, layout[1], &self.theme);
                state.render_modal_header(frame, layout[0], &self.theme);
                self.render_status_line(frame, layout[2]);
            }
            Mode::AccountSwitcher(state) => {
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ])
                    .split(size);
                state.render(frame, layout[1], &self.theme);
                AccountSwitcherScreen::render_modal_header(frame, layout[0], &self.theme);
                self.render_status_line(frame, layout[2]);
            }
            // The prompt lives in the status row; the body behind it is
            // whatever timeline tab is active.
            Mode::Timeline | Mode::SearchPrompt(_) => {
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ])
                    .split(size);
                self.render_tabs(frame, layout[0]);
                self.render_body(frame, layout[1]);
                self.render_status_line(frame, layout[2]);
            }
        }

        if self.show_help {
            self.render_help_overlay(frame, size);
        } else if !self.toasts.is_empty()
            && matches!(
                self.mode,
                Mode::Timeline
                    | Mode::StatusDetail(_)
                    | Mode::Profile(_)
                    | Mode::AccountList(_)
                    | Mode::AccountSwitcher(_)
                    | Mode::DeleteConfirm(_)
                    | Mode::QuitConfirm
                    | Mode::Search(_)
            )
        {
            self.render_toasts(frame, size);
        }
    }

    pub(super) fn render_splash(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let text = "mastoot.";
        let w = text.chars().count() as u16;
        let rect = Rect {
            x: area.x + area.width.saturating_sub(w) / 2,
            y: area.y + area.height / 2,
            width: w.min(area.width),
            height: 1,
        };
        let p = Paragraph::new(Line::from(Span::styled(text, self.theme.secondary())));
        frame.render_widget(p, rect);
    }

    /// Header row shown at the top of the detail page — a thin
    /// breadcrumb mirroring the timeline tab strip's vertical weight.
    pub(super) fn render_detail_header(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let line = Line::from(vec![
            Span::styled("← ", self.theme.tertiary()),
            Span::styled("thread", self.theme.secondary()),
            Span::styled("   ·   ", self.theme.tertiary()),
            Span::styled("h / Esc to go back", self.theme.tertiary()),
        ]);
        let p = Paragraph::new(line).style(self.theme.primary());
        frame.render_widget(p, area);
    }

    pub(super) fn render_discard_confirm(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let lines = vec![
            Line::default(),
            Line::from(Span::styled(
                "  Discard this draft?  (y / n)  ",
                self.theme.primary(),
            )),
            Line::default(),
        ];
        let w = 40.min(area.width);
        let h = 5.min(area.height);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        frame.render_widget(ratatui::widgets::Clear, rect);
        let block = ratatui::widgets::Block::new()
            .borders(ratatui::widgets::Borders::ALL)
            .border_style(self.theme.tertiary());
        let p = Paragraph::new(lines)
            .style(self.theme.primary())
            .block(block);
        frame.render_widget(p, rect);
    }

    /// Centered "Delete this post?" confirm. `Enter` deletes,
    /// `Esc` / `h` cancels. The modal draws on top of whatever the
    /// caller painted first; `Mode::DeleteConfirm`'s render arm
    /// paints the tab strip + status line so the user still has
    /// contextual chrome behind the box.
    pub(super) fn render_delete_confirm(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let lines = vec![
            Line::default(),
            Line::from(Span::styled("  Delete this post?  ", self.theme.primary())),
            Line::from(Span::styled(
                "  Enter: delete   ·   Esc: cancel  ",
                self.theme.tertiary(),
            )),
            Line::default(),
        ];
        let w = 44.min(area.width);
        let h = 6.min(area.height);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        frame.render_widget(ratatui::widgets::Clear, rect);
        let block = ratatui::widgets::Block::new()
            .borders(ratatui::widgets::Borders::ALL)
            .border_style(self.theme.error_style());
        let p = Paragraph::new(lines)
            .style(self.theme.primary())
            .block(block);
        frame.render_widget(p, rect);
    }

    /// Centered "Quit mastoot?" confirm. `Enter` / `y` quits,
    /// `Esc` / `n` stays.
    pub(super) fn render_quit_confirm(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let lines = vec![
            Line::default(),
            Line::from(Span::styled("  Quit mastoot?  ", self.theme.primary())),
            Line::from(Span::styled(
                "  Enter: quit   ·   Esc: stay  ",
                self.theme.tertiary(),
            )),
            Line::default(),
        ];
        let w = 40.min(area.width);
        let h = 6.min(area.height);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        frame.render_widget(ratatui::widgets::Clear, rect);
        let block = ratatui::widgets::Block::new()
            .borders(ratatui::widgets::Borders::ALL)
            .border_style(self.theme.tertiary());
        let p = Paragraph::new(lines)
            .style(self.theme.primary())
            .block(block);
        frame.render_widget(p, rect);
    }

    pub(super) fn render_tabs(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let labels = [
            (Some(TimelineKind::Home), "1 Home"),
            (Some(TimelineKind::Local), "2 Local"),
            (Some(TimelineKind::Federated), "3 Federated"),
            (Some(TimelineKind::Notifications), "4 Notifications"),
            // Profile is a Mode, not a timeline kind; matched via
            // `tab_5_active()` below.
            (None, "5 Profile"),
            (Some(TimelineKind::Favourites), "6 Favourites"),
            (Some(TimelineKind::Bookmarks), "7 Bookmarks"),
        ];
        let profile_active = self.tab_5_active();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(labels.len() * 2);
        for (i, (kind, label)) in labels.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ·  ", self.theme.tertiary()));
            }
            let active = match kind {
                Some(k) => *k == self.active && !profile_active,
                None => profile_active,
            };
            let style = if active {
                self.theme
                    .primary()
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                self.theme.secondary()
            };
            spans.push(Span::styled((*label).to_string(), style));
        }
        let line = Line::from(spans);
        let p = Paragraph::new(line).style(self.theme.primary());
        frame.render_widget(p, area);
    }

    /// Whether the visible mode is the self-profile view (tab 5).
    pub(super) fn tab_5_active(&self) -> bool {
        matches!(&self.mode, Mode::Profile(p) if p.is_self)
    }

    pub(super) fn render_body(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let prefs = self.prefs();
        if self.active == TimelineKind::Notifications {
            self.notifications_screen
                .render(frame, area, &self.notifications, &self.theme, prefs);
            return;
        }
        let empty: Vec<Status> = Vec::new();
        let items = self.timelines.get(&self.active).unwrap_or(&empty);
        if let Some(screen) = self.screens.get_mut(&self.active) {
            screen.render(
                frame,
                area,
                items,
                &self.theme,
                prefs,
                &self.parents,
                &mut self.music,
                &mut self.images,
            );
        }
    }

    pub(super) fn render_status_line(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        // Search prompt takes over the whole row: `/ query▏`.
        if let Mode::SearchPrompt(q) = &self.mode {
            let hint = "Enter: search   ·   Esc: cancel ";
            let left_visual = 3 + q.chars().count() + 1;
            let pad = (area.width as usize).saturating_sub(left_visual + hint.chars().count());
            let line = Line::from(vec![
                Span::raw(" "),
                Span::styled("/ ", self.theme.link()),
                Span::styled(q.clone(), self.theme.primary()),
                Span::styled("▏", self.theme.cursor()),
                Span::raw(" ".repeat(pad)),
                Span::styled(hint, self.theme.tertiary()),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
        // The glyph encodes the user-selected live-update mode; the
        // color encodes REST health. Two orthogonal signals, one dot.
        // - ● streaming (full-size, attention value)
        // - … polling   (three dots, intermittent feel)
        // - · off       (smallest mark, quietest)
        let dot_glyph = match self.stream_mode {
            StreamMode::Streaming => "●",
            StreamMode::Polling => "…",
            StreamMode::Off => "·",
        };
        let dot_style = match self.api_health {
            ApiHealth::Healthy => self.theme.tertiary(),
            ApiHealth::Degraded => Style::default().fg(self.theme.favorite).bg(self.theme.bg),
            ApiHealth::Offline | ApiHealth::AuthInvalid => self.theme.error_style(),
        };
        // Label follows the current mode — `self.active` only tracks
        // *which timeline tab* we last opened, so on a sub-page (detail
        // / profile / account switcher / …) we'd otherwise keep saying
        // "home" even though the user is clearly somewhere else.
        let kind_label = match &self.mode {
            Mode::Timeline => format!("{:?}", self.active).to_lowercase(),
            Mode::StatusDetail(_) => "thread".to_string(),
            Mode::Profile(p) => {
                if p.is_self {
                    "profile".to_string()
                } else {
                    p.account
                        .as_ref()
                        .map_or_else(|| "profile".to_string(), |a| format!("@{}", a.acct))
                }
            }
            Mode::AccountList(list) => list.kind.label().to_string(),
            Mode::AccountSwitcher(_) => "switch account".to_string(),
            Mode::Compose(_) | Mode::ComposeConfirmDiscard(_) => "compose".to_string(),
            Mode::DeleteConfirm(_) => "delete?".to_string(),
            Mode::QuitConfirm => "quit?".to_string(),
            Mode::Search(s) => format!("search \"{}\"", s.query),
            Mode::SearchPrompt(_) => "search".to_string(),
        };

        // Suffix only shows when something is *not* normal. In streaming
        // mode a brief reconnect isn't interesting; only show it if we
        // spend real time disconnected. In polling / off modes we never
        // surface stream state (the glyph already told the user).
        let (suffix_label, suffix_style) = match (self.api_health, self.stream_mode, self.stream) {
            (ApiHealth::AuthInvalid, _, _) => (Some("login?"), dot_style),
            (ApiHealth::Offline, _, _) => (Some("offline"), dot_style),
            (ApiHealth::Degraded, _, _) => (Some("degraded"), dot_style),
            (
                ApiHealth::Healthy,
                StreamMode::Streaming,
                StreamState::Reconnecting | StreamState::Connecting,
            ) => (Some("reconnecting"), self.theme.tertiary()),
            _ => (None, self.theme.tertiary()),
        };
        let hint = "?:help  esc:quit  j/k  f/b  c/r/q  R:refresh";

        // Widths: leading " ● " (3) + kind + (" · label" when present).
        let suffix_visual = suffix_label.map_or(0, |s| 3 + s.chars().count());
        let left_visual = 3 + kind_label.chars().count() + suffix_visual;
        let right_visual = hint.chars().count() + 1;
        let pad = (area.width as usize).saturating_sub(left_visual + right_visual);

        let mut spans = vec![
            Span::raw(" "),
            Span::styled(dot_glyph, dot_style),
            Span::raw(" "),
            Span::styled(kind_label, self.theme.secondary()),
        ];
        if let Some(label) = suffix_label {
            spans.push(Span::styled(" · ", self.theme.tertiary()));
            spans.push(Span::styled(label, suffix_style));
        }
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(format!("{hint} "), self.theme.tertiary()));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    pub(super) fn render_toasts(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let h = self.toasts.len() as u16;
        let rect = Rect {
            x: area.x + 2,
            y: area.y + area.height.saturating_sub(h + 2),
            width: area.width.saturating_sub(4),
            height: h,
        };
        let lines: Vec<Line<'static>> = self
            .toasts
            .iter()
            .map(|t| {
                let style = match t.level {
                    ToastLevel::Info => self.theme.secondary(),
                    ToastLevel::Warn => Style::default().fg(self.theme.favorite).bg(self.theme.bg),
                    ToastLevel::Error => self.theme.error_style(),
                };
                Line::from(Span::styled(format!("  {}", t.message), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rect);
    }

    pub(super) fn render_help_overlay(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let mut help_lines = crate::ui::keymap::help_lines();
        let width_of = |l: &String| {
            l.chars()
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1))
                .sum::<usize>()
        };
        // The list plus its closing hint, inside the borders.
        let inner_h = area.height.saturating_sub(2);
        let overflows = help_lines.len() + 1 > usize::from(inner_h);
        help_lines.push(
            if overflows {
                "  (j/k scroll · any other key closes)"
            } else {
                "  (press any key to dismiss)"
            }
            .to_string(),
        );
        let max_scroll = (help_lines.len() as u16).saturating_sub(inner_h);
        self.help_scroll = self.help_scroll.min(max_scroll);

        // Fit the box to the longest line (+ borders + a little air)
        // instead of a fixed width, so no entry gets clipped.
        let longest = help_lines.iter().map(width_of).max().unwrap_or(0) as u16;
        let w = (longest + 4).min(area.width);
        let h = (help_lines.len() as u16 + 2).min(area.height);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        let lines: Vec<Line<'static>> = help_lines
            .into_iter()
            .map(|s| Line::from(Span::styled(s, self.theme.primary())))
            .collect();
        frame.render_widget(ratatui::widgets::Clear, rect);
        let mut block = ratatui::widgets::Block::new()
            .borders(ratatui::widgets::Borders::ALL)
            .border_style(self.theme.tertiary());
        // Which way there is more, quietly, in the bottom border.
        let more = match (self.help_scroll > 0, self.help_scroll < max_scroll) {
            (false, true) => " ↓ ",
            (true, true) => " ↑↓ ",
            (true, false) => " ↑ ",
            (false, false) => "",
        };
        if !more.is_empty() {
            block = block.title_bottom(Line::from(more).right_aligned());
        }
        let p = Paragraph::new(lines)
            .style(self.theme.primary())
            .block(block)
            .scroll((self.help_scroll, 0));
        frame.render_widget(p, rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, MediaRender};
    use crate::ui::Theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let mut cfg = Config::default();
        cfg.ui.media_render = MediaRender::TextOnly;
        App::new(Theme::by_name("frost"), StreamMode::Off, cfg)
    }

    fn help_grid(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.render_help_overlay(f, f.area())).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn help_overlay_scrolls_when_the_terminal_is_short() {
        let mut app = app();
        app.show_help = true;
        let top = help_grid(&mut app, 100, 20);
        assert!(top.contains("mastoot — keys"), "{top}");
        assert!(!top.contains("j/k scroll"), "the hint sits at the very end");
        assert!(top.contains('↓'), "{top}");

        // Overshooting clamps to the last page, which ends on the hint.
        app.help_scroll = u16::MAX;
        let bottom = help_grid(&mut app, 100, 20);
        assert!(
            bottom.contains("j/k scroll · any other key closes"),
            "{bottom}"
        );
        assert!(!bottom.contains("mastoot — keys"), "{bottom}");
        let max = app.help_scroll;
        assert!(max > 0 && max < u16::MAX);
    }

    #[test]
    fn help_overlay_fits_without_scrolling_on_a_tall_terminal() {
        let mut app = app();
        app.show_help = true;
        app.help_scroll = 5;
        let grid = help_grid(&mut app, 100, 120);
        assert_eq!(app.help_scroll, 0, "nothing to scroll");
        assert!(grid.contains("press any key to dismiss"), "{grid}");
        assert!(grid.contains("mastoot — keys"), "{grid}");
    }
}
