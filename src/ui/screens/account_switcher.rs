//! Account switcher — modal list of every logged-in handle.
//!
//! Entered with `A`. Arrow / `j` / `k` to move, `Enter` to switch,
//! `h` / `Esc` / `A` to close. The current default is marked with a
//! subtle glyph so users don't accidentally "switch" to the account
//! they're already on.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::config::AccountRef;
use crate::ui::Theme;

pub enum SwitcherOutcome {
    Continue,
    Back,
    /// User picked an account. App builds the client + dispatches
    /// `Action::SwitchAccount`. Carries the selected [`AccountRef`].
    Pick(AccountRef),
}

pub struct AccountSwitcherScreen {
    pub accounts: Vec<AccountRef>,
    /// Handle of the account that is currently the default. Used to
    /// draw the `·` marker and to skip a no-op switch when the user
    /// Enters on the account they're already signed into.
    pub current: Option<String>,
    pub selected: usize,
    pub scroll: u16,
}

impl AccountSwitcherScreen {
    #[must_use]
    pub fn new(accounts: Vec<AccountRef>, current: Option<String>) -> Self {
        let selected = current
            .as_ref()
            .and_then(|h| accounts.iter().position(|a| a.handle == *h))
            .unwrap_or(0);
        Self {
            accounts,
            current,
            selected,
            scroll: 0,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SwitcherOutcome {
        let len = self.accounts.len();
        match key.code {
            KeyCode::Char('h' | 'A') | KeyCode::Esc | KeyCode::Backspace => SwitcherOutcome::Back,
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < len {
                    self.selected += 1;
                }
                SwitcherOutcome::Continue
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                SwitcherOutcome::Continue
            }
            KeyCode::Char('g') => {
                self.selected = 0;
                SwitcherOutcome::Continue
            }
            KeyCode::Char('G') => {
                if len > 0 {
                    self.selected = len - 1;
                }
                SwitcherOutcome::Continue
            }
            KeyCode::Char('l') | KeyCode::Enter => match self.accounts.get(self.selected) {
                Some(a) => SwitcherOutcome::Pick(a.clone()),
                None => SwitcherOutcome::Continue,
            },
            _ => SwitcherOutcome::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        const H_PAD: u16 = 1;
        let inner_width = area.width.saturating_sub(H_PAD * 2);
        let _ = inner_width; // reserved for future truncation polish

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::default());

        if self.accounts.is_empty() {
            lines.push(Line::from(Span::styled(
                "no accounts logged in — run `mastoot login`".to_string(),
                theme.tertiary(),
            )));
        }

        for (i, acc) in self.accounts.iter().enumerate() {
            if i > 0 {
                lines.push(Line::default());
            }
            let is_current = self.current.as_deref() == Some(acc.handle.as_str());
            let is_selected = i == self.selected;
            let cursor = if is_selected {
                Span::styled(format!("{} ", crate::icons::CURSOR), theme.cursor())
            } else {
                Span::raw("  ")
            };
            let marker = if is_current {
                Span::styled("● ", theme.secondary())
            } else {
                Span::styled("  ", theme.tertiary())
            };
            let name_style = if is_selected {
                theme.display_name().add_modifier(Modifier::BOLD)
            } else {
                theme.display_name()
            };
            let display = acc
                .display_name
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| acc.handle.clone());
            let header = Line::from(vec![
                cursor,
                marker,
                Span::styled(display, name_style),
                Span::raw("  "),
                Span::styled(format!("@{}", acc.handle), theme.handle()),
            ]);
            lines.push(header);
            let suffix = if is_current {
                "current"
            } else {
                "press Enter to switch"
            };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(suffix.to_string(), theme.tertiary()),
            ]));
        }

        let height = area.height;
        let sel_row = 1 + (self.selected as u16) * 3;
        if sel_row < self.scroll {
            self.scroll = sel_row;
        } else if sel_row + 2 >= self.scroll + height {
            self.scroll = (sel_row + 3).saturating_sub(height);
        }

        let p = Paragraph::new(lines)
            .style(Style::default().fg(theme.fg_primary).bg(theme.bg))
            .scroll((self.scroll, 0))
            .block(Block::new().padding(Padding::new(H_PAD, H_PAD, 1, 0)));
        frame.render_widget(p, area);
    }

    pub fn render_modal_header(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let line = Line::from(vec![
            Span::styled("← ", theme.tertiary()),
            Span::styled("switch account", theme.secondary()),
            Span::styled("   ·   ", theme.tertiary()),
            Span::styled("h / Esc to go back", theme.tertiary()),
        ]);
        frame.render_widget(Paragraph::new(line).style(theme.primary()), area);
    }
}
