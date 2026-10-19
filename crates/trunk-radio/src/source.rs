//! A source: a sample stream, the shared channelizer it feeds, and the
//! thread that runs the two. Everything decoded on the source's slots runs
//! on that thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use num_complex::Complex32;
use trunk_channelizer::{
    ConditioningSettings, ConditioningSpec, DetectorConfig, NARROW, SharedChannelizer,
};
use trunk_p25::Modulation;
use trunk_sdr::{Running, Stream};

use crate::ChannelizerStats;
use crate::p25::{self, ControlChannel, ControlPort};
use crate::recorder::{EXCESS_BW_PUB, NARROW_RATE};
use crate::types::{DetectedSignal, EventSender, RadioEvent};

/// How long a block read waits before the thread checks for a stop.
const READ_WAIT: Duration = Duration::from_millis(100);

/// Live channelizers by source index (see [`channelizer_stats`]).
static CHANNELIZERS: Mutex<Vec<(i32, Weak<SharedChannelizer>)>> = Mutex::new(Vec::new());

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
    chz: Arc<SharedChannelizer>,
    stream: Stream,
    ports: Arc<Mutex<Vec<ControlPort>>>,
    events: EventSender,
    stopping: Arc<AtomicBool>,
    got_samples: Arc<AtomicBool>,
    idx: i32,
    center_hz: f64,
    device: String,
}

impl Thread {
    fn run(self) {
        let hop = self.chz.config().hop();
        let mut buf = vec![Complex32::ZERO; hop];
        let mut filled = 0;
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
            self.chz.run(&buf);
            self.got_samples.store(true, Ordering::Relaxed);
            self.drain();
        }
        tracing::info!(source = self.idx, device = %self.device, "source thread finished");
    }

    /// What the block decoded: control-channel messages and detections.
    fn drain(&self) {
        let mut ports = self.ports.lock().unwrap_or_else(PoisonError::into_inner);
        for port in ports.iter_mut() {
            port.drain(&mut |m| self.events.send(RadioEvent::TrunkMessages(m)));
        }
        drop(ports);
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

/// What a source is tuned to.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceSpec {
    pub device: String,
    pub center_hz: f64,
    pub rate_hz: f64,
}

/// One SDR (or file) and its channelizer, running.
pub struct Source {
    pub idx: i32,
    pub center_hz: f64,
    pub rate_hz: f64,
    pub device: String,
    pub channelizer: Arc<SharedChannelizer>,
    ports: Arc<Mutex<Vec<ControlPort>>>,
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
    ) -> Result<Self> {
        let SourceSpec {
            device,
            center_hz,
            rate_hz,
        } = spec;
        let cfg = trunk_channelizer::Config::standard(rate_hz)
            .with_context(|| format!("channelizer for {rate_hz} S/s"))?;
        let channelizer = Arc::new(SharedChannelizer::with_conditioning(cfg, conditioning)?);
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
        let ports = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let got_samples = Arc::new(AtomicBool::new(false));
        let t = Thread {
            chz: Arc::clone(&channelizer),
            stream,
            ports: Arc::clone(&ports),
            events,
            stopping: Arc::clone(&stopping),
            got_samples: Arc::clone(&got_samples),
            idx,
            center_hz,
            device: device.clone(),
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
            ports,
            next_port: 0,
            stopping,
            got_samples,
            thread: Some(thread),
            running,
            detector: false,
            detector_threshold: None,
        })
    }

    /// The next free channelizer slot.
    pub fn alloc_port(&mut self) -> usize {
        let p = self.next_port;
        self.next_port += 1;
        p
    }

    /// Whether the channelizer can carry analog (wide) channels at this rate.
    pub fn has_wide(&self) -> bool {
        self.channelizer.config().profiles.len() > 1
    }

    pub fn covers(&self, freq_hz: f64) -> bool {
        (freq_hz - self.center_hz).abs() <= self.rate_hz / 2.0
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

    /// Decode a P25 control channel at `freq_hz` for system `sys_num` on a
    /// new slot; with a `tap_base`, every message is written to
    /// `<tap_base>.msgs.jsonl`. Returns the slot.
    pub fn add_control_channel(
        &mut self,
        freq_hz: f64,
        modulation: Modulation,
        sys_num: i32,
        system: &str,
        tap_base: &str,
    ) -> usize {
        let port = self.alloc_port();
        let cfg = self.channelizer.config();
        self.channelizer.set_slot_conditioning(
            port,
            ConditioningSpec {
                sps: 5,
                excess_bw: EXCESS_BW_PUB,
                in_rate: cfg.out_rate(NARROW),
                out_rate: NARROW_RATE,
                squelch: None,
            },
        );
        let channel = ControlChannel {
            system: system.to_owned(),
            freq_hz,
            source: self.idx,
            port: u32::try_from(port).unwrap_or(u32::MAX),
        };
        let (dsp, shared) = p25::attach(channel, modulation, NARROW_RATE);
        self.channelizer.set_slot_decoder(port, Some(dsp));
        self.ports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(ControlPort::new(shared, sys_num, tap_base));
        self.channelizer
            .set_slot(port, NARROW, freq_hz - self.center_hz);
        port
    }

    /// Retune a control channel's slot (its decoder resets itself on a
    /// real jump).
    pub fn tune_control_channel(&self, port: usize, freq_hz: f64) {
        self.channelizer
            .set_slot(port, NARROW, freq_hz - self.center_hz);
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
