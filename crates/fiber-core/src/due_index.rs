//! In-memory earliest-due cache (memoturn DueIndex pattern).
//!
//! `record` only moves a key earlier (may over-report, never miss).
//! `set` is authoritative after a sweep. Not a source of truth — seed from DB on start.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct DueIndex<K: Eq + Hash> {
    inner: Mutex<HashMap<K, DateTime<Utc>>>,
}

impl<K: Eq + Hash + Clone> DueIndex<K> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Note that `key` has work due no later than `due` (keeps the earliest).
    pub fn record(&self, key: K, due: DateTime<Utc>) {
        let mut map = self.inner.lock().expect("due index lock");
        match map.get(&key) {
            Some(current) if *current <= due => {}
            _ => {
                map.insert(key, due);
            }
        }
    }

    /// Authoritatively set (or clear when `due` is `None`) a key's next due time.
    pub fn set(&self, key: K, due: Option<DateTime<Utc>>) {
        let mut map = self.inner.lock().expect("due index lock");
        match due {
            Some(d) => {
                map.insert(key, d);
            }
            None => {
                map.remove(&key);
            }
        }
    }

    /// Keys with work due by `now`.
    pub fn due_keys(&self, now: DateTime<Utc>) -> Vec<K> {
        let map = self.inner.lock().expect("due index lock");
        map.iter()
            .filter(|(_, due)| **due <= now)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Earliest due time across all keys, if any.
    pub fn earliest(&self) -> Option<DateTime<Utc>> {
        let map = self.inner.lock().expect("due index lock");
        map.values().copied().min()
    }

    pub fn clear(&self) {
        self.inner.lock().expect("due index lock").clear();
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("due index lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn record_keeps_earliest() {
        let idx = DueIndex::new();
        let t1 = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 11, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 1, 1, 13, 0, 0).unwrap();
        idx.record(1u32, t1);
        idx.record(1u32, t2);
        idx.record(1u32, t0);
        assert_eq!(idx.earliest(), Some(t0));
    }

    #[test]
    fn set_clears_and_authoritative() {
        let idx = DueIndex::new();
        let t1 = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 1, 1, 14, 0, 0).unwrap();
        idx.record(1u32, t1);
        idx.set(1u32, Some(t2));
        assert_eq!(idx.earliest(), Some(t2));
        idx.set(1u32, None);
        assert!(idx.is_empty());
    }

    #[test]
    fn due_keys_filters() {
        let idx = DueIndex::new();
        let past = Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 0).unwrap();
        let future = Utc.with_ymd_and_hms(2026, 1, 1, 14, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        idx.record("a", past);
        idx.record("b", future);
        let due = idx.due_keys(now);
        assert_eq!(due, vec!["a"]);
    }
}
