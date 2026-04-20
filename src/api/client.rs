//! `reqwest`-backed Mastodon HTTP client.
//!
//! The client is cheap to `clone()` — the inner `reqwest::Client` already
//! is `Arc`-shared, and the token is a `SecretString` (`Clone` produces a
//! fresh zeroize-on-drop allocation).

use std::time::Duration;

use reqwest::{Method, RequestBuilder, Response, StatusCode, header};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{debug, warn};
use url::Url;

use crate::api::error::{ApiError, ApiResult, ErrorBody};
use crate::api::pagination::{Page, parse_link_header};

/// User-Agent string sent on every request.
pub const USER_AGENT: &str = concat!(
    "mastoot/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ReflectionL/mastoot)"
);

/// Per-request total timeout. Generous because some small instances plus
/// an HTTP proxy can push a `verify_credentials` into the 15–20 s range
/// cold. Streaming requests bypass this via a separate client.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// TCP + TLS handshake budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Main Mastodon client.
#[derive(Clone)]
pub struct MastodonClient {
    base_url: Url,
    token: Option<SecretString>,
    http: reqwest::Client,
}

impl std::fmt::Debug for MastodonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MastodonClient")
            .field("base_url", &self.base_url.as_str())
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl MastodonClient {
    /// Build an authenticated client. `instance` accepts either a bare
    /// hostname (`mastodon.social`) or a full URL (`https://mastodon.social`).
    pub fn new(instance: &str, token: SecretString) -> ApiResult<Self> {
        let base_url = parse_instance(instance)?;
        let http = default_http_client()?;
        Ok(Self {
            base_url,
            token: Some(token),
            http,
        })
    }

    /// Build an anonymous client for unauthenticated endpoints (instance
    /// info, public timelines on servers that allow them, OAuth calls).
    pub fn anonymous(instance: &str) -> ApiResult<Self> {
        let base_url = parse_instance(instance)?;
        let http = default_http_client()?;
        Ok(Self {
            base_url,
            token: None,
            http,
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn token(&self) -> Option<&SecretString> {
        self.token.as_ref()
    }

    fn build(&self, method: Method, path: &str) -> ApiResult<RequestBuilder> {
        let url = self.base_url.join(path).map_err(ApiError::Url)?;
        let mut req = self.http.request(method, url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token.expose_secret());
        }
        Ok(req)
    }

    // ---- high-level verbs -------------------------------------------------

    pub async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> ApiResult<T> {
        let req = self.build(Method::GET, path)?.query(query);
        self.send_json(req).await
    }

    pub async fn get_page<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> ApiResult<Page<T>> {
        let req = self.build(Method::GET, path)?.query(query);
        self.send_page(req).await
    }

    pub async fn post_json<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        let req = self.build(Method::POST, path)?.json(body);
        self.send_json(req).await
    }

    /// POST JSON with custom headers. Primary use case is sending
    /// `Idempotency-Key` with a new status.
    pub async fn post_json_with_headers<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
        headers: &[(&str, &str)],
    ) -> ApiResult<T> {
        let mut req = self.build(Method::POST, path)?.json(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        self.send_json(req).await
    }

    pub async fn post_form<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        form: &B,
    ) -> ApiResult<T> {
        let req = self.build(Method::POST, path)?.form(form);
        self.send_json(req).await
    }

    /// POST with no body — many toggle endpoints work this way.
    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        let req = self.build(Method::POST, path)?;
        self.send_json(req).await
    }

    pub async fn put_json<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        let req = self.build(Method::PUT, path)?.json(body);
        self.send_json(req).await
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        let req = self.build(Method::DELETE, path)?;
        self.send_json(req).await
    }

    // ---- transport --------------------------------------------------------

    async fn send_json<T: DeserializeOwned>(&self, req: RequestBuilder) -> ApiResult<T> {
        let bytes = self.send_with_backoff(req).await?.bytes().await?;
        serde_json::from_slice::<T>(&bytes).map_err(Into::into)
    }

    async fn send_page<T: DeserializeOwned>(&self, req: RequestBuilder) -> ApiResult<Page<T>> {
        let resp = self.send_with_backoff(req).await?;
        let link = resp
            .headers()
            .get(header::LINK)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = resp.bytes().await?;
        let items = serde_json::from_slice::<T>(&bytes)?;
        let (next, prev) = link.as_deref().map_or((None, None), parse_link_header);
        Ok(Page { items, next, prev })
    }

    /// Sends a request; on 429 honors `X-RateLimit-Reset` / `Retry-After`
    /// and retries up to 3 times with exponential backoff.
    async fn send_with_backoff(&self, req: RequestBuilder) -> ApiResult<Response> {
        let mut attempts = 0u32;
        loop {
            let this_req = req.try_clone().ok_or_else(|| {
                ApiError::Other("request body is not cloneable (retry impossible)".into())
            })?;
            let resp = this_req.send().await?;
            let status = resp.status();

            if status.is_success() {
                return Ok(resp);
            }

            if status == StatusCode::TOO_MANY_REQUESTS && attempts < 3 {
                let wait = retry_after(&resp)
                    .unwrap_or_else(|| Duration::from_secs(2u64.saturating_pow(attempts)));
                warn!(?wait, "429 rate limited; backing off");
                tokio::time::sleep(wait).await;
                attempts += 1;
                continue;
            }

            return Err(translate_error(resp).await);
        }
    }
}

fn default_http_client() -> ApiResult<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(DEFAULT_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .https_only(false) // some onion/self-signed dev instances are http
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .map_err(Into::into)
}

fn parse_instance(instance: &str) -> ApiResult<Url> {
    let trimmed = instance.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(ApiError::InvalidUrl(instance.to_string()));
    }
    let normalized = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let mut url = Url::parse(&normalized).map_err(ApiError::Url)?;
    // Force trailing slash so `Url::join` treats the host as a base.
    if url.path().is_empty() {
        url.set_path("/");
    }
    Ok(url)
}

fn retry_after(resp: &Response) -> Option<Duration> {
    // Standard `Retry-After`.
    if let Some(v) = resp
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        && let Ok(secs) = v.parse::<u64>()
    {
        return Some(Duration::from_secs(secs));
    }
    // Mastodon's specific `X-RateLimit-Reset`, an RFC3339 timestamp.
    if let Some(v) = resp
        .headers()
        .get("X-RateLimit-Reset")
        .and_then(|h| h.to_str().ok())
        && let Ok(reset) = chrono::DateTime::parse_from_rfc3339(v)
    {
        let now = chrono::Utc::now();
        let delta = reset.signed_duration_since(now).num_seconds();
        if delta > 0 {
            return Some(Duration::from_secs(delta as u64));
        }
    }
    None
}

async fn translate_error(resp: Response) -> ApiError {
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap_or_default();
    let message = serde_json::from_slice::<ErrorBody>(&bytes).map_or_else(
        |_| String::from_utf8_lossy(&bytes).into_owned(),
        |b| {
            b.error_description
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or(b.error)
        },
    );
    debug!(%status, %message, "api error");
    match status {
        StatusCode::UNAUTHORIZED => ApiError::Unauthorized,
        StatusCode::NOT_FOUND => ApiError::NotFound(message),
        StatusCode::TOO_MANY_REQUESTS => ApiError::RateLimited {
            retry_after_secs: 60,
        },
        _ => ApiError::Server {
            status: status.as_u16(),
            message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_instance_accepts_bare_host() {
        let u = parse_instance("mastodon.social").unwrap();
        assert_eq!(u.as_str(), "https://mastodon.social/");
    }

    #[test]
    fn parse_instance_accepts_url() {
        let u = parse_instance("https://mastodon.social/").unwrap();
        assert_eq!(u.as_str(), "https://mastodon.social/");
    }

    #[test]
    fn parse_instance_rejects_empty() {
        assert!(parse_instance("").is_err());
    }
}
