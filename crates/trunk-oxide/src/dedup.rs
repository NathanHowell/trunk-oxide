use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

/// Drops an item seen less than `window` ago: the same control channel
/// packet reaching us through several decoders. Only the first admission
/// starts the window (a rejected repeat does not extend it), so a packet the
/// control channel *repeats* — a grant update refreshing an active call
/// about once a second — gets through every time.
///
/// A count-based window (the last N distinct keys) suppressed those
/// refreshes indefinitely on a quiet system, letting every call run into
/// its grant timeout mid-transmission.
pub struct TimedDedup {
    window: Duration,
    /// When each key was last admitted.
    admitted: HashMap<u64, Instant>,
    /// Admissions in time order, for expiry.
    order: VecDeque<(u64, Instant)>,
}

impl TimedDedup {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            admitted: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Whether `key` is new at `now` (not admitted within the window).
    pub fn admit(&mut self, key: &impl Hash, now: Instant) -> bool {
        while let Some(&(h, at)) = self.order.front() {
            if now.duration_since(at) < self.window {
                break;
            }
            self.order.pop_front();
            if self.admitted.get(&h) == Some(&at) {
                self.admitted.remove(&h);
            }
        }
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let h = hasher.finish();
        if self.admitted.contains_key(&h) {
            return false;
        }
        self.admitted.insert(h, now);
        self.order.push_back((h, now));
        true
    }

    /// Keys inside the window.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.admitted.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = Duration::from_millis(500);

    fn at(ms: u64) -> Instant {
        static T0: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
        *T0 + Duration::from_millis(ms)
    }

    #[test]
    fn copies_from_several_decoders_are_dropped() {
        let mut d = TimedDedup::new(W);
        assert!(d.admit(&42, at(0)));
        assert!(!d.admit(&42, at(5)));
        assert!(!d.admit(&42, at(499)));
        assert!(d.admit(&43, at(10)));
    }

    #[test]
    fn repeats_a_second_apart_all_get_through() {
        // The grant-timeout bug: an active call's update repeats unchanged
        // about once a second, and every one must refresh the call.
        let mut d = TimedDedup::new(W);
        for s in 0..10 {
            assert!(d.admit(&(0i32, 2u8, 1501i64), at(s * 1000)), "repeat {s}");
            assert!(!d.admit(&(0i32, 2u8, 1501i64), at(s * 1000 + 30)));
        }
    }

    #[test]
    fn a_rejected_copy_does_not_extend_the_window() {
        let mut d = TimedDedup::new(W);
        assert!(d.admit(&1, at(0)));
        assert!(!d.admit(&1, at(400)));
        assert!(d.admit(&1, at(500)));
    }

    #[test]
    fn expired_keys_are_forgotten() {
        let mut d = TimedDedup::new(W);
        for k in 0..100 {
            d.admit(&k, at(0));
        }
        d.admit(&1000, at(600));
        assert_eq!(d.len(), 1);
    }
}
