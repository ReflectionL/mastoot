//! `reqwest`-backed Mastodon HTTP client.
//!
//! The client is cheap to `clone()` — the inner `reqwest::Client` already
//! is `Arc`-shared, and the token is a `SecretString` (`Clone` produces a
//! fresh zeroize-on-drop allocation).

use std::sync::OnceLock;
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

/// Inactivity budget on a long-lived SSE body. Mastodon emits a
/// `:thump` keepalive comment roughly every 15 s, so 90 s of silence
/// means the socket is dead (NAT timeout, laptop sleep) even though
/// the kernel hasn't noticed yet. Tripping this turns into a stream
/// error → reconnect, instead of a "Connected" dot that never updates.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(90);

/// Process-wide `reqwest::Client` for ordinary requests (REST, media
/// downloads, iTunes lookups). Building a client is expensive (TLS
/// roots, connection pool) and the pool is only useful when shared —
/// every call site that used to `reqwest::Client::new()` per request
/// now goes through here.
pub fn shared_http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| default_http_client().expect("failed to build shared reqwest client"))
}

/// Process-wide client for SSE streams: no total request timeout (the
/// body is meant to live for hours) but a read-inactivity timeout so
/// a dead socket is detected.
pub fn shared_stream_http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(STREAM_READ_TIMEOUT)
            .https_only(false)
            .build()
            .expect("failed to build shared streaming reqwest client")
    })
}

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
        Ok(Self {
            base_url,
            token: Some(token),
            http: shared_http().clone(),
        })
    }

    /// Build an anonymous client for unauthenticated endpoints (instance
    /// info, public timelines on servers that allow them, OAuth calls).
    pub fn anonymous(instance: &str) -> ApiResult<Self> {
        let base_url = parse_instance(instance)?;
        Ok(Self {
            base_url,
            token: None,
            http: shared_http().clone(),
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// The underlying HTTP client, for the rare endpoint that needs to
    /// build its own request (multipart uploads).
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
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

    // ---- transport tests against a throwaway local HTTP server ------
    //
    // No mock-server crate: a dozen lines of tokio TCP is enough to
    // hand back canned responses, and it keeps the dev-dependency
    // surface at zero.

    use std::fmt::Write as _;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn http_response(status: &str, headers: &[(&str, &str)], body: &str) -> String {
        let mut s = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n",
            body.len()
        );
        for (k, v) in headers {
            let _ = write!(s, "{k}: {v}\r\n");
        }
        s.push_str("\r\n");
        s.push_str(body);
        s
    }

    /// Serve `responses` one per connection, in order. Returns the base
    /// URL and a receiver that yields each raw request head.
    async fn serve(
        responses: Vec<String>,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for resp in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 16 * 1024];
                let mut n = 0;
                while let Ok(m) = sock.read(&mut buf[n..]).await {
                    if m == 0 {
                        break;
                    }
                    n += m;
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn rate_limit_is_retried_then_succeeds() {
        let (base, mut reqs) = serve(vec![
            http_response("429 Too Many Requests", &[("Retry-After", "0")], "{}"),
            http_response("200 OK", &[], r#"{"ok":true}"#),
        ])
        .await;
        let client = MastodonClient::anonymous(&base).unwrap();
        let v: serde_json::Value = client.get("/api/v1/ping", &[]).await.unwrap();
        assert_eq!(v["ok"], true);
        // Both requests hit the wire.
        assert!(reqs.recv().await.unwrap().starts_with("GET /api/v1/ping"));
        assert!(reqs.recv().await.unwrap().starts_with("GET /api/v1/ping"));
    }

    #[tokio::test]
    async fn link_header_becomes_page_cursors() {
        let link = format!(
            "<{0}/api/v1/x?max_id=100>; rel=\"next\", <{0}/api/v1/x?min_id=200>; rel=\"prev\"",
            "http://h"
        );
        let (base, _reqs) = serve(vec![http_response(
            "200 OK",
            &[("Link", &link)],
            r#"[{"a":1}]"#,
        )])
        .await;
        let client = MastodonClient::anonymous(&base).unwrap();
        let page: Page<Vec<serde_json::Value>> = client
            .get_page("/api/v1/x", &[("limit", "1".to_string())])
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.next.unwrap().max_id.as_deref(), Some("100"));
        assert_eq!(page.prev.unwrap().min_id.as_deref(), Some("200"));
    }

    #[tokio::test]
    async fn bearer_token_and_query_are_sent() {
        let (base, mut reqs) = serve(vec![http_response("200 OK", &[], "{}")]).await;
        let client = MastodonClient::new(&base, SecretString::from("tok-123".to_string())).unwrap();
        let _: serde_json::Value = client
            .get("/api/v1/y", &[("q", "hi there".to_string())])
            .await
            .unwrap();
        let head = reqs.recv().await.unwrap();
        assert!(head.starts_with("GET /api/v1/y?q=hi+there"), "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer tok-123"),
            "{head}"
        );
        assert!(head.contains(USER_AGENT), "{head}");
    }

    #[tokio::test]
    async fn error_statuses_map_to_typed_errors() {
        let (base, _r) = serve(vec![
            http_response("401 Unauthorized", &[], r#"{"error":"nope"}"#),
            http_response("404 Not Found", &[], r#"{"error":"Record not found"}"#),
            http_response("503 Service Unavailable", &[], "down"),
        ])
        .await;
        let client = MastodonClient::anonymous(&base).unwrap();
        let e = client
            .get::<serde_json::Value>("/a", &[])
            .await
            .unwrap_err();
        assert!(matches!(e, ApiError::Unauthorized), "{e:?}");
        let e = client
            .get::<serde_json::Value>("/b", &[])
            .await
            .unwrap_err();
        assert!(
            matches!(&e, ApiError::NotFound(m) if m == "Record not found"),
            "{e:?}"
        );
        let e = client
            .get::<serde_json::Value>("/c", &[])
            .await
            .unwrap_err();
        assert!(
            matches!(&e, ApiError::Server { status: 503, message } if message == "down"),
            "{e:?}"
        );
    }
}
