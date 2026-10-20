//! Replay faster than real time: the sources set the pace, not the wall
//! clock.
//!
//! Live, a source's samples arrive at their rate and everything downstream
//! keeps up in milliseconds. A recorded file has no rate of its own, so a
//! [`Pacer`] stands in for one, and runs the sources in lockstep with the
//! monitor loop:
//!
//! - a source takes its [`Pacer::turn`] before each block: when its sample
//!   time (samples consumed / rate) is the earliest of the sources still
//!   playing (ties to the lower slot), the monitor has handled everything it
//!   sent, and the consumer downstream has room;
//! - after the block it publishes its new sample time and sends the monitor
//!   a [`RadioEvent::Paced`] marker behind the block's events;
//! - the monitor moves the state machine's clock to the capture time
//!   ([`Pacer::capture_time`], the earliest source's), runs whatever falls
//!   due, and acknowledges ([`Pacer::acknowledge`]).
//!
//! One block runs at a time, in sample-time order, and everything it caused
//! is handled before the next: a replay comes out the same every time, and
//! as real time would have it but for the monitor's few milliseconds.
//!
//! Recording timestamps ([`now`], [`wall_now`]) follow the capture time once
//! a pacer is installed, so a call's audio age means capture time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use crate::types::{EventSender, RadioEvent};

/// How long a waiting source sleeps before it looks again at the stop flag
/// and the consumer downstream (which do not wake it).
const POLL: Duration = Duration::from_millis(1);

/// Capture time for a paced replay (see the module docs). Cheap to clone.
#[derive(Clone)]
pub struct Pacer(Arc<Inner>);

impl std::fmt::Debug for Pacer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pacer")
            .field("capture_time", &self.capture_time())
            .finish_non_exhaustive()
    }
}

struct Inner {
    epoch: Instant,
    wall_epoch: SystemTime,
    /// Sources stop at this much sample time.
    limit: Option<Duration>,
    /// The consumer downstream (the replay's event recorder) has room.
    ready: Box<dyn Fn() -> bool + Send + Sync>,
    state: Mutex<State>,
    moved: Condvar,
    events: OnceLock<EventSender>,
}

#[derive(Default)]
struct State {
    /// Sources the run will have; none takes a turn until all have joined.
    expected: usize,
    sources: Vec<Clocked>,
    /// Time the monitor has been carried on to after the sources ended.
    coast: Duration,
    coast_acked: Duration,
}

#[derive(Clone, Copy, Default)]
struct Clocked {
    at: Duration,
    acked: Duration,
    ended: bool,
}

impl State {
    fn capture_time(&self) -> Duration {
        let active = self.sources.iter().filter(|s| !s.ended).map(|s| s.at).min();
        let last = self.sources.iter().map(|s| s.at).max().unwrap_or_default();
        active.unwrap_or(last).max(self.coast)
    }

    /// Whether `slot` goes next: everyone has joined, the monitor has
    /// handled every source's last block (so nothing it does next can land
    /// in the middle of this one), and no source still playing is earlier.
    fn turn_of(&self, slot: usize) -> bool {
        let me = self.sources[slot];
        self.sources.len() >= self.expected
            && self.sources.iter().all(|s| s.acked >= s.at)
            && self
                .sources
                .iter()
                .enumerate()
                .filter(|(i, s)| *i != slot && !s.ended)
                .all(|(i, s)| (s.at, i) > (me.at, slot))
    }
}

impl Pacer {
    /// A pacer whose capture time starts at wall time `wall_epoch`; sources
    /// end at `limit` of sample time, if given. `ready` says whether the
    /// consumer downstream can take more (a source waits until it can).
    pub fn new(
        wall_epoch: SystemTime,
        limit: Option<Duration>,
        ready: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(Inner {
            epoch: Instant::now(),
            wall_epoch,
            limit,
            ready: Box::new(ready),
            state: Mutex::new(State::default()),
            moved: Condvar::new(),
            events: OnceLock::new(),
        }))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait_until(&self, done: impl Fn(&State) -> bool) {
        let mut s = self.state();
        while !done(&s) {
            s = self.wait(s);
        }
        drop(s);
    }

    /// Change the state, then wake whoever waits on it.
    fn update<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let r = f(&mut self.state());
        self.0.moved.notify_all();
        r
    }

    fn wait<'a>(&self, s: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.0
            .moved
            .wait_timeout(s, POLL)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    /// The run will have `n` sources.
    pub fn expect(&self, n: usize) {
        self.update(|s| s.expected = n);
    }

    /// Where the markers go (the radio's event channel).
    pub(crate) fn attach(&self, events: EventSender) {
        let _ = self.0.events.set(events);
    }

    fn mark(&self, slot: Option<usize>, at: Duration) {
        if let Some(e) = self.0.events.get() {
            e.send_blocking(RadioEvent::Paced { slot, at });
        }
    }

    /// The earliest playing source's sample time (the last source's once
    /// all have ended, or later while coasting).
    pub fn capture_time(&self) -> Duration {
        self.state().capture_time()
    }

    /// The capture time as an instant (the epoch is when the pacer was made).
    pub fn now(&self) -> Instant {
        self.0.epoch + self.capture_time()
    }

    /// The wall time of instant `t` on the capture's clock.
    pub fn wall(&self, t: Instant) -> SystemTime {
        self.0.wall_epoch + t.saturating_duration_since(self.0.epoch)
    }

    pub fn wall_epoch(&self) -> SystemTime {
        self.0.wall_epoch
    }

    /// Every source has ended (end of file, or the limit).
    pub fn all_ended(&self) -> bool {
        let s = self.state();
        s.sources.len() >= s.expected && s.sources.iter().all(|c| c.ended)
    }

    /// A source thread starts: its slot.
    pub(crate) fn join(&self) -> usize {
        self.update(|s| {
            s.sources.push(Clocked::default());
            s.sources.len() - 1
        })
    }

    /// Wait for `slot`'s turn to run a block; false if it should stop
    /// instead (`stopping`, or it reached the limit).
    pub(crate) fn turn(&self, slot: usize, stopping: &AtomicBool) -> bool {
        let mut s = self.state();
        loop {
            if stopping.load(Ordering::Relaxed) || s.sources[slot].ended {
                return false;
            }
            if s.turn_of(slot) && (self.0.ready)() {
                return true;
            }
            s = self.wait(s);
        }
    }

    /// Source `slot` has run a block and consumed `at` of samples in all.
    pub(crate) fn advanced(&self, slot: usize, at: Duration) {
        let done = self.0.limit.is_some_and(|l| at >= l);
        self.update(|s| {
            let c = &mut s.sources[slot];
            c.at = at;
            c.ended |= done;
        });
        self.mark(Some(slot), at);
    }

    /// Source `slot` has no more samples.
    pub(crate) fn ended(&self, slot: usize) {
        let at = self.update(|s| {
            let c = &mut s.sources[slot];
            c.ended = true;
            c.at
        });
        self.mark(Some(slot), at);
    }

    /// The monitor loop has handled everything up to `slot`'s marker at `at`
    /// (`None`: a coasting step).
    pub fn acknowledge(&self, slot: Option<usize>, at: Duration) {
        self.update(|s| match slot {
            Some(i) => {
                if let Some(c) = s.sources.get_mut(i) {
                    c.acked = c.acked.max(at);
                }
            }
            None => s.coast_acked = s.coast_acked.max(at),
        });
    }

    /// After the sources have ended, carry capture time on by `by` in
    /// `step`s, waiting for the monitor to handle each (so calls time out as
    /// they would while the last of the audio played out). Blocks; call it
    /// off the async runtime.
    pub fn coast(&self, by: Duration, step: Duration) {
        let end = self.capture_time() + by;
        loop {
            let at = self.update(|s| {
                s.coast = (s.capture_time() + step).min(end);
                s.coast
            });
            self.mark(None, at);
            self.wait_until(|s| s.coast_acked >= at);
            if at >= end {
                return;
            }
        }
    }
}

/// The pacer the radio runs on, once [`install`]ed (a paced replay).
static PACER: OnceLock<Pacer> = OnceLock::new();

/// Run this process's recordings on `pacer`'s capture time.
pub(crate) fn install(pacer: &Pacer) {
    let _ = PACER.set(pacer.clone());
}

/// Now, for a recording: the capture time in a paced replay, else the
/// machine's monotonic clock.
pub(crate) fn now() -> Instant {
    PACER.get().map_or_else(Instant::now, Pacer::now)
}

/// Wall time now, for a recording (see [`now`]).
pub(crate) fn wall_now() -> SystemTime {
    PACER
        .get()
        .map_or_else(SystemTime::now, |p| p.wall(p.now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn capture_time_is_the_earliest_playing_source() {
        let p = Pacer::new(SystemTime::UNIX_EPOCH, None, || true);
        p.expect(2);
        let (a, b) = (p.join(), p.join());
        p.advanced(a, ms(3000));
        p.advanced(b, ms(1000));
        assert_eq!(p.capture_time(), ms(1000));
        p.ended(b);
        assert_eq!(p.capture_time(), ms(3000), "b ended");
        p.ended(a);
        assert!(p.all_ended());
        assert_eq!(p.capture_time(), ms(3000), "the last");
    }

    #[test]
    fn the_earliest_source_goes_once_the_monitor_has_caught_up() {
        let pacer = Pacer::new(SystemTime::UNIX_EPOCH, Some(ms(100)), || true);
        pacer.expect(2);
        let first = pacer.join();
        assert!(!pacer.state().turn_of(first), "the second has not joined");
        let second = pacer.join();
        // Both at 0: the lower slot first.
        assert!(pacer.state().turn_of(first) && !pacer.state().turn_of(second));
        pacer.advanced(first, ms(10));
        // Nobody goes until the monitor has handled the first's block; then the
        // second, the earlier.
        assert!(!pacer.state().turn_of(first) && !pacer.state().turn_of(second));
        pacer.acknowledge(Some(first), ms(10));
        assert!(!pacer.state().turn_of(first) && pacer.state().turn_of(second));
        // A waiting source is woken by the acknowledgement.
        pacer.advanced(second, ms(20));
        let waiter = pacer.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let turn = std::thread::spawn(move || waiter.turn(first, &s2));
        std::thread::sleep(ms(20));
        assert!(
            !turn.is_finished(),
            "the first waits for the second's block to be handled"
        );
        pacer.acknowledge(Some(second), ms(20));
        assert!(turn.join().unwrap());
        // At the limit a source is done.
        pacer.advanced(second, ms(100));
        assert!(!pacer.turn(second, &stop));
    }
}
