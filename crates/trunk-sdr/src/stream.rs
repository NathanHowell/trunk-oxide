//! A bounded sample queue between a device's reader thread and the DSP.
//!
//! The reader pushes blocks of converted samples; the consumer pulls as many
//! as it wants. When the consumer falls behind by more than the queue's
//! capacity, whole blocks are dropped at the producer and counted, so the
//! queue never grows and the loss is visible in the metrics rather than in
//! USB overruns nobody can see.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::time::Duration;

use num_complex::Complex32;

/// What a stream has done so far; readable from any thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// Samples the consumer was offered (queued).
    pub samples: u64,
    /// Samples dropped because the queue was full.
    pub dropped: u64,
    /// Times a block was dropped.
    pub drops: u64,
    /// USB transfers received.
    pub transfers: u64,
    /// Samples the ADC clipped (a component on its rail).
    pub clipped: u64,
    /// The reader is still delivering.
    pub running: bool,
}

struct Queue {
    blocks: VecDeque<Vec<Complex32>>,
    /// Samples in `blocks`, less the `head` already consumed.
    len: usize,
    /// Consumed prefix of `blocks[0]`.
    head: usize,
    /// Blocks handed back for reuse.
    spare: Vec<Vec<Complex32>>,
}

pub(crate) struct Inner {
    label: String,
    capacity: usize,
    queue: Mutex<Queue>,
    /// Samples arrived (or the stream ended).
    ready: Condvar,
    /// Samples were taken: room for a waiting producer.
    space: Condvar,
    ended: AtomicBool,
    samples: AtomicU64,
    dropped: AtomicU64,
    drops: AtomicU64,
    transfers: AtomicU64,
    clipped: AtomicU64,
}

static REGISTRY: Mutex<Vec<Weak<Inner>>> = Mutex::new(Vec::new());

/// Every live stream's label and counters.
pub fn streams() -> Vec<(String, StreamStats)> {
    let mut reg = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    reg.retain(|w| w.strong_count() > 0);
    reg.iter()
        .filter_map(Weak::upgrade)
        .map(|s| (s.label.clone(), s.stats()))
        .collect()
}

impl Inner {
    fn stats(&self) -> StreamStats {
        StreamStats {
            samples: self.samples.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            drops: self.drops.load(Ordering::Relaxed),
            transfers: self.transfers.load(Ordering::Relaxed),
            clipped: self.clipped.load(Ordering::Relaxed),
            running: !self.ended.load(Ordering::Relaxed),
        }
    }
}

/// The producer's end: owned by the reader thread.
pub struct Producer {
    inner: Arc<Inner>,
}

impl Producer {
    /// A block to fill (reused when the consumer has returned one).
    pub fn take_block(&self) -> Vec<Complex32> {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spare
            .pop()
            .map_or_else(Vec::new, |mut v| {
                v.clear();
                v
            })
    }

    /// Queue a filled block; dropped (and counted) if it does not fit.
    pub fn push(&self, block: Vec<Complex32>) {
        let n = block.len();
        let mut q = self
            .inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if q.len + n > self.inner.capacity {
            self.inner.dropped.fetch_add(n as u64, Ordering::Relaxed);
            self.inner.drops.fetch_add(1, Ordering::Relaxed);
            q.spare.push(block);
            return;
        }
        q.len += n;
        q.blocks.push_back(block);
        drop(q);
        self.inner.samples.fetch_add(n as u64, Ordering::Relaxed);
        self.inner.ready.notify_one();
    }

    /// Queue a filled block, waiting for room if it does not fit (a file
    /// player must not lose samples). Returns false, with the block
    /// unqueued, once `stop` is set or the stream has ended.
    pub fn push_wait(&self, block: Vec<Complex32>, stop: &AtomicBool) -> bool {
        let n = block.len();
        let mut q = self
            .inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while q.len + n > self.inner.capacity {
            if stop.load(Ordering::Relaxed) || self.inner.ended.load(Ordering::SeqCst) {
                return false;
            }
            q = self
                .inner
                .space
                .wait_timeout(q, Duration::from_millis(50))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        q.len += n;
        q.blocks.push_back(block);
        drop(q);
        self.inner.samples.fetch_add(n as u64, Ordering::Relaxed);
        self.inner.ready.notify_one();
        true
    }

    pub fn count_transfer(&self) {
        self.inner.transfers.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count_clipped(&self, n: u64) {
        self.inner.clipped.fetch_add(n, Ordering::Relaxed);
    }

    /// No more blocks will come; readers drain what is queued and then see 0.
    pub fn end(&self) {
        self.inner.ended.store(true, Ordering::SeqCst);
        self.inner.ready.notify_all();
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.end();
    }
}

/// The consumer's end.
pub struct Stream {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("label", &self.inner.label)
            .field("stats", &self.inner.stats())
            .finish()
    }
}

impl Stream {
    /// A queue holding up to `capacity` samples, registered under `label`
    /// for [`streams`].
    pub fn new(label: impl Into<String>, capacity: usize) -> (Producer, Self) {
        let inner = Arc::new(Inner {
            label: label.into(),
            capacity,
            queue: Mutex::new(Queue {
                blocks: VecDeque::new(),
                len: 0,
                head: 0,
                spare: Vec::new(),
            }),
            ready: Condvar::new(),
            space: Condvar::new(),
            ended: AtomicBool::new(false),
            samples: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            drops: AtomicU64::new(0),
            transfers: AtomicU64::new(0),
            clipped: AtomicU64::new(0),
        });
        REGISTRY
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&inner));
        (
            Producer {
                inner: Arc::clone(&inner),
            },
            Self { inner },
        )
    }

    pub fn label(&self) -> &str {
        &self.inner.label
    }

    pub fn stats(&self) -> StreamStats {
        self.inner.stats()
    }

    /// True once the producer has ended and the queue is drained.
    pub fn is_ended(&self) -> bool {
        self.inner.ended.load(Ordering::SeqCst)
            && self
                .inner
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len
                == 0
    }

    /// Fill `out` with queued samples, waiting up to `timeout` for the
    /// first; returns how many were written. 0 means the wait ran out, or
    /// the stream has ended (see [`Self::is_ended`]).
    #[allow(clippy::significant_drop_tightening)]
    pub fn read(&self, out: &mut [Complex32], timeout: Duration) -> usize {
        let mut q = self
            .inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if q.len == 0 {
            if self.inner.ended.load(Ordering::SeqCst) {
                return 0;
            }
            let (guard, _) = self
                .inner
                .ready
                .wait_timeout_while(q, timeout, |q| {
                    q.len == 0 && !self.inner.ended.load(Ordering::SeqCst)
                })
                .unwrap_or_else(PoisonError::into_inner);
            q = guard;
        }
        let mut written = 0;
        while written < out.len() {
            let Some(front) = q.blocks.front() else { break };
            let front_len = front.len();
            let avail = &front[q.head..];
            let n = avail.len().min(out.len() - written);
            out[written..written + n].copy_from_slice(&avail[..n]);
            written += n;
            q.head += n;
            q.len -= n;
            if q.head == front_len {
                let done = q.blocks.pop_front().expect("front exists");
                q.head = 0;
                if q.spare.len() < 4 {
                    q.spare.push(done);
                }
            }
        }
        if written > 0 {
            self.inner.space.notify_one();
        }
        written
    }
}

/// A source's device or player, kept alive while its stream is consumed.
/// Stops on drop; `stop` makes that explicit and waits for the thread.
pub trait Running: Send + std::fmt::Debug {
    fn stop(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(start: usize, n: usize) -> Vec<Complex32> {
        #[allow(clippy::cast_precision_loss)]
        (start..start + n)
            .map(|i| Complex32::new(i as f32, 0.0))
            .collect()
    }

    #[test]
    fn reads_cross_block_boundaries_in_order() {
        let (p, s) = Stream::new("t", 100);
        p.push(block(0, 10));
        p.push(block(10, 10));
        let mut out = [Complex32::ZERO; 15];
        assert_eq!(s.read(&mut out, Duration::ZERO), 15);
        assert_eq!(out[14].re, 14.0);
        assert_eq!(s.read(&mut out, Duration::ZERO), 5);
        assert_eq!(out[4].re, 19.0);
        assert_eq!(s.read(&mut out, Duration::from_millis(1)), 0);
        assert!(!s.is_ended());
        assert_eq!(s.stats().samples, 20);
    }

    #[test]
    fn full_queue_drops_whole_blocks_and_counts_them() {
        let (p, s) = Stream::new("t", 25);
        p.push(block(0, 10));
        p.push(block(10, 10));
        p.push(block(20, 10)); // 30 > 25: dropped
        let st = s.stats();
        assert_eq!((st.samples, st.dropped, st.drops), (20, 10, 1));
        let mut out = [Complex32::ZERO; 30];
        assert_eq!(s.read(&mut out, Duration::ZERO), 20);
        // the dropped block's Vec is reused
        assert!(p.take_block().capacity() >= 10);
    }

    #[test]
    fn end_drains_then_reports_zero() {
        let (p, s) = Stream::new("t", 100);
        p.push(block(0, 3));
        p.end();
        let mut out = [Complex32::ZERO; 8];
        assert_eq!(s.read(&mut out, Duration::ZERO), 3);
        assert_eq!(s.read(&mut out, Duration::from_secs(1)), 0);
        assert!(s.is_ended());
        assert!(!s.stats().running);
    }

    #[test]
    fn push_wait_blocks_until_the_reader_makes_room() {
        let (p, s) = Stream::new("t", 10);
        p.push(block(0, 10));
        let stop = Arc::new(AtomicBool::new(false));
        let st = Arc::clone(&stop);
        let t = std::thread::spawn(move || p.push_wait(block(10, 5), &st));
        std::thread::sleep(Duration::from_millis(30));
        assert!(!t.is_finished(), "should be waiting for room");
        let mut out = [Complex32::ZERO; 6];
        assert_eq!(s.read(&mut out, Duration::ZERO), 6);
        // 4 left + 5 = 9 <= 10: the block fits now that a whole one is gone
        assert!(t.join().unwrap());
        assert_eq!(s.stats().samples, 15);
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn waiting_reader_wakes_on_push() {
        let (p, s) = Stream::new("t", 100);
        let t = std::thread::spawn(move || {
            let mut out = [Complex32::ZERO; 4];
            s.read(&mut out, Duration::from_secs(5))
        });
        std::thread::sleep(Duration::from_millis(20));
        p.push(block(0, 4));
        assert_eq!(t.join().unwrap(), 4);
    }

    #[test]
    fn registry_lists_live_streams() {
        let (p, s) = Stream::new("registry-test", 10);
        assert!(streams().iter().any(|(l, _)| l == "registry-test"));
        drop(s);
        drop(p);
        assert!(!streams().iter().any(|(l, _)| l == "registry-test"));
    }
}
