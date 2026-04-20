//! TUI main loop and screen router.
//!
//! Owns the terminal, the state-task [`Handle`], and per-screen state.
//! Runs three concurrent streams inside `tokio::select!`:
//!
//! - keyboard events (`crossterm::event::EventStream`)
//! - state-task events ([`crate::state::Event`])
//! - a 30 s background tick (refreshes relative timestamps)

use std::collections::HashMap;
use std::io;

use anyhow::{Context, Result};
use crossterm::event::{
    Event as CEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tokio::time::{Duration, Interval, MissedTickBehavior, interval};
use tracing::debug;

use crate::api::MastodonClient;
use crate::api::models::{Account, Notification, Status, StatusId};
use crate::api::music::MusicCache;
use crate::config::{self, AccountRef, Config};
use crate::state::{
    self, Action, ApiHealth, Event, Handle, StreamMode, StreamState, TimelineKind, ToastLevel,
    event::{AccountListKind, FailedAction},
};
use crate::ui::Theme;
use crate::ui::images::ImageCache;
use crate::ui::screens::account_list::{AccountListOutcome, AccountListScreen};
use crate::ui::screens::account_switcher::{AccountSwitcherScreen, SwitcherOutcome};
use crate::ui::screens::compose::{
    ComposeOutcome, ComposeState, DEFAULT_MAX_CHARS, quote_context_from, reply_context_from,
};
use crate::ui::screens::notifications::{NotifOutcome, NotificationsScreen};
use crate::ui::screens::profile::{ProfileOutcome, ProfileScreen};
use crate::ui::screens::status_detail::{DetailOutcome, DetailState};
use crate::ui::screens::timeline::TimelineScreen;

/// Size of the in-memory toast buffer. Additional toasts bump older ones.
const TOAST_LIMIT: usize = 3;
/// How long a toast stays on screen, in seconds.
const TOAST_TTL_SECS: u64 = 4;

type Term = Terminal<CrosstermBackend<io::Stdout>>;

/// Run the TUI until the user quits or the state task dies. Takes
/// ownership of the API client and the config so it can derive the
/// theme.
pub async fn run(client: MastodonClient, cfg: Config) -> Result<()> {
    install_panic_hook();
    let mut term = enter_terminal().context("failed to enter raw mode")?;

    // Spawn the state task.
    let mut handle = state::spawn(client);
    // Kick off the initial home timeline load + set the live-update
    // mode from config before the UI starts receiving events, so the
    // status-bar dot reflects the right mode from the first frame.
    let initial_mode = cfg.ui.stream_mode;
    let _ = handle
        .actions
        .send(Action::SetStreamMode(initial_mode))
        .await;
    let _ = handle
        .actions
        .send(Action::LoadTimeline(TimelineKind::Home))
        .await;

    let theme = Theme::by_name(&cfg.theme.name);
    let nerd_font = cfg.ui.nerd_font;
    let mut app = App::new(theme, nerd_font, initial_mode, cfg);

    let outcome = Box::pin(main_loop(&mut term, &mut app, &mut handle)).await;
    leave_terminal(&mut term);
    handle.shutdown();
    outcome
}

async fn main_loop(term: &mut Term, app: &mut App, handle: &mut Handle) -> Result<()> {
    let mut keys = EventStream::new();
    let mut tick = new_ticker();

    loop {
        term.draw(|frame| app.render(frame))?;

        tokio::select! {
            Some(Ok(event)) = keys.next() => {
                if let CEvent::Key(k) = event
                    && matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && !Box::pin(app.handle_key(k, &handle.actions)).await
                {
                    return Ok(());
                }
            }
            Some(ev) = handle.events.recv() => {
                app.handle_event(ev);
            }
            _ = tick.tick() => {
                app.on_tick();
            }
            else => return Ok(()),
        }
    }
}

fn new_ticker() -> Interval {
    let mut t = interval(Duration::from_secs(30));
    t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    t
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct App {
    theme: Theme,
    nerd_font: bool,
    active: TimelineKind,
    screens: HashMap<TimelineKind, TimelineScreen>,
    timelines: HashMap<TimelineKind, Vec<Status>>,
    /// Notifications live separately because they're a different model.
    /// Same lifecycle as a timeline: replace on Refresh, append on
    /// LoadMore.
    notifications: Vec<Notification>,
    notifications_screen: NotificationsScreen,
    stream: StreamState,
    /// User-selected live-update mode. Drives the status-bar dot
    /// *glyph* (● streaming / … polling / · off); color is still
    /// `api_health`. Cycled with the `S` key.
    stream_mode: StreamMode,
    /// REST health indicator. Broadcast from the state task whenever
    /// the most recent API response flips category. Rendered as a tint
    /// on the status-bar dot + a short suffix label when not Healthy.
    api_health: ApiHealth,
    toasts: Vec<Toast>,
    show_help: bool,
    mode: Mode,
    /// Server-reported character cap for compose. Defaults to
    /// [`DEFAULT_MAX_CHARS`] until `Event::InstanceLoaded` arrives.
    max_chars: usize,
    /// Generic mode back-stack. Sub-page entries (`l` / `r` / `c` /
    /// `u` / `5` etc.) push the current mode here so `h` / `Esc` /
    /// `Backspace` can return to it. Tab keys 1-4 clear the stack
    /// (they're a hard reset to the top level). Pop returns the most
    /// recent prior mode; if empty, [`Mode::Timeline`] is the default.
    back_stack: Vec<Mode>,
    /// Logged-in user's account. Populated by [`Event::CredentialsLoaded`].
    me: Option<Account>,
    /// Inline-image cache + downloader. Initialized at App::new with a
    /// terminal-protocol probe; if the host terminal can't render
    /// images, this is functionally a no-op but the field is always
    /// present so call sites don't need to gate on `Option`.
    images: ImageCache,
    /// Apple Music enrichment cache — does `music.apple.com` URL
    /// lookups against the free iTunes API and hands back typed
    /// `AppleMusicMeta` for status cards to render as compact text
    /// (density 1) or full cover-art cards (density 2).
    music: MusicCache,
    /// Owned copy of the user's config. Kept live-editable so the
    /// account switcher can persist a new `default_account` the
    /// instant the user confirms, without round-tripping through the
    /// state task.
    cfg: Config,
    /// Cold-start splash. True until the first home timeline lands or
    /// the API reports a non-healthy state; while true `render` shows
    /// only a centered wordmark and `handle_key` swallows everything
    /// except Ctrl+C.
    splash: bool,
}

enum Mode {
    Timeline,
    Compose(ComposeState),
    /// "Discard this draft?" confirm when user hits Esc with non-empty body.
    ComposeConfirmDiscard(ComposeState),
    /// Reading the focal post + its reply chain.
    StatusDetail(DetailState),
    /// Profile page — covers tab 5 (self) and modal `u` view (others).
    Profile(ProfileScreen),
    /// Modal followers / following list entered from a profile.
    AccountList(AccountListScreen),
    /// Account switcher modal entered with `A`.
    AccountSwitcher(AccountSwitcherScreen),
    /// "Delete this post? Enter · Esc" confirm. Only reachable after
    /// an ownership check (user pressed `d` on their own post).
    DeleteConfirm(StatusId),
}

struct Toast {
    level: ToastLevel,
    message: String,
    ticks_remaining: u8,
}

impl App {
    fn new(theme: Theme, nerd_font: bool, stream_mode: StreamMode, cfg: Config) -> Self {
        let mut screens = HashMap::new();
        for k in [
            TimelineKind::Home,
            TimelineKind::Local,
            TimelineKind::Federated,
            TimelineKind::Notifications,
        ] {
            screens.insert(k, TimelineScreen::new(k));
        }
        let images = ImageCache::new();
        Self {
            theme,
            nerd_font,
            active: TimelineKind::Home,
            screens,
            timelines: HashMap::new(),
            notifications: Vec::new(),
            notifications_screen: NotificationsScreen::new(),
            stream: StreamState::Disconnected,
            stream_mode,
            api_health: ApiHealth::Healthy,
            toasts: Vec::new(),
            show_help: false,
            mode: Mode::Timeline,
            max_chars: DEFAULT_MAX_CHARS,
            back_stack: Vec::new(),
            me: None,
            images,
            music: MusicCache::new(),
            cfg,
            splash: true,
        }
    }

    /// Push the current mode onto the back-stack and replace it with
    /// `new`. Use for navigation entries that should be poppable via
    /// `h` / `Esc` (status detail, profile, compose).
    fn push_mode(&mut self, new: Mode) {
        let prev = std::mem::replace(&mut self.mode, new);
        self.back_stack.push(prev);
    }

    /// Pop the back-stack, restoring whatever mode the user was in
    /// before they navigated to a sub-page. Defaults to
    /// [`Mode::Timeline`] when the stack is empty.
    fn pop_mode(&mut self) {
        self.mode = self.back_stack.pop().unwrap_or(Mode::Timeline);
    }

    /// Common exit point from any compose flow. Pops the back-stack
    /// to whatever the user was looking at before they opened compose.
    /// `kick_reload` only matters when the popped mode is a status
    /// detail page — in that case we fire a fresh `OpenStatus` so the
    /// just-posted reply appears under the focal status.
    async fn exit_compose(&mut self, tx: &tokio::sync::mpsc::Sender<Action>, kick_reload: bool) {
        let prev = self.back_stack.pop().unwrap_or(Mode::Timeline);
        if kick_reload && let Mode::StatusDetail(d) = &prev {
            let _ = tx.send(Action::OpenStatus(d.focal_id().clone())).await;
        }
        self.mode = prev;
    }

    /// Handle a key press. Returns `false` when the app should quit.
    async fn handle_key(&mut self, key: KeyEvent, tx: &tokio::sync::mpsc::Sender<Action>) -> bool {
        // Ctrl-C always quits (belt-and-suspenders: raw mode swallows
        // SIGINT, so we must handle it at the key layer).
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }

        // Splash swallows the rest — nothing to navigate until the
        // first timeline or health signal lands.
        if self.splash {
            return true;
        }

        // Any key dismisses the help overlay.
        if self.show_help {
            self.show_help = false;
            return true;
        }

        // Density toggle works everywhere — `D` flips inter-post
        // blank-line count between 1 and 2 so the user can A/B
        // information density vs breathing room without restarting.
        // Filtered out of compose mode below so it doesn't eat a
        // literal `D` key in the body editor.
        if key.code == KeyCode::Char('D')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !matches!(self.mode, Mode::Compose(_) | Mode::ComposeConfirmDiscard(_))
        {
            let cur = crate::ui::widgets::status_card::inter_post_blank_lines();
            crate::ui::widgets::status_card::set_inter_post_blank_lines(if cur >= 2 {
                1
            } else {
                2
            });
            return true;
        }

        // Live-update mode cycle: streaming → polling → off → streaming.
        // Like `D`, the `S` key works everywhere *except* compose so it
        // doesn't eat a literal S in the body.
        if key.code == KeyCode::Char('S')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !matches!(self.mode, Mode::Compose(_) | Mode::ComposeConfirmDiscard(_))
        {
            let next = self.stream_mode.cycle();
            self.stream_mode = next;
            let _ = tx.send(Action::SetStreamMode(next)).await;
            return true;
        }

        // `Q` opens the quoted post of the currently selected status.
        // Dedicated key (vs. reusing `l` / `Enter`) keeps "open outer
        // post" and "open quoted post" unambiguous. Works wherever a
        // selection exists except compose.
        if key.code == KeyCode::Char('Q')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !matches!(self.mode, Mode::Compose(_) | Mode::ComposeConfirmDiscard(_))
        {
            if let Some(quoted) = self.selected_quoted_status() {
                let detail = DetailState::new(quoted);
                let id = detail.focal_id().clone();
                self.push_mode(Mode::StatusDetail(detail));
                let _ = tx.send(Action::OpenStatus(id)).await;
            }
            return true;
        }

        // `A` opens the account switcher. A second `A` or `Esc` closes
        // it. Unlike the tab keys, this is a *modal* — we push the
        // current mode onto the back-stack so exiting without picking
        // leaves the user where they were.
        if key.code == KeyCode::Char('A')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !matches!(
                self.mode,
                Mode::Compose(_) | Mode::ComposeConfirmDiscard(_) | Mode::AccountSwitcher(_)
            )
        {
            let accounts = self.cfg.accounts.clone();
            let current = self.cfg.default_account.clone();
            self.push_mode(Mode::AccountSwitcher(AccountSwitcherScreen::new(
                accounts, current,
            )));
            return true;
        }

        match std::mem::replace(&mut self.mode, Mode::Timeline) {
            Mode::Compose(mut state) => {
                match state.handle_key(key) {
                    ComposeOutcome::Continue => {
                        self.mode = Mode::Compose(state);
                    }
                    ComposeOutcome::Cancel => {
                        if state.is_body_empty() {
                            // nothing to lose — bounce straight back to
                            // wherever we came from.
                            self.exit_compose(tx, false).await;
                        } else {
                            self.mode = Mode::ComposeConfirmDiscard(state);
                        }
                    }
                    ComposeOutcome::Submit(draft) => {
                        let was_reply = draft.in_reply_to_id.is_some();
                        let _ = tx
                            .send(Action::Compose {
                                text: draft.text,
                                in_reply_to_id: draft.in_reply_to_id,
                                quote_id: draft.quote_id,
                                content_warning: draft.content_warning,
                                sensitive: draft.sensitive,
                                visibility: draft.visibility,
                            })
                            .await;
                        // If we came from a detail page and this was a
                        // reply, kick a context reload so the new post
                        // shows up under the focal.
                        let kick = was_reply
                            && matches!(self.back_stack.last(), Some(Mode::StatusDetail(_)));
                        self.exit_compose(tx, kick).await;
                        self.push_toast(ToastLevel::Info, "posting…".into());
                    }
                }
                return true;
            }
            Mode::ComposeConfirmDiscard(state) => {
                match key.code {
                    KeyCode::Char('y' | 'Y') => {
                        // discard — drop state, back to wherever.
                        self.exit_compose(tx, false).await;
                    }
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                        // keep editing.
                        self.mode = Mode::Compose(state);
                    }
                    _ => {
                        self.mode = Mode::ComposeConfirmDiscard(state);
                    }
                }
                return true;
            }
            Mode::DeleteConfirm(id) => {
                match key.code {
                    KeyCode::Enter => {
                        let _ = tx.send(Action::DeleteStatus(id)).await;
                        self.pop_mode();
                    }
                    KeyCode::Esc | KeyCode::Char('h') | KeyCode::Backspace => {
                        self.pop_mode();
                    }
                    _ => {
                        self.mode = Mode::DeleteConfirm(id);
                    }
                }
                return true;
            }
            Mode::Profile(mut state) => {
                // App-level intercept: tab keys + `u` (re-open another
                // profile) before delegating.
                match key.code {
                    KeyCode::Char('5') if state.is_self => {
                        // Already on self-profile — keep state, no-op.
                        self.mode = Mode::Profile(state);
                        return true;
                    }
                    KeyCode::Char('1' | '2' | '3' | '4' | '5') => {
                        // Restore mode to Timeline so the tab handlers
                        // below see the right starting state, then let
                        // the timeline-mode key table run.
                        self.mode = Mode::Timeline;
                        // fall through to timeline keys
                    }
                    KeyCode::Char('u') if state.selected_target().is_some() => {
                        let t = state.selected_target().unwrap().clone();
                        let acc = t.account.clone();
                        let id = acc.id.clone();
                        // Re-stash current profile so `h` returns to it.
                        self.back_stack.push(Mode::Profile(state));
                        self.mode = Mode::Profile(ProfileScreen::new(acc, false));
                        let _ = tx
                            .send(Action::LoadProfile {
                                id: id.clone(),
                                max_id: None,
                            })
                            .await;
                        let _ = tx.send(Action::LoadRelationship(id)).await;
                        return true;
                    }
                    KeyCode::Char('o' | 'O') => {
                        let kind = if matches!(key.code, KeyCode::Char('O')) {
                            AccountListKind::Following
                        } else {
                            AccountListKind::Followers
                        };
                        let id = state.account_id.clone();
                        let handle = state
                            .account
                            .as_ref()
                            .map_or_else(|| format!("@{id}"), |a| format!("@{}", a.acct));
                        self.back_stack.push(Mode::Profile(state));
                        self.mode =
                            Mode::AccountList(AccountListScreen::new(id.clone(), handle, kind));
                        let _ = tx
                            .send(Action::LoadAccountList {
                                id,
                                kind,
                                max_id: None,
                            })
                            .await;
                        return true;
                    }
                    KeyCode::Char('q')
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && state.selected_target().is_some() =>
                    {
                        let target = state.selected_target().unwrap().clone();
                        let quote = quote_context_from(&target, 80);
                        self.back_stack.push(Mode::Profile(state));
                        self.mode = Mode::Compose(ComposeState::quote(quote, self.max_chars));
                        return true;
                    }
                    KeyCode::Char('d') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let own_id = state.selected_target().and_then(|t| {
                            let me_id = self.me.as_ref().map(|a| a.id.clone())?;
                            (t.account.id == me_id).then(|| t.id.clone())
                        });
                        if let Some(id) = own_id {
                            self.back_stack.push(Mode::Profile(state));
                            self.mode = Mode::DeleteConfirm(id);
                        } else {
                            self.mode = Mode::Profile(state);
                        }
                        return true;
                    }
                    _ => {
                        match state.handle_key(key) {
                            ProfileOutcome::Continue => {
                                self.mode = Mode::Profile(state);
                            }
                            ProfileOutcome::Back => {
                                self.pop_mode();
                            }
                            ProfileOutcome::Dispatch(a) => {
                                let _ = tx.send(a).await;
                                self.mode = Mode::Profile(state);
                            }
                            ProfileOutcome::OpenStatus(s) => {
                                let detail = DetailState::new(s);
                                let id = detail.focal_id().clone();
                                // Push the profile so `h` from detail
                                // returns here.
                                self.back_stack.push(Mode::Profile(state));
                                self.mode = Mode::StatusDetail(detail);
                                let _ = tx.send(Action::OpenStatus(id)).await;
                            }
                        }
                        return true;
                    }
                }
                // Fall through to timeline keys (tab switch).
            }
            Mode::AccountList(mut state) => {
                if let KeyCode::Char('1' | '2' | '3' | '4' | '5') = key.code {
                    // Tab switch — fall through to timeline keys.
                    self.mode = Mode::Timeline;
                } else {
                    match state.handle_key(key) {
                        AccountListOutcome::Continue => {
                            self.mode = Mode::AccountList(state);
                        }
                        AccountListOutcome::Back => {
                            self.pop_mode();
                        }
                        AccountListOutcome::Dispatch(a) => {
                            let _ = tx.send(a).await;
                            self.mode = Mode::AccountList(state);
                        }
                        AccountListOutcome::OpenProfile(acc) => {
                            let id = acc.id.clone();
                            self.back_stack.push(Mode::AccountList(state));
                            self.mode = Mode::Profile(ProfileScreen::new(acc, false));
                            let _ = tx
                                .send(Action::LoadProfile {
                                    id: id.clone(),
                                    max_id: None,
                                })
                                .await;
                            let _ = tx.send(Action::LoadRelationship(id)).await;
                        }
                    }
                    return true;
                }
                // Fall through for tab keys.
            }
            Mode::AccountSwitcher(mut state) => {
                match state.handle_key(key) {
                    SwitcherOutcome::Continue => {
                        self.mode = Mode::AccountSwitcher(state);
                    }
                    SwitcherOutcome::Back => {
                        self.pop_mode();
                    }
                    SwitcherOutcome::Pick(acc) => {
                        if Some(acc.handle.as_str()) == self.cfg.default_account.as_deref() {
                            // Picking the already-current account —
                            // just close, no churn.
                            self.pop_mode();
                        } else {
                            match self.begin_account_switch(&acc, tx).await {
                                Ok(()) => {
                                    // Clear back-stack; after a switch
                                    // the previous mode's cached state
                                    // refers to the old session's data.
                                    self.back_stack.clear();
                                    self.mode = Mode::Timeline;
                                }
                                Err(msg) => {
                                    self.push_toast(ToastLevel::Error, msg);
                                    self.mode = Mode::AccountSwitcher(state);
                                }
                            }
                        }
                    }
                }
                return true;
            }
            Mode::StatusDetail(mut state) => {
                // App-level intercepts that need to switch *modes* go
                // first; everything else flows into DetailState.
                match key.code {
                    KeyCode::Char('r')
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && state.selected_target().is_some() =>
                    {
                        let target = state.selected_target().unwrap().clone();
                        let reply = reply_context_from(&target, 80);
                        let vis = api_to_state_vis(target.visibility);
                        // Stash the detail; exit_compose() pops it after
                        // submit / cancel / discard.
                        self.back_stack.push(Mode::StatusDetail(state));
                        self.mode = Mode::Compose(ComposeState::reply(reply, vis, self.max_chars));
                        return true;
                    }
                    KeyCode::Char('q')
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && state.selected_target().is_some() =>
                    {
                        let target = state.selected_target().unwrap().clone();
                        let quote = quote_context_from(&target, 80);
                        self.back_stack.push(Mode::StatusDetail(state));
                        self.mode = Mode::Compose(ComposeState::quote(quote, self.max_chars));
                        return true;
                    }
                    KeyCode::Char('d') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let own_id = state.selected_target().and_then(|t| {
                            let me_id = self.me.as_ref().map(|a| a.id.clone())?;
                            (t.account.id == me_id).then(|| t.id.clone())
                        });
                        if let Some(id) = own_id {
                            self.back_stack.push(Mode::StatusDetail(state));
                            self.mode = Mode::DeleteConfirm(id);
                        } else {
                            self.mode = Mode::StatusDetail(state);
                        }
                        return true;
                    }
                    KeyCode::Char('c') => {
                        self.back_stack.push(Mode::StatusDetail(state));
                        self.mode = Mode::Compose(ComposeState::blank(self.max_chars));
                        return true;
                    }
                    KeyCode::Char('u') if state.selected_target().is_some() => {
                        let t = state.selected_target().unwrap().clone();
                        let acc = t.account.clone();
                        let id = acc.id.clone();
                        self.back_stack.push(Mode::StatusDetail(state));
                        self.mode = Mode::Profile(ProfileScreen::new(acc, false));
                        let _ = tx
                            .send(Action::LoadProfile {
                                id: id.clone(),
                                max_id: None,
                            })
                            .await;
                        let _ = tx.send(Action::LoadRelationship(id)).await;
                        return true;
                    }
                    _ => {}
                }
                match state.handle_key(key) {
                    DetailOutcome::Continue => {
                        self.mode = Mode::StatusDetail(state);
                    }
                    DetailOutcome::Back => {
                        self.pop_mode();
                    }
                    DetailOutcome::Dispatch(action) => {
                        let _ = tx.send(action).await;
                        self.mode = Mode::StatusDetail(state);
                    }
                }
                return true;
            }
            Mode::Timeline => {
                // fall through into the timeline key table below
            }
        }

        // Timeline mode keys.
        match key.code {
            KeyCode::Esc => return false,
            KeyCode::Char('?') => {
                self.show_help = true;
            }
            KeyCode::Char('1') => self.switch_to(TimelineKind::Home, tx).await,
            KeyCode::Char('2') => self.switch_to(TimelineKind::Local, tx).await,
            KeyCode::Char('3') => self.switch_to(TimelineKind::Federated, tx).await,
            KeyCode::Char('4') => self.switch_to(TimelineKind::Notifications, tx).await,
            KeyCode::Char('5') => self.open_self_profile(tx).await,
            KeyCode::Char('u') => {
                if let Some(target) = self.selected_target_status() {
                    let acc = target.account.clone();
                    let id = acc.id.clone();
                    self.push_mode(Mode::Profile(ProfileScreen::new(acc, false)));
                    let _ = tx
                        .send(Action::LoadProfile {
                            id: id.clone(),
                            max_id: None,
                        })
                        .await;
                    let _ = tx.send(Action::LoadRelationship(id)).await;
                }
            }
            KeyCode::Char('f') => {
                if let Some(action) = self.toggle_favourite_optimistic() {
                    let _ = tx.send(action).await;
                }
            }
            KeyCode::Char('b') => {
                if let Some(action) = self.toggle_reblog_optimistic() {
                    let _ = tx.send(action).await;
                }
            }
            KeyCode::Char('B') => {
                if let Some(action) = self.force_unreblog_optimistic() {
                    let _ = tx.send(action).await;
                }
            }
            KeyCode::Char('c') => {
                self.push_mode(Mode::Compose(ComposeState::blank(self.max_chars)));
            }
            KeyCode::Char('r') => {
                if let Some(state) = self.compose_reply_for_selection() {
                    self.push_mode(Mode::Compose(state));
                }
            }
            KeyCode::Char('q') => {
                if let Some(state) = self.compose_quote_for_selection() {
                    self.push_mode(Mode::Compose(state));
                }
            }
            KeyCode::Char('d') => {
                if let Some(id) = self.selected_own_status_id() {
                    self.push_mode(Mode::DeleteConfirm(id));
                }
            }
            KeyCode::Char('l') | KeyCode::Enter => {
                if let Some(detail) = self.open_detail_for_selection() {
                    let id = detail.focal_id().clone();
                    self.push_mode(Mode::StatusDetail(detail));
                    let _ = tx.send(Action::OpenStatus(id)).await;
                }
            }
            _ => {
                // Delegate to the active screen.
                let kind = self.active;
                if kind == TimelineKind::Notifications {
                    let outcome = self
                        .notifications_screen
                        .handle_key(key, &self.notifications);
                    match outcome {
                        NotifOutcome::Continue => {}
                        NotifOutcome::Dispatch(a) => {
                            let _ = tx.send(a).await;
                        }
                        NotifOutcome::OpenStatus(status) => {
                            let detail = DetailState::new(status);
                            let id = detail.focal_id().clone();
                            self.push_mode(Mode::StatusDetail(detail));
                            let _ = tx.send(Action::OpenStatus(id)).await;
                        }
                    }
                } else {
                    let empty: Vec<Status> = Vec::new();
                    let items = self.timelines.get(&kind).unwrap_or(&empty);
                    let screen = self.screens.get_mut(&kind).expect("screen initialized");
                    if let Some(action) = screen.handle_key(key, items) {
                        let _ = tx.send(action).await;
                    }
                }
            }
        }
        true
    }

    /// Build a ComposeState pre-filled to reply to the currently
    /// selected status. `r` on a boost replies to the original.
    fn compose_reply_for_selection(&self) -> Option<ComposeState> {
        let target = self.selected_target_status()?;
        let reply = reply_context_from(target, 80);
        let vis = api_to_state_vis(target.visibility);
        Some(ComposeState::reply(reply, vis, self.max_chars))
    }

    /// Build a ComposeState pre-filled to quote the currently
    /// selected status (Mastodon 4.5 native quote). `q` on a boost
    /// quotes the inner post, matching the convention used by
    /// favourite / reblog.
    fn compose_quote_for_selection(&self) -> Option<ComposeState> {
        let target = self.selected_target_status()?;
        let quote = quote_context_from(target, 80);
        Some(ComposeState::quote(quote, self.max_chars))
    }

    /// Build a fresh DetailState seeded with the focal post (the inner
    /// status if the selection points at a boost). Returns `None` if
    /// nothing is selected. The `OpenStatus` action is fired by the
    /// caller so the focal id is known here.
    fn open_detail_for_selection(&self) -> Option<DetailState> {
        let target = self.selected_target_status()?;
        Some(DetailState::new(target.clone()))
    }

    /// Read-only sibling of `selected_target_status_mut`.
    fn selected_target_status(&self) -> Option<&Status> {
        let kind = self.active;
        let idx = self.screens.get(&kind)?.selected;
        let outer = self.timelines.get(&kind)?.get(idx)?;
        Some(outer.reblog.as_deref().unwrap_or(outer))
    }

    /// Status id to target for a deletion *if* it belongs to the
    /// signed-in user. Same mode coverage as
    /// [`Self::selected_quoted_status`]. `None` when there's no
    /// selection, no `me`, or the author doesn't match.
    fn selected_own_status_id(&self) -> Option<StatusId> {
        let me_id = self.me.as_ref().map(|a| a.id.clone())?;
        let target: &Status = match &self.mode {
            Mode::Timeline => self.selected_target_status()?,
            Mode::StatusDetail(d) => d.selected_target()?,
            Mode::Profile(p) => p.selected_target()?,
            _ => return None,
        };
        if target.account.id == me_id {
            Some(target.id.clone())
        } else {
            None
        }
    }

    /// If the currently selected post carries a quote payload with a
    /// resolved `quoted_status`, return an owned clone of that quoted
    /// status. Looks at whichever mode the user is in: timeline,
    /// status detail, profile. `None` when there's no selection, no
    /// quote, or the quote's state is not `accepted` (no payload).
    fn selected_quoted_status(&self) -> Option<Status> {
        let target: &Status = match &self.mode {
            Mode::Timeline => self.selected_target_status()?,
            Mode::StatusDetail(d) => d.selected_target()?,
            Mode::Profile(p) => p.selected_target()?,
            _ => return None,
        };
        target.quote.as_ref()?.quoted_status.as_deref().cloned()
    }

    /// Flip the favourite flag on the currently-selected status' inner
    /// post (following Mastodon convention: favouriting a boost
    /// favourites the original). Returns the API action to dispatch;
    /// `None` if there's no selection.
    fn toggle_favourite_optimistic(&mut self) -> Option<Action> {
        let target = self.selected_target_status_mut()?;
        let currently = target.favourited.unwrap_or(false);
        target.favourited = Some(!currently);
        target.favourites_count = if currently {
            target.favourites_count.saturating_sub(1)
        } else {
            target.favourites_count.saturating_add(1)
        };
        let id = target.id.clone();
        Some(if currently {
            Action::Unfavourite(id)
        } else {
            Action::Favourite(id)
        })
    }

    fn toggle_reblog_optimistic(&mut self) -> Option<Action> {
        let target = self.selected_target_status_mut()?;
        let currently = target.reblogged.unwrap_or(false);
        target.reblogged = Some(!currently);
        target.reblogs_count = if currently {
            target.reblogs_count.saturating_sub(1)
        } else {
            target.reblogs_count.saturating_add(1)
        };
        let id = target.id.clone();
        Some(if currently {
            Action::Unreblog(id)
        } else {
            Action::Reblog(id)
        })
    }

    fn force_unreblog_optimistic(&mut self) -> Option<Action> {
        let target = self.selected_target_status_mut()?;
        if !target.reblogged.unwrap_or(false) {
            return None;
        }
        target.reblogged = Some(false);
        target.reblogs_count = target.reblogs_count.saturating_sub(1);
        let id = target.id.clone();
        Some(Action::Unreblog(id))
    }

    /// Mut-borrow the inner status the current selection points at.
    /// For a reblog, that's `outer.reblog`; otherwise just the outer
    /// status itself.
    fn selected_target_status_mut(&mut self) -> Option<&mut Status> {
        let kind = self.active;
        let idx = self.screens.get(&kind)?.selected;
        let list = self.timelines.get_mut(&kind)?;
        let outer = list.get_mut(idx)?;
        if outer.reblog.is_some() {
            outer.reblog.as_deref_mut()
        } else {
            Some(outer)
        }
    }

    async fn switch_to(&mut self, kind: TimelineKind, tx: &tokio::sync::mpsc::Sender<Action>) {
        // Tabs 1-4 are a *hard reset*: drop any open sub-page (detail /
        // profile / compose) and clear the back-stack. Pressing 1 from
        // deep inside a thread shouldn't leave breadcrumbs.
        self.back_stack.clear();
        self.mode = Mode::Timeline;
        if self.active == kind {
            return;
        }
        self.active = kind;
        if !self.timelines.contains_key(&kind) {
            let _ = tx.send(Action::LoadTimeline(kind)).await;
        }
    }

    /// Enter Mode::Profile with the logged-in user's profile. Tab-5
    /// behaves like tabs 1-4: a *hard reset*. Any open sub-page is
    /// dropped and the back-stack cleared, so `h` from self-profile
    /// returns to Home (not to whatever the user was accidentally on
    /// before pressing 5). That matches the usual tab-bar mental model
    /// — 5 is a top-level destination, not a navigation step.
    /// Persist the new default account, load its token from the
    /// keyring, and dispatch [`Action::SwitchAccount`]. Also wipes
    /// the local UI caches (timelines, notifications, pending
    /// profile state) so the incoming fresh data isn't polluted by
    /// the previous account's posts. Returns a human-readable error
    /// to show as a toast if any step fails; the caller is expected
    /// to keep the switcher open when that happens.
    async fn begin_account_switch(
        &mut self,
        acc: &AccountRef,
        tx: &tokio::sync::mpsc::Sender<Action>,
    ) -> Result<(), String> {
        let token =
            config::load_token(&acc.handle).map_err(|e| format!("keyring lookup failed · {e}"))?;

        // Persist the new default so the next `mastoot run` lands on
        // this account. A failed save is non-fatal for *this* session —
        // the switch below still proceeds.
        self.cfg.default_account = Some(acc.handle.clone());
        self.cfg.default_instance = Some(acc.instance.clone());
        if let Err(e) = self.cfg.save(None) {
            tracing::warn!(?e, "failed to persist account switch to config");
        }

        // Wipe every UI-side cache that held the old session's data.
        self.timelines.clear();
        self.notifications.clear();
        self.notifications_screen.reset();
        for screen in self.screens.values_mut() {
            screen.reset();
        }
        self.me = None;
        self.active = TimelineKind::Home;

        let _ = tx
            .send(Action::SwitchAccount {
                instance: acc.instance.clone(),
                handle: acc.handle.clone(),
                token,
            })
            .await;
        Ok(())
    }

    async fn open_self_profile(&mut self, tx: &tokio::sync::mpsc::Sender<Action>) {
        // Reuse an already-open self profile (avoid re-fetching every
        // time the user presses `5`).
        if let Mode::Profile(p) = &self.mode
            && p.is_self
        {
            return;
        }
        let me = self.me.clone();
        let Some(me) = me else {
            self.push_toast(ToastLevel::Warn, "credentials still loading…".into());
            return;
        };
        let id = me.id.clone();
        self.back_stack.clear();
        self.active = TimelineKind::Home;
        self.mode = Mode::Profile(ProfileScreen::new(me, true));
        let _ = tx.send(Action::LoadProfile { id, max_id: None }).await;
    }

    fn handle_event(&mut self, event: Event) {
        if self.splash {
            let dismiss = match &event {
                Event::TimelineUpdated {
                    kind: TimelineKind::Home,
                    ..
                } => true,
                Event::ApiHealthChanged(h) => *h != ApiHealth::Healthy,
                _ => false,
            };
            if dismiss {
                self.splash = false;
            }
        }
        match event {
            Event::TimelineUpdated {
                kind,
                statuses,
                appended,
            } => {
                let slot = self.timelines.entry(kind).or_default();
                if appended {
                    let known: std::collections::HashSet<_> =
                        slot.iter().map(|s| s.id.clone()).collect();
                    for s in statuses {
                        if !known.contains(&s.id) {
                            slot.push(s);
                        }
                    }
                } else {
                    *slot = statuses;
                }
                let len = slot.len();
                if let Some(screen) = self.screens.get_mut(&kind) {
                    screen.on_items_changed(len, appended);
                }
            }
            Event::StatusUpdated(status) => {
                // The update may apply to a status that lives inside a
                // boost (e.g. favouriting someone else's boosted post:
                // the action targets the inner id, so match both outer
                // and nested reblog ids).
                for list in self.timelines.values_mut() {
                    for slot in list.iter_mut() {
                        if slot.id == status.id {
                            *slot = status.clone();
                        } else if let Some(inner) = slot.reblog.as_deref_mut()
                            && inner.id == status.id
                        {
                            *inner = status.clone();
                        }
                    }
                }
                if let Mode::StatusDetail(state) = &mut self.mode {
                    state.on_status_updated(&status);
                }
                if let Mode::Profile(p) = &mut self.mode {
                    p.on_status_updated(&status);
                }
                for prev in &mut self.back_stack {
                    match prev {
                        Mode::StatusDetail(d) => d.on_status_updated(&status),
                        Mode::Profile(p) => p.on_status_updated(&status),
                        _ => {}
                    }
                }
            }
            Event::StatusContext {
                focal_id,
                ancestors,
                descendants,
            } => {
                if let Mode::StatusDetail(state) = &mut self.mode
                    && state.focal_id() == &focal_id
                {
                    state.on_context_loaded(ancestors, descendants);
                }
            }
            Event::InstanceLoaded { max_characters } => {
                self.max_chars = max_characters as usize;
            }
            Event::CredentialsLoaded(account) => {
                self.me = Some(account);
            }
            Event::ProfileLoaded {
                account,
                statuses,
                appended,
            } => {
                if let Mode::Profile(p) = &mut self.mode
                    && p.account_id == account.id
                {
                    p.on_loaded(account, statuses, appended);
                }
            }
            Event::RelationshipLoaded(rel) => {
                if let Mode::Profile(p) = &mut self.mode {
                    p.on_relationship_loaded(rel.clone());
                }
                for prev in &mut self.back_stack {
                    if let Mode::Profile(p) = prev {
                        p.on_relationship_loaded(rel.clone());
                    }
                }
            }
            Event::AccountListLoaded {
                for_id,
                kind,
                accounts,
                appended,
            } => {
                if let Mode::AccountList(state) = &mut self.mode
                    && state.for_id == for_id
                    && state.kind == kind
                {
                    state.on_loaded(accounts, appended);
                }
            }
            Event::RelationshipActionFailed {
                id,
                attempted_follow,
            } => {
                if let Mode::Profile(p) = &mut self.mode
                    && p.account_id == id
                {
                    p.revert_follow_action(attempted_follow);
                }
                for prev in &mut self.back_stack {
                    if let Mode::Profile(p) = prev
                        && p.account_id == id
                    {
                        p.revert_follow_action(attempted_follow);
                    }
                }
            }
            Event::StatusActionFailed { id, action } => {
                // Walk every cached status that *might* hold this id —
                // outer or inner reblog — across all timelines and the
                // open detail page, and reverse the optimistic flip.
                for list in self.timelines.values_mut() {
                    for slot in list.iter_mut() {
                        if slot.id == id {
                            apply_revert(slot, action);
                        } else if let Some(inner) = slot.reblog.as_deref_mut()
                            && inner.id == id
                        {
                            apply_revert(inner, action);
                        }
                    }
                }
                if let Mode::StatusDetail(state) = &mut self.mode {
                    state.revert_action(&id, action);
                }
                if let Mode::Profile(p) = &mut self.mode {
                    p.revert_action(&id, action);
                }
                // Sub-pages stashed in the back-stack also need patching
                // so the user doesn't see stale optimistic state when
                // they navigate back.
                for prev in &mut self.back_stack {
                    match prev {
                        Mode::StatusDetail(d) => d.revert_action(&id, action),
                        Mode::Profile(p) => p.revert_action(&id, action),
                        _ => {}
                    }
                }
            }
            Event::StatusDeleted(id) => {
                for list in self.timelines.values_mut() {
                    list.retain(|s| s.id != id);
                }
                // If we're staring at the deleted post as the focal
                // of a thread, bounce back — there's nothing useful
                // to see anymore. Works around any stale detail
                // state still sitting on the back stack.
                if let Mode::StatusDetail(d) = &self.mode
                    && d.focal_id() == &id
                {
                    self.pop_mode();
                }
            }
            Event::TimelineStatusAdded { kind, status } => {
                let slot = self.timelines.entry(kind).or_default();
                // Dedup — reconnects replay recent events, and the
                // hot-start fetch may have already pulled this id.
                if slot.iter().any(|s| s.id == status.id) {
                    return;
                }
                slot.insert(0, status);
                let new_len = slot.len();
                if let Some(screen) = self.screens.get_mut(&kind) {
                    screen.on_prepended(1, new_len);
                }
            }
            Event::NotificationsUpdated { items, appended } => {
                if appended {
                    let known: std::collections::HashSet<_> =
                        self.notifications.iter().map(|n| n.id.clone()).collect();
                    for n in items {
                        if !known.contains(&n.id) {
                            self.notifications.push(n);
                        }
                    }
                } else {
                    self.notifications = items;
                }
                self.notifications_screen
                    .on_items_changed(self.notifications.len(), appended);
            }
            Event::NotificationReceived(n) => {
                if self
                    .notifications
                    .iter()
                    .any(|existing| existing.id == n.id)
                {
                    return;
                }
                self.notifications.insert(0, n);
                let len = self.notifications.len();
                self.notifications_screen.on_prepended(1, len);
            }
            Event::Toast { level, message } => {
                self.push_toast(level, message);
            }
            Event::StreamState(s) => {
                self.stream = s;
            }
            Event::ApiHealthChanged(h) => {
                self.api_health = h;
            }
            Event::AccountSwitched { handle } => {
                // UI-side caches were already wiped in
                // `begin_account_switch`. This event is confirmation
                // from the state task — a sanity refresh in case the
                // switch also raced past an in-flight `StatusUpdated`
                // or similar from the outgoing session.
                self.timelines.clear();
                self.notifications.clear();
                self.back_stack.clear();
                self.active = TimelineKind::Home;
                for screen in self.screens.values_mut() {
                    screen.reset();
                }
                self.notifications_screen.reset();
                tracing::debug!(%handle, "account switched");
            }
        }
    }

    fn on_tick(&mut self) {
        // Decay toasts (~8 ticks ≈ 4 min; TTL is enforced at render time too).
        self.toasts.retain_mut(|t| {
            t.ticks_remaining = t.ticks_remaining.saturating_sub(1);
            t.ticks_remaining > 0
        });
    }

    fn push_toast(&mut self, level: ToastLevel, message: String) {
        self.toasts.push(Toast {
            level,
            message,
            ticks_remaining: (TOAST_TTL_SECS / 4 + 1) as u8,
        });
        while self.toasts.len() > TOAST_LIMIT {
            self.toasts.remove(0);
        }
    }

    fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        // Decode anything the image-download workers finished since the
        // last frame. Cheap when nothing arrived; never blocks.
        self.images.drain();
        self.music.drain();

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
                    self.nerd_font,
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
                    self.nerd_font,
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
            Mode::Timeline => {
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
            )
        {
            self.render_toasts(frame, size);
        }
    }

    fn render_splash(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
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
    fn render_detail_header(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let line = Line::from(vec![
            Span::styled("← ", self.theme.tertiary()),
            Span::styled("thread", self.theme.secondary()),
            Span::styled("   ·   ", self.theme.tertiary()),
            Span::styled("h / Esc to go back", self.theme.tertiary()),
        ]);
        let p = Paragraph::new(line).style(self.theme.primary());
        frame.render_widget(p, area);
    }

    fn render_discard_confirm(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
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
    fn render_delete_confirm(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
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

    fn render_tabs(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let labels = [
            (Some(TimelineKind::Home), "1 Home"),
            (Some(TimelineKind::Local), "2 Local"),
            (Some(TimelineKind::Federated), "3 Federated"),
            (Some(TimelineKind::Notifications), "4 Notifications"),
            // Profile is a Mode, not a timeline kind; matched via
            // `tab_5_active()` below.
            (None, "5 Profile"),
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
    fn tab_5_active(&self) -> bool {
        matches!(&self.mode, Mode::Profile(p) if p.is_self)
    }

    fn render_body(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        if self.active == TimelineKind::Notifications {
            self.notifications_screen.render(
                frame,
                area,
                &self.notifications,
                &self.theme,
                self.nerd_font,
            );
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
                self.nerd_font,
                &mut self.music,
                &mut self.images,
            );
        }
    }

    fn render_status_line(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
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

    fn render_toasts(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
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

    fn render_help_overlay(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let help_lines = vec![
            "mastoot — keys",
            "",
            "  Esc / Ctrl+C quit",
            "  ?             toggle this help",
            "  1 / 2 / 3 / 4 / 5  Home / Local / Federated / Notifications / Profile",
            "  u             open profile of selected post's author",
            "  F             (in other-user profile) follow / unfollow",
            "  o / O         (in profile) open followers / following list",
            "",
            "  j / ↓         next post",
            "  k / ↑         previous post",
            "  gg / G        top / bottom",
            "  R             refresh timeline",
            "",
            "  f             favourite / unfavourite",
            "  b             boost / unboost",
            "  B             force unboost",
            "  c             new post",
            "  r             reply to selected",
            "  q             quote selected (Mastodon 4.5+ native quote)",
            "  d             delete selected (only your own posts; Enter / Esc to confirm)",
            "  l / Enter     open thread (status detail)",
            "  Q             open quoted post (when selected post is a quote)",
            "  h / Esc       (in detail) back to timeline",
            "  s             reveal / hide CW body for selected post",
            "  D             toggle inter-post density (1 ↔ 2 blank lines)",
            "  S             cycle live updates: streaming · polling · off",
            "  A             switch account",
            "  Tab / S-Tab   (in notifications) cycle filter",
            "",
            "  in compose mode",
            "    Ctrl+Enter   send  (Alt+Enter / Ctrl+D also work)",
            "    Esc          cancel (confirm if draft non-empty)",
            "    Ctrl+W       cycle visibility",
            "    Ctrl+S       toggle content warning",
            "    Tab          toggle focus body ↔ CW field",
            "",
            "  (press any key to dismiss)",
        ];
        let w = 62.min(area.width);
        let h = (help_lines.len() as u16 + 2).min(area.height);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        let lines: Vec<Line<'static>> = help_lines
            .into_iter()
            .map(|s| Line::from(Span::styled(s.to_string(), self.theme.primary())))
            .collect();
        frame.render_widget(ratatui::widgets::Clear, rect);
        let block = ratatui::widgets::Block::new()
            .borders(ratatui::widgets::Borders::ALL)
            .border_style(self.theme.tertiary());
        let p = Paragraph::new(lines)
            .style(self.theme.primary())
            .block(block);
        frame.render_widget(p, rect);
    }
}

/// Reverse the optimistic flip applied earlier when a server action
/// fails. `attempted` is what the UI tried to do; we apply the
/// opposite. Counts saturate so a missed update can't underflow.
pub(crate) fn apply_revert(s: &mut Status, attempted: FailedAction) {
    match attempted {
        FailedAction::Favourite => {
            s.favourited = Some(false);
            s.favourites_count = s.favourites_count.saturating_sub(1);
        }
        FailedAction::Unfavourite => {
            s.favourited = Some(true);
            s.favourites_count = s.favourites_count.saturating_add(1);
        }
        FailedAction::Reblog => {
            s.reblogged = Some(false);
            s.reblogs_count = s.reblogs_count.saturating_sub(1);
        }
        FailedAction::Unreblog => {
            s.reblogged = Some(true);
            s.reblogs_count = s.reblogs_count.saturating_add(1);
        }
        FailedAction::Bookmark => {
            s.bookmarked = Some(false);
        }
        FailedAction::Unbookmark => {
            s.bookmarked = Some(true);
        }
    }
}

/// Map an API-layer visibility to its state-layer twin. Used in two
/// places (timeline-mode reply and detail-mode reply) — keeping it here
/// avoids the four-arm match repeating.
fn api_to_state_vis(v: crate::api::models::Visibility) -> state::Visibility {
    match v {
        crate::api::models::Visibility::Public => state::Visibility::Public,
        crate::api::models::Visibility::Unlisted => state::Visibility::Unlisted,
        crate::api::models::Visibility::Private => state::Visibility::Private,
        crate::api::models::Visibility::Direct => state::Visibility::Direct,
    }
}

// ---------------------------------------------------------------------------
// Terminal setup / teardown
// ---------------------------------------------------------------------------

fn enter_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(Into::into)
}

fn leave_terminal(term: &mut Term) {
    let _ = disable_raw_mode();
    let _ = execute!(term.backend_mut(), LeaveAlternateScreen);
    let _ = term.show_cursor();
}

/// Restore the terminal if we panic mid-render, so the user isn't left
/// with a mangled TTY.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev(info);
    }));
    debug!("panic hook installed");
}
