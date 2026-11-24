//! Lists left recently, kept live: their watches go on (paused, like a hidden page's), so
//! coming back shows current rows at once and lists nothing again. A few at most, within a
//! row budget, for a while after they were left; the least recently left go first.

use std::time::{Duration, Instant};

/// Lists kept at most.
pub(super) const MAX_LISTS: usize = 4;
/// Rows kept across all of them, in one window: each watch holds its rows (about 1 KB each).
/// Counted when a list is left: what its watch adds while it is kept comes on top.
pub(super) const MAX_ROWS: usize = 60_000;
/// How long a list is kept after it was left: its watch's events cost little, but a list
/// not seen for this long is unlikely to be the next one. As long as a pods list's watch
/// of their requests and limits goes on off screen: kept longer, it would come back
/// without them and list every pod again for them.
pub(super) const KEPT_FOR: Duration = crate::k8s::LINGER;

struct Entry<T> {
    key: String,
    value: T,
    rows: usize,
    left: Instant,
}

/// Kept lists by view key (clusters, kind, namespaces, custom columns), least recently
/// left first. Dropping a value stops its watches.
pub(super) struct KeptLists<T> {
    entries: Vec<Entry<T>>,
}

impl<T> Default for KeptLists<T> {
    fn default() -> Self {
        Self { entries: Vec::new() }
    }
}

impl<T> KeptLists<T> {
    /// Keep `value`, a list of `rows` rows left at `now`, under `key`, dropping the least
    /// recently left past the limits. A list bigger than the whole budget is not kept.
    pub fn put(&mut self, key: String, value: T, rows: usize, now: Instant) {
        self.entries.retain(|e| e.key != key);
        self.expire(now);
        if rows > MAX_ROWS {
            return;
        }
        self.entries.push(Entry { key, value, rows, left: now });
        while self.entries.len() > MAX_LISTS || self.rows() > MAX_ROWS {
            self.entries.remove(0);
        }
    }

    /// The list kept under `key`, out of the kept ones (it is on screen again), unless
    /// it was left too long ago.
    pub fn take(&mut self, key: &str, now: Instant) -> Option<T> {
        self.expire(now);
        let at = self.entries.iter().position(|e| e.key == key)?;
        Some(self.entries.remove(at).value)
    }

    /// Drop the lists left too long ago.
    pub fn expire(&mut self, now: Instant) {
        self.entries.retain(|e| now.saturating_duration_since(e.left) < KEPT_FOR);
    }

    /// Drop the list kept under `key`, if any.
    pub fn remove(&mut self, key: &str) {
        self.entries.retain(|e| e.key != key);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn rows(&self) -> usize {
        self.entries.iter().map(|e| e.rows).sum()
    }

    #[cfg(test)]
    fn keys(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.key.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use proptest::prelude::*;

    use super::{KEPT_FOR, KeptLists, MAX_LISTS, MAX_ROWS};

    #[test]
    fn the_least_recently_left_go_first() {
        let now = Instant::now();
        let mut k = KeptLists::default();
        for i in 0..=MAX_LISTS {
            k.put(format!("v{i}"), i, 10, now);
        }
        assert!(!k.keys().contains(&"v0"), "past the count, the oldest goes");
        // Left again: kept longest.
        k.put("v1".into(), 1, 10, now);
        k.put("big".into(), 9, MAX_ROWS - 25, now);
        assert!(k.keys().contains(&"v1") && k.keys().contains(&"big"));
        assert!(!k.keys().contains(&"v2"), "dropped to stay within the row budget");
        // A list over the whole budget is never kept, and drops the one it replaces.
        k.put("big".into(), 10, MAX_ROWS + 1, now);
        assert!(!k.keys().contains(&"big"));
    }

    #[test]
    fn taking_a_list_removes_it_and_old_ones_expire() {
        let now = Instant::now();
        let mut k = KeptLists::default();
        k.put("pods".into(), 1, 20_000, now);
        k.put("deploys".into(), 2, 300, now + KEPT_FOR / 2);
        assert_eq!(k.take("pods", now + Duration::from_secs(60)), Some(1));
        assert_eq!(k.take("pods", now + Duration::from_secs(60)), None, "on screen: not kept any more");
        k.put("pods".into(), 1, 20_000, now);
        assert_eq!(k.take("pods", now + KEPT_FOR), None, "left too long ago");
        assert_eq!(k.take("deploys", now + KEPT_FOR), Some(2));
        assert!(k.is_empty());
    }

    #[derive(Debug, Clone)]
    enum Op {
        Put(u8, usize),
        Take(u8),
        Wait(u64),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..8, 0usize..MAX_ROWS + 10_000).prop_map(|(k, r)| Op::Put(k, r)),
            (0u8..8).prop_map(Op::Take),
            (0u64..400).prop_map(Op::Wait),
        ]
    }

    proptest! {
        /// Whatever lists are left and taken, and however long apart: never more than
        /// `MAX_LISTS` lists or `MAX_ROWS` rows kept, none taken back after `KEPT_FOR`, and
        /// the list just left is kept when it fits the budget.
        #[test]
        fn kept_lists_stay_within_the_limits(ops in prop::collection::vec(op(), 0..60)) {
            let start = Instant::now();
            let mut now = start;
            let mut k = KeptLists::default();
            let mut left: std::collections::HashMap<String, Instant> = Default::default();
            for op in ops {
                match op {
                    Op::Put(key, rows) => {
                        let key = format!("k{key}");
                        k.put(key.clone(), rows, rows, now);
                        left.insert(key.clone(), now);
                        if rows <= MAX_ROWS {
                            prop_assert_eq!(k.keys().last().copied(), Some(key.as_str()));
                        } else {
                            prop_assert!(!k.keys().contains(&key.as_str()));
                        }
                    }
                    Op::Take(key) => {
                        let key = format!("k{key}");
                        if k.take(&key, now).is_some() {
                            prop_assert!(now.duration_since(left[&key]) < KEPT_FOR);
                        }
                        prop_assert!(!k.keys().contains(&key.as_str()));
                    }
                    Op::Wait(secs) => now += Duration::from_secs(secs),
                }
                prop_assert!(k.entries.len() <= MAX_LISTS);
                prop_assert!(k.rows() <= MAX_ROWS);
            }
        }
    }
}
