//! A source: a sample stream, the shared channelizer it feeds, and the
//! thread that runs the two. Everything decoded on the source's slots runs
//! on that thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use num_complex::Complex32;
use trunk_channelizer::{ConditioningSettings, DetectorConfig, NARROW, ProfileId};
use trunk_sdr::{Running, Stream};

use crate::ChannelizerStats;
use crate::correction::Correction;
use crate::kind::Chz;
use crate::pace::Pacer;
use crate::snapshot::{Snapshots, Taker};
use crate::types::{AudioSink, Decoded, DetectedSignal, EventSender, RadioEvent};

/// How long a block read waits before the thread checks for a stop.
const READ_WAIT: Duration = Duration::from_millis(100);

/// Live channelizers by source index (see [`channelizer_stats`]).
static CHANNELIZERS: Mutex<Vec<(i32, Weak<Chz>)>> = Mutex::new(Vec::new());

/// Counters for every live shared channelizer. `max_ns` is the longest block
/// since the previous call (it is reset by reading).
pub fn channelizer_stats() -> Vec<ChannelizerStats> {
    let reg = CHANNELIZERS.lock().unwrap_or_else(PoisonError::into_inner);
    reg.iter()
        .filter_map(|(source, w)| {
            let c = w.upgrade()?;
            Some(ChannelizerStats {
                source: *source,
                counters: c.take_counters(),
                block_budget_ns: c.block_budget_ns(),
            })
        })
        .collect()
}

struct Thread {
    chz: Arc<Chz>,
    stream: Stream,
    events: EventSender,
    sink: Arc<dyn AudioSink>,
    stopping: Arc<AtomicBool>,
    got_samples: Arc<AtomicBool>,
    idx: i32,
    center_hz: f64,
    rate_hz: f64,
    device: String,
    pacer: Option<Pacer>,
    snapshots: Option<Taker>,
}

impl Thread {
    fn run(mut self) {
        let hop = self.chz.config().hop();
        let mut buf = vec![Complex32::ZERO; hop];
        let mut filled = 0;
        let mut out = Decoded::default();
        // A paced replay runs one block at a time across the sources (see
        // `crate::pace`).
        let slot = self.pacer.as_ref().map(Pacer::join);
        let mut consumed = 0u64;
        while !self.stopping.load(Ordering::Relaxed) {
            let n = self.stream.read(&mut buf[filled..], READ_WAIT);
            filled += n;
            if filled < hop {
                if n == 0 && self.stream.is_ended() {
                    break;
                }
                continue;
            }
            filled = 0;
            if let (Some(p), Some(slot)) = (&self.pacer, slot)
                && !p.turn(slot, &self.stopping)
            {
                break;
            }
            if let Some(t) = &mut self.snapshots {
                t.block(&buf);
            }
            self.chz.run(&buf, &mut out);
            self.got_samples.store(true, Ordering::Relaxed);
            self.route(&mut out);
            if let (Some(p), Some(slot)) = (&self.pacer, slot) {
                consumed += hop as u64;
                #[allow(clippy::cast_precision_loss)]
                p.advanced(
                    slot,
                    Duration::from_secs_f64(consumed as f64 / self.rate_hz),
                );
            }
        }
        if let (Some(p), Some(slot)) = (&self.pacer, slot) {
            p.ended(slot);
        }
        tracing::info!(source = self.idx, device = %self.device, "source thread finished");
    }

    /// Send on what the block decoded: audio to the sink, events to the
    /// monitor loop, and what the signal detector found.
    fn route(&self, out: &mut Decoded) {
        for (call, pcm) in out.audio.drain(..) {
            self.sink.audio(call, &pcm);
        }
        for e in out.events.drain(..) {
            self.events.send(e);
        }
        let found = self.chz.take_detections();
        if !found.is_empty() {
            #[allow(clippy::cast_possible_truncation)]
            let signals = found
                .into_iter()
                .map(|d| DetectedSignal {
                    center_freq: d.center_hz,
                    max_rssi: d.max_rssi_db as i32,
                    threshold: d.threshold_db as i32,
                })
                .collect();
            self.events.send(RadioEvent::SignalsDetected {
                source_num: self.idx,
                source_center_freq: self.center_hz,
                signals,
            });
        }
    }
}

/// What a source is tuned to, and the pacer it keeps time for in a replay
/// faster than real time.
#[derive(Clone, Debug)]
pub struct SourceSpec {
    pub device: String,
    pub center_hz: f64,
    pub rate_hz: f64,
    pub pacer: Option<Pacer>,
    pub snapshots: Option<Snapshots>,
}

/// One SDR (or file) and its channelizer, running.
pub struct Source {
    pub idx: i32,
    pub center_hz: f64,
    pub rate_hz: f64,
    pub device: String,
    pub channelizer: Arc<Chz>,
    /// The software clock correction added to every channel tuned on it.
    pub correction: Correction,
    /// The hardware's (whole-ppm) correction, for the ppm the FLL errors
    /// suggest.
    pub ppm: f64,
    next_port: usize,
    stopping: Arc<AtomicBool>,
    got_samples: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    running: Box<dyn Running>,
    detector: bool,
    detector_threshold: Option<f32>,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("idx", &self.idx)
            .field("device", &self.device)
            .field("center_hz", &self.center_hz)
            .field("rate_hz", &self.rate_hz)
            .finish_non_exhaustive()
    }
}

impl Source {
    /// Start `stream` (kept alive by `running`) into a channelizer built for
    /// `spec.rate_hz`, on its own thread.
    pub fn start(
        idx: i32,
        spec: SourceSpec,
        stream: Stream,
        running: Box<dyn Running>,
        conditioning: ConditioningSettings,
        events: EventSender,
        sink: Arc<dyn AudioSink>,
    ) -> Result<Self> {
        let SourceSpec {
            device,
            center_hz,
            rate_hz,
            pacer,
            snapshots,
        } = spec;
        let cfg = trunk_channelizer::Config::standard(rate_hz)
            .with_context(|| format!("channelizer for {rate_hz} S/s"))?;
        let channelizer = Arc::new(Chz::with_conditioning(cfg, conditioning)?);
        {
            let mut reg = CHANNELIZERS.lock().unwrap_or_else(PoisonError::into_inner);
            reg.retain(|(s, w)| *s != idx && w.strong_count() > 0);
            reg.push((idx, Arc::downgrade(&channelizer)));
        }
        tracing::info!(
            source = idx,
            device = %device,
            narrow_rate = channelizer.config().out_rate(NARROW),
            wide = channelizer.config().profiles.len() > 1,
            hop = channelizer.config().hop(),
            "source channelizer attached"
        );
        let stopping = Arc::new(AtomicBool::new(false));
        let got_samples = Arc::new(AtomicBool::new(false));
        let t = Thread {
            chz: Arc::clone(&channelizer),
            stream,
            events,
            sink,
            stopping: Arc::clone(&stopping),
            got_samples: Arc::clone(&got_samples),
            idx,
            center_hz,
            rate_hz,
            device: device.clone(),
            pacer,
            snapshots: snapshots.map(|s| Taker::new(s, idx, device.clone(), rate_hz)),
        };
        let thread = std::thread::Builder::new()
            .name(format!("source-{idx}"))
            .spawn(move || t.run())
            .context("spawning the source thread")?;
        Ok(Self {
            idx,
            center_hz,
            rate_hz,
            device,
            channelizer,
            correction: Correction::new(0.0, false),
            ppm: 0.0,
            next_port: 0,
            stopping,
            got_samples,
            thread: Some(thread),
            running,
            detector: false,
            detector_threshold: None,
        })
    }

    /// A channelizer slot no one has used.
    pub const fn alloc_port(&mut self) -> usize {
        let p = self.next_port;
        self.next_port += 1;
        p
    }

    /// Whether the channelizer can carry `profile` channels at this rate.
    pub fn offers(&self, profile: ProfileId) -> bool {
        profile < self.channelizer.config().profiles.len()
    }

    /// Whether a channel at `freq_hz` can be placed on this source at all
    /// (see [`Self::depth`] for how well).
    pub fn covers(&self, freq_hz: f64) -> bool {
        self.depth(freq_hz) <= 1.0
    }

    /// How deep in this source's band `freq_hz` sits
    /// ([`trunk_channelizer::passband::depth`]): past
    /// [`trunk_channelizer::passband::USABLE`] it is in the roll-off.
    pub fn depth(&self, freq_hz: f64) -> f64 {
        trunk_channelizer::passband::depth(freq_hz - self.center_hz, self.rate_hz)
    }

    /// A fixed signal-detector threshold (dB); otherwise it finds its own.
    pub const fn set_detector_threshold(&mut self, threshold_db: Option<f32>) {
        self.detector_threshold = threshold_db;
    }

    /// Run the wideband signal detector (once; later calls are no-ops).
    pub fn attach_detector(&mut self) {
        if self.detector {
            return;
        }
        self.detector = true;
        self.channelizer.set_signal_detector(Some(DetectorConfig {
            threshold_db: self.detector_threshold,
            ..DetectorConfig::default()
        }));
    }

    /// True if samples flowed since the last call.
    pub fn got_samples(&self) -> bool {
        self.got_samples.swap(false, Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.running.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop();
    }
}
