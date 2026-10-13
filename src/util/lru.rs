//! A tiny capacity-bounded map that evicts the least-recently-used entry.
//!
//! Used for caches that would otherwise only ever grow during a long
//! session (decoded images). Eviction is O(n) in the cache size, which
//! is fine for the few dozen entries these caches hold and keeps the
//! implementation dependency-free.

use std::collections::HashMap;
use std::hash::Hash;

/// Map with a fixed capacity. Reading through [`LruMap::get_mut`] or
/// writing through [`LruMap::insert`] marks the entry as most recently
/// used; inserting past capacity drops the stalest entry.
#[derive(Debug)]
pub struct LruMap<K, V> {
    cap: usize,
    clock: u64,
    map: HashMap<K, (V, u64)>,
}

impl<K: Hash + Eq + Clone, V> LruMap<K, V> {
    /// Create an empty map holding at most `cap` entries (minimum 1).
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            clock: 0,
            map: HashMap::new(),
        }
    }

    /// Whether `key` is cached. Does not refresh recency.
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Mutable access to an entry, refreshing its recency.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.clock += 1;
        let now = self.clock;
        self.map.get_mut(key).map(|(v, used)| {
            *used = now;
            v
        })
    }

    /// Insert (or replace) an entry as most recently used, evicting the
    /// stalest one if the map would exceed its capacity.
    pub fn insert(&mut self, key: K, value: V) {
        self.clock += 1;
        if !self.map.contains_key(&key)
            && self.map.len() >= self.cap
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
        {
            self.map.remove(&oldest);
        }
        self.map.insert(key, (value, self.clock));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used() {
        let mut m = LruMap::new(2);
        m.insert("a", 1);
        m.insert("b", 2);
        // Touch "a" so "b" becomes the stalest.
        assert_eq!(m.get_mut(&"a"), Some(&mut 1));
        m.insert("c", 3);
        assert!(m.contains_key(&"a"));
        assert!(!m.contains_key(&"b"));
        assert!(m.contains_key(&"c"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn replacing_existing_key_does_not_evict() {
        let mut m = LruMap::new(2);
        m.insert("a", 1);
        m.insert("b", 2);
        m.insert("a", 10);
        assert_eq!(m.len(), 2);
        assert_eq!(m.get_mut(&"a"), Some(&mut 10));
        assert!(m.contains_key(&"b"));
    }
}
