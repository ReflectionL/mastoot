//! Actions flow UI → state; Events flow state → UI.

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::api::ApiErrorCategory;
use crate::api::models::{Account, AccountId, Notification, Relationship, Status, StatusId};
use crate::state::TimelineKind;

/// Intent from the UI layer. Never carries ratatui types.
#[derive(Debug, Clone)]
pub enum Action {
    /// Fetch the initial page of a timeline.
    LoadTimeline(TimelineKind),
    /// Fetch older posts (pagination via `max_id`).
    LoadMore(TimelineKind),
    /// Hard refresh (ignore cache, replace timeline).
    Refresh(TimelineKind),
    /// Open a status' full detail (ancestors + descendants).
    OpenStatus(StatusId),
    /// Fetch a user's profile: account header plus a page of their
    /// statuses. `max_id` Some → paginate older; None → fresh first
    /// page (replaces the current cache).
    LoadProfile {
        id: AccountId,
        max_id: Option<String>,
    },
    /// Fetch the viewer ↔ target relationship (following / followed_by /
    /// muted / blocked / requested). Used to render the follow chip
    /// on a profile page.
    LoadRelationship(AccountId),
    /// POST /accounts/{id}/follow.
    Follow(AccountId),
    /// POST /accounts/{id}/unfollow.
    Unfollow(AccountId),
    /// Fetch a page of followers / following for a given user.
    LoadAccountList {
        id: AccountId,
        kind: AccountListKind,
        max_id: Option<String>,
    },
    /// Toggle favourite.
    Favourite(StatusId),
    Unfavourite(StatusId),
    /// Toggle boost.
    Reblog(StatusId),
    Unreblog(StatusId),
    /// Bookmark / unbookmark.
    Bookmark(StatusId),
    Unbookmark(StatusId),
    /// DELETE /api/v1/statuses/{id}. Only dispatched after UI-side
    /// ownership check + user confirmation. Server-side Mastodon
    /// returns the deleted status; we surface only the id via
    /// [`Event::StatusDeleted`] on success.
    DeleteStatus(StatusId),
    /// Post a new status.
    Compose {
        text: String,
        in_reply_to_id: Option<StatusId>,
        /// Mastodon 4.5+ native quote target. `None` for plain /
        /// reply posts.
        quote_id: Option<StatusId>,
        content_warning: Option<String>,
        sensitive: bool,
        visibility: Visibility,
    },
    /// User toggled live-update mode. The state task spawns / aborts
    /// the appropriate background loop and broadcasts a new
    /// [`StreamState`] so the UI indicator stays in sync.
    SetStreamMode(StreamMode),
    /// User picked a different account. State task rebuilds the
    /// [`MastodonClient`], resets cached state, re-verifies
    /// credentials, and kicks a fresh Home fetch. UI is expected to
    /// wipe its local timeline / notification caches and navigate
    /// back to the Home tab; fresh data arrives through the usual
    /// `TimelineUpdated` / `CredentialsLoaded` events.
    SwitchAccount {
        instance: String,
        handle: String,
        token: SecretString,
    },
    /// Polling tick — fetch any statuses newer than what we already
    /// have in `kind` and prepend them. Fired by the internal polling
    /// loop; not meant for UI to emit directly.
    FetchNewer(TimelineKind),
    /// User asked to quit.
    Quit,
}

/// Update from the state layer, to be rendered by the UI.
#[derive(Debug, Clone)]
pub enum Event {
    /// One or more statuses arrived (prepend or append depending on kind).
    TimelineUpdated {
        kind: TimelineKind,
        statuses: Vec<Status>,
        appended: bool,
    },
    /// A single fresh status arrived from the SSE user stream. UI
    /// prepends it to the relevant timeline (deduped by id) and nudges
    /// the selection so the cursor keeps tracking its previous target.
    TimelineStatusAdded {
        kind: TimelineKind,
        status: Status,
    },
    StatusUpdated(Status),
    StatusDeleted(StatusId),
    /// `/api/v1/notifications` fetched. `appended` matches the timeline
    /// semantics — `false` replaces, `true` appends older entries.
    NotificationsUpdated {
        items: Vec<Notification>,
        appended: bool,
    },
    /// `/api/v1/statuses/{id}/context` reply for a focal status the UI is
    /// showing in detail view. Carries the surrounding thread.
    StatusContext {
        focal_id: StatusId,
        ancestors: Vec<Status>,
        descendants: Vec<Status>,
    },
    /// `/api/v2/instance.configuration.statuses.max_characters` arrived;
    /// UI should rebuild compose budgets that were using the default.
    InstanceLoaded {
        max_characters: u32,
    },
    /// `verify_credentials` finished — UI now knows who "me" is.
    CredentialsLoaded(Account),
    /// Result of [`Action::LoadProfile`]. The UI matches by `account.id`
    /// so a stale reply doesn't overwrite a freshly opened other-user
    /// profile.
    ProfileLoaded {
        account: Account,
        statuses: Vec<Status>,
        appended: bool,
    },
    /// Result of [`Action::LoadRelationship`] *or* a successful
    /// follow / unfollow round-trip (both API endpoints return the
    /// updated relationship). Matched against the open profile by id.
    RelationshipLoaded(Relationship),
    /// A follow / unfollow attempt failed server-side; UI should
    /// reverse the optimistic state on the matching profile.
    RelationshipActionFailed {
        id: AccountId,
        attempted_follow: bool,
    },
    /// Result of [`Action::LoadAccountList`]. UI matches by `(for_id,
    /// kind)` to avoid stale replies overwriting a freshly opened
    /// account list.
    AccountListLoaded {
        for_id: AccountId,
        kind: AccountListKind,
        accounts: Vec<Account>,
        appended: bool,
    },
    /// A status-level action (`favourite`, `reblog`, …) that the UI
    /// applied optimistically failed server-side. The UI should reverse
    /// the local state change.
    StatusActionFailed {
        id: StatusId,
        action: FailedAction,
    },
    NotificationReceived(Notification),
    /// A one-shot human-readable toast.
    Toast {
        level: ToastLevel,
        message: String,
    },
    /// State task successfully switched to a different account. UI
    /// uses this to show a confirmation toast; the actual data
    /// refresh already started.
    AccountSwitched {
        handle: String,
    },
    /// SSE stream connection state (Phase 4 · B).
    StreamState(StreamState),
    /// REST API health — broadcast only when it changes. Drives the
    /// status-bar dot tint: healthy → dim, degraded → amber, offline →
    /// red, auth-invalid → red with an `! login?` hint.
    ApiHealthChanged(ApiHealth),
}

/// Identifies which optimistic flip the UI needs to undo when a status
/// action fails. Every variant maps to "the action that was attempted",
/// so undoing means applying the *opposite* flag and counter delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedAction {
    Favourite,
    Unfavourite,
    Reblog,
    Unreblog,
    Bookmark,
    Unbookmark,
}

/// Which side of the account graph an account-list page represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountListKind {
    Followers,
    Following,
}

impl AccountListKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Followers => "followers",
            Self::Following => "following",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
}

/// How the client receives live updates. Cycled at runtime with `S`
/// and persisted via `[ui] stream_mode` in the config file.
///
/// - `Streaming`: SSE push, near-realtime, reconnect on drop.
/// - `Polling`: periodic `since_id` fetch (30 s by default). Less
///   chatty; works behind proxies that buffer long-lived HTTP.
/// - `Off`: no background updates. User hits `R` to refresh manually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamMode {
    /// Full SSE user-stream.
    #[default]
    #[serde(alias = "on", alias = "streaming", alias = "sse")]
    Streaming,
    /// Poll `since_id` on an interval.
    #[serde(alias = "poll")]
    Polling,
    /// No background updates — manual refresh only.
    Off,
}

impl StreamMode {
    /// Next mode in the cycle `Streaming → Polling → Off → Streaming`.
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::Streaming => Self::Polling,
            Self::Polling => Self::Off,
            Self::Off => Self::Streaming,
        }
    }

    /// Short word for the help overlay.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Polling => "polling",
            Self::Off => "off",
        }
    }
}

/// Overall REST health, orthogonal to the SSE [`StreamState`]. Derived
/// from the category of the most recent API result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApiHealth {
    /// Last call succeeded (or nothing has been attempted yet).
    #[default]
    Healthy,
    /// 429 rate limited or a single 5xx — transient, likely self-heals.
    Degraded,
    /// DNS / TCP / TLS / timeout — we can't reach the server at all.
    Offline,
    /// 401 — the user's token is no longer valid.
    AuthInvalid,
}

impl From<ApiErrorCategory> for ApiHealth {
    fn from(c: ApiErrorCategory) -> Self {
        match c {
            ApiErrorCategory::Offline | ApiErrorCategory::Timeout => Self::Offline,
            ApiErrorCategory::RateLimited | ApiErrorCategory::ServerError => Self::Degraded,
            ApiErrorCategory::Unauthorized => Self::AuthInvalid,
            // NotFound / Client errors don't signal that the link is
            // broken — they're logical errors (bad id, bad payload).
            ApiErrorCategory::NotFound | ApiErrorCategory::Client => Self::Healthy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Unlisted,
    Private,
    Direct,
}

impl Visibility {
    #[must_use]
    pub fn as_api_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Unlisted => "unlisted",
            Self::Private => "private",
            Self::Direct => "direct",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_errors_dont_degrade_health() {
        // A 404 or malformed-JSON is about the request, not the link.
        // The user should still see a green dot.
        assert_eq!(
            ApiHealth::from(ApiErrorCategory::NotFound),
            ApiHealth::Healthy
        );
        assert_eq!(
            ApiHealth::from(ApiErrorCategory::Client),
            ApiHealth::Healthy
        );
    }

    #[test]
    fn transport_errors_flip_to_offline() {
        assert_eq!(
            ApiHealth::from(ApiErrorCategory::Offline),
            ApiHealth::Offline
        );
        assert_eq!(
            ApiHealth::from(ApiErrorCategory::Timeout),
            ApiHealth::Offline
        );
    }

    #[test]
    fn rate_limit_and_server_err_are_degraded_not_offline() {
        assert_eq!(
            ApiHealth::from(ApiErrorCategory::RateLimited),
            ApiHealth::Degraded
        );
        assert_eq!(
            ApiHealth::from(ApiErrorCategory::ServerError),
            ApiHealth::Degraded
        );
    }

    #[test]
    fn unauthorized_maps_to_auth_invalid() {
        assert_eq!(
            ApiHealth::from(ApiErrorCategory::Unauthorized),
            ApiHealth::AuthInvalid
        );
    }

    #[test]
    fn stream_mode_cycles_through_three_states_back_to_start() {
        let m = StreamMode::Streaming;
        let m = m.cycle();
        assert_eq!(m, StreamMode::Polling);
        let m = m.cycle();
        assert_eq!(m, StreamMode::Off);
        let m = m.cycle();
        assert_eq!(m, StreamMode::Streaming);
    }

    #[test]
    fn stream_mode_default_is_streaming() {
        assert_eq!(StreamMode::default(), StreamMode::Streaming);
    }
}
