//! Timeline identifiers shared by the UI and the state task.
//!
//! The statuses themselves live UI-side (see [`crate::state::AppState`]
//! for why); the state task only tracks pagination cursors per kind.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimelineKind {
    Home,
    Local,
    Federated,
    Notifications,
    /// Ad-hoc single-user profile timeline.
    Profile,
    /// Favourited statuses.
    Favourites,
    /// Bookmarked statuses.
    Bookmarks,
}

impl TimelineKind {
    /// Lower-case label for toasts and the status bar.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Local => "local",
            Self::Federated => "federated",
            Self::Notifications => "notifications",
            Self::Profile => "profile",
            Self::Favourites => "favourites",
            Self::Bookmarks => "bookmarks",
        }
    }
}
