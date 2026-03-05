use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};

/// Sliding-window deduplicator. Keeps hashes of the last `capacity` admitted
/// items and rejects items whose hash has been seen within that window.
pub struct WindowedDedup {
    recent: VecDeque<u64>,
    capacity: usize,
}

impl WindowedDedup {
    pub fn new(capacity: usize) -> Self {
        Self {
            recent: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns `true` if the key is new (not seen in the window).
    pub fn admit(&mut self, key: &impl Hash) -> bool {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let h = hasher.finish();

        if self.recent.contains(&h) {
            return false;
        }
        if self.recent.len() >= self.capacity {
            self.recent.pop_front();
        }
        self.recent.push_back(h);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_first_occurrence() {
        let mut d = WindowedDedup::new(4);
        assert!(d.admit(&42));
    }

    #[test]
    fn rejects_duplicate_within_window() {
        let mut d = WindowedDedup::new(4);
        assert!(d.admit(&42));
        assert!(!d.admit(&42));
    }

    #[test]
    fn admits_different_keys() {
        let mut d = WindowedDedup::new(4);
        assert!(d.admit(&1));
        assert!(d.admit(&2));
        assert!(d.admit(&3));
    }

    #[test]
    fn evicts_oldest_when_full() {
        let mut d = WindowedDedup::new(3);
        assert!(d.admit(&1));
        assert!(d.admit(&2));
        assert!(d.admit(&3));
        // Window full: [1, 2, 3]. Adding 4 evicts 1.
        assert!(d.admit(&4));
        // 1 was evicted, should be admitted again.
        assert!(d.admit(&1));
        // 2 should still be in window.
        assert!(!d.admit(&2));
    }

    #[test]
    fn works_with_tuples() {
        let mut d = WindowedDedup::new(8);
        assert!(d.admit(&(0i32, 1u8, 100i64)));
        assert!(!d.admit(&(0i32, 1u8, 100i64)));
        assert!(d.admit(&(0i32, 1u8, 200i64)));
    }

    #[test]
    fn capacity_one() {
        let mut d = WindowedDedup::new(1);
        assert!(d.admit(&"a"));
        assert!(!d.admit(&"a"));
        // Adding "b" evicts "a"
        assert!(d.admit(&"b"));
        assert!(d.admit(&"a"));
    }
}
