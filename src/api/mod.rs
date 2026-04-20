//! Mastodon HTTP API client.
//!
//! This module is deliberately independent of the TUI: you can depend on
//! it as a library and talk to any Mastodon-compatible server (Mastodon,
//! GoToSocial, Pleroma, Firefish, …). See `examples/fetch_home.rs`.
//!
//! The flow is:
//!
//! 1. [`auth`] obtains an OAuth access token (one-off, interactive).
//! 2. [`MastodonClient::new`] wraps the token and server URL.
//! 3. The `endpoints` methods on [`MastodonClient`] issue typed requests.
//! 4. [`streaming`] layers an SSE user stream on top of the same client.

pub mod auth;
pub mod client;
pub mod endpoints;
pub mod error;
pub mod html;
pub mod models;
pub mod music;
pub mod pagination;
pub mod streaming;

pub use client::MastodonClient;
pub use error::{ApiError, ApiErrorCategory, ApiResult};
pub use pagination::Page;
