//! Top-level application state. Owned by the state task; never held across
//! `.await` points by the UI.

use std::collections::HashMap;

use crate::api::models::{Account, NotificationId};
use crate::state::event::{ApiHealth, StreamState};
use crate::state::timeline::{TimelineKind, TimelineStore};

#[derive(Debug)]
pub struct AppState {
    pub me: Option<Account>,
    pub timelines: HashMap<TimelineKind, TimelineStore>,
    pub stream: StreamState,
    /// Overall REST health. Mutated inside the state task; the UI gets
    /// a dedicated [`crate::state::Event::ApiHealthChanged`] broadcast
    /// whenever this value transitions.
    pub api_health: ApiHealth,
    /// Oldest notification id seen so far. Used by `LoadMore` to walk
    /// further back; updated by the notifications fetcher.
    pub notifications_oldest: Option<NotificationId>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            me: None,
            timelines: HashMap::new(),
            stream: StreamState::Disconnected,
            api_health: ApiHealth::Healthy,
            notifications_oldest: None,
        }
    }
}

impl AppState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn timeline_mut(&mut self, kind: TimelineKind) -> &mut TimelineStore {
        self.timelines.entry(kind).or_default()
    }

    pub fn timeline(&self, kind: TimelineKind) -> Option<&TimelineStore> {
        self.timelines.get(&kind)
    }
}
