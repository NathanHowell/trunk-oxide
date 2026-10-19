//! Decoders' counters, readable from any thread for the metrics.
//!
//! A decoder registers what it publishes and keeps it; readers see every
//! entry, each under an id no other entry has had. An entry outlives its
//! decoder by one read, so counts a decoder made between the last read and
//! its end are still reported, once, marked gone. A channel opened per call
//! comes and goes between reads all the time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// One entry as a read saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry<R> {
    /// The entry's own: two decoders never share one.
    pub id: u64,
    /// False once its decoder is gone: this is its last report.
    pub live: bool,
    pub value: R,
}

pub(crate) struct Registry<S> {
    entries: Mutex<Vec<(u64, Arc<S>)>>,
    next: AtomicU64,
}

impl<S> Registry<S> {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        }
    }

    /// Publish `shared`; the decoder keeps the handle while it runs.
    pub(crate) fn register(&self, shared: S) -> Arc<S> {
        let shared = Arc::new(shared);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, Arc::clone(&shared)));
        shared
    }

    /// Every entry, read with `f`; the gone ones are forgotten after this.
    pub(crate) fn read<R>(&self, f: impl Fn(&S) -> R) -> Vec<Entry<R>> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let out = entries
            .iter()
            .map(|(id, s)| Entry {
                id: *id,
                live: Arc::strong_count(s) > 1,
                value: f(s),
            })
            .collect();
        entries.retain(|(_, s)| Arc::strong_count(s) > 1);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn a_gone_decoder_is_reported_once_more_then_forgotten() {
        let reg: Registry<AtomicU32> = Registry::new();
        let a = reg.register(AtomicU32::new(0));
        let b = reg.register(AtomicU32::new(0));
        a.store(3, Ordering::Relaxed);
        let read = |reg: &Registry<AtomicU32>| reg.read(|s| s.load(Ordering::Relaxed));
        let first = read(&reg);
        assert_eq!(first.len(), 2);
        assert_ne!(first[0].id, first[1].id);
        assert!(first.iter().all(|e| e.live));
        // counts made after the last read, then the decoder goes
        b.store(7, Ordering::Relaxed);
        drop(b);
        let second = read(&reg);
        assert_eq!(
            second.iter().map(|e| (e.live, e.value)).collect::<Vec<_>>(),
            vec![(true, 3), (false, 7)]
        );
        assert_eq!(read(&reg).len(), 1, "forgotten");
    }
}
