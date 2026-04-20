//! Background task that owns the [`MastodonClient`] and [`AppState`].
//!
//! The UI never awaits on network calls directly. It sends [`Action`]s
//! over an mpsc channel and consumes [`Event`]s off another. This keeps
//! the render loop snappy and localizes the client's lifetime.

use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::api::endpoints::{
    AccountListParams, AccountStatusesParams, NotificationParams, StatusDraft, TimelineParams,
};
use crate::api::error::ApiErrorCategory;
use crate::api::models::{Status, StatusId, Visibility as ApiVisibility};
use crate::api::streaming::{StreamEvent, UserStream};
use crate::api::{ApiError, MastodonClient};
use crate::state::app::AppState;
use crate::state::event::{
    AccountListKind, Action, ApiHealth, Event, FailedAction, StreamMode, StreamState, ToastLevel,
    Visibility,
};
use crate::state::timeline::TimelineKind;

const ACTION_CAP: usize = 64;
const EVENT_CAP: usize = 256;
const PAGE_SIZE: u32 = 40;
/// Reconnect backoff grows 1 → 2 → 4 → 8 → 16 → 30 (capped). Resets to
/// 1 on a successful open.
const STREAM_BACKOFF_MIN: Duration = Duration::from_secs(1);
const STREAM_BACKOFF_MAX: Duration = Duration::from_secs(30);

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
    // re-enter the main loop's match arms via Action. Never handed to
    // the UI.
    let internal_tx = action_tx.clone();
    let task = tokio::spawn(run(client, action_rx, internal_tx, event_tx));
    Handle {
        actions: action_tx,
        events: event_rx,
        task,
    }
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
    async fn set(
        &mut self,
        new: StreamMode,
        client: &MastodonClient,
        actions: &mpsc::Sender<Action>,
        events: &mpsc::Sender<Event>,
    ) {
        if new == self.mode && self.handle.is_some() {
            return;
        }
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        self.mode = new;
        self.handle = match new {
            StreamMode::Streaming if client.token().is_some() => {
                Some(tokio::spawn(streaming_loop(client.clone(), events.clone())))
            }
            StreamMode::Polling => {
                Some(tokio::spawn(polling_loop(actions.clone(), POLLING_PERIOD)))
            }
            // Streaming-but-no-token collapses to Off — Mastodon rejects
            // anonymous streams, so there's nothing to spawn.
            StreamMode::Streaming | StreamMode::Off => {
                let _ = events
                    .send(Event::StreamState(StreamState::Disconnected))
                    .await;
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

/// Polling cadence. Matches the background tick (toast decay / relative
/// timestamps) — no point polling faster than the UI can paint.
const POLLING_PERIOD: Duration = Duration::from_secs(30);

async fn run(
    client: MastodonClient,
    mut actions: mpsc::Receiver<Action>,
    actions_tx: mpsc::Sender<Action>,
    events: mpsc::Sender<Event>,
) {
    let mut client = client;
    let mut state = AppState::new();
    // Live-update slot — starts idle. The UI sends SetStreamMode as
    // its first action so the initial mode comes from config, not a
    // hardcoded default.
    let mut live = LiveUpdateSlot::idle();

    bootstrap_session(&client, &mut state, &events).await;

    while let Some(action) = actions.recv().await {
        match action {
            Action::LoadTimeline(kind) | Action::Refresh(kind) => {
                if matches!(kind, TimelineKind::Notifications) {
                    state.notifications_oldest = None;
                }
                load_timeline(&client, &mut state, &events, kind, None, false).await;
            }
            Action::LoadMore(kind) => {
                let max_id = if matches!(kind, TimelineKind::Notifications) {
                    state.notifications_oldest.clone().map(|id| id.0)
                } else {
                    state
                        .timeline(kind)
                        .and_then(|t| t.oldest_id().cloned())
                        .map(|id| id.0)
                };
                load_timeline(&client, &mut state, &events, kind, max_id, true).await;
            }
            Action::Favourite(id) => {
                let r = client.favourite(&id).await;
                status_action(&events, &mut state, id, FailedAction::Favourite, r).await;
            }
            Action::Unfavourite(id) => {
                let r = client.unfavourite(&id).await;
                status_action(&events, &mut state, id, FailedAction::Unfavourite, r).await;
            }
            Action::Reblog(id) => {
                let r = client.reblog(&id).await;
                status_action(&events, &mut state, id, FailedAction::Reblog, r).await;
            }
            Action::Unreblog(id) => {
                let r = client.unreblog(&id).await;
                status_action(&events, &mut state, id, FailedAction::Unreblog, r).await;
            }
            Action::Bookmark(id) => {
                let r = client.bookmark(&id).await;
                status_action(&events, &mut state, id, FailedAction::Bookmark, r).await;
            }
            Action::DeleteStatus(id) => match client.delete_status(&id).await {
                Ok(_) => {
                    send(&events, Event::StatusDeleted(id)).await;
                    send(
                        &events,
                        Event::Toast {
                            level: ToastLevel::Info,
                            message: "deleted".into(),
                        },
                    )
                    .await;
                    note_api_ok(&events, &mut state).await;
                }
                Err(e) => {
                    report_api_error(&events, &mut state, "delete", &e).await;
                }
            },
            Action::Unbookmark(id) => {
                let r = client.unbookmark(&id).await;
                status_action(&events, &mut state, id, FailedAction::Unbookmark, r).await;
            }
            Action::Compose {
                text,
                in_reply_to_id,
                quote_id,
                content_warning,
                sensitive,
                visibility,
            } => {
                let posted = post_status(
                    &client,
                    &mut state,
                    &events,
                    text,
                    in_reply_to_id,
                    quote_id,
                    content_warning,
                    sensitive,
                    visibility,
                )
                .await;
                if posted {
                    // Pull a fresh home timeline so the just-posted
                    // status shows up immediately.
                    load_timeline(
                        &client,
                        &mut state,
                        &events,
                        TimelineKind::Home,
                        None,
                        false,
                    )
                    .await;
                }
            }
            Action::LoadProfile { id, max_id } => {
                load_profile(&client, &mut state, &events, id, max_id).await;
            }
            Action::LoadRelationship(id) => {
                load_relationship(&client, &mut state, &events, id).await;
            }
            Action::Follow(id) => {
                follow_action(&client, &mut state, &events, id, true).await;
            }
            Action::Unfollow(id) => {
                follow_action(&client, &mut state, &events, id, false).await;
            }
            Action::LoadAccountList { id, kind, max_id } => {
                load_account_list(&client, &mut state, &events, id, kind, max_id).await;
            }
            Action::OpenStatus(id) => match client.status_context(&id).await {
                Ok(ctx) => {
                    send(
                        &events,
                        Event::StatusContext {
                            focal_id: id,
                            ancestors: ctx.ancestors,
                            descendants: ctx.descendants,
                        },
                    )
                    .await;
                    note_api_ok(&events, &mut state).await;
                }
                Err(e) => {
                    report_api_error(&events, &mut state, "thread", &e).await;
                }
            },
            Action::SetStreamMode(mode) => {
                live.set(mode, &client, &actions_tx, &events).await;
            }
            Action::SwitchAccount {
                instance,
                handle,
                token,
            } => {
                let new_client = match MastodonClient::new(&instance, token) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(?e, %handle, "failed to build client for account switch");
                        send(
                            &events,
                            Event::Toast {
                                level: ToastLevel::Error,
                                message: format!("switch failed · {}", e.terse()),
                            },
                        )
                        .await;
                        continue;
                    }
                };
                // Tear down the live-update task first — it holds a
                // cloned handle of the outgoing client.
                live.shutdown();
                client = new_client;
                // Reset server-derived state. Everything else (back_stack,
                // profile caches, toast queue) lives UI-side and the
                // AccountSwitched event tells the UI to wipe there too.
                state = AppState::new();

                send(
                    &events,
                    Event::AccountSwitched {
                        handle: handle.clone(),
                    },
                )
                .await;
                send(
                    &events,
                    Event::Toast {
                        level: ToastLevel::Info,
                        message: format!("switched to {handle}"),
                    },
                )
                .await;

                bootstrap_session(&client, &mut state, &events).await;

                // Re-arm live updates with whatever mode the UI was
                // last running in. If the UI hadn't sent SetStreamMode
                // yet, this remains Off.
                let mode = live.mode;
                live.set(mode, &client, &actions_tx, &events).await;

                // Kick a fresh Home fetch so the timeline paints
                // immediately — UI already cleared its caches on the
                // AccountSwitched event.
                load_timeline(
                    &client,
                    &mut state,
                    &events,
                    TimelineKind::Home,
                    None,
                    false,
                )
                .await;
            }
            Action::FetchNewer(kind) => {
                fetch_newer(&client, &mut state, &events, kind).await;
            }
            Action::Quit => break,
        }
    }
    live.shutdown();
    debug!("state task exiting");
}

/// Called by the polling loop every [`POLLING_PERIOD`] seconds. Fetches
/// any statuses newer than the current store's newest_id and prepends
/// each one via `TimelineStatusAdded` — same event SSE updates emit.
async fn fetch_newer(
    client: &MastodonClient,
    state: &mut AppState,
    events: &mpsc::Sender<Event>,
    kind: TimelineKind,
) {
    let since_id = state
        .timeline(kind)
        .and_then(|t| t.newest_id().cloned())
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
        TimelineKind::Home => client.home_timeline(&params).await,
        TimelineKind::Local | TimelineKind::Federated => client.public_timeline(&params).await,
        _ => return, // polling other kinds not supported for now
    };
    match result {
        Ok(page) => {
            note_api_ok(events, state).await;
            let store = state.timeline_mut(kind);
            // Mastodon returns newest-first; iterate reversed so the
            // oldest-new item arrives first and the final prepend sits
            // at the top.
            for status in page.items.into_iter().rev() {
                if store.update(status.clone()) {
                    continue; // already had it
                }
                store.prepend(vec![status.clone()]);
                let _ = events
                    .send(Event::TimelineStatusAdded { kind, status })
                    .await;
            }
        }
        Err(e) => {
            // Polling failures are quiet: the dot dims, no toast spam.
            note_api_error(events, state, &e).await;
        }
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
        if actions
            .send(Action::FetchNewer(TimelineKind::Home))
            .await
            .is_err()
        {
            // main loop is gone — bail
            break;
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
async fn streaming_loop(client: MastodonClient, events: mpsc::Sender<Event>) {
    let mut backoff = STREAM_BACKOFF_MIN;
    loop {
        let _ = events
            .send(Event::StreamState(StreamState::Connecting))
            .await;
        match UserStream::open(&client).await {
            Ok(mut s) => {
                let _ = events
                    .send(Event::StreamState(StreamState::Connected))
                    .await;
                backoff = STREAM_BACKOFF_MIN;
                while let Some(ev) = s.next().await {
                    if !dispatch_stream_event(&events, ev).await {
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
        let _ = events
            .send(Event::StreamState(StreamState::Reconnecting))
            .await;
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(STREAM_BACKOFF_MAX);
    }
}

/// Translate a single [`StreamEvent`] into the right UI event(s).
/// Returns `false` on Disconnect (signals the caller to reconnect);
/// otherwise `true` regardless of whether the event was emitted.
async fn dispatch_stream_event(events: &mpsc::Sender<Event>, ev: StreamEvent) -> bool {
    match ev {
        StreamEvent::Update(status) => {
            let _ = events
                .send(Event::TimelineStatusAdded {
                    kind: TimelineKind::Home,
                    status: *status,
                })
                .await;
        }
        StreamEvent::Delete(id) => {
            let _ = events.send(Event::StatusDeleted(id)).await;
        }
        StreamEvent::Notification(n) => {
            let _ = events.send(Event::NotificationReceived(*n)).await;
        }
        StreamEvent::StatusUpdate(status) => {
            let _ = events.send(Event::StatusUpdated(*status)).await;
        }
        StreamEvent::Disconnect => return false,
        // Phase 4 · B scope: ignore filters / announcements /
        // conversations / unknown — they're fine to drop for MVP and
        // can be surfaced later without touching the reconnect loop.
        _ => {}
    }
    true
}

async fn load_timeline(
    client: &MastodonClient,
    state: &mut AppState,
    events: &mpsc::Sender<Event>,
    kind: TimelineKind,
    max_id: Option<String>,
    appended: bool,
) {
    let params = TimelineParams {
        max_id,
        limit: Some(PAGE_SIZE),
        local: matches!(kind, TimelineKind::Local),
        ..Default::default()
    };
    let result = match kind {
        TimelineKind::Home => client.home_timeline(&params).await,
        TimelineKind::Local | TimelineKind::Federated => client.public_timeline(&params).await,
        TimelineKind::Notifications => {
            load_notifications(client, state, events, params.max_id, appended).await;
            return;
        }
        _ => {
            send(
                events,
                Event::Toast {
                    level: ToastLevel::Info,
                    message: format!("{kind:?}: coming in phase 3"),
                },
            )
            .await;
            return;
        }
    };
    match result {
        Ok(page) => {
            let store = state.timeline_mut(kind);
            if appended {
                store.append(page.items.clone());
            } else {
                store.replace(page.items.clone());
            }
            send(
                events,
                Event::TimelineUpdated {
                    kind,
                    statuses: page.items,
                    appended,
                },
            )
            .await;
            note_api_ok(events, state).await;
        }
        Err(e) => {
            let verb = format!("{} timeline", timeline_label(kind));
            report_api_error(events, state, &verb, &e).await;
        }
    }
}

fn timeline_label(kind: TimelineKind) -> &'static str {
    match kind {
        TimelineKind::Home => "home",
        TimelineKind::Local => "local",
        TimelineKind::Federated => "federated",
        TimelineKind::Notifications => "notifications",
        TimelineKind::Profile => "profile",
        TimelineKind::Favourites => "favourites",
        TimelineKind::Bookmarks => "bookmarks",
    }
}

/// Fetch a page of notifications. The state task tracks only the
/// oldest-seen id so `LoadMore` can paginate; the full list lives in
/// the UI layer (notifications aren't shared across screens the way
/// statuses are, so a separate store on AppState would be dead weight).
async fn load_notifications(
    client: &MastodonClient,
    state: &mut AppState,
    events: &mpsc::Sender<Event>,
    max_id: Option<String>,
    appended: bool,
) {
    let params = NotificationParams {
        max_id,
        limit: Some(PAGE_SIZE),
        ..Default::default()
    };
    match client.notifications(&params).await {
        Ok(page) => {
            if let Some(last) = page.items.last() {
                state.notifications_oldest = Some(last.id.clone());
            }
            send(
                events,
                Event::NotificationsUpdated {
                    items: page.items,
                    appended,
                },
            )
            .await;
            note_api_ok(events, state).await;
        }
        Err(e) => {
            report_api_error(events, state, "notifications", &e).await;
        }
    }
}

/// Fetch a profile (account + statuses page). When `max_id` is `Some`
/// we skip the account refetch — header is already on screen and the
/// fresh page is what the user is paginating into.
async fn load_profile(
    client: &MastodonClient,
    state: &mut AppState,
    events: &mpsc::Sender<Event>,
    id: crate::api::models::AccountId,
    max_id: Option<String>,
) {
    let appended = max_id.is_some();
    let account = if appended {
        // Don't bother refetching on pagination.
        None
    } else {
        match client.account(&id).await {
            Ok(a) => {
                note_api_ok(events, state).await;
                Some(a)
            }
            Err(e) => {
                report_api_error(events, state, "profile", &e).await;
                return;
            }
        }
    };
    let params = AccountStatusesParams {
        max_id,
        limit: Some(PAGE_SIZE),
        ..Default::default()
    };
    match client.account_statuses(&id, &params).await {
        Ok(page) => {
            // For pagination calls we don't have an Account on hand;
            // synthesize a stub with just the id so the UI can match
            // the event back to the open profile. UI ignores the rest.
            let acc = account.unwrap_or_else(|| crate::api::models::Account {
                id: id.clone(),
                ..Default::default()
            });
            send(
                events,
                Event::ProfileLoaded {
                    account: acc,
                    statuses: page.items,
                    appended,
                },
            )
            .await;
            note_api_ok(events, state).await;
        }
        Err(e) => {
            report_api_error(events, state, "profile posts", &e).await;
        }
    }
}

/// Pull the viewer's current relationship to `id`. The API returns
/// an array; we want the single entry matching the input.
async fn load_relationship(
    client: &MastodonClient,
    state: &mut AppState,
    events: &mpsc::Sender<Event>,
    id: crate::api::models::AccountId,
) {
    match client.relationships(&[&id]).await {
        Ok(mut rels) if !rels.is_empty() => {
            send(events, Event::RelationshipLoaded(rels.remove(0))).await;
            note_api_ok(events, state).await;
        }
        Ok(_) => {
            warn!(%id, "relationships returned empty array");
            note_api_ok(events, state).await;
        }
        Err(e) => {
            report_api_error(events, state, "relationship", &e).await;
        }
    }
}

/// Fetch a page of followers / following for `id`. UI matches the
/// reply by `(for_id, kind)` — both fields are echoed so two
/// concurrent fetches (e.g., user opens followers then quickly
/// switches to following) can't cross-pollute each other's state.
async fn load_account_list(
    client: &MastodonClient,
    state: &mut AppState,
    events: &mpsc::Sender<Event>,
    id: crate::api::models::AccountId,
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
        AccountListKind::Followers => client.account_followers(&id, &params).await,
        AccountListKind::Following => client.account_following(&id, &params).await,
    };
    match result {
        Ok(page) => {
            send(
                events,
                Event::AccountListLoaded {
                    for_id: id,
                    kind,
                    accounts: page.items,
                    appended,
                },
            )
            .await;
            note_api_ok(events, state).await;
        }
        Err(e) => {
            report_api_error(events, state, kind.label(), &e).await;
        }
    }
}

/// Drive the follow / unfollow endpoint. Both return a fresh
/// `Relationship`, so on success we just funnel the same event the
/// UI already knows how to consume from `LoadRelationship`. On
/// failure, send a typed revert event so the optimistic UI flip can
/// reverse cleanly.
async fn follow_action(
    client: &MastodonClient,
    state: &mut AppState,
    events: &mpsc::Sender<Event>,
    id: crate::api::models::AccountId,
    attempted_follow: bool,
) {
    let result = if attempted_follow {
        client.follow(&id).await
    } else {
        client.unfollow(&id).await
    };
    match result {
        Ok(rel) => {
            send(events, Event::RelationshipLoaded(rel)).await;
            note_api_ok(events, state).await;
        }
        Err(e) => {
            let verb = if attempted_follow {
                "follow"
            } else {
                "unfollow"
            };
            send(
                events,
                Event::RelationshipActionFailed {
                    id: id.clone(),
                    attempted_follow,
                },
            )
            .await;
            report_api_error(events, state, verb, &e).await;
        }
    }
}

async fn status_action(
    events: &mpsc::Sender<Event>,
    state: &mut AppState,
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
            send(events, Event::StatusUpdated(status)).await;
            note_api_ok(events, state).await;
        }
        Err(e) => {
            send(
                events,
                Event::StatusActionFailed {
                    id,
                    action: attempted,
                },
            )
            .await;
            report_api_error(events, state, verb, &e).await;
        }
    }
}

async fn send(tx: &mpsc::Sender<Event>, event: Event) {
    let _ = tx.send(event).await;
}

/// Record a successful API round-trip. If health was previously
/// degraded / offline / auth-invalid, flips it back to Healthy and
/// broadcasts. Cheap no-op when already Healthy.
async fn note_api_ok(events: &mpsc::Sender<Event>, state: &mut AppState) {
    if state.api_health != ApiHealth::Healthy {
        state.api_health = ApiHealth::Healthy;
        send(events, Event::ApiHealthChanged(ApiHealth::Healthy)).await;
    }
}

/// Transition health based on an error category (only fires the event
/// when the value actually changes).
async fn note_api_error(events: &mpsc::Sender<Event>, state: &mut AppState, err: &ApiError) {
    let new_health = ApiHealth::from(err.category());
    if new_health != ApiHealth::Healthy && state.api_health != new_health {
        state.api_health = new_health;
        send(events, Event::ApiHealthChanged(new_health)).await;
    }
}

/// Full error-report flow: logs with `warn!`, emits a user-facing Toast
/// with a clean terse message, and bumps the health indicator. `verb`
/// is a short phrase describing what was being attempted (e.g.
/// `"favourite"` → `"favourite failed · network unreachable"`).
async fn report_api_error(
    events: &mpsc::Sender<Event>,
    state: &mut AppState,
    verb: &str,
    err: &ApiError,
) {
    warn!(?err, %verb, "api call failed");
    let level = match err.category() {
        ApiErrorCategory::NotFound | ApiErrorCategory::Client => ToastLevel::Warn,
        _ => ToastLevel::Error,
    };
    send(
        events,
        Event::Toast {
            level,
            message: format!("{verb} failed · {}", err.terse()),
        },
    )
    .await;
    note_api_error(events, state, err).await;
}

#[allow(clippy::too_many_arguments)]
async fn post_status(
    client: &MastodonClient,
    state: &mut AppState,
    events: &mpsc::Sender<Event>,
    text: String,
    in_reply_to_id: Option<StatusId>,
    quote_id: Option<StatusId>,
    content_warning: Option<String>,
    sensitive: bool,
    visibility: Visibility,
) -> bool {
    let has_quote = quote_id.is_some();
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

    match client.post_status(&draft).await {
        Ok(status) => {
            let msg = if has_quote {
                "quote posted"
            } else if status.in_reply_to_id.is_some() {
                "reply sent"
            } else {
                "posted"
            };
            send(
                events,
                Event::Toast {
                    level: ToastLevel::Info,
                    message: msg.into(),
                },
            )
            .await;
            note_api_ok(events, state).await;
            true
        }
        Err(e) => {
            let verb = if has_quote { "quote" } else { "post" };
            report_api_error(events, state, verb, &e).await;
            false
        }
    }
}

/// Initial (or post-switch) session kick-off: verify the token, learn
/// the instance's `max_characters`. Emits the same events the startup
/// path does so the UI can reuse `CredentialsLoaded` / `InstanceLoaded`
/// handlers for both first boot and account-switch.
async fn bootstrap_session(
    client: &MastodonClient,
    state: &mut AppState,
    events: &mpsc::Sender<Event>,
) {
    match client.verify_credentials().await {
        Ok(me) => {
            send(events, Event::CredentialsLoaded(me.clone())).await;
            state.me = Some(me);
            note_api_ok(events, state).await;
        }
        Err(e) => {
            warn!(?e, "verify_credentials failed");
            report_api_error(events, state, "sign-in check", &e).await;
        }
    }
    match client.instance().await {
        Ok(inst) => {
            if let Some(max) = inst.max_characters() {
                send(
                    events,
                    Event::InstanceLoaded {
                        max_characters: max,
                    },
                )
                .await;
            }
            note_api_ok(events, state).await;
        }
        Err(e) => {
            warn!(?e, "instance fetch failed");
            note_api_error(events, state, &e).await;
        }
    }
}
