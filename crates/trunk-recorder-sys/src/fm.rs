//! trunk-fm on a conventional analog channel's channelizer slot.
//!
//! The decoder runs on the DSP thread on the slot's conditioned output
//! (see [`trunk_channelizer::ChannelDecoder`]): demodulates, measures each
//! hop and runs the gate. For now it shadows the C++ analog recorder on the
//! same slot: its decisions are counted and read from any thread for the
//! metrics, and its audio goes nowhere. Cutting the call boundaries and
//! audio over to it is the next step.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use num_complex::Complex32;
use trunk_channelizer::ChannelDecoder;
use trunk_fm::gate::{CloseReason, Config, Event, Gate};
use trunk_fm::{Audio, Decimation, Detector, FmDemod, Measurement, TAU_US, Tone};

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
    /// Hops whose input was all zeros (the slot's carrier squelch muted it).
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

struct Shared {
    channel: AnalogChannel,
    tone: Option<Tone>,
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
            channel: s.channel.clone(),
            tone: s.tone,
            stats: *s.stats.lock().unwrap_or_else(PoisonError::into_inner),
        })
        .collect()
}

/// The DSP-thread half.
struct AnalogDecoder {
    demod: FmDemod,
    detector: Detector,
    gate: Gate,
    audio: Audio,
    measurements: Vec<Measurement>,
    gated: Vec<f32>,
    events: Vec<Event>,
    /// Voice samples not yet handed to the gate (it takes a hop at a time).
    voice_pending: Vec<f32>,
    shared: Arc<Shared>,
}

impl fmt::Debug for AnalogDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnalogDecoder")
            .field("channel", &self.shared.channel)
            .field("tone", &self.shared.tone)
            .finish_non_exhaustive()
    }
}

impl ChannelDecoder for AnalogDecoder {
    fn process(&mut self, iq: &[Complex32]) {
        self.audio.clear();
        self.demod.process(iq, &mut self.audio);
        self.measurements.clear();
        self.detector
            .process(&self.audio.raw, &mut self.measurements);
        self.voice_pending.extend_from_slice(&self.audio.voice);

        if self.measurements.is_empty() {
            self.shared.blocks.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.gated.clear();
        self.events.clear();
        let hop = trunk_fm::detect::HOP;
        let mut consumed = 0;
        for m in &self.measurements {
            // The measurements and the voice stream advance in step; a hop
            // of voice per measurement, or whatever is there if the band-pass
            // delayed it (it never lags by more than its taps).
            let end = (consumed + hop).min(self.voice_pending.len());
            let voice = &self.voice_pending[consumed..end];
            consumed = end;
            self.gate.push(m, voice, &mut self.gated, &mut self.events);
        }
        self.voice_pending.drain(..consumed);

        let mut st = self
            .shared
            .stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for m in &self.measurements {
            if m.silent() {
                st.hops_silent += 1;
            } else if self.gate.is_open() {
                st.hops_open += 1;
            } else {
                st.hops_closed += 1;
            }
        }
        for e in &self.events {
            match e {
                Event::Open { .. } => st.opens += 1,
                Event::Close { reason, .. } => match reason {
                    CloseReason::Quieting => st.closes_quieting += 1,
                    CloseReason::ToneLost => st.closes_tone_lost += 1,
                    CloseReason::Silence => st.closes_silence += 1,
                },
            }
        }
        let last = self.measurements[self.measurements.len() - 1];
        let cfg = self.gate.config();
        st.quieting_db = last.quieting_db;
        st.tone_amplitude = last.tone_amplitude;
        st.tone_prominence = last.tone_prominence;
        st.tone = (last.tone_amplitude >= cfg.tone_min_amplitude
            && last.tone_prominence >= cfg.tone_min_prominence)
            .then_some(last.tone);
        st.open = self.gate.is_open();
        drop(st);
        self.shared.blocks.fetch_add(1, Ordering::Relaxed);
    }

    fn retune(&mut self) {
        self.demod.reset();
        self.detector.reset();
        self.gate.reset();
        self.voice_pending.clear();
    }
}

/// Build a decoder for `channel` at the slot's conditioned `rate`, gating
/// on `tone_hz` (0 = carrier only); returns the DSP half to install.
///
/// # Panics
/// If `rate` is not a multiple of the audio rate (the wide profile's
/// 96 kS/s is).
pub(crate) fn attach(channel: AnalogChannel, rate: f64, tone_hz: f64) -> Box<dyn ChannelDecoder> {
    #[allow(clippy::cast_possible_truncation)]
    let tone = (tone_hz > 0.0)
        .then(|| Tone::from_hz(tone_hz as f32))
        .flatten();
    if tone_hz > 0.0 && tone.is_none() {
        tracing::warn!(
            system = %channel.system, talkgroup = channel.talkgroup, tone_hz,
            "analog channel's tone is not a standard CTCSS tone; gating on carrier alone"
        );
    }
    let shared = Arc::new(Shared {
        channel,
        tone,
        stats: Mutex::new(AnalogStats::default()),
        blocks: AtomicU64::new(0),
    });
    REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Arc::downgrade(&shared));
    Box::new(AnalogDecoder {
        demod: FmDemod::new(rate, TAU_US, Decimation::AntiAliased),
        detector: Detector::new(),
        gate: Gate::new(Config::for_tone(tone)),
        audio: Audio::default(),
        measurements: Vec::new(),
        gated: Vec::new(),
        events: Vec::new(),
        voice_pending: Vec::new(),
        shared,
    })
}

#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn a_pl_transmission_is_counted_as_one_open_and_close() {
        let channel = AnalogChannel {
            system: "conv".into(),
            talkgroup: 2,
            freq_hz: 489_087_500.0,
            source: 0,
            port: 3,
        };
        let mut dec = attach(channel.clone(), 96_000.0, 162.2);
        let rate = 96_000.0f32;
        // 1 s of open-squelch noise (wide, random phase steps), 2 s of a
        // carrier with 162.2 Hz PL at 700 Hz deviation plus speech-band
        // modulation, 1 s of noise.
        let mut s = 99u64;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f32 / (1u64 << 53) as f32
        };
        let mut phase = 0.0f32;
        let mut iq = Vec::new();
        for n in 0..(4.0 * rate) as usize {
            let t = n as f32 / rate;
            let carrier = (1.0..3.0).contains(&t);
            if carrier {
                let m = 700.0 * (TAU * 162.2 * t).sin() + 2_000.0 * (TAU * 800.0 * t).sin();
                phase += TAU * m / rate;
            } else {
                phase += TAU * (rnd() - 0.5);
            }
            iq.push(Complex32::from_polar(0.85, phase));
        }
        for block in iq.chunks(4_096) {
            dec.process(block);
        }
        let r = analog_channels()
            .into_iter()
            .find(|r| r.channel == channel)
            .expect("registered");
        assert_eq!(r.tone, Tone::from_hz(162.2));
        assert_eq!(r.stats.opens, 1, "{:?}", r.stats);
        assert_eq!(r.stats.closes_quieting, 1, "{:?}", r.stats);
        assert!((18..=22).contains(&r.stats.hops_open), "{:?}", r.stats);
        assert!(!r.stats.open);
    }
}
