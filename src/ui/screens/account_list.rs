//! Followers / Following list. Modal sub-page entered from a profile
//! via `o` (followers) or `O` (following). Each row is a compact
//! two-line account card; `l` / `Enter` opens the selected account
//! in a fresh profile (pushed onto the back stack).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::api::html;
use crate::api::models::{Account, AccountId};
use crate::state::Action;
use crate::state::event::AccountListKind;
use crate::ui::Theme;
use crate::ui::widgets::wrap;

const LOAD_MORE_TRIGGER: usize = 5;

/// What the cursor's selection becomes after a key press.
pub enum AccountListOutcome {
    Continue,
    Back,
    Dispatch(Action),
    /// Open the selected account's profile (App pushes a new
    /// Mode::Profile and dispatches LoadProfile/Relationship).
    OpenProfile(Account),
}

pub struct AccountListScreen {
    pub for_id: AccountId,
    /// `@user` of the account whose followers/following we're viewing,
    /// rendered in the header. Pre-populated when entering from a
    /// profile so the title is right immediately.
    pub for_handle: String,
    pub kind: AccountListKind,
    pub accounts: Vec<Account>,
    pub selected: usize,
    pub scroll: u16,
    last_g: bool,
    pub load_more_pending: bool,
    pub loading: bool,
}

impl AccountListScreen {
    #[must_use]
    pub fn new(for_id: AccountId, for_handle: String, kind: AccountListKind) -> Self {
        Self {
            for_id,
            for_handle,
            kind,
            accounts: Vec::new(),
            selected: 0,
            scroll: 0,
            last_g: false,
            load_more_pending: false,
            loading: true,
        }
    }

    pub fn on_loaded(&mut self, accounts: Vec<Account>, appended: bool) {
        if appended {
            // Cheap dedup by id since the server may overlap pages on a
            // since_id boundary (rare, but cheap to defend against).
            let known: std::collections::HashSet<_> =
                self.accounts.iter().map(|a| a.id.clone()).collect();
            for a in accounts {
                if !known.contains(&a.id) {
                    self.accounts.push(a);
                }
            }
        } else {
            self.accounts = accounts;
            self.selected = 0;
            self.scroll = 0;
        }
        self.loading = false;
        self.load_more_pending = false;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AccountListOutcome {
        let len = self.accounts.len();
        let reset_g = !matches!(key.code, KeyCode::Char('g'));
        let outcome = match key.code {
            KeyCode::Char('h') | KeyCode::Esc | KeyCode::Backspace => AccountListOutcome::Back,
            KeyCode::Char('R') => AccountListOutcome::Dispatch(Action::LoadAccountList {
                id: self.for_id.clone(),
                kind: self.kind,
                max_id: None,
            }),
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                AccountListOutcome::Dispatch(Action::LoadAccountList {
                    id: self.for_id.clone(),
                    kind: self.kind,
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
                AccountListOutcome::Continue
            }
            KeyCode::Char('g') => {
                if self.last_g {
                    self.selected = 0;
                    self.scroll = 0;
                }
                self.last_g = !self.last_g;
                AccountListOutcome::Continue
            }
            KeyCode::Char('G') => {
                if len > 0 {
                    self.selected = len - 1;
                }
                self.check_load_more(len)
            }
            KeyCode::Char('l') | KeyCode::Enter => match self.accounts.get(self.selected) {
                Some(a) => AccountListOutcome::OpenProfile(a.clone()),
                None => AccountListOutcome::Continue,
            },
            _ => AccountListOutcome::Continue,
        };
        if reset_g {
            self.last_g = false;
        }
        outcome
    }

    fn check_load_more(&mut self, len: usize) -> AccountListOutcome {
        if !self.load_more_pending && len > 0 && self.selected + LOAD_MORE_TRIGGER >= len {
            self.load_more_pending = true;
            let max_id = self.accounts.last().map(|a| a.id.0.clone());
            AccountListOutcome::Dispatch(Action::LoadAccountList {
                id: self.for_id.clone(),
                kind: self.kind,
                max_id,
            })
        } else {
            AccountListOutcome::Continue
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        const H_PAD: u16 = 1;
        let inner_width = area.width.saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line<'static>> = Vec::new();

        // Two empty rows between header and the first account give the
        // page room to breathe; matches the rhythm of detail page.
        lines.push(Line::default());

        if self.loading && self.accounts.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("loading {}…", self.kind.label()),
                theme.tertiary(),
            )));
        } else if self.accounts.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("no {}", self.kind.label()),
                theme.tertiary(),
            )));
        }

        let mut sel_range: (u16, u16) = (0, 0);
        for (i, acc) in self.accounts.iter().enumerate() {
            if i > 0 {
                lines.push(Line::default());
            }
            let card = render_account_card(acc, theme, i == self.selected, inner_width);
            let start = lines.len() as u16;
            let len = card.len() as u16;
            lines.extend(card);
            if i == self.selected {
                sel_range = (start, start + len);
            }
        }

        let height = area.height;
        let (sel_start, sel_end) = sel_range;
        if !self.accounts.is_empty() {
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
    }

    /// Top bar mirrors detail / profile modals: `← <verb> @<handle>
    /// · h / Esc to go back`.
    pub fn render_modal_header(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let line = Line::from(vec![
            Span::styled("← ", theme.tertiary()),
            Span::styled(
                format!("{} of {}", self.kind.label(), self.for_handle),
                theme.secondary(),
            ),
            Span::styled("   ·   ", theme.tertiary()),
            Span::styled("h / Esc to go back", theme.tertiary()),
        ]);
        frame.render_widget(Paragraph::new(line).style(theme.primary()), area);
    }
}

/// One account row: header line (display name + handle [+ relationship
/// chips later]) and a one-line dim bio excerpt (first wrapped line of
/// the user's note). Already-wrapped to `inner_width`. Selected row
/// gets the standard 2-col cursor gutter.
fn render_account_card(
    acc: &Account,
    theme: &Theme,
    selected: bool,
    inner_width: u16,
) -> Vec<Line<'static>> {
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
    if acc.bot {
        header.push(Span::styled(
            "  [bot]",
            Style::default().fg(theme.fg_tertiary).bg(theme.bg),
        ));
    }
    if acc.locked {
        header.push(Span::styled("  ", theme.tertiary()));
        header.push(Span::styled(
            crate::icons::LOCK.to_string(),
            theme.secondary(),
        ));
    }
    let mut logical = vec![Line::from(header)];

    if !acc.note.is_empty() {
        let bio = html::render(&acc.note, theme);
        let wrap_w = inner_width.saturating_sub(2);
        let wrapped = wrap::wrap_lines(&bio, wrap_w);
        // Take only the first non-empty wrapped line as a one-liner
        // excerpt — keeps the list scannable.
        if let Some(mut first) = wrapped.into_iter().find(|l| !l.spans.is_empty()) {
            for span in &mut first.spans {
                if span.style.fg.is_none() {
                    span.style = span.style.patch(theme.tertiary());
                }
                span.style = span.style.add_modifier(Modifier::ITALIC);
            }
            logical.push(first);
        }
    }

    let wrap_w = inner_width.saturating_sub(2);
    let wrapped = wrap::wrap_lines(&logical, wrap_w);
    wrapped
        .into_iter()
        .map(|l| with_gutter(l, theme, selected))
        .collect()
}

fn with_gutter(line: Line<'static>, theme: &Theme, selected: bool) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    if selected {
        spans.push(Span::styled(
            format!("{} ", crate::icons::CURSOR),
            theme.cursor(),
        ));
    } else {
        spans.push(Span::raw("  "));
    }
    spans.extend(line.spans);
    Line::from(spans)
}
