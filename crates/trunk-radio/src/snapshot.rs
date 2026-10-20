//! Raw samples from a running source, now and then.
//!
//! For whoever measures them (trunk-oxide's live noise floor): the source
//! thread only copies a run of its stream and hands it over; the measuring
//! happens elsewhere.

use std::sync::mpsc::SyncSender;
use std::time::Duration;

use num_complex::Complex32;

/// A run of one source's samples as they came off the stream.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// The radio's index of the source.
    pub source: i32,
    /// The device string as configured.
    pub device: String,
    /// Stream time of its first sample (capture time in a replay).
    pub at: Duration,
    pub iq: Vec<Complex32>,
}

/// Where and how often the sources take snapshots.
#[derive(Clone, Debug)]
pub struct Snapshots {
    /// Stream time between snapshots; the first is taken one period in, once
    /// the stream has settled.
    pub every: Duration,
    /// Samples in each.
    pub len: usize,
    /// A snapshot the receiver is not ready for is dropped, never waited on.
    pub tx: SyncSender<Snapshot>,
}

/// One source thread's side of [`Snapshots`].
pub(crate) struct Taker {
    cfg: Snapshots,
    source: i32,
    device: String,
    rate_hz: f64,
    /// Samples read so far.
    read: u64,
    /// Where the next snapshot starts, in samples.
    next: u64,
    filling: Option<Snapshot>,
}

impl Taker {
    pub(crate) fn new(cfg: Snapshots, source: i32, device: String, rate_hz: f64) -> Self {
        let next = samples(cfg.every, rate_hz);
        Self {
            cfg,
            source,
            device,
            rate_hz,
            read: 0,
            next,
            filling: None,
        }
    }

    /// Note `block`, the next samples of the stream.
    pub(crate) fn block(&mut self, block: &[Complex32]) {
        let start = self.read;
        self.read += block.len() as u64;
        if self.filling.is_none() && self.read > self.next {
            #[allow(clippy::cast_precision_loss)]
            let at = Duration::from_secs_f64(self.next as f64 / self.rate_hz);
            self.filling = Some(Snapshot {
                source: self.source,
                device: self.device.clone(),
                at,
                iq: Vec::with_capacity(self.cfg.len),
            });
        }
        let Some(s) = &mut self.filling else {
            return;
        };
        #[allow(clippy::cast_possible_truncation)]
        let from = self.next.saturating_sub(start).min(block.len() as u64) as usize;
        let want = self.cfg.len - s.iq.len();
        let tail = &block[from..];
        s.iq.extend_from_slice(&tail[..want.min(tail.len())]);
        if s.iq.len() == self.cfg.len
            && let Some(done) = self.filling.take()
        {
            self.next = samples(done.at + self.cfg.every, self.rate_hz);
            // Full: the receiver is still measuring the last one.
            let _ = self.cfg.tx.try_send(done);
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn samples(t: Duration, rate_hz: f64) -> u64 {
    (t.as_secs_f64() * rate_hz).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn ramp(from: u64, n: usize) -> Vec<Complex32> {
        #[allow(clippy::cast_precision_loss)]
        (0..n as u64)
            .map(|i| Complex32::new((from + i) as f32, 0.0))
            .collect()
    }

    #[test]
    fn takes_contiguous_runs_on_stream_time_across_blocks() {
        let (tx, rx) = sync_channel(4);
        let cfg = Snapshots {
            every: Duration::from_secs(1),
            len: 250,
            tx,
        };
        let mut t = Taker::new(cfg, 2, "rtl=1".into(), 1000.0);
        let mut at = 0;
        while at < 3500 {
            t.block(&ramp(at, 100));
            at += 100;
        }
        let got: Vec<Snapshot> = rx.try_iter().collect();
        assert_eq!(got.len(), 3);
        for (k, s) in got.iter().enumerate() {
            let first = 1000 * (k as u64 + 1);
            assert_eq!(s.at, Duration::from_secs(k as u64 + 1));
            assert_eq!((s.source, s.device.as_str()), (2, "rtl=1"));
            assert_eq!(s.iq, ramp(first, 250), "snapshot {k}");
        }
    }

    #[test]
    fn a_busy_receiver_loses_a_snapshot_and_the_stream_goes_on() {
        let (tx, rx) = sync_channel(1);
        let cfg = Snapshots {
            every: Duration::from_millis(100),
            len: 10,
            tx,
        };
        let mut t = Taker::new(cfg, 0, String::new(), 1000.0);
        for k in 0..10 {
            t.block(&ramp(k * 64, 64));
        }
        // six fell due; one fit in the queue
        assert_eq!(rx.try_iter().count(), 1);
        t.block(&ramp(640, 128));
        assert_eq!(rx.try_iter().count(), 1);
    }
}
