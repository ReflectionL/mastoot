//! TUI main loop and screen router.
//!
//! Owns the terminal, the state-task [`Handle`], and per-screen state.
//! Runs three concurrent streams inside `tokio::select!`:
//!
//! - keyboard events (`crossterm::event::EventStream`)
//! - state-task events ([`crate::state::Event`])
//! - a 30 s background tick (refreshes relative timestamps)

use std::collections::{HashMap, HashSet};
use std::io;
use std::time::Instant;

use anyhow::{Context, Result};
use crossterm::event::{
    Event as CEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
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
use crate::ui::screens::search::{SearchOutcome, SearchScreen};
use crate::ui::screens::status_detail::{DetailOutcome, DetailState};
use crate::ui::screens::timeline::TimelineScreen;
use crate::ui::widgets::status_card::RenderPrefs;

/// Size of the in-memory toast buffer. Additional toasts bump older ones.
const TOAST_LIMIT: usize = 3;
/// How long a toast stays on screen.
const TOAST_TTL: Duration = Duration::from_secs(4);

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
    let mut app = App::new(theme, initial_mode, cfg);

    let outcome = Box::pin(main_loop(&mut term, &mut app, &mut handle)).await;
    leave_terminal(&mut term);
    handle.shutdown();
    outcome
}

async fn main_loop(term: &mut Term, app: &mut App, handle: &mut Handle) -> Result<()> {
    let mut keys = EventStream::new();
    let mut tick = new_ticker();
    // Fires once a second, but only while a toast is on screen — see
    // the branch guard below. Keeps idle redraws at the 30 s cadence.
    let mut toast_tick = interval(Duration::from_secs(1));
    toast_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Background downloads (images, Apple Music metadata) ping these
    // when they finish; without them a picture would only appear on
    // the next key press or 30 s tick.
    let image_wakeup = app.images.wakeup();
    let music_wakeup = app.music.wakeup();

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
                app.flush_pending(&handle.actions).await;
            }
            _ = tick.tick() => {
                app.on_tick();
            }
            _ = toast_tick.tick(), if !app.toasts.is_empty() => {
                app.expire_toasts();
            }
            () = image_wakeup.notified() => {}
            () = music_wakeup.notified() => {}
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
    /// `[ui] show_relative_time = false` → `Jan 15 14:32` timestamps.
    absolute_time: bool,
    /// Blank rows between posts; `D` flips 1 ↔ 2 at runtime.
    density: usize,
    /// Parents of replies, fetched on demand for the `↪ @…: "…"`
    /// preview line. Keyed by the parent's id.
    parents: HashMap<StatusId, Status>,
    /// Parent ids with a fetch in flight (or that failed — no retry
    /// storms on a deleted parent).
    parents_requested: HashSet<StatusId>,
    /// Actions queued by event handlers (which have no channel in
    /// hand); the main loop flushes them after each event.
    pending_actions: Vec<Action>,
    /// `e` was pressed on this post; when its source arrives, open
    /// compose in edit mode with this visibility.
    pending_edit: Option<(StatusId, state::Visibility)>,
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
    /// One-line query being typed after `/`. Rendered in the status
    /// row; `Enter` turns it into [`Mode::Search`].
    SearchPrompt(String),
    /// Search results (`/api/v2/search` or a hashtag timeline).
    Search(SearchScreen),
    /// "Quit? Enter · Esc" confirm, entered with `Esc` from a
    /// top-level timeline. `Ctrl+C` still quits instantly; the modal
    /// exists because `Esc` is also "go back" one level up, and two
    /// reflexive presses shouldn't end the session.
    QuitConfirm,
}

struct Toast {
    level: ToastLevel,
    message: String,
    created: Instant,
}

impl App {
    fn new(theme: Theme, stream_mode: StreamMode, cfg: Config) -> Self {
        let mut screens = HashMap::new();
        for k in [
            TimelineKind::Home,
            TimelineKind::Local,
            TimelineKind::Federated,
            TimelineKind::Notifications,
            TimelineKind::Favourites,
            TimelineKind::Bookmarks,
        ] {
            screens.insert(k, TimelineScreen::new(k));
        }
        let images = ImageCache::from_config(cfg.ui.media_render, cfg.ui.image_protocol.as_deref());
        let nerd_font = cfg.ui.nerd_font;
        let absolute_time = !cfg.ui.show_relative_time;
        Self {
            theme,
            nerd_font,
            absolute_time,
            density: 1,
            parents: HashMap::new(),
            parents_requested: HashSet::new(),
            pending_actions: Vec::new(),
            pending_edit: None,
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

    /// Send whatever event handlers queued up.
    async fn flush_pending(&mut self, tx: &tokio::sync::mpsc::Sender<Action>) {
        for a in self.pending_actions.drain(..) {
            let _ = tx.send(a).await;
        }
    }

    /// Queue parent fetches for the replies in `statuses` whose parent
    /// is neither in the same list nor already cached / requested.
    fn request_parents(&mut self, statuses: &[Status]) {
        for pid in missing_parents(statuses, &self.parents, &self.parents_requested) {
            self.parents_requested.insert(pid.clone());
            self.pending_actions.push(Action::LoadStatus(pid));
        }
    }

    /// Rendering preferences handed to every list screen.
    fn prefs(&self) -> RenderPrefs {
        RenderPrefs {
            nerd_font: self.nerd_font,
            absolute_time: self.absolute_time,
            inter_post_blank_lines: self.density,
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
            self.density = if self.density >= 2 { 1 } else { 2 };
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

        // `/` opens the search prompt from any browsing mode.
        if key.code == KeyCode::Char('/')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                self.mode,
                Mode::Timeline
                    | Mode::StatusDetail(_)
                    | Mode::Profile(_)
                    | Mode::AccountList(_)
                    | Mode::Search(_)
            )
        {
            self.push_mode(Mode::SearchPrompt(String::new()));
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

        // Keys that act on "the selected status" behave identically in
        // timeline / thread / profile, so they're handled once here
        // instead of per mode below.
        if self.handle_status_keys(key, tx).await {
            return true;
        }

        match std::mem::replace(&mut self.mode, Mode::Timeline) {
            Mode::SearchPrompt(mut query) => {
                match key.code {
                    KeyCode::Esc => self.pop_mode(),
                    KeyCode::Enter => {
                        let q = query.trim().to_string();
                        if q.is_empty() {
                            self.pop_mode();
                        } else {
                            // Replace the prompt (not push): the prompt
                            // is transient, `h` from results should
                            // return to where `/` was pressed.
                            self.mode = Mode::Search(SearchScreen::new(q.clone()));
                            let _ = tx.send(Action::Search { query: q }).await;
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        self.mode = Mode::SearchPrompt(query);
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.mode = Mode::SearchPrompt(String::new());
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        query.push(c);
                        self.mode = Mode::SearchPrompt(query);
                    }
                    _ => self.mode = Mode::SearchPrompt(query),
                }
                return true;
            }
            Mode::Search(mut state) => {
                if let KeyCode::Char('1' | '2' | '3' | '4' | '5' | '6' | '7') = key.code {
                    // Tab switch — fall through to timeline keys.
                    self.mode = Mode::Timeline;
                } else {
                    match state.handle_key(key) {
                        SearchOutcome::Continue => self.mode = Mode::Search(state),
                        SearchOutcome::Back => self.pop_mode(),
                        SearchOutcome::Dispatch(a) => {
                            let _ = tx.send(a).await;
                            self.mode = Mode::Search(state);
                        }
                        SearchOutcome::OpenProfile(acc) => {
                            self.mode = Mode::Search(state);
                            self.open_profile(acc, tx).await;
                        }
                        SearchOutcome::OpenStatus(s) => {
                            self.mode = Mode::Search(state);
                            self.open_detail(s, tx).await;
                        }
                        SearchOutcome::SearchTag(name) => {
                            self.back_stack.push(Mode::Search(state));
                            self.mode = Mode::Search(SearchScreen::new(format!("#{name}")));
                            let _ = tx.send(Action::SearchTag { name }).await;
                        }
                    }
                    return true;
                }
                // Fall through for tab keys.
            }
            Mode::QuitConfirm => {
                match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y') => return false,
                    KeyCode::Esc | KeyCode::Char('n' | 'N' | 'h') | KeyCode::Backspace => {
                        self.pop_mode();
                    }
                    _ => {
                        self.mode = Mode::QuitConfirm;
                    }
                }
                return true;
            }
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
                                edit_of: draft.edit_of,
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
                    KeyCode::Char('1' | '2' | '3' | '4' | '5' | '6' | '7') => {
                        // Restore mode to Timeline so the tab handlers
                        // below see the right starting state, then let
                        // the timeline-mode key table run.
                        self.mode = Mode::Timeline;
                        // fall through to timeline keys
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
                if let KeyCode::Char('1' | '2' | '3' | '4' | '5' | '6' | '7') = key.code {
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
            KeyCode::Esc => {
                self.push_mode(Mode::QuitConfirm);
            }
            KeyCode::Char('?') => {
                self.show_help = true;
            }
            KeyCode::Char('1') => self.switch_to(TimelineKind::Home, tx).await,
            KeyCode::Char('2') => self.switch_to(TimelineKind::Local, tx).await,
            KeyCode::Char('3') => self.switch_to(TimelineKind::Federated, tx).await,
            KeyCode::Char('4') => self.switch_to(TimelineKind::Notifications, tx).await,
            KeyCode::Char('5') => self.open_self_profile(tx).await,
            KeyCode::Char('6') => self.switch_to(TimelineKind::Favourites, tx).await,
            KeyCode::Char('7') => self.switch_to(TimelineKind::Bookmarks, tx).await,
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

    /// Status-scoped keys shared by every mode that has a selected
    /// post (timeline incl. notifications, thread, profile). Returns
    /// `true` when the key was consumed. Keys that only make sense in
    /// one place (`o` = followers inside a profile) are left to that
    /// mode's own table.
    async fn handle_status_keys(
        &mut self,
        key: KeyEvent,
        tx: &tokio::sync::mpsc::Sender<Action>,
    ) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if !matches!(
            self.mode,
            Mode::Timeline | Mode::StatusDetail(_) | Mode::Profile(_) | Mode::Search(_)
        ) {
            return false;
        }
        let in_profile = matches!(self.mode, Mode::Profile(_));
        match key.code {
            KeyCode::Char('c') => {
                self.push_mode(Mode::Compose(ComposeState::blank(self.max_chars)));
            }
            KeyCode::Char('r') => {
                if let Some(target) = self.current_target().cloned() {
                    let reply = reply_context_from(&target, 80);
                    let vis = api_to_state_vis(target.visibility);
                    self.push_mode(Mode::Compose(ComposeState::reply(
                        reply,
                        vis,
                        self.max_chars,
                    )));
                }
            }
            KeyCode::Char('q') => {
                if let Some(target) = self.current_target().cloned() {
                    let quote = quote_context_from(&target, 80);
                    self.push_mode(Mode::Compose(ComposeState::quote(quote, self.max_chars)));
                }
            }
            KeyCode::Char('d') => {
                if let Some(id) = self.selected_own_status_id() {
                    self.push_mode(Mode::DeleteConfirm(id));
                }
            }
            KeyCode::Char('e') => {
                // Edit own post: fetch the original text first; the
                // `StatusSource` event opens compose in edit mode.
                if let Some(id) = self.selected_own_status_id() {
                    let vis = self
                        .current_target()
                        .map_or(state::Visibility::Public, |t| {
                            api_to_state_vis(t.visibility)
                        });
                    self.pending_edit = Some((id.clone(), vis));
                    let _ = tx.send(Action::LoadSource(id)).await;
                }
            }
            KeyCode::Char('u') => {
                if let Some(acc) = self.current_target().map(|t| t.account.clone()) {
                    self.open_profile(acc, tx).await;
                }
            }
            // `Q` opens the quoted post of the selected status. A
            // dedicated key (vs. reusing `l` / `Enter`) keeps "open
            // outer post" and "open quoted post" unambiguous.
            KeyCode::Char('Q') => {
                if let Some(quoted) = self.selected_quoted_status() {
                    self.open_detail(quoted, tx).await;
                }
            }
            KeyCode::Char('o') if !in_profile => {
                if let Some(url) = self.current_target_url() {
                    match open::that_detached(&url) {
                        Ok(()) => self.push_toast(ToastLevel::Info, "opened in browser".into()),
                        Err(e) => {
                            self.push_toast(
                                ToastLevel::Error,
                                format!("couldn't open browser · {e}"),
                            );
                        }
                    }
                }
            }
            KeyCode::Char('y') => {
                if let Some(url) = self.current_target_url() {
                    match crate::util::clipboard::copy(&url) {
                        Ok(()) => self.push_toast(ToastLevel::Info, "link copied".into()),
                        Err(e) => self.push_toast(ToastLevel::Error, format!("copy failed · {e}")),
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// Push a profile page for `acc` and kick off its fetches.
    async fn open_profile(&mut self, acc: Account, tx: &tokio::sync::mpsc::Sender<Action>) {
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

    /// Push a thread page for `focal` and request its context.
    async fn open_detail(&mut self, focal: Status, tx: &tokio::sync::mpsc::Sender<Action>) {
        let detail = DetailState::new(focal);
        let id = detail.focal_id().clone();
        self.push_mode(Mode::StatusDetail(detail));
        let _ = tx.send(Action::OpenStatus(id)).await;
    }

    /// Build a fresh DetailState seeded with the focal post (the inner
    /// status if the selection points at a boost). Returns `None` if
    /// nothing is selected. The `OpenStatus` action is fired by the
    /// caller so the focal id is known here.
    fn open_detail_for_selection(&self) -> Option<DetailState> {
        let target = self.selected_target_status()?;
        Some(DetailState::new(target.clone()))
    }

    /// Read-only sibling of `selected_target_status_mut`. Timeline
    /// tabs only — see [`Self::current_target`] for the mode-aware
    /// version.
    fn selected_target_status(&self) -> Option<&Status> {
        let kind = self.active;
        let idx = self.screens.get(&kind)?.selected;
        let outer = self.timelines.get(&kind)?.get(idx)?;
        Some(outer.reblog.as_deref().unwrap_or(outer))
    }

    /// The status the cursor points at in whatever mode the user is
    /// in — inner post for boosts. Covers the four timeline tabs
    /// (notifications resolve to their attached status), the thread
    /// page and the profile page.
    fn current_target(&self) -> Option<&Status> {
        match &self.mode {
            Mode::Timeline if self.active == TimelineKind::Notifications => {
                let idx = self
                    .notifications_screen
                    .selected_index(&self.notifications)?;
                let s = self.notifications.get(idx)?.status.as_ref()?;
                Some(s.reblog.as_deref().unwrap_or(s))
            }
            Mode::Timeline => self.selected_target_status(),
            Mode::StatusDetail(d) => d.selected_target(),
            Mode::Profile(p) => p.selected_target(),
            Mode::Search(s) => s.selected_target(),
            _ => None,
        }
    }

    /// Public URL of the current target (falls back to the
    /// ActivityPub URI, which is also a browser-openable URL for
    /// Mastodon-family servers).
    fn current_target_url(&self) -> Option<String> {
        let t = self.current_target()?;
        let url = t.url.clone().unwrap_or_else(|| t.uri.clone());
        (!url.is_empty()).then_some(url)
    }

    /// Status id to target for a deletion *if* it belongs to the
    /// signed-in user. `None` when there's no selection, no `me`, or
    /// the author doesn't match.
    fn selected_own_status_id(&self) -> Option<StatusId> {
        let me_id = self.me.as_ref().map(|a| a.id.clone())?;
        let target = self.current_target()?;
        if target.account.id == me_id {
            Some(target.id.clone())
        } else {
            None
        }
    }

    /// If the currently selected post carries a quote payload with a
    /// resolved `quoted_status`, return an owned clone of that quoted
    /// status. `None` when there's no selection, no quote, or the
    /// quote's state is not `accepted` (no payload).
    fn selected_quoted_status(&self) -> Option<Status> {
        self.current_target()?
            .quote
            .as_ref()?
            .quoted_status
            .as_deref()
            .cloned()
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
                if kind == TimelineKind::Home {
                    let wanted = missing_parents(
                        self.timelines.get(&kind).map_or(&[][..], Vec::as_slice),
                        &self.parents,
                        &self.parents_requested,
                    );
                    for pid in wanted {
                        self.parents_requested.insert(pid.clone());
                        self.pending_actions.push(Action::LoadStatus(pid));
                    }
                }
            }
            Event::StatusSource(src) => {
                if let Some((id, vis)) = self.pending_edit.take()
                    && id == src.id
                    && !matches!(self.mode, Mode::Compose(_) | Mode::ComposeConfirmDiscard(_))
                {
                    self.push_mode(Mode::Compose(ComposeState::edit(src, vis, self.max_chars)));
                }
            }
            Event::StatusLoaded(status) => {
                self.parents_requested.remove(&status.id);
                self.parents.insert(status.id.clone(), status);
            }
            Event::StatusLoadFailed(id) => {
                // Stays in `parents_requested` on purpose: don't retry.
                tracing::debug!(%id, "parent status unavailable");
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
                if let Mode::Search(s) = &mut self.mode {
                    s.on_status_updated(&status);
                }
                for prev in &mut self.back_stack {
                    match prev {
                        Mode::StatusDetail(d) => d.on_status_updated(&status),
                        Mode::Profile(p) => p.on_status_updated(&status),
                        Mode::Search(s) => s.on_status_updated(&status),
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
                    let wanted =
                        missing_parents(&p.statuses, &self.parents, &self.parents_requested);
                    for pid in wanted {
                        self.parents_requested.insert(pid.clone());
                        self.pending_actions.push(Action::LoadStatus(pid));
                    }
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
                if let Mode::Search(s) = &mut self.mode {
                    s.revert_action(&id, action);
                }
                // Sub-pages stashed in the back-stack also need patching
                // so the user doesn't see stale optimistic state when
                // they navigate back.
                for prev in &mut self.back_stack {
                    match prev {
                        Mode::StatusDetail(d) => d.revert_action(&id, action),
                        Mode::Profile(p) => p.revert_action(&id, action),
                        Mode::Search(s) => s.revert_action(&id, action),
                        _ => {}
                    }
                }
            }
            Event::StatusDeleted(id) => {
                let gone = |s: &Status| s.id == id || s.reblog.as_ref().is_some_and(|r| r.id == id);
                for (kind, list) in &mut self.timelines {
                    list.retain(|s| !gone(s));
                    if let Some(screen) = self.screens.get_mut(kind) {
                        screen.on_len_changed(list.len());
                    }
                }
                self.notifications
                    .retain(|n| n.status.as_ref().is_none_or(|s| !gone(s)));
                self.notifications_screen
                    .on_len_changed(self.notifications.len());
                // Thread / profile pages — the live one and any
                // stashed on the back-stack. A thread whose *focal*
                // was deleted has nothing left to anchor on: drop it
                // from the stack, and if it's the live page, leave.
                let mut leave = false;
                match &mut self.mode {
                    Mode::StatusDetail(d) => leave = d.on_status_deleted(&id),
                    Mode::Profile(p) => p.on_status_deleted(&id),
                    Mode::Search(s) => s.on_status_deleted(&id),
                    _ => {}
                }
                self.back_stack.retain_mut(|prev| match prev {
                    Mode::StatusDetail(d) => !d.on_status_deleted(&id),
                    Mode::Profile(p) => {
                        p.on_status_deleted(&id);
                        true
                    }
                    Mode::Search(s) => {
                        s.on_status_deleted(&id);
                        true
                    }
                    _ => true,
                });
                if leave {
                    self.pop_mode();
                }
            }
            Event::LoadMoreFailed(kind) => {
                if kind == TimelineKind::Notifications {
                    self.notifications_screen.on_load_more_failed();
                } else if let Some(screen) = self.screens.get_mut(&kind) {
                    screen.on_load_more_failed();
                }
            }
            Event::ProfileLoadFailed(id) => {
                if let Mode::Profile(p) = &mut self.mode
                    && p.account_id == id
                {
                    p.on_load_failed();
                }
                for prev in &mut self.back_stack {
                    if let Mode::Profile(p) = prev
                        && p.account_id == id
                    {
                        p.on_load_failed();
                    }
                }
            }
            Event::AccountListLoadFailed { for_id, kind } => {
                if let Mode::AccountList(l) = &mut self.mode
                    && l.for_id == for_id
                    && l.kind == kind
                {
                    l.on_load_failed();
                }
            }
            Event::TimelineStatusAdded { kind, status } => {
                let slot = self.timelines.entry(kind).or_default();
                // Dedup — reconnects replay recent events, and the
                // hot-start fetch may have already pulled this id.
                if slot.iter().any(|s| s.id == status.id) {
                    return;
                }
                slot.insert(0, status.clone());
                let new_len = slot.len();
                if let Some(screen) = self.screens.get_mut(&kind) {
                    screen.on_prepended(1, new_len);
                }
                if kind == TimelineKind::Home {
                    self.request_parents(std::slice::from_ref(&status));
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
            Event::SearchResults { query, results } => {
                if let Mode::Search(s) = &mut self.mode
                    && s.query == query
                {
                    s.on_results(results);
                }
            }
            Event::SearchStatuses { query, statuses } => {
                if let Mode::Search(s) = &mut self.mode
                    && s.query == query
                {
                    s.on_statuses(statuses);
                }
            }
            Event::SearchFailed { query } => {
                if let Mode::Search(s) = &mut self.mode
                    && s.query == query
                {
                    s.on_failed();
                }
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
                self.parents.clear();
                self.parents_requested.clear();
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

    /// 30 s background tick. Nothing to mutate — the redraw it forces
    /// is what refreshes the relative timestamps (`2h` → `3h`).
    fn on_tick(&mut self) {
        self.expire_toasts();
    }

    /// Drop toasts older than [`TOAST_TTL`]. Called from the 1 s toast
    /// ticker (only armed while toasts exist) and before every render.
    fn expire_toasts(&mut self) {
        self.toasts.retain(|t| t.created.elapsed() < TOAST_TTL);
    }

    fn push_toast(&mut self, level: ToastLevel, message: String) {
        self.toasts.push(Toast {
            level,
            message,
            created: Instant::now(),
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

    /// Centered "Quit mastoot?" confirm. `Enter` / `y` quits,
    /// `Esc` / `n` stays.
    fn render_quit_confirm(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
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

    fn render_tabs(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
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
    fn tab_5_active(&self) -> bool {
        matches!(&self.mode, Mode::Profile(p) if p.is_self)
    }

    fn render_body(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
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

    fn render_status_line(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
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
            "  Esc           quit (asks first) · Ctrl+C quits at once",
            "  ?             toggle this help",
            "  1 / 2 / 3 / 4 / 5  Home / Local / Federated / Notifications / Profile",
            "  6 / 7         Favourites / Bookmarks",
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
            "  d             delete own post (Enter / Esc to confirm)",
            "  e             edit own post",
            "  l / Enter     open thread (status detail)",
            "  Q             open quoted post (when selected post is a quote)",
            "  o             open in browser (in profile: followers)",
            "  y             copy link of selected post",
            "  /             search accounts · hashtags · posts",
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
        // Fit the box to the longest line (+ borders + a little air)
        // instead of a fixed width, so no entry gets clipped.
        let longest = help_lines
            .iter()
            .map(|l| {
                l.chars()
                    .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1))
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0) as u16;
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

/// Parent ids worth fetching for the reply previews in `statuses`:
/// replies whose parent is neither in the same list nor already
/// cached / requested. Self-replies (thread continuations) are
/// skipped — the card says "in a thread" and that's enough.
fn missing_parents(
    statuses: &[Status],
    parents: &HashMap<StatusId, Status>,
    requested: &HashSet<StatusId>,
) -> Vec<StatusId> {
    let mut out: Vec<StatusId> = Vec::new();
    for s in statuses {
        let shown = s.reblog.as_deref().unwrap_or(s);
        let Some(pid) = shown.in_reply_to_id.as_ref() else {
            continue;
        };
        if shown.in_reply_to_account_id.as_ref() == Some(&shown.account.id)
            || parents.contains_key(pid)
            || requested.contains(pid)
            || out.contains(pid)
            || statuses
                .iter()
                .any(|o| o.reblog.as_deref().unwrap_or(o).id == *pid)
        {
            continue;
        }
        out.push(pid.clone());
    }
    out
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
    // Kitty keyboard protocol: lets the terminal distinguish Ctrl+Enter
    // from plain Enter (and other modified specials). Without this most
    // terminals collapse both to a bare `\r`, so our Ctrl+Enter submit
    // binding gets reported as KeyCode::Enter with no modifier and the
    // textarea inserts a newline instead. Probed first; only pushed on
    // terminals that advertise support (kitty / WezTerm / Ghostty /
    // foot / Konsole / iTerm2 with CSI-u enabled). Apple Terminal etc.
    // are left untouched. Failures are non-fatal.
    if supports_keyboard_enhancement().unwrap_or(false) {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        );
    }
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(Into::into)
}

fn leave_terminal(term: &mut Term) {
    // Best-effort pop — if the push above was skipped or the terminal
    // ignored it, this is a no-op.
    let _ = execute!(term.backend_mut(), PopKeyboardEnhancementFlags);
    let _ = disable_raw_mode();
    let _ = execute!(term.backend_mut(), LeaveAlternateScreen);
    let _ = term.show_cursor();
}

/// Restore the terminal if we panic mid-render, so the user isn't left
/// with a mangled TTY.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev(info);
    }));
    debug!("panic hook installed");
}
