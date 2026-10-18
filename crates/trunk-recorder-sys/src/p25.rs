//! P25 control-channel decoders hosted on shared-channelizer slots.
//!
//! The decoder runs on the channelizer's DSP thread, on the slot's
//! conditioned output (see [`trunk_channelizer::ChannelDecoder`]); its
//! messages wait in a bounded queue until the GNU Radio block drains them
//! into the control channel's message callback, as op25's frame assembler
//! used to deliver them. Counters are readable from any thread for metrics.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use num_complex::Complex32;
use trunk_channelizer::ChannelDecoder;
use trunk_p25::voice::Regenerator;
use trunk_p25::{
    ControlChannelDecoder, DecoderConfig, DemodConfig, Message, Modulation, Nac, Received,
};

pub use trunk_p25::DecoderStats;
pub use trunk_p25::voice::VoiceStats;

use crate::ffi;

/// Messages held for the GR block; beyond this the oldest are dropped (the
/// block drains every channelizer block, so this only fills if it stalls).
const QUEUE_CAPACITY: usize = 1024;

/// Which control channel a decoder serves.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlChannel {
    pub system: String,
    pub freq_hz: f64,
    pub source: i32,
    pub port: u32,
}

/// A decoder's counters at one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlChannelReport {
    pub channel: ControlChannel,
    pub modulation: Modulation,
    pub nac: Option<Nac>,
    pub stats: DecoderStats,
    /// Messages dropped because the queue was full.
    pub dropped: u64,
}

/// State shared by the DSP-side decoder and the drain side.
pub(crate) struct Shared {
    channel: ControlChannel,
    modulation: Modulation,
    queue: Mutex<VecDeque<Received>>,
    snapshot: Mutex<(DecoderStats, Option<Nac>)>,
    dropped: AtomicU64,
}

impl Shared {
    pub(crate) fn pop(&self) -> Option<Received> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    fn report(&self) -> ControlChannelReport {
        let (stats, nac) = *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        ControlChannelReport {
            channel: self.channel.clone(),
            modulation: self.modulation,
            nac,
            stats,
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }
}

/// Every live decoder, for [`control_channels`].
static REGISTRY: Mutex<Vec<Weak<Shared>>> = Mutex::new(Vec::new());

/// Counters of every running control-channel decoder.
pub fn control_channels() -> Vec<ControlChannelReport> {
    let mut reg = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    reg.retain(|w| w.strong_count() > 0);
    reg.iter()
        .filter_map(Weak::upgrade)
        .map(|s| s.report())
        .collect()
}

/// The DSP-thread half: owns the decoder.
struct SlotDecoder {
    decoder: ControlChannelDecoder,
    shared: Arc<Shared>,
    scratch: Vec<Received>,
}

impl fmt::Debug for SlotDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotDecoder")
            .field("channel", &self.shared.channel)
            .finish_non_exhaustive()
    }
}

impl ChannelDecoder for SlotDecoder {
    fn process(&mut self, iq: &[Complex32]) {
        self.decoder.process(iq, &mut self.scratch);
        if !self.scratch.is_empty() {
            let mut q = self
                .shared
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for m in self.scratch.drain(..) {
                if q.len() == QUEUE_CAPACITY {
                    q.pop_front();
                    self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                }
                q.push_back(m);
            }
        }
        *self
            .shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = (self.decoder.stats(), self.decoder.nac());
    }

    fn retune(&mut self) {
        self.decoder.reset();
        self.shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// Build a decoder for `channel` at the slot's conditioned `rate`; returns
/// the DSP half to install on the slot and the drain half to keep.
pub(crate) fn attach(
    channel: ControlChannel,
    modulation: Modulation,
    rate: f64,
) -> (Box<dyn ChannelDecoder>, Arc<Shared>) {
    let decoder =
        ControlChannelDecoder::new(DecoderConfig::new(DemodConfig::new(rate, modulation)));
    let shared = Arc::new(Shared {
        channel,
        modulation,
        queue: Mutex::new(VecDeque::new()),
        snapshot: Mutex::new((decoder.stats(), None)),
        dropped: AtomicU64::new(0),
    });
    REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Arc::downgrade(&shared));
    (
        Box::new(SlotDecoder {
            decoder,
            shared: Arc::clone(&shared),
            scratch: Vec::new(),
        }),
        shared,
    )
}

// ── Phase 1 voice: a regenerator per recorder ──

/// A voice regenerator's counters at one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceChannelReport {
    pub recorder: i32,
    /// The system of the call in progress (or last call).
    pub system: String,
    pub stats: VoiceStats,
}

struct VoiceShared {
    recorder: i32,
    snapshot: Mutex<(VoiceStats, String)>,
}

static VOICE_REGISTRY: Mutex<Vec<Weak<VoiceShared>>> = Mutex::new(Vec::new());

/// Counters of every recorder's voice regenerator.
pub fn voice_channels() -> Vec<VoiceChannelReport> {
    let mut reg = VOICE_REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    reg.retain(|w| w.strong_count() > 0);
    reg.iter()
        .filter_map(Weak::upgrade)
        .map(|s| {
            let (stats, system) = s
                .snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            VoiceChannelReport {
                recorder: s.recorder,
                system,
                stats,
            }
        })
        .collect()
}

/// Rust half of the C++ `symbol_regenerator_backend` (see bridge.cpp). The
/// GR block serialises every call.
pub struct P25RegeneratorHandle {
    regen: Regenerator,
    rate: f64,
    shared: Arc<VoiceShared>,
    events: Vec<Received<trunk_p25::voice::VoiceEvent>>,
}

pub(crate) fn regenerator_new(rate: f64, recorder: i32) -> Box<P25RegeneratorHandle> {
    let shared = Arc::new(VoiceShared {
        recorder,
        snapshot: Mutex::new((VoiceStats::default(), String::new())),
    });
    VOICE_REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Arc::downgrade(&shared));
    Box::new(P25RegeneratorHandle {
        regen: Regenerator::new(DemodConfig::new(rate, Modulation::Cqpsk)),
        rate,
        shared,
        events: Vec::new(),
    })
}

impl P25RegeneratorHandle {
    pub(crate) fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        out.clear();
        self.events.clear();
        self.regen.process(iq, out, &mut self.events);
        self.shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0 = self.regen.stats();
    }

    pub(crate) fn start_call(&mut self, qpsk: bool, system: &str) {
        let modulation = if qpsk {
            Modulation::Cqpsk
        } else {
            Modulation::C4fm
        };
        self.regen
            .start_call(DemodConfig::new(self.rate, modulation));
        system.clone_into(
            &mut self
                .shared
                .snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .1,
        );
    }

    pub(crate) const fn take_call_counts(&mut self) -> ffi::TrCallCounts {
        let c = self.regen.take_call_counts();
        ffi::TrCallCounts {
            errors: c.errors,
            spikes: c.spikes,
        }
    }
}

impl TryFrom<ffi::TrControlDecoder> for Modulation {
    type Error = ();

    fn try_from(d: ffi::TrControlDecoder) -> Result<Self, ()> {
        match d {
            ffi::TrControlDecoder::P25Cqpsk => Ok(Self::Cqpsk),
            ffi::TrControlDecoder::P25C4fm => Ok(Self::C4fm),
            _ => Err(()),
        }
    }
}

impl From<&Message> for ffi::TrDecodedMessage {
    fn from(m: &Message) -> Self {
        Self {
            msg_type: m.op25_type(),
            payload: m.op25_payload(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunk_p25::testgen;

    fn channel() -> ControlChannel {
        ControlChannel {
            system: "test".into(),
            freq_hz: 851e6,
            source: 0,
            port: 3,
        }
    }

    #[test]
    fn decodes_into_the_queue_and_reports() {
        let nac = Nac::new(0x38d).unwrap();
        let mut rng = testgen::Rng::new(1);
        let mut dibits = Vec::new();
        for _ in 0..20 {
            dibits.extend(testgen::tsdu(nac, &testgen::random_tsbks(&mut rng, 3)));
        }
        let iq = testgen::modulate_cqpsk(&dibits, 5);
        let (mut dsp, shared) = attach(channel(), Modulation::Cqpsk, 24_000.0);
        for chunk in iq.chunks(600) {
            dsp.process(chunk);
        }
        let mut tsbks = 0;
        while let Some(r) = shared.pop() {
            let wire = ffi::TrDecodedMessage::from(&r.message);
            if wire.msg_type == 7 {
                assert_eq!(wire.payload.len(), 12);
                assert_eq!(wire.payload[..2], [0x03, 0x8d]);
                tsbks += 1;
            }
        }
        assert!(tsbks >= 57, "{tsbks}");
        let report = control_channels()
            .into_iter()
            .find(|r| r.channel == channel())
            .unwrap();
        assert_eq!(report.nac, Some(nac));
        assert_eq!(report.stats.tsbk_ok(), tsbks);
        assert_eq!(report.dropped, 0);

        dsp.retune();
        assert!(shared.pop().is_none());
        drop((dsp, shared));
        assert!(control_channels().iter().all(|r| r.channel != channel()));
    }

    #[test]
    fn only_p25_kinds_map_to_a_modulation() {
        assert_eq!(
            Modulation::try_from(ffi::TrControlDecoder::P25C4fm),
            Ok(Modulation::C4fm)
        );
        assert!(Modulation::try_from(ffi::TrControlDecoder::None).is_err());
    }
}
