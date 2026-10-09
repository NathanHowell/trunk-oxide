//! A bounded concurrency gate that admits the highest-priority waiter first.
//!
//! Replaces a fair [`tokio::sync::Semaphore`] for transcription admission. Under
//! saturation the fair semaphore serves requests strictly FIFO, so a low-value
//! transit channel transmitted just before a dispatch call would be decoded
//! first and the dispatch call would time out behind it. This gate instead wakes
//! the waiter with the most important talkgroup whenever a permit frees, while
//! still draining everything when there is spare capacity (e.g. overnight).
//!
//! Priority follows the talkgroup CSV convention: **lower number = more
//! important** (1 = dispatch). A non-positive priority (0/unset) is treated as
//! least important and sorts behind every explicitly-prioritized call. Ties
//! break FIFO by arrival sequence.
//!
//! Correctness notes:
//! - The inner [`std::sync::Mutex`] is never held across an `.await`: acquire
//!   takes the lock, decides, drops it, then awaits a oneshot; release takes the
//!   lock, pops a waiter, drops it, then sends.
//! - A permit is handed to a waiter by *moving* a [`PriorityPermit`] through the
//!   oneshot. If the waiter was cancelled (its receiver dropped), the send fails
//!   and returns the permit; we `forget` it and re-dispatch to the next waiter,
//!   so a cancelled acquire can never leak a permit (no `available += 1` was
//!   done for a handed-off permit).

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

/// A waiter parked in the heap. Ordered so [`BinaryHeap::pop`] (a max-heap)
/// yields the *most important* waiter: smallest `bucket`, then smallest `seq`.
struct Waiter {
    /// Effective priority bucket — `i32::MAX` for non-positive input so unset
    /// talkgroups sort last. Smaller is more important.
    bucket: i32,
    /// Monotonic arrival sequence for FIFO tie-breaking within a bucket.
    seq: u64,
    /// Permit handoff channel. The permit is sent through on release.
    tx: oneshot::Sender<PriorityPermit>,
}

impl PartialEq for Waiter {
    fn eq(&self, other: &Self) -> bool {
        self.bucket == other.bucket && self.seq == other.seq
    }
}
impl Eq for Waiter {}
impl Ord for Waiter {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse the natural order so the max-heap pops the smallest (bucket,
        // seq) — i.e. the most important, earliest-arriving waiter.
        other
            .bucket
            .cmp(&self.bucket)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for Waiter {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct Inner {
    available: usize,
    seq: u64,
    waiters: BinaryHeap<Waiter>,
}

/// A priority-ordered admission gate with a fixed number of permits.
pub struct PrioritySemaphore {
    inner: Mutex<Inner>,
}

impl PrioritySemaphore {
    pub fn new(permits: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                available: permits,
                seq: 0,
                waiters: BinaryHeap::new(),
            }),
        })
    }

    /// Acquire a permit, waking ahead of any lower-priority waiter. Resolves once
    /// a permit is available; the returned guard releases it on drop.
    pub async fn acquire(self: &Arc<Self>, priority: i32) -> PriorityPermit {
        let rx = {
            let mut inner = self.inner.lock().expect("poisoned");
            if inner.available > 0 {
                inner.available -= 1;
                return PriorityPermit {
                    sem: Arc::clone(self),
                };
            }
            let seq = inner.seq;
            inner.seq += 1;
            let bucket = if priority > 0 { priority } else { i32::MAX };
            let (tx, rx) = oneshot::channel();
            inner.waiters.push(Waiter { bucket, seq, tx });
            rx
        };
        // Lock is released; park until a releasing permit is handed to us. The
        // sender side only drops without sending if this future is itself being
        // dropped, so a recv error here means "cancelled" and the permit (if any)
        // is reclaimed by the release loop.
        rx.await.unwrap_or_else(|_| PriorityPermit {
            sem: Arc::clone(self),
        })
    }

    /// Permits not currently held. Used for the utilization gauge.
    pub fn available_permits(&self) -> usize {
        self.inner.lock().expect("poisoned").available
    }

    /// Number of requests currently waiting for a permit (backlog depth).
    pub fn waiting(&self) -> usize {
        self.inner.lock().expect("poisoned").waiters.len()
    }

    fn release(self: &Arc<Self>) {
        loop {
            let waiter = {
                let mut inner = self.inner.lock().expect("poisoned");
                let popped = inner.waiters.pop();
                if popped.is_none() {
                    inner.available += 1;
                }
                drop(inner);
                match popped {
                    Some(w) => w,
                    None => return,
                }
            };
            // Hand the permit off without holding the lock (the receiver's task
            // may run synchronously and re-enter release on a later drop).
            let permit = PriorityPermit {
                sem: Arc::clone(self),
            };
            match waiter.tx.send(permit) {
                Ok(()) => return,
                Err(returned) => {
                    // Waiter cancelled before we could hand it the permit. Do not
                    // drop `returned` (its Drop would recurse into release and
                    // double-count); forget it and re-dispatch to the next waiter.
                    std::mem::forget(returned);
                }
            }
        }
    }
}

/// Permit guard. Releasing on drop wakes the next-most-important waiter, or
/// returns the slot to the available pool if none are waiting.
pub struct PriorityPermit {
    sem: Arc<PrioritySemaphore>,
}

impl Drop for PriorityPermit {
    fn drop(&mut self) {
        self.sem.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fast_path_when_permits_available() {
        let sem = PrioritySemaphore::new(2);
        let _a = sem.acquire(1).await;
        let _b = sem.acquire(3).await;
        assert_eq!(sem.available_permits(), 0);
    }

    #[tokio::test]
    async fn higher_priority_served_first() {
        let sem = PrioritySemaphore::new(1);
        let held = sem.acquire(2).await; // occupy the only permit

        // Park three waiters of differing priority while the permit is held.
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (label, prio) in [("low", 3), ("high", 1), ("mid", 2)] {
            let sem = Arc::clone(&sem);
            let order = Arc::clone(&order);
            tasks.push(tokio::spawn(async move {
                let _p = sem.acquire(prio).await;
                order.lock().unwrap().push(label);
                // Hold briefly so the next waiter is admitted only after we drop.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }));
        }

        // Let all three park before releasing.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(held);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec!["high", "mid", "low"]);
    }

    #[tokio::test]
    async fn unset_priority_sorts_last() {
        let sem = PrioritySemaphore::new(1);
        let held = sem.acquire(1).await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (label, prio) in [("unset", 0), ("low", 5)] {
            let sem = Arc::clone(&sem);
            let order = Arc::clone(&order);
            tasks.push(tokio::spawn(async move {
                let _p = sem.acquire(prio).await;
                order.lock().unwrap().push(label);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(held);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec!["low", "unset"]);
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_leak_permit() {
        let sem = PrioritySemaphore::new(1);
        let held = sem.acquire(1).await;

        // Park a waiter then cancel it by dropping the acquire future.
        let fut = {
            let sem = Arc::clone(&sem);
            tokio::spawn(async move { sem.acquire(1).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        fut.abort();
        let _ = fut.await;

        // Release the held permit. The cancelled waiter must not consume it; a
        // fresh acquire should succeed promptly.
        drop(held);
        let got = tokio::time::timeout(std::time::Duration::from_millis(200), sem.acquire(1)).await;
        assert!(got.is_ok(), "permit was leaked by the cancelled waiter");
        assert_eq!(sem.available_permits(), 0);
    }
}
