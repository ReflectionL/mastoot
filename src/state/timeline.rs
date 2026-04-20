//! In-memory timeline: an ordered list of statuses with id-deduplication.

use std::collections::HashSet;

use crate::api::models::{Status, StatusId};

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

#[derive(Debug, Default)]
pub struct TimelineStore {
    items: Vec<Status>,
    ids: HashSet<StatusId>,
}

impl TimelineStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn items(&self) -> &[Status] {
        &self.items
    }

    /// Newest status id in the store, for `since_id` pagination.
    pub fn newest_id(&self) -> Option<&StatusId> {
        self.items.first().map(|s| &s.id)
    }

    /// Oldest status id in the store, for `max_id` pagination (load older).
    pub fn oldest_id(&self) -> Option<&StatusId> {
        self.items.last().map(|s| &s.id)
    }

    /// Prepend newer statuses (e.g. from a streaming `update` event).
    /// Preserves ordering: caller should pass statuses newest-first.
    pub fn prepend(&mut self, statuses: Vec<Status>) {
        let mut fresh: Vec<Status> = statuses
            .into_iter()
            .filter(|s| !self.ids.contains(&s.id))
            .collect();
        for s in &fresh {
            self.ids.insert(s.id.clone());
        }
        fresh.extend(std::mem::take(&mut self.items));
        self.items = fresh;
    }

    /// Append older statuses (e.g. from "load more").
    pub fn append(&mut self, statuses: Vec<Status>) {
        for s in statuses {
            if self.ids.insert(s.id.clone()) {
                self.items.push(s);
            }
        }
    }

    /// Replace the entire contents (e.g. from a hard refresh).
    pub fn replace(&mut self, statuses: Vec<Status>) {
        self.ids.clear();
        for s in &statuses {
            self.ids.insert(s.id.clone());
        }
        self.items = statuses;
    }

    /// In-place update of an already-present status (e.g. after favouriting).
    pub fn update(&mut self, updated: Status) -> bool {
        if let Some(slot) = self.items.iter_mut().find(|s| s.id == updated.id) {
            *slot = updated;
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, id: &StatusId) -> bool {
        if self.ids.remove(id) {
            self.items.retain(|s| s.id != *id);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_status(id: &str) -> Status {
        Status {
            id: StatusId(id.to_string()),
            content: format!("<p>hello {id}</p>"),
            ..Default::default()
        }
    }

    #[test]
    fn dedup_across_prepend_and_append() {
        let mut tl = TimelineStore::new();
        tl.append(vec![fake_status("3"), fake_status("2"), fake_status("1")]);
        assert_eq!(tl.len(), 3);

        tl.prepend(vec![fake_status("4"), fake_status("3")]); // 3 is dup
        assert_eq!(tl.len(), 4);
        assert_eq!(tl.newest_id().map(|i| i.0.as_str()), Some("4"));
        assert_eq!(tl.oldest_id().map(|i| i.0.as_str()), Some("1"));
    }

    #[test]
    fn update_in_place() {
        let mut tl = TimelineStore::new();
        tl.append(vec![fake_status("1")]);
        let mut updated = fake_status("1");
        updated.content = "<p>edited</p>".to_string();
        assert!(tl.update(updated));
        assert_eq!(tl.items()[0].content, "<p>edited</p>");
    }
}
