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

mod events;
mod input;
mod render;

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
