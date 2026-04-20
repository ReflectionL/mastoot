//! Mastodon entity models, decoded as pure `serde` structs.
//!
//! Field shapes follow <https://docs.joinmastodon.org/entities/>. Every id
//! is typed as a newtype around `String` — Mastodon uses Snowflake ids
//! that overflow JavaScript's `Number` and are deliberately returned as
//! strings. Don't `parse()` them.
//!
//! Anything that might realistically be absent on some deployment is
//! wrapped in `Option`. Fields documented as "at least from version X" or
//! "added in 4.y.z" are also `Option`, so older servers and forks
//! (Pleroma, GoToSocial, Firefish) still deserialize.

#![allow(clippy::struct_excessive_bools)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Id newtypes — every Mastodon id is an opaque string.
// ---------------------------------------------------------------------------

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

id_newtype!(StatusId);
id_newtype!(AccountId);
id_newtype!(NotificationId);
id_newtype!(MediaId);
id_newtype!(ListId);
id_newtype!(PollId);
id_newtype!(TagId);
id_newtype!(ReportId);
id_newtype!(FilterId);
id_newtype!(MarkerId);

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub username: String,
    /// `user@instance` for remote accounts; bare `user` for local.
    pub acct: String,
    #[serde(default)]
    pub url: Option<String>,
    /// ActivityPub actor URI; non-null even when `url` is null (suspended
    /// / deleted remote accounts).
    #[serde(default)]
    pub uri: Option<String>,
    pub display_name: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub avatar: String,
    #[serde(default)]
    pub avatar_static: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub header_static: String,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub bot: bool,
    #[serde(default)]
    pub discoverable: Option<bool>,
    #[serde(default)]
    pub group: Option<bool>,
    #[serde(default)]
    pub noindex: Option<bool>,
    /// Server-local creation timestamp. Some forks return date-only,
    /// hence the tolerant deserializer.
    #[serde(default, with = "opt_datetime")]
    pub created_at: Option<DateTime<Utc>>,
    /// Server-provided date, in `YYYY-MM-DD` form (not a datetime). Stored
    /// verbatim; parse in UI if you need a chrono type.
    #[serde(default)]
    pub last_status_at: Option<String>,
    #[serde(default)]
    pub statuses_count: u64,
    #[serde(default)]
    pub followers_count: u64,
    #[serde(default)]
    pub following_count: u64,
    #[serde(default)]
    pub fields: Vec<AccountField>,
    #[serde(default)]
    pub emojis: Vec<CustomEmoji>,
    /// Set when the account has moved to a new address.
    #[serde(default)]
    pub moved: Option<Box<Account>>,
    #[serde(default)]
    pub suspended: Option<bool>,
    #[serde(default)]
    pub limited: Option<bool>,
    #[serde(default)]
    pub memorial: Option<bool>,
    #[serde(default)]
    pub indexable: Option<bool>,
    /// v4.5+. Whether media should be shown inline on profile grid.
    #[serde(default)]
    pub show_media: Option<bool>,
    #[serde(default)]
    pub show_media_replies: Option<bool>,
    #[serde(default)]
    pub show_featured: Option<bool>,
    /// List of roles; local accounts only.
    #[serde(default)]
    pub roles: Option<Vec<AccountRole>>,
    /// Present only on `/api/v1/accounts/verify_credentials`.
    #[serde(default)]
    pub source: Option<AccountSource>,
    /// Present on the same endpoint.
    #[serde(default)]
    pub role: Option<AccountRole>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountField {
    pub name: String,
    pub value: String,
    #[serde(default, with = "opt_datetime")]
    pub verified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSource {
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub fields: Vec<AccountField>,
    #[serde(default)]
    pub privacy: Option<String>,
    #[serde(default)]
    pub sensitive: Option<bool>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub follow_requests_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRole {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub permissions: Option<String>,
    #[serde(default)]
    pub highlighted: bool,
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub id: StatusId,
    pub uri: String,
    #[serde(default)]
    pub url: Option<String>,
    pub account: Account,
    /// HTML. Render via [`crate::api::html`], never with `println!`.
    #[serde(default)]
    pub content: String,
    #[serde(default, with = "opt_datetime")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, with = "opt_datetime")]
    pub edited_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub replies_count: u64,
    #[serde(default)]
    pub reblogs_count: u64,
    #[serde(default)]
    pub favourites_count: u64,
    /// v4.5+. Number of quote-posts referencing this status.
    #[serde(default)]
    pub quotes_count: Option<u64>,
    #[serde(default)]
    pub in_reply_to_id: Option<StatusId>,
    #[serde(default)]
    pub in_reply_to_account_id: Option<AccountId>,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub spoiler_text: String,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub media_attachments: Vec<MediaAttachment>,
    #[serde(default)]
    pub mentions: Vec<Mention>,
    #[serde(default)]
    pub tags: Vec<Tag>,
    #[serde(default)]
    pub emojis: Vec<CustomEmoji>,
    #[serde(default)]
    pub card: Option<Card>,
    #[serde(default)]
    pub poll: Option<Poll>,
    #[serde(default)]
    pub application: Option<Application>,
    #[serde(default)]
    pub reblog: Option<Box<Status>>,

    /// v4.5+. Present when this is a quote post. Servers that predate
    /// the quote feature simply omit the field, so deserialization on
    /// older instances stays a no-op.
    #[serde(default)]
    pub quote: Option<QuoteData>,
    #[serde(default)]
    pub quote_approval: Option<serde_json::Value>,

    // Viewer-relative flags — only present for authenticated requests.
    #[serde(default)]
    pub favourited: Option<bool>,
    #[serde(default)]
    pub reblogged: Option<bool>,
    #[serde(default)]
    pub muted: Option<bool>,
    #[serde(default)]
    pub bookmarked: Option<bool>,
    #[serde(default)]
    pub pinned: Option<bool>,
    /// Text originally submitted; only present when `?source=true` is set
    /// (used for editing). Optional.
    #[serde(default)]
    pub text: Option<String>,
    /// Filter matches — Mastodon 4.x.
    #[serde(default)]
    pub filtered: Option<serde_json::Value>,
}

/// v4.5+ quote payload nested in [`Status::quote`]. The Mastodon API
/// returns the quoted post directly inside `quoted_status`; we keep it
/// boxed because Status is recursive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuoteData {
    /// `accepted` | `revoked` | `pending` | `rejected` | `deleted` |
    /// `unauthorized`. Anything other than `accepted` means the quoted
    /// post should be treated as unavailable.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub quoted_status: Option<Box<Status>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Public,
    Unlisted,
    Private,
    Direct,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mention {
    pub id: AccountId,
    pub username: String,
    pub url: String,
    pub acct: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub history: Vec<TagHistory>,
    #[serde(default)]
    pub following: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagHistory {
    pub day: String,
    pub uses: String,
    pub accounts: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomEmoji {
    pub shortcode: String,
    pub url: String,
    pub static_url: String,
    #[serde(default)]
    pub visible_in_picker: bool,
    #[serde(default)]
    pub category: Option<String>,
}

// ---------------------------------------------------------------------------
// MediaAttachment
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaAttachment {
    pub id: MediaId,
    #[serde(rename = "type")]
    pub media_type: MediaType,
    pub url: Option<String>,
    #[serde(default)]
    pub preview_url: Option<String>,
    #[serde(default)]
    pub remote_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub blurhash: Option<String>,
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
    /// Added in 3.5.0 — preview for a remote attachment when the local
    /// copy isn't available yet.
    #[serde(default)]
    pub preview_remote_url: Option<String>,
    /// Optional text description submitted by the uploader; identical to
    /// `description` on most servers but kept for forward-compat.
    #[serde(default)]
    pub text_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Image,
    Video,
    Gifv,
    Audio,
    Unknown,
}

// ---------------------------------------------------------------------------
// Card (link preview)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Card {
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "type", default)]
    pub card_type: CardType,
    #[serde(default)]
    pub author_name: Option<String>,
    #[serde(default)]
    pub author_url: Option<String>,
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub provider_url: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub embed_url: Option<String>,
    #[serde(default)]
    pub blurhash: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CardType {
    #[default]
    Link,
    Photo,
    Video,
    Rich,
}

// ---------------------------------------------------------------------------
// Poll
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Poll {
    pub id: PollId,
    #[serde(with = "opt_datetime")]
    pub expires_at: Option<DateTime<Utc>>,
    pub expired: bool,
    pub multiple: bool,
    pub votes_count: u64,
    #[serde(default)]
    pub voters_count: Option<u64>,
    pub options: Vec<PollOption>,
    #[serde(default)]
    pub emojis: Vec<CustomEmoji>,
    #[serde(default)]
    pub voted: Option<bool>,
    #[serde(default)]
    pub own_votes: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollOption {
    pub title: String,
    #[serde(default)]
    pub votes_count: Option<u64>,
}

// ---------------------------------------------------------------------------
// Application (via X)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Application {
    pub name: String,
    #[serde(default)]
    pub website: Option<String>,
    /// Present on POST /api/v1/apps only.
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub vapid_key: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
}

// ---------------------------------------------------------------------------
// Notification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    #[serde(rename = "type")]
    pub notification_type: NotificationType,
    #[serde(with = "opt_datetime", default)]
    pub created_at: Option<DateTime<Utc>>,
    pub account: Account,
    #[serde(default)]
    pub status: Option<Status>,
    /// Present on `admin.report` notifications.
    #[serde(default)]
    pub report: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationType {
    Mention,
    Reblog,
    Favourite,
    Follow,
    FollowRequest,
    Poll,
    Status,
    Update,
    /// v4.3+
    SeveredRelationships,
    /// v4.3+
    ModerationWarning,
    /// v4.5+
    Quote,
    /// v4.5+
    QuotedUpdate,
    #[serde(rename = "admin.sign_up")]
    AdminSignUp,
    #[serde(rename = "admin.report")]
    AdminReport,
    /// Fallback for types we don't yet know.
    #[serde(other)]
    Other,
}

// ---------------------------------------------------------------------------
// Context (reply chain)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    #[serde(default)]
    pub ancestors: Vec<Status>,
    #[serde(default)]
    pub descendants: Vec<Status>,
}

// ---------------------------------------------------------------------------
// Relationship (between viewer and a target account)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub id: AccountId,
    pub following: bool,
    #[serde(default)]
    pub showing_reblogs: bool,
    #[serde(default)]
    pub notifying: bool,
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    pub followed_by: bool,
    pub blocking: bool,
    #[serde(default)]
    pub blocked_by: bool,
    pub muting: bool,
    pub muting_notifications: bool,
    #[serde(default, with = "opt_datetime")]
    pub muting_expires_at: Option<DateTime<Utc>>,
    pub requested: bool,
    #[serde(default)]
    pub requested_by: Option<bool>,
    pub domain_blocking: bool,
    pub endorsed: bool,
    #[serde(default)]
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// Search result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub statuses: Vec<Status>,
    #[serde(default)]
    pub hashtags: Vec<Tag>,
}

// ---------------------------------------------------------------------------
// Token (OAuth response)
// ---------------------------------------------------------------------------

/// Response from `POST /oauth/token`. Note that `created_at` is a **UNIX
/// timestamp integer**, not a datetime string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub scope: String,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Instance v2 (abbreviated)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub domain: String,
    pub title: String,
    pub version: String,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub usage: Option<serde_json::Value>,
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    #[serde(default)]
    pub configuration: Option<serde_json::Value>,
    #[serde(default)]
    pub registrations: Option<serde_json::Value>,
    #[serde(default)]
    pub contact: Option<serde_json::Value>,
    #[serde(default)]
    pub rules: Option<Vec<serde_json::Value>>,
}

impl Instance {
    /// Pull `configuration.statuses.max_characters` out of the loosely
    /// typed `configuration` blob. Returns `None` if the server omits
    /// the field — caller should fall back to its own default.
    #[must_use]
    pub fn max_characters(&self) -> Option<u32> {
        let n = self
            .configuration
            .as_ref()?
            .get("statuses")?
            .get("max_characters")?
            .as_u64()?;
        u32::try_from(n).ok()
    }
}

// ---------------------------------------------------------------------------
// RFC3339-ish datetime helper.
// ---------------------------------------------------------------------------

mod opt_datetime {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

    pub fn serialize<S: Serializer>(dt: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match dt {
            Some(v) => v.to_rfc3339().serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        // Accept null, missing, empty string, or a valid RFC3339 string.
        let opt: Option<String> = Option::deserialize(d)?;
        match opt {
            None => Ok(None),
            Some(s) if s.is_empty() => Ok(None),
            Some(s) => DateTime::parse_from_rfc3339(&s)
                .map(|dt| Some(dt.with_timezone(&Utc)))
                .map_err(D::Error::custom),
        }
    }
}
