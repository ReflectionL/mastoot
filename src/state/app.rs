//! Server-side bookkeeping owned by the state task.
//!
//! Deliberately tiny. The UI layer holds the actual timeline / thread /
//! profile data (it needs it for rendering and optimistic updates
//! anyway), so keeping a second copy here would only create two
//! sources of truth. What the state task *does* need to remember is:
//!
//! - who "me" is (for the account switcher and ownership checks),
//! - the current REST health tier (so the UI is only told on change),
//! - per-timeline pagination cursors (`since_id` / `max_id`), because
//!   the polling loop and `LoadMore` both run without the UI in the
//!   loop.
//!
//! Actions run concurrently (one tokio task each), so this lives
//! behind a `std::sync::Mutex` — every critical section is a few field
//! reads, never held across an `.await`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::api::models::{Account, NotificationId, StatusId};
use crate::state::event::ApiHealth;
use crate::state::timeline::TimelineKind;

/// Newest / oldest status id seen for one timeline. `newest` feeds
/// `since_id` (poll for fresh posts), `oldest` feeds `max_id` (load
/// older posts).
#[derive(Debug, Default, Clone)]
pub struct Cursors {
    pub newest: Option<StatusId>,
    pub oldest: Option<StatusId>,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub me: Option<Account>,
    /// Overall REST health. The UI gets a dedicated
    /// [`crate::state::Event::ApiHealthChanged`] broadcast whenever this
    /// value transitions.
    pub api_health: ApiHealth,
    pub cursors: HashMap<TimelineKind, Cursors>,
    /// Oldest notification id seen so far — `LoadMore` walks back from
    /// here.
    pub notifications_oldest: Option<NotificationId>,
    /// Newest notification id seen — the polling loop fetches
    /// `since_id` from here.
    pub notifications_newest: Option<NotificationId>,
}

/// Shared handle to the state, cloned into every action task.
pub type Shared = Arc<Mutex<AppState>>;

impl AppState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn shared() -> Shared {
        Arc::new(Mutex::new(Self::new()))
    }

    pub fn cursors_mut(&mut self, kind: TimelineKind) -> &mut Cursors {
        self.cursors.entry(kind).or_default()
    }

    #[must_use]
    pub fn cursors(&self, kind: TimelineKind) -> Option<&Cursors> {
        self.cursors.get(&kind)
    }

    /// Record a full page (replace semantics): both ends move.
    pub fn note_page(
        &mut self,
        kind: TimelineKind,
        newest: Option<StatusId>,
        oldest: Option<StatusId>,
    ) {
        let c = self.cursors_mut(kind);
        if newest.is_some() {
            c.newest = newest;
        }
        if oldest.is_some() {
            c.oldest = oldest;
        }
    }
}

/// Lock helper that survives a poisoned mutex — a panic in one action
/// task must not take the whole state task down with it.
pub fn lock(shared: &Shared) -> MutexGuard<'_, AppState> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_page_only_moves_ends_that_are_present() {
        let mut s = AppState::new();
        s.note_page(
            TimelineKind::Home,
            Some(StatusId::new("9")),
            Some(StatusId::new("1")),
        );
        // A `LoadMore` page carries only an older end.
        s.note_page(TimelineKind::Home, None, Some(StatusId::new("0")));
        let c = s.cursors(TimelineKind::Home).unwrap();
        assert_eq!(c.newest.as_ref().map(StatusId::as_str), Some("9"));
        assert_eq!(c.oldest.as_ref().map(StatusId::as_str), Some("0"));
    }
}
