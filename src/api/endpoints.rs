//! Typed wrappers over Mastodon REST endpoints.
//!
//! Only the endpoints mastoot's MVP actually calls are modeled. Adding
//! more is a matter of matching against
//! <https://docs.joinmastodon.org/methods/>.

use serde::Serialize;

use crate::api::client::MastodonClient;
use crate::api::error::ApiResult;
use crate::api::models::{
    Account, AccountId, Context, Instance, MediaAttachment, MediaId, Notification, NotificationId,
    Relationship, SearchResults, Status, StatusId, Visibility,
};
use crate::api::pagination::Page;

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

impl MastodonClient {
    /// GET /api/v1/accounts/verify_credentials
    pub async fn verify_credentials(&self) -> ApiResult<Account> {
        self.get("/api/v1/accounts/verify_credentials", &[]).await
    }

    /// GET /api/v1/accounts/{id}
    pub async fn account(&self, id: &AccountId) -> ApiResult<Account> {
        self.get(&format!("/api/v1/accounts/{id}"), &[]).await
    }

    /// GET /api/v1/accounts/{id}/statuses
    pub async fn account_statuses(
        &self,
        id: &AccountId,
        params: &AccountStatusesParams,
    ) -> ApiResult<Page<Vec<Status>>> {
        let query = params.to_query();
        self.get_page(&format!("/api/v1/accounts/{id}/statuses"), &query)
            .await
    }

    /// GET /api/v1/accounts/relationships?id[]=...
    pub async fn relationships(&self, ids: &[&AccountId]) -> ApiResult<Vec<Relationship>> {
        let query: Vec<(&str, String)> = ids.iter().map(|id| ("id[]", id.to_string())).collect();
        self.get("/api/v1/accounts/relationships", &query).await
    }

    /// POST /api/v1/accounts/{id}/follow
    pub async fn follow(&self, id: &AccountId) -> ApiResult<Relationship> {
        self.post_empty(&format!("/api/v1/accounts/{id}/follow"))
            .await
    }

    /// POST /api/v1/accounts/{id}/unfollow
    pub async fn unfollow(&self, id: &AccountId) -> ApiResult<Relationship> {
        self.post_empty(&format!("/api/v1/accounts/{id}/unfollow"))
            .await
    }

    /// GET /api/v1/accounts/{id}/followers
    pub async fn account_followers(
        &self,
        id: &AccountId,
        params: &AccountListParams,
    ) -> ApiResult<Page<Vec<Account>>> {
        let query = params.to_query();
        self.get_page(&format!("/api/v1/accounts/{id}/followers"), &query)
            .await
    }

    /// GET /api/v1/accounts/{id}/following
    pub async fn account_following(
        &self,
        id: &AccountId,
        params: &AccountListParams,
    ) -> ApiResult<Page<Vec<Account>>> {
        let query = params.to_query();
        self.get_page(&format!("/api/v1/accounts/{id}/following"), &query)
            .await
    }
}

/// Pagination params for the followers / following endpoints. Same
/// max_id / since_id / limit triple shared across most listings.
#[derive(Debug, Default, Clone)]
pub struct AccountListParams {
    pub max_id: Option<String>,
    pub since_id: Option<String>,
    pub limit: Option<u32>,
}

impl AccountListParams {
    fn to_query(&self) -> Vec<(&'static str, String)> {
        let mut q = Vec::new();
        if let Some(v) = &self.max_id {
            q.push(("max_id", v.clone()));
        }
        if let Some(v) = &self.since_id {
            q.push(("since_id", v.clone()));
        }
        if let Some(v) = self.limit {
            q.push(("limit", v.to_string()));
        }
        q
    }
}

#[derive(Debug, Default, Clone)]
pub struct AccountStatusesParams {
    pub max_id: Option<String>,
    pub since_id: Option<String>,
    pub min_id: Option<String>,
    pub limit: Option<u32>,
    pub only_media: bool,
    pub exclude_replies: bool,
    pub exclude_reblogs: bool,
    pub pinned: bool,
    pub tagged: Option<String>,
}

impl AccountStatusesParams {
    fn to_query(&self) -> Vec<(&'static str, String)> {
        let mut q = Vec::new();
        if let Some(v) = &self.max_id {
            q.push(("max_id", v.clone()));
        }
        if let Some(v) = &self.since_id {
            q.push(("since_id", v.clone()));
        }
        if let Some(v) = &self.min_id {
            q.push(("min_id", v.clone()));
        }
        if let Some(v) = self.limit {
            q.push(("limit", v.to_string()));
        }
        if self.only_media {
            q.push(("only_media", "true".into()));
        }
        if self.exclude_replies {
            q.push(("exclude_replies", "true".into()));
        }
        if self.exclude_reblogs {
            q.push(("exclude_reblogs", "true".into()));
        }
        if self.pinned {
            q.push(("pinned", "true".into()));
        }
        if let Some(v) = &self.tagged {
            q.push(("tagged", v.clone()));
        }
        q
    }
}

// ---------------------------------------------------------------------------
// Timelines
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct TimelineParams {
    pub max_id: Option<String>,
    pub since_id: Option<String>,
    pub min_id: Option<String>,
    pub limit: Option<u32>,
    /// Only relevant for public timelines.
    pub local: bool,
    pub remote: bool,
    pub only_media: bool,
}

impl TimelineParams {
    fn to_query(&self) -> Vec<(&'static str, String)> {
        let mut q = Vec::new();
        if let Some(v) = &self.max_id {
            q.push(("max_id", v.clone()));
        }
        if let Some(v) = &self.since_id {
            q.push(("since_id", v.clone()));
        }
        if let Some(v) = &self.min_id {
            q.push(("min_id", v.clone()));
        }
        if let Some(v) = self.limit {
            q.push(("limit", v.to_string()));
        }
        if self.local {
            q.push(("local", "true".into()));
        }
        if self.remote {
            q.push(("remote", "true".into()));
        }
        if self.only_media {
            q.push(("only_media", "true".into()));
        }
        q
    }
}

impl MastodonClient {
    /// GET /api/v1/timelines/home
    pub async fn home_timeline(&self, params: &TimelineParams) -> ApiResult<Page<Vec<Status>>> {
        self.get_page("/api/v1/timelines/home", &params.to_query())
            .await
    }

    /// GET /api/v1/timelines/public
    pub async fn public_timeline(&self, params: &TimelineParams) -> ApiResult<Page<Vec<Status>>> {
        self.get_page("/api/v1/timelines/public", &params.to_query())
            .await
    }

    /// GET /api/v1/timelines/tag/{hashtag}
    pub async fn tag_timeline(
        &self,
        tag: &str,
        params: &TimelineParams,
    ) -> ApiResult<Page<Vec<Status>>> {
        self.get_page(
            &format!(
                "/api/v1/timelines/tag/{}",
                percent_encoding::utf8_percent_encode(tag, percent_encoding::NON_ALPHANUMERIC,)
            ),
            &params.to_query(),
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Statuses
// ---------------------------------------------------------------------------

impl MastodonClient {
    /// GET /api/v1/statuses/{id}
    pub async fn status(&self, id: &StatusId) -> ApiResult<Status> {
        self.get(&format!("/api/v1/statuses/{id}"), &[]).await
    }

    /// GET /api/v1/statuses/{id}/context
    pub async fn status_context(&self, id: &StatusId) -> ApiResult<Context> {
        self.get(&format!("/api/v1/statuses/{id}/context"), &[])
            .await
    }

    /// POST /api/v1/statuses — compose a new status.
    ///
    /// An idempotency key is generated if the draft does not carry one;
    /// the Mastodon server deduplicates by this key for one hour, which
    /// protects against double-posts on network retries.
    pub async fn post_status(&self, draft: &StatusDraft) -> ApiResult<Status> {
        let key = draft
            .idempotency_key
            .clone()
            .unwrap_or_else(random_idempotency_key);
        self.post_json_with_headers(
            "/api/v1/statuses",
            draft,
            &[("Idempotency-Key", key.as_str())],
        )
        .await
    }

    /// PUT /api/v1/statuses/{id} — edit an existing status.
    pub async fn edit_status(&self, id: &StatusId, draft: &StatusDraft) -> ApiResult<Status> {
        self.put_json(&format!("/api/v1/statuses/{id}"), draft)
            .await
    }

    /// DELETE /api/v1/statuses/{id} — returns the deleted status so clients
    /// can re-populate a compose buffer.
    pub async fn delete_status(&self, id: &StatusId) -> ApiResult<Status> {
        self.delete(&format!("/api/v1/statuses/{id}")).await
    }

    /// POST /api/v1/statuses/{id}/favourite
    pub async fn favourite(&self, id: &StatusId) -> ApiResult<Status> {
        self.post_empty(&format!("/api/v1/statuses/{id}/favourite"))
            .await
    }

    /// POST /api/v1/statuses/{id}/unfavourite
    pub async fn unfavourite(&self, id: &StatusId) -> ApiResult<Status> {
        self.post_empty(&format!("/api/v1/statuses/{id}/unfavourite"))
            .await
    }

    /// POST /api/v1/statuses/{id}/reblog
    pub async fn reblog(&self, id: &StatusId) -> ApiResult<Status> {
        self.post_empty(&format!("/api/v1/statuses/{id}/reblog"))
            .await
    }

    /// POST /api/v1/statuses/{id}/unreblog
    pub async fn unreblog(&self, id: &StatusId) -> ApiResult<Status> {
        self.post_empty(&format!("/api/v1/statuses/{id}/unreblog"))
            .await
    }

    /// POST /api/v1/statuses/{id}/bookmark
    pub async fn bookmark(&self, id: &StatusId) -> ApiResult<Status> {
        self.post_empty(&format!("/api/v1/statuses/{id}/bookmark"))
            .await
    }

    /// POST /api/v1/statuses/{id}/unbookmark
    pub async fn unbookmark(&self, id: &StatusId) -> ApiResult<Status> {
        self.post_empty(&format!("/api/v1/statuses/{id}/unbookmark"))
            .await
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusDraft {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to_id: Option<StatusId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spoiler_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub sensitive: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub media_ids: Vec<MediaId>,
    /// Mastodon 4.5+ native quote. Serialized as `quoted_status_id` —
    /// the field the server actually looks at (per the 4.5 API docs,
    /// <https://docs.joinmastodon.org/client/quotes/>). Old instances
    /// ignore the unknown field instead of returning 422.
    #[serde(skip_serializing_if = "Option::is_none", rename = "quoted_status_id")]
    pub quote_id: Option<StatusId>,
    /// Not sent in the body — consumed by [`MastodonClient::post_status`]
    /// as an HTTP header.
    #[serde(skip)]
    pub idempotency_key: Option<String>,
}

/// Generates a random 32-char base32 string suitable for Idempotency-Key.
fn random_idempotency_key() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuv";
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

impl StatusDraft {
    #[must_use]
    pub fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            in_reply_to_id: None,
            spoiler_text: None,
            visibility: None,
            language: None,
            sensitive: false,
            media_ids: Vec::new(),
            quote_id: None,
            idempotency_key: None,
        }
    }

    #[must_use]
    pub fn in_reply_to(mut self, id: StatusId) -> Self {
        self.in_reply_to_id = Some(id);
        self
    }

    #[must_use]
    pub fn visibility(mut self, v: Visibility) -> Self {
        self.visibility = Some(v);
        self
    }

    #[must_use]
    pub fn spoiler(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        if !text.is_empty() {
            self.spoiler_text = Some(text);
            self.sensitive = true;
        }
        self
    }
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct NotificationParams {
    pub max_id: Option<String>,
    pub since_id: Option<String>,
    pub min_id: Option<String>,
    pub limit: Option<u32>,
    pub exclude_types: Vec<&'static str>,
    pub types: Vec<&'static str>,
    pub account_id: Option<AccountId>,
}

impl NotificationParams {
    fn to_query(&self) -> Vec<(&'static str, String)> {
        let mut q = Vec::new();
        if let Some(v) = &self.max_id {
            q.push(("max_id", v.clone()));
        }
        if let Some(v) = &self.since_id {
            q.push(("since_id", v.clone()));
        }
        if let Some(v) = &self.min_id {
            q.push(("min_id", v.clone()));
        }
        if let Some(v) = self.limit {
            q.push(("limit", v.to_string()));
        }
        for t in &self.exclude_types {
            q.push(("exclude_types[]", (*t).to_string()));
        }
        for t in &self.types {
            q.push(("types[]", (*t).to_string()));
        }
        if let Some(v) = &self.account_id {
            q.push(("account_id", v.to_string()));
        }
        q
    }
}

impl MastodonClient {
    /// GET /api/v1/notifications
    pub async fn notifications(
        &self,
        params: &NotificationParams,
    ) -> ApiResult<Page<Vec<Notification>>> {
        self.get_page("/api/v1/notifications", &params.to_query())
            .await
    }

    /// GET /api/v1/notifications/{id}
    pub async fn notification(&self, id: &NotificationId) -> ApiResult<Notification> {
        self.get(&format!("/api/v1/notifications/{id}"), &[]).await
    }

    /// POST /api/v1/notifications/clear
    pub async fn clear_notifications(&self) -> ApiResult<serde_json::Value> {
        self.post_empty("/api/v1/notifications/clear").await
    }
}

// ---------------------------------------------------------------------------
// Favourites / bookmarks (as listings)
// ---------------------------------------------------------------------------

impl MastodonClient {
    /// GET /api/v1/favourites
    pub async fn favourites(&self) -> ApiResult<Page<Vec<Status>>> {
        self.get_page("/api/v1/favourites", &[]).await
    }

    /// GET /api/v1/bookmarks
    pub async fn bookmarks(&self) -> ApiResult<Page<Vec<Status>>> {
        self.get_page("/api/v1/bookmarks", &[]).await
    }
}

// ---------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------

impl MastodonClient {
    /// POST /api/v2/media — upload a file as a multipart body.
    /// Attaches `description` (alt text) when provided.
    pub async fn upload_media(
        &self,
        bytes: Vec<u8>,
        filename: impl Into<String>,
        mime: impl Into<String>,
        description: Option<String>,
    ) -> ApiResult<MediaAttachment> {
        let url = self
            .base_url()
            .join("/api/v2/media")
            .map_err(crate::api::error::ApiError::Url)?;

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.into())
            .mime_str(&mime.into())
            .map_err(|e| crate::api::error::ApiError::Other(e.to_string()))?;
        let mut form = reqwest::multipart::Form::new().part("file", part);
        if let Some(desc) = description {
            form = form.text("description", desc);
        }

        let mut req = reqwest::Client::new().post(url).multipart(form);
        if let Some(token) = self.token() {
            use secrecy::ExposeSecret;
            req = req.bearer_auth(token.expose_secret());
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::api::error::ApiError::Server {
                status,
                message: body,
            });
        }
        let bytes = resp.bytes().await?;
        serde_json::from_slice(&bytes).map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct SearchParams<'a> {
    pub q: &'a str,
    pub kind: Option<&'a str>, // "accounts" | "hashtags" | "statuses"
    pub resolve: bool,
    pub following: bool,
    pub limit: Option<u32>,
}

impl MastodonClient {
    /// GET /api/v2/search
    pub async fn search(&self, params: &SearchParams<'_>) -> ApiResult<SearchResults> {
        let mut q: Vec<(&str, String)> = vec![("q", params.q.to_string())];
        if let Some(k) = params.kind {
            q.push(("type", k.to_string()));
        }
        if params.resolve {
            q.push(("resolve", "true".into()));
        }
        if params.following {
            q.push(("following", "true".into()));
        }
        if let Some(n) = params.limit {
            q.push(("limit", n.to_string()));
        }
        self.get("/api/v2/search", &q).await
    }
}

// ---------------------------------------------------------------------------
// Instance
// ---------------------------------------------------------------------------

impl MastodonClient {
    /// GET /api/v2/instance — modern replacement for v1. Works without auth.
    pub async fn instance(&self) -> ApiResult<Instance> {
        self.get("/api/v2/instance", &[]).await
    }
}
