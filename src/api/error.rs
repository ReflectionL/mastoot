//! Error type for the Mastodon SDK layer.
//!
//! Kept library-style (using `thiserror`) so downstream callers can
//! pattern-match; `main` and the TUI layer convert into `anyhow::Error`.

use thiserror::Error;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("invalid instance URL: {0}")]
    InvalidUrl(String),

    #[error("http transport error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("failed to parse url: {0}")]
    Url(#[from] url::ParseError),

    #[error("server returned {status}: {message}")]
    Server { status: u16, message: String },

    #[error("rate limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("unauthorized — token missing, revoked, or expired")]
    Unauthorized,

    #[error("not found: {0}")]
    NotFound(String),

    #[error("json decode error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("OAuth flow error: {0}")]
    OAuth(String),

    #[error("streaming error: {0}")]
    Stream(String),

    #[error("{0}")]
    Other(String),
}

/// High-level classification used by the TUI to pick a toast level, a
/// short human phrase, and a status-bar connection health tint. Keeps
/// the raw `reqwest::Error` diagnostics out of the user's face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorCategory {
    /// DNS / TCP / TLS / connect — we never reached the server.
    Offline,
    /// Connected but the request didn't complete in time.
    Timeout,
    /// HTTP 429. Backoff did happen; we're giving up.
    RateLimited,
    /// HTTP 401 — token gone, revoked, or expired.
    Unauthorized,
    /// HTTP 404.
    NotFound,
    /// HTTP 5xx.
    ServerError,
    /// Local or protocol-level error (bad URL, JSON decode, OAuth flow).
    Client,
}

impl ApiError {
    /// Classify this error for UI purposes. Does not lose information —
    /// callers still have the full `ApiError` in hand.
    #[must_use]
    pub fn category(&self) -> ApiErrorCategory {
        match self {
            Self::Unauthorized => ApiErrorCategory::Unauthorized,
            Self::RateLimited { .. } => ApiErrorCategory::RateLimited,
            Self::NotFound(_) => ApiErrorCategory::NotFound,
            Self::Server { status, .. } => {
                if *status >= 500 {
                    ApiErrorCategory::ServerError
                } else {
                    ApiErrorCategory::Client
                }
            }
            Self::Transport(e) => {
                if e.is_timeout() {
                    ApiErrorCategory::Timeout
                } else if e.is_connect() || e.is_request() {
                    ApiErrorCategory::Offline
                } else {
                    // decode / body / unknown transport — caller dropped
                    // the body or TLS hiccuped mid-stream. Treat as
                    // offline so the status bar shows the red dot.
                    ApiErrorCategory::Offline
                }
            }
            // SSE stream hiccups count as "network down" for the dot.
            Self::Stream(_) => ApiErrorCategory::Offline,
            Self::InvalidUrl(_)
            | Self::Url(_)
            | Self::Json(_)
            | Self::OAuth(_)
            | Self::Other(_) => ApiErrorCategory::Client,
        }
    }

    /// One-line, no-jargon phrase for toasts. No `http transport error:`
    /// prefix, no stack of `: Display`-chained reqwest internals — just
    /// what the user needs to know. Pair with a verb prefix at the
    /// call site (e.g. `format!("{verb} failed · {}", err.terse())`).
    #[must_use]
    pub fn terse(&self) -> String {
        match self {
            Self::Unauthorized => "session expired · run `mastoot login`".into(),
            Self::RateLimited { retry_after_secs } => {
                format!("rate limited · retry in {retry_after_secs}s")
            }
            Self::NotFound(msg) if !msg.is_empty() => format!("not found · {msg}"),
            Self::NotFound(_) => "not found".into(),
            Self::Server { status, message } if *status >= 500 => {
                if message.is_empty() {
                    format!("server error {status}")
                } else {
                    format!("server {status} · {message}")
                }
            }
            Self::Server { status, message } => {
                if message.is_empty() {
                    format!("error {status}")
                } else {
                    format!("error {status} · {message}")
                }
            }
            Self::Transport(e) if e.is_timeout() => "request timed out".into(),
            Self::Transport(e) if e.is_connect() || e.is_request() => "network unreachable".into(),
            Self::Transport(_) => "network error".into(),
            Self::Stream(msg) => {
                if msg.is_empty() {
                    "stream disconnected".into()
                } else {
                    format!("stream · {msg}")
                }
            }
            Self::InvalidUrl(u) => format!("invalid url · {u}"),
            Self::Url(_) => "invalid url".into(),
            Self::Json(_) => "couldn't parse server response".into(),
            Self::OAuth(msg) => format!("auth · {msg}"),
            Self::Other(msg) => msg.clone(),
        }
    }
}

/// Error payload Mastodon returns in the body of 4xx/5xx responses.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ErrorBody {
    pub error: String,
    #[serde(default)]
    pub error_description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauthorized_categorizes_and_tells_user_to_relogin() {
        let e = ApiError::Unauthorized;
        assert_eq!(e.category(), ApiErrorCategory::Unauthorized);
        assert!(e.terse().contains("mastoot login"));
    }

    #[test]
    fn rate_limit_surfaces_retry_window() {
        let e = ApiError::RateLimited {
            retry_after_secs: 42,
        };
        assert_eq!(e.category(), ApiErrorCategory::RateLimited);
        assert!(e.terse().contains("42"));
    }

    #[test]
    fn server_5xx_is_server_error_4xx_is_client() {
        let five = ApiError::Server {
            status: 503,
            message: "nope".into(),
        };
        assert_eq!(five.category(), ApiErrorCategory::ServerError);
        assert!(five.terse().contains("503"));

        let four = ApiError::Server {
            status: 422,
            message: "bad input".into(),
        };
        assert_eq!(four.category(), ApiErrorCategory::Client);
        assert!(four.terse().contains("422"));
    }

    #[test]
    fn not_found_stays_logical_does_not_degrade_health() {
        let e = ApiError::NotFound("status".into());
        assert_eq!(e.category(), ApiErrorCategory::NotFound);
        // Surface the detail so the user knows *what* is not found.
        assert!(e.terse().contains("status"));
    }

    #[test]
    fn terse_strips_transport_jargon() {
        // NotFound does not contain the "http transport error:" prefix.
        let e = ApiError::NotFound("user".into());
        let s = e.terse();
        assert!(!s.contains("transport"));
        assert!(!s.starts_with("http "));
    }
}
