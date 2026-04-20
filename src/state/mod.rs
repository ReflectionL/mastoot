//! Application state layer. UI emits [`Action`]s; the state task processes
//! them, talks to [`crate::api`], and emits [`Event`]s back to the UI.

pub mod app;
pub mod event;
pub mod task;
pub mod timeline;

pub use app::AppState;
pub use event::{Action, ApiHealth, Event, StreamMode, StreamState, ToastLevel, Visibility};
pub use task::{Handle, spawn};
pub use timeline::{TimelineKind, TimelineStore};
