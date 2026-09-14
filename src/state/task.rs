//! Background task that owns the [`MastodonClient`] and [`AppState`].
//!
//! The UI never awaits on network calls directly. It sends [`Action`]s
//! over an mpsc channel and consumes [`Event`]s off another. This keeps
//! the render loop snappy and localizes the client's lifetime.
//!
//! **Concurrency model.** The dispatcher loop itself never awaits a
//! network call. Every action that talks to the server is spawned as
//! its own tokio task (the client is `Clone`, the bookkeeping state is
//! behind a mutex), so a slow `/context` fetch can't hold up a
//! favourite, a `LoadMore`, or the polling tick behind it. The two
//! actions that mutate the dispatcher's own state — [`Action::SetStreamMode`]
//! and [`Action::SwitchAccount`] — run inline; an account switch also
//! aborts every in-flight task so a late reply from the old session
//! can't land in the new one.

use std::time::Duration;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, warn};

use crate::api::endpoints::{
    AccountListParams, AccountStatusesParams, NotificationParams, StatusDraft, TimelineParams,
};
use crate::api::error::ApiErrorCategory;
use crate::api::models::{AccountId, Status, StatusId, Visibility as ApiVisibility};
use crate::api::streaming::{StreamEvent, UserStream};
use crate::api::{ApiError, MastodonClient};
use crate::state::app::{AppState, Shared, lock};
use crate::state::event::{
    AccountListKind, Action, ApiHealth, Event, FailedAction, StreamMode, StreamState, ToastLevel,
    Visibility,
};
use crate::state::timeline::TimelineKind;

/// Action queue depth. Generous: the dispatcher drains it instantly
/// (every action is spawned), so the UI's `send().await` only ever
/// blocks if the tokio runtime itself is wedged.
const ACTION_CAP: usize = 256;
const EVENT_CAP: usize = 1024;
const PAGE_SIZE: u32 = 40;
/// Reconnect backoff grows 1 → 2 → 4 → 8 → 16 → 30 (capped). Resets to
/// 1 on a successful open.
const STREAM_BACKOFF_MIN: Duration = Duration::from_secs(1);
const STREAM_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Polling cadence. Matches the background tick (toast decay / relative
/// timestamps) — no point polling faster than the UI can paint.
const POLLING_PERIOD: Duration = Duration::from_secs(30);

/// Handle returned by [`spawn`]. The UI holds this for the lifetime of
/// the TUI; drop it to signal shutdown.
pub struct Handle {
    pub actions: mpsc::Sender<Action>,
    pub events: mpsc::Receiver<Event>,
    task: JoinHandle<()>,
}

impl Handle {
    /// Abort the background task. Called from `ui::app` on exit.
    pub fn shutdown(self) {
        self.task.abort();
    }
}

/// Spawn the state task. Ownership of `client` moves in.
pub fn spawn(client: MastodonClient) -> Handle {
    let (action_tx, action_rx) = mpsc::channel::<Action>(ACTION_CAP);
    let (event_tx, event_rx) = mpsc::channel::<Event>(EVENT_CAP);
    // Inner action-sender clone: used by sub-tasks (polling loop) to
    // re-enter the dispatcher via Action. Never handed to the UI.
    let internal_tx = action_tx.clone();
    let task = tokio::spawn(run(client, action_rx, internal_tx, event_tx));
    Handle {
        actions: action_tx,
        events: event_rx,
        task,
    }
}

/// Everything an action task needs, bundled so the spawn sites stay
/// one-liners.
#[derive(Clone)]
struct Ctx {
    client: MastodonClient,
    state: Shared,
    events: mpsc::Sender<Event>,
}

/// Holder for whichever live-update task is currently running.
/// Exactly one of `streaming` / `polling` / nothing is active at a time.
struct LiveUpdateSlot {
    mode: StreamMode,
    handle: Option<JoinHandle<()>>,
}

impl LiveUpdateSlot {
    fn idle() -> Self {
        Self {
            mode: StreamMode::Off,
            handle: None,
        }
    }

    /// Swap to `new`. Aborts the previous task, spawns whatever the new
    /// mode requires, and broadcasts a fresh [`StreamState`] so the UI
    /// dot updates immediately.
    async fn set(&mut self, new: StreamMode, ctx: &Ctx, actions: &mpsc::Sender<Action>) {
        if new == self.mode && self.handle.is_some() {
            return;
        }
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        self.mode = new;
        self.handle = match new {
            StreamMode::Streaming if ctx.client.token().is_some() => {
                Some(tokio::spawn(streaming_loop(ctx.clone())))
            }
            StreamMode::Polling => {
                Some(tokio::spawn(polling_loop(actions.clone(), POLLING_PERIOD)))
            }
            // Streaming-but-no-token collapses to Off — Mastodon rejects
            // anonymous streams, so there's nothing to spawn.
            StreamMode::Streaming | StreamMode::Off => {
                send(&ctx.events, Event::StreamState(StreamState::Disconnected)).await;
                None
            }
        };
    }

    fn shutdown(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

async fn run(
    client: MastodonClient,
    mut actions: mpsc::Receiver<Action>,
    actions_tx: mpsc::Sender<Action>,
    events: mpsc::Sender<Event>,
) {
    let mut ctx = Ctx {
        client,
        state: AppState::shared(),
        events,
    };
    // Live-update slot — starts idle. The UI sends SetStreamMode as
    // its first action so the initial mode comes from config, not a
    // hardcoded default.
    let mut live = LiveUpdateSlot::idle();
    // Every network-bound action lives here. Reaped as they finish;
    // aborted wholesale on account switch / shutdown.
    let mut inflight: JoinSet<()> = JoinSet::new();

    inflight.spawn(bootstrap_session(ctx.clone()));

    loop {
        tokio::select! {
            maybe = actions.recv() => {
                let Some(action) = maybe else { break };
                match action {
                    Action::Quit => break,
                    Action::SetStreamMode(mode) => {
                        live.set(mode, &ctx, &actions_tx).await;
                    }
                    Action::SwitchAccount { instance, handle, token } => {
                        let new_client = match MastodonClient::new(&instance, token) {
                            Ok(c) => c,
                            Err(e) => {
                                warn!(?e, %handle, "failed to build client for account switch");
                                toast(&ctx.events, ToastLevel::Error, format!("switch failed · {}", e.terse())).await;
                                continue;
                            }
                        };
                        // Tear down everything that holds the outgoing
                        // client: the live-update task and every
                        // in-flight action. A late reply from the old
                        // account must never land in the new session.
                        live.shutdown();
                        inflight.abort_all();
                        ctx.client = new_client;
                        *lock(&ctx.state) = AppState::new();

                        send(&ctx.events, Event::AccountSwitched { handle: handle.clone() }).await;
                        toast(&ctx.events, ToastLevel::Info, format!("switched to {handle}")).await;

                        inflight.spawn(bootstrap_session(ctx.clone()));
                        // Re-arm live updates with whatever mode the UI
                        // was last running in.
                        let mode = live.mode;
                        live.set(mode, &ctx, &actions_tx).await;
                        // Kick a fresh Home fetch so the timeline paints
                        // immediately — UI already cleared its caches on
                        // the AccountSwitched event.
                        inflight.spawn(handle_action(ctx.clone(), Action::LoadTimeline(TimelineKind::Home)));
                    }
                    other => {
                        inflight.spawn(handle_action(ctx.clone(), other));
                    }
                }
            }
            Some(finished) = inflight.join_next(), if !inflight.is_empty() => {
                if let Err(e) = finished
                    && e.is_panic()
                {
                    warn!(?e, "action task panicked");
                }
            }
        }
    }
    inflight.abort_all();
    live.shutdown();
    debug!("state task exiting");
}

/// One action, one task. Everything here runs concurrently with every
/// other action; shared bookkeeping goes through `ctx.state`.
async fn handle_action(ctx: Ctx, action: Action) {
    match action {
        Action::LoadTimeline(kind) | Action::Refresh(kind) => {
            if matches!(kind, TimelineKind::Notifications) {
                lock(&ctx.state).notifications_oldest = None;
            }
            load_timeline(&ctx, kind, None, false).await;
        }
        Action::LoadMore(kind) => {
            let max_id = {
                let st = lock(&ctx.state);
                if matches!(kind, TimelineKind::Notifications) {
                    st.notifications_oldest.clone().map(|id| id.0)
                } else {
                    st.cursors(kind)
                        .and_then(|c| c.oldest.clone())
                        .map(|id| id.0)
                }
            };
            load_timeline(&ctx, kind, max_id, true).await;
        }
        Action::FetchNewer(kind) => fetch_newer(&ctx, kind).await,
        Action::Favourite(id) => {
            let r = ctx.client.favourite(&id).await;
            status_action(&ctx, id, FailedAction::Favourite, r).await;
        }
        Action::Unfavourite(id) => {
            let r = ctx.client.unfavourite(&id).await;
            status_action(&ctx, id, FailedAction::Unfavourite, r).await;
        }
        Action::Reblog(id) => {
            let r = ctx.client.reblog(&id).await;
            status_action(&ctx, id, FailedAction::Reblog, r).await;
        }
        Action::Unreblog(id) => {
            let r = ctx.client.unreblog(&id).await;
            status_action(&ctx, id, FailedAction::Unreblog, r).await;
        }
        Action::Bookmark(id) => {
            let r = ctx.client.bookmark(&id).await;
            status_action(&ctx, id, FailedAction::Bookmark, r).await;
        }
        Action::Unbookmark(id) => {
            let r = ctx.client.unbookmark(&id).await;
            status_action(&ctx, id, FailedAction::Unbookmark, r).await;
        }
        Action::DeleteStatus(id) => match ctx.client.delete_status(&id).await {
            Ok(_) => {
                send(&ctx.events, Event::StatusDeleted(id)).await;
                toast(&ctx.events, ToastLevel::Info, "deleted".into()).await;
                note_api_ok(&ctx).await;
            }
            Err(e) => report_api_error(&ctx, "delete", &e).await,
        },
        Action::Compose {
            text,
            in_reply_to_id,
            quote_id,
            content_warning,
            sensitive,
            visibility,
            edit_of,
        } => {
            let mut draft = StatusDraft::new(text);
            draft.in_reply_to_id = in_reply_to_id;
            draft.quote_id = quote_id;
            draft.spoiler_text = content_warning;
            draft.sensitive = sensitive;
            draft.visibility = Some(match visibility {
                Visibility::Public => ApiVisibility::Public,
                Visibility::Unlisted => ApiVisibility::Unlisted,
                Visibility::Private => ApiVisibility::Private,
                Visibility::Direct => ApiVisibility::Direct,
            });
            if let Some(id) = edit_of {
                edit_status(&ctx, &id, &draft).await;
            } else if post_status(&ctx, &draft).await {
                // Pull a fresh home timeline so the just-posted
                // status shows up immediately.
                load_timeline(&ctx, TimelineKind::Home, None, false).await;
            }
        }
        Action::LoadSource(id) => match ctx.client.status_source(&id).await {
            Ok(src) => {
                send(&ctx.events, Event::StatusSource(src)).await;
                note_api_ok(&ctx).await;
            }
            Err(e) => report_api_error(&ctx, "edit", &e).await,
        },
        Action::LoadProfile { id, max_id } => load_profile(&ctx, id, max_id).await,
        Action::LoadRelationship(id) => load_relationship(&ctx, id).await,
        Action::Follow(id) => follow_action(&ctx, id, true).await,
        Action::Unfollow(id) => follow_action(&ctx, id, false).await,
        Action::LoadAccountList { id, kind, max_id } => {
            load_account_list(&ctx, id, kind, max_id).await;
        }
        Action::LoadStatus(id) => match ctx.client.status(&id).await {
            Ok(s) => {
                send(&ctx.events, Event::StatusLoaded(s)).await;
                note_api_ok(&ctx).await;
            }
            Err(e) => {
                // A missing parent is not worth a toast; the card just
                // keeps its plain "replying to @…" hint.
                send(&ctx.events, Event::StatusLoadFailed(id)).await;
                note_api_error(&ctx, &e).await;
            }
        },
        Action::Search { query } => {
            let params = crate::api::endpoints::SearchParams {
                q: &query,
                kind: None,
                resolve: true,
                following: false,
                limit: Some(20),
            };
            match ctx.client.search(&params).await {
                Ok(results) => {
                    send(&ctx.events, Event::SearchResults { query, results }).await;
                    note_api_ok(&ctx).await;
                }
                Err(e) => {
                    send(&ctx.events, Event::SearchFailed { query }).await;
                    report_api_error(&ctx, "search", &e).await;
                }
            }
        }
        Action::SearchTag { name } => {
            let params = TimelineParams {
                limit: Some(PAGE_SIZE),
                ..Default::default()
            };
            let query = format!("#{name}");
            match ctx.client.tag_timeline(&name, &params).await {
                Ok(page) => {
                    send(
                        &ctx.events,
                        Event::SearchStatuses {
                            query,
                            statuses: page.items,
                        },
                    )
                    .await;
                    note_api_ok(&ctx).await;
                }
                Err(e) => {
                    send(&ctx.events, Event::SearchFailed { query }).await;
                    report_api_error(&ctx, "hashtag", &e).await;
                }
            }
        }
        Action::OpenStatus(id) => match ctx.client.status_context(&id).await {
            Ok(c) => {
                send(
                    &ctx.events,
                    Event::StatusContext {
                        focal_id: id,
                        ancestors: c.ancestors,
                        descendants: c.descendants,
                    },
                )
                .await;
                note_api_ok(&ctx).await;
            }
            Err(e) => report_api_error(&ctx, "thread", &e).await,
        },
        // Handled inline by the dispatcher; never reaches here.
        Action::SetStreamMode(_) | Action::SwitchAccount { .. } | Action::Quit => {}
    }
}

/// Called by the polling loop every [`POLLING_PERIOD`] seconds. Fetches
/// anything newer than the cursor and prepends each item via the same
/// events SSE updates emit.
async fn fetch_newer(ctx: &Ctx, kind: TimelineKind) {
    match kind {
        TimelineKind::Home | TimelineKind::Local | TimelineKind::Federated => {
            fetch_newer_statuses(ctx, kind).await;
        }
        TimelineKind::Notifications => fetch_newer_notifications(ctx).await,
        _ => {}
    }
}

async fn fetch_newer_statuses(ctx: &Ctx, kind: TimelineKind) {
    let since_id = lock(&ctx.state)
        .cursors(kind)
        .and_then(|c| c.newest.clone())
        .map(|id| id.0);
    // No cursor yet (empty timeline) — wait for the first full load.
    let Some(since_id) = since_id else {
        return;
    };
    let params = TimelineParams {
        since_id: Some(since_id),
        limit: Some(PAGE_SIZE),
        local: matches!(kind, TimelineKind::Local),
        ..Default::default()
    };
    let result = match kind {
        TimelineKind::Home => ctx.client.home_timeline(&params).await,
        _ => ctx.client.public_timeline(&params).await,
    };
    match result {
        Ok(page) => {
            note_api_ok(ctx).await;
            if let Some(first) = page.items.first() {
                lock(&ctx.state).note_page(kind, Some(first.id.clone()), None);
            }
            // Mastodon returns newest-first; iterate reversed so the
            // oldest-new item arrives first and the final prepend sits
            // at the top. The UI dedups by id.
            for status in page.items.into_iter().rev() {
                send(&ctx.events, Event::TimelineStatusAdded { kind, status }).await;
            }
        }
        Err(e) => {
            // Polling failures are quiet: the dot dims, no toast spam.
            note_api_error(ctx, &e).await;
        }
    }
}

async fn fetch_newer_notifications(ctx: &Ctx) {
    let since_id = lock(&ctx.state).notifications_newest.clone().map(|id| id.0);
    let Some(since_id) = since_id else {
        return;
    };
    let params = NotificationParams {
        since_id: Some(since_id),
        limit: Some(PAGE_SIZE),
        ..Default::default()
    };
    match ctx.client.notifications(&params).await {
        Ok(page) => {
            note_api_ok(ctx).await;
            if let Some(first) = page.items.first() {
                lock(&ctx.state).notifications_newest = Some(first.id.clone());
            }
            for n in page.items.into_iter().rev() {
                send(&ctx.events, Event::NotificationReceived(n)).await;
            }
        }
        Err(e) => note_api_error(ctx, &e).await,
    }
}

/// Background loop that fires [`Action::FetchNewer`] at a fixed cadence.
/// Sleeps first (the UI just loaded the timeline — polling immediately
/// would be wasted).
async fn polling_loop(actions: mpsc::Sender<Action>, period: Duration) {
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first fire; `interval` ticks at t=0 by default.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        for kind in [TimelineKind::Home, TimelineKind::Notifications] {
            if actions.send(Action::FetchNewer(kind)).await.is_err() {
                // dispatcher is gone — bail
                return;
            }
        }
    }
}

/// Background task that keeps the SSE user-stream connection alive.
/// Loops forever: open → decode events → on close/error broadcast
/// Reconnecting, sleep with exponential backoff (capped at 30 s), try
/// again. A successful open resets the backoff to 1 s.
///
/// This task does *not* touch `ApiHealth`. SSE disruptions are a
/// separate signal (the REST API might be fine while streaming is
/// down, e.g. behind a proxy that buffers) — UI renders them as a
/// secondary status-bar label.
async fn streaming_loop(ctx: Ctx) {
    let mut backoff = STREAM_BACKOFF_MIN;
    loop {
        send(&ctx.events, Event::StreamState(StreamState::Connecting)).await;
        match UserStream::open(&ctx.client).await {
            Ok(mut s) => {
                send(&ctx.events, Event::StreamState(StreamState::Connected)).await;
                backoff = STREAM_BACKOFF_MIN;
                while let Some(ev) = s.next().await {
                    if !dispatch_stream_event(&ctx, ev).await {
                        // Disconnect sentinel — bail out to the reconnect path.
                        break;
                    }
                }
                debug!("stream ended; will reconnect");
            }
            Err(e) => {
                warn!(?e, "failed to open user stream");
            }
        }
        send(&ctx.events, Event::StreamState(StreamState::Reconnecting)).await;
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(STREAM_BACKOFF_MAX);
    }
}

/// Translate a single [`StreamEvent`] into the right UI event(s) and
/// keep the `since_id` cursors current so a later switch to polling
/// mode doesn't refetch what the stream already delivered.
/// Returns `false` on Disconnect (signals the caller to reconnect);
/// otherwise `true` regardless of whether the event was emitted.
async fn dispatch_stream_event(ctx: &Ctx, ev: StreamEvent) -> bool {
    match ev {
        StreamEvent::Update(status) => {
            lock(&ctx.state).note_page(TimelineKind::Home, Some(status.id.clone()), None);
            send(
                &ctx.events,
                Event::TimelineStatusAdded {
                    kind: TimelineKind::Home,
                    status: *status,
                },
            )
            .await;
        }
        StreamEvent::Delete(id) => {
            send(&ctx.events, Event::StatusDeleted(id)).await;
        }
        StreamEvent::Notification(n) => {
            lock(&ctx.state).notifications_newest = Some(n.id.clone());
            send(&ctx.events, Event::NotificationReceived(*n)).await;
        }
        StreamEvent::StatusUpdate(status) => {
            send(&ctx.events, Event::StatusUpdated(*status)).await;
        }
        StreamEvent::Disconnect => return false,
        // Filters / announcements / conversations / unknown are fine to
        // drop for now and can be surfaced later without touching the
        // reconnect loop.
        _ => {}
    }
    true
}

async fn load_timeline(ctx: &Ctx, kind: TimelineKind, max_id: Option<String>, appended: bool) {
    let params = TimelineParams {
        max_id,
        limit: Some(PAGE_SIZE),
        local: matches!(kind, TimelineKind::Local),
        ..Default::default()
    };
    // Favourites / bookmarks paginate by an internal id carried in the
    // `Link` header, not by status id — their "oldest" cursor is that
    // opaque token.
    let link_paged = matches!(kind, TimelineKind::Favourites | TimelineKind::Bookmarks);
    let result = match kind {
        TimelineKind::Home => ctx.client.home_timeline(&params).await,
        TimelineKind::Local | TimelineKind::Federated => ctx.client.public_timeline(&params).await,
        TimelineKind::Favourites | TimelineKind::Bookmarks => {
            let p = AccountListParams {
                max_id: params.max_id.clone(),
                since_id: None,
                limit: params.limit,
            };
            if matches!(kind, TimelineKind::Favourites) {
                ctx.client.favourites(&p).await
            } else {
                ctx.client.bookmarks(&p).await
            }
        }
        TimelineKind::Notifications => {
            load_notifications(ctx, params.max_id, appended).await;
            return;
        }
        TimelineKind::Profile => return,
    };
    match result {
        Ok(page) => {
            {
                let mut st = lock(&ctx.state);
                let oldest = if link_paged {
                    page.next
                        .as_ref()
                        .and_then(|c| c.max_id.clone())
                        .map(StatusId::new)
                } else {
                    page.items.last().map(|s| s.id.clone())
                };
                if appended {
                    st.note_page(kind, None, oldest);
                } else {
                    let newest = page.items.first().map(|s| s.id.clone());
                    st.note_page(kind, newest, oldest);
                }
            }
            send(
                &ctx.events,
                Event::TimelineUpdated {
                    kind,
                    statuses: page.items,
                    appended,
                },
            )
            .await;
            note_api_ok(ctx).await;
        }
        Err(e) => {
            if appended {
                send(&ctx.events, Event::LoadMoreFailed(kind)).await;
            }
            let verb = format!("{} timeline", kind.label());
            report_api_error(ctx, &verb, &e).await;
        }
    }
}

/// Fetch a page of notifications. The state task tracks only the
/// newest / oldest ids for pagination; the full list lives in the UI.
async fn load_notifications(ctx: &Ctx, max_id: Option<String>, appended: bool) {
    let params = NotificationParams {
        max_id,
        limit: Some(PAGE_SIZE),
        ..Default::default()
    };
    match ctx.client.notifications(&params).await {
        Ok(page) => {
            {
                let mut st = lock(&ctx.state);
                if let Some(last) = page.items.last() {
                    st.notifications_oldest = Some(last.id.clone());
                }
                if !appended && let Some(first) = page.items.first() {
                    st.notifications_newest = Some(first.id.clone());
                }
            }
            send(
                &ctx.events,
                Event::NotificationsUpdated {
                    items: page.items,
                    appended,
                },
            )
            .await;
            note_api_ok(ctx).await;
        }
        Err(e) => {
            if appended {
                send(
                    &ctx.events,
                    Event::LoadMoreFailed(TimelineKind::Notifications),
                )
                .await;
            }
            report_api_error(ctx, "notifications", &e).await;
        }
    }
}

/// Fetch a profile (account + statuses page). The two requests run
/// concurrently on a first load; pagination skips the account refetch
/// since the header is already on screen.
async fn load_profile(ctx: &Ctx, id: AccountId, max_id: Option<String>) {
    let appended = max_id.is_some();
    let params = AccountStatusesParams {
        max_id,
        limit: Some(PAGE_SIZE),
        ..Default::default()
    };
    let (account, statuses) = if appended {
        (Ok(None), ctx.client.account_statuses(&id, &params).await)
    } else {
        let (a, s) = tokio::join!(
            ctx.client.account(&id),
            ctx.client.account_statuses(&id, &params)
        );
        (a.map(Some), s)
    };
    let account = match account {
        Ok(a) => a,
        Err(e) => {
            send(&ctx.events, Event::ProfileLoadFailed(id)).await;
            report_api_error(ctx, "profile", &e).await;
            return;
        }
    };
    match statuses {
        Ok(page) => {
            // For pagination calls we don't have an Account on hand;
            // synthesize a stub with just the id so the UI can match
            // the event back to the open profile. UI ignores the rest.
            let acc = account.unwrap_or_else(|| crate::api::models::Account {
                id: id.clone(),
                ..Default::default()
            });
            send(
                &ctx.events,
                Event::ProfileLoaded {
                    account: acc,
                    statuses: page.items,
                    appended,
                },
            )
            .await;
            note_api_ok(ctx).await;
        }
        Err(e) => {
            send(&ctx.events, Event::ProfileLoadFailed(id)).await;
            report_api_error(ctx, "profile posts", &e).await;
        }
    }
}

/// Pull the viewer's current relationship to `id`. The API returns
/// an array; we want the single entry matching the input.
async fn load_relationship(ctx: &Ctx, id: AccountId) {
    match ctx.client.relationships(&[&id]).await {
        Ok(mut rels) if !rels.is_empty() => {
            send(&ctx.events, Event::RelationshipLoaded(rels.remove(0))).await;
            note_api_ok(ctx).await;
        }
        Ok(_) => {
            warn!(%id, "relationships returned empty array");
            note_api_ok(ctx).await;
        }
        Err(e) => report_api_error(ctx, "relationship", &e).await,
    }
}

/// Fetch a page of followers / following for `id`. UI matches the
/// reply by `(for_id, kind)` — both fields are echoed so two
/// concurrent fetches can't cross-pollute each other's state.
async fn load_account_list(
    ctx: &Ctx,
    id: AccountId,
    kind: AccountListKind,
    max_id: Option<String>,
) {
    let appended = max_id.is_some();
    let params = AccountListParams {
        max_id,
        limit: Some(PAGE_SIZE),
        ..Default::default()
    };
    let result = match kind {
        AccountListKind::Followers => ctx.client.account_followers(&id, &params).await,
        AccountListKind::Following => ctx.client.account_following(&id, &params).await,
    };
    match result {
        Ok(page) => {
            send(
                &ctx.events,
                Event::AccountListLoaded {
                    for_id: id,
                    kind,
                    accounts: page.items,
                    appended,
                },
            )
            .await;
            note_api_ok(ctx).await;
        }
        Err(e) => {
            send(
                &ctx.events,
                Event::AccountListLoadFailed { for_id: id, kind },
            )
            .await;
            report_api_error(ctx, kind.label(), &e).await;
        }
    }
}

/// Drive the follow / unfollow endpoint. Both return a fresh
/// `Relationship`, so on success we just funnel the same event the
/// UI already knows how to consume from `LoadRelationship`. On
/// failure, send a typed revert event so the optimistic UI flip can
/// reverse cleanly.
async fn follow_action(ctx: &Ctx, id: AccountId, attempted_follow: bool) {
    let result = if attempted_follow {
        ctx.client.follow(&id).await
    } else {
        ctx.client.unfollow(&id).await
    };
    match result {
        Ok(rel) => {
            send(&ctx.events, Event::RelationshipLoaded(rel)).await;
            note_api_ok(ctx).await;
        }
        Err(e) => {
            let verb = if attempted_follow {
                "follow"
            } else {
                "unfollow"
            };
            send(
                &ctx.events,
                Event::RelationshipActionFailed {
                    id: id.clone(),
                    attempted_follow,
                },
            )
            .await;
            report_api_error(ctx, verb, &e).await;
        }
    }
}

async fn status_action(
    ctx: &Ctx,
    id: StatusId,
    attempted: FailedAction,
    result: crate::api::ApiResult<Status>,
) {
    let verb = match attempted {
        FailedAction::Favourite => "favourite",
        FailedAction::Unfavourite => "unfavourite",
        FailedAction::Reblog => "reblog",
        FailedAction::Unreblog => "unreblog",
        FailedAction::Bookmark => "bookmark",
        FailedAction::Unbookmark => "unbookmark",
    };
    match result {
        Ok(status) => {
            send(&ctx.events, Event::StatusUpdated(status)).await;
            note_api_ok(ctx).await;
        }
        Err(e) => {
            send(
                &ctx.events,
                Event::StatusActionFailed {
                    id,
                    action: attempted,
                },
            )
            .await;
            report_api_error(ctx, verb, &e).await;
        }
    }
}

async fn send(tx: &mpsc::Sender<Event>, event: Event) {
    let _ = tx.send(event).await;
}

async fn toast(tx: &mpsc::Sender<Event>, level: ToastLevel, message: String) {
    send(tx, Event::Toast { level, message }).await;
}

/// Record a successful API round-trip. If health was previously
/// degraded / offline / auth-invalid, flips it back to Healthy and
/// broadcasts. Cheap no-op when already Healthy.
async fn note_api_ok(ctx: &Ctx) {
    let changed = {
        let mut st = lock(&ctx.state);
        if st.api_health == ApiHealth::Healthy {
            false
        } else {
            st.api_health = ApiHealth::Healthy;
            true
        }
    };
    if changed {
        send(&ctx.events, Event::ApiHealthChanged(ApiHealth::Healthy)).await;
    }
}

/// Transition health based on an error category (only fires the event
/// when the value actually changes).
async fn note_api_error(ctx: &Ctx, err: &ApiError) {
    let new_health = ApiHealth::from(err.category());
    let changed = {
        let mut st = lock(&ctx.state);
        if new_health != ApiHealth::Healthy && st.api_health != new_health {
            st.api_health = new_health;
            true
        } else {
            false
        }
    };
    if changed {
        send(&ctx.events, Event::ApiHealthChanged(new_health)).await;
    }
}

/// Full error-report flow: logs with `warn!`, emits a user-facing Toast
/// with a clean terse message, and bumps the health indicator. `verb`
/// is a short phrase describing what was being attempted (e.g.
/// `"favourite"` → `"favourite failed · network unreachable"`).
async fn report_api_error(ctx: &Ctx, verb: &str, err: &ApiError) {
    warn!(?err, %verb, "api call failed");
    let level = match err.category() {
        ApiErrorCategory::NotFound | ApiErrorCategory::Client => ToastLevel::Warn,
        _ => ToastLevel::Error,
    };
    toast(
        &ctx.events,
        level,
        format!("{verb} failed · {}", err.terse()),
    )
    .await;
    note_api_error(ctx, err).await;
}

/// `PUT /statuses/{id}`. The server returns the edited status, which
/// flows through the normal `StatusUpdated` patch path.
async fn edit_status(ctx: &Ctx, id: &StatusId, draft: &StatusDraft) {
    match ctx.client.edit_status(id, draft).await {
        Ok(status) => {
            send(&ctx.events, Event::StatusUpdated(status)).await;
            toast(&ctx.events, ToastLevel::Info, "edited".into()).await;
            note_api_ok(ctx).await;
        }
        Err(e) => report_api_error(ctx, "edit", &e).await,
    }
}

async fn post_status(ctx: &Ctx, draft: &StatusDraft) -> bool {
    let has_quote = draft.quote_id.is_some();
    match ctx.client.post_status(draft).await {
        Ok(status) => {
            let msg = if has_quote {
                "quote posted"
            } else if status.in_reply_to_id.is_some() {
                "reply sent"
            } else {
                "posted"
            };
            toast(&ctx.events, ToastLevel::Info, msg.into()).await;
            note_api_ok(ctx).await;
            true
        }
        Err(e) => {
            let verb = if has_quote { "quote" } else { "post" };
            report_api_error(ctx, verb, &e).await;
            false
        }
    }
}

/// Initial (or post-switch) session kick-off: verify the token and
/// learn the instance's `max_characters`, concurrently. Emits the same
/// events for first boot and account-switch so the UI reuses its
/// `CredentialsLoaded` / `InstanceLoaded` handlers.
async fn bootstrap_session(ctx: Ctx) {
    let (me, inst) = tokio::join!(ctx.client.verify_credentials(), ctx.client.instance());
    match me {
        Ok(me) => {
            send(&ctx.events, Event::CredentialsLoaded(me.clone())).await;
            lock(&ctx.state).me = Some(me);
            note_api_ok(&ctx).await;
        }
        Err(e) => {
            warn!(?e, "verify_credentials failed");
            report_api_error(&ctx, "sign-in check", &e).await;
        }
    }
    match inst {
        Ok(inst) => {
            if let Some(max) = inst.max_characters() {
                send(
                    &ctx.events,
                    Event::InstanceLoaded {
                        max_characters: max,
                    },
                )
                .await;
            }
            note_api_ok(&ctx).await;
        }
        Err(e) => {
            warn!(?e, "instance fetch failed");
            note_api_error(&ctx, &e).await;
        }
    }
}
