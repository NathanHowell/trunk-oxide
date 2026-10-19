//! What trunk-fm's analog recorders report, readable from any thread for
//! the `trunk.oxide.fm.*` metrics. The decoding itself is in
//! [`crate::recorder`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use trunk_fm::Tone;
use trunk_fm::gate::{CloseReason, Event};

/// Which channel a decoder serves.
#[derive(Clone, Debug, PartialEq)]
pub struct AnalogChannel {
    pub system: String,
    pub talkgroup: i64,
    pub freq_hz: f64,
    pub source: i32,
    pub port: u32,
}

/// A decoder's counters at one instant. Hops are 100 ms.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AnalogStats {
    pub hops_open: u64,
    pub hops_closed: u64,
    /// Hops whose input was all zeros.
    pub hops_silent: u64,
    pub opens: u64,
    pub closes_quieting: u64,
    pub closes_tone_lost: u64,
    pub closes_silence: u64,
    /// The last hop's measurements.
    pub quieting_db: f64,
    pub tone_amplitude: f32,
    pub tone_prominence: f32,
    /// The last hop's dominant tone, when it stood out.
    pub tone: Option<Tone>,
    pub open: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnalogReport {
    pub channel: AnalogChannel,
    /// The tone the gate requires, if any.
    pub tone: Option<Tone>,
    pub stats: AnalogStats,
}

pub(crate) struct Shared {
    channel: Mutex<AnalogChannel>,
    tone: Mutex<Option<Tone>>,
    stats: Mutex<AnalogStats>,
    /// Blocks processed, for liveness.
    blocks: AtomicU64,
}

static REGISTRY: Mutex<Vec<Weak<Shared>>> = Mutex::new(Vec::new());

/// Counters of every running analog decoder.
pub fn analog_channels() -> Vec<AnalogReport> {
    let mut reg = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    reg.retain(|w| w.strong_count() > 0);
    reg.iter()
        .filter_map(Weak::upgrade)
        .map(|s| AnalogReport {
            channel: s
                .channel
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
            tone: *s.tone.lock().unwrap_or_else(PoisonError::into_inner),
            stats: *s.stats.lock().unwrap_or_else(PoisonError::into_inner),
        })
        .collect()
}

impl Shared {
    pub(crate) fn register(channel: AnalogChannel) -> Arc<Self> {
        let shared = Arc::new(Self {
            channel: Mutex::new(channel),
            tone: Mutex::new(None),
            stats: Mutex::new(AnalogStats::default()),
            blocks: AtomicU64::new(0),
        });
        REGISTRY
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&shared));
        shared
    }

    /// A new call on the channel: what it is and the tone it gates on.
    pub(crate) fn call(&self, system: &str, talkgroup: i64, freq_hz: f64, tone: Option<Tone>) {
        let mut c = self.channel.lock().unwrap_or_else(PoisonError::into_inner);
        system.clone_into(&mut c.system);
        c.talkgroup = talkgroup;
        c.freq_hz = freq_hz;
        drop(c);
        *self.tone.lock().unwrap_or_else(PoisonError::into_inner) = tone;
    }

    /// Count a block's hops and events, and keep its last measurement.
    pub(crate) fn block(
        &self,
        measurements: &[trunk_fm::Measurement],
        open: bool,
        events: &[Event],
        cfg: &trunk_fm::gate::Config,
    ) {
        self.blocks.fetch_add(1, Ordering::Relaxed);
        let Some(last) = measurements.last() else {
            return;
        };
        let mut st = self.stats.lock().unwrap_or_else(PoisonError::into_inner);
        for m in measurements {
            if m.silent() {
                st.hops_silent += 1;
            } else if open {
                st.hops_open += 1;
            } else {
                st.hops_closed += 1;
            }
        }
        for e in events {
            match e {
                Event::Open { .. } => st.opens += 1,
                Event::Close { reason, .. } => match reason {
                    CloseReason::Quieting => st.closes_quieting += 1,
                    CloseReason::ToneLost => st.closes_tone_lost += 1,
                    CloseReason::Silence => st.closes_silence += 1,
                },
            }
        }
        st.quieting_db = last.quieting_db;
        st.tone_amplitude = last.tone_amplitude;
        st.tone_prominence = last.tone_prominence;
        st.tone = (last.tone_amplitude >= cfg.tone_min_amplitude
            && last.tone_prominence >= cfg.tone_min_prominence)
            .then_some(last.tone);
        st.open = open;
    }
}
