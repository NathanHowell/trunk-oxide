//! The one time source for the call state machine and its executor.
//!
//! Everything inside runs on monotonic [`Instant`]s from [`Clock::now`].
//! Wall time exists only at the edge, where a call's start or a segment is
//! reported (Discord, gRPC, `TrCallData`): [`Clock::wall`] maps an instant
//! to wall time through one anchor taken when the clock was made, so
//! reported times never step backwards with NTP and a duration is always
//! the difference of two instants.
//!
//! [`Clock::system`] follows the machine's monotonic clock. [`Clock::manual`]
//! only moves when told to, so a replay can run on its capture's time (and
//! faster than real time) and tests need not sleep.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Debug)]
pub struct Clock(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    /// The instant `wall_epoch` corresponds to.
    epoch: Instant,
    wall_epoch: SystemTime,
    /// `None`: the system clock. `Some`: nanoseconds since `epoch`.
    manual: Option<AtomicU64>,
}

impl Clock {
    /// The machine's monotonic clock, anchored to the wall clock now.
    pub fn system() -> Self {
        Self(Arc::new(Inner {
            epoch: Instant::now(),
            wall_epoch: SystemTime::now(),
            manual: None,
        }))
    }

    /// A clock that starts at wall time `wall_epoch` and moves only by
    /// [`Self::advance_to`] / [`Self::advance`].
    pub fn manual(wall_epoch: SystemTime) -> Self {
        Self(Arc::new(Inner {
            epoch: Instant::now(),
            wall_epoch,
            manual: Some(AtomicU64::new(0)),
        }))
    }

    pub fn now(&self) -> Instant {
        self.0.manual.as_ref().map_or_else(Instant::now, |ns| {
            self.0.epoch + Duration::from_nanos(ns.load(Ordering::Acquire))
        })
    }

    /// Time since `t`, never negative.
    pub fn since(&self, t: Instant) -> Duration {
        self.now().saturating_duration_since(t)
    }

    /// The wall time of instant `t`.
    pub fn wall(&self, t: Instant) -> SystemTime {
        t.checked_duration_since(self.0.epoch).map_or_else(
            || self.0.wall_epoch - self.0.epoch.duration_since(t),
            |d| self.0.wall_epoch + d,
        )
    }

    pub fn wall_now(&self) -> SystemTime {
        self.wall(self.now())
    }

    /// Time since the clock started.
    pub fn elapsed(&self) -> Duration {
        self.since(self.0.epoch)
    }

    /// Moves a manual clock to `elapsed` since its start, if that is later
    /// than where it is (it never runs backwards). No-op on the system clock.
    pub fn advance_to(&self, elapsed: Duration) {
        if let Some(ns) = &self.0.manual {
            let to = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
            ns.fetch_max(to, Ordering::AcqRel);
        }
    }

    /// Moves a manual clock on by `d`. No-op on the system clock.
    pub fn advance(&self, d: Duration) {
        if let Some(ns) = &self.0.manual {
            let by = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
            ns.fetch_add(by, Ordering::AcqRel);
        }
    }

    pub fn is_manual(&self) -> bool {
        self.0.manual.is_some()
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::system()
    }
}

/// Seconds since the Unix epoch (0 before it).
pub fn unix_seconds(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_clock_moves_only_when_told() {
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_760_000_000);
        let c = Clock::manual(wall);
        let t0 = c.now();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(c.now(), t0);
        c.advance(Duration::from_secs(3));
        assert_eq!(c.since(t0), Duration::from_secs(3));
        assert_eq!(c.wall_now(), wall + Duration::from_secs(3));
        // Never backwards.
        c.advance_to(Duration::from_secs(1));
        assert_eq!(c.elapsed(), Duration::from_secs(3));
        c.advance_to(Duration::from_secs(10));
        assert_eq!(c.elapsed(), Duration::from_secs(10));
    }

    #[test]
    fn wall_time_is_anchored_once() {
        let c = Clock::system();
        let t = c.now();
        let later = t + Duration::from_secs(60);
        assert_eq!(
            c.wall(later).duration_since(c.wall(t)).unwrap(),
            Duration::from_secs(60)
        );
        // Instants before the anchor map before it.
        let before = c.0.epoch.checked_sub(Duration::from_secs(1));
        if let Some(b) = before {
            assert_eq!(
                c.0.wall_epoch.duration_since(c.wall(b)).unwrap(),
                Duration::from_secs(1)
            );
        }
    }

    #[test]
    fn since_is_never_negative() {
        let c = Clock::manual(SystemTime::UNIX_EPOCH);
        let future = c.now() + Duration::from_secs(5);
        assert_eq!(c.since(future), Duration::ZERO);
    }
}
