//! mastoot — an aesthetically-driven Mastodon TUI client.
//!
//! The crate is split into three strictly unidirectional layers:
//!
//! ```text
//! ui      ratatui widgets, renders state, emits Actions
//!  │
//! state   owns AppState, translates Actions ↔ Events
//!  │
//! api     reqwest-based Mastodon SDK; has no TUI knowledge and
//!         can be used standalone.
//! ```
//!
//! The [`api`] module is intentionally usable as a library — see
//! `examples/fetch_home.rs`.

pub mod api;
pub mod cli;
pub mod commands;
pub mod config;
pub mod icons;
pub mod logging;
pub mod state;
pub mod ui;
pub mod util;
