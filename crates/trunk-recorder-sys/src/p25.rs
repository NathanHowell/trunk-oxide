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
use trunk_p25::tdma::{BurstContent, Scrambler, TdmaEvent, TdmaRegenerator};
use trunk_p25::voice::{Encryption, EncryptionTracker, Regenerator, VoiceEvent};
use trunk_p25::{
    ControlChannelDecoder, DecoderConfig, DemodConfig, Message, Modulation, Nac, Received,
};

pub use trunk_p25::DecoderStats;
pub use trunk_p25::tdma::TdmaStats;
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

/// Which implementation produced a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Impl {
    Rust,
    Op25,
}

impl Impl {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Op25 => "op25",
        }
    }
}

/// What a decoder's output was used for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// It drives trunking or the call's audio.
    #[default]
    Primary,
    /// It runs alongside the primary only to be compared.
    Shadow,
}

impl Role {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Shadow => "shadow",
        }
    }
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

// ── Voice: a regenerator per recorder, phase 1 or phase 2 per call ──

/// A voice regenerator's counters at one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceChannelReport {
    pub recorder: i32,
    /// The system of the call in progress (or last call).
    pub system: String,
    /// Phase 1 calls.
    pub stats: VoiceStats,
    /// Phase 2 (TDMA) calls.
    pub tdma: TdmaStats,
    /// The role of the call in progress (or last call).
    pub role: Role,
}

#[derive(Clone, Default)]
struct Snapshot {
    phase1: VoiceStats,
    phase2: TdmaStats,
    system: String,
    role: Role,
}

struct VoiceShared {
    recorder: i32,
    snapshot: Mutex<Snapshot>,
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
            let snap = s
                .snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            VoiceChannelReport {
                recorder: s.recorder,
                system: snap.system,
                stats: snap.phase1,
                tdma: snap.phase2,
                role: snap.role,
            }
        })
        .collect()
}

/// Rust half of the C++ `symbol_regenerator_backend` (see bridge.cpp). The
/// GR block serialises every call.
pub struct P25RegeneratorHandle {
    regen: Regenerator,
    tdma: TdmaRegenerator,
    /// The call in progress is phase 2.
    phase2: bool,
    /// Its TDMA slot (the logical channel this recorder follows).
    slot: u8,
    /// Its encryption, from the ESS on its own slot.
    crypto: EncryptionTracker,
    /// Encryption confirmed but not yet taken by the recorder.
    crypto_pending: Option<Encryption>,
    rate: f64,
    shared: Arc<VoiceShared>,
    events: Vec<Received<trunk_p25::voice::VoiceEvent>>,
    tdma_events: Vec<Received<TdmaEvent>>,
}

pub(crate) fn regenerator_new(rate: f64, recorder: i32) -> Box<P25RegeneratorHandle> {
    let shared = Arc::new(VoiceShared {
        recorder,
        snapshot: Mutex::new(Snapshot::default()),
    });
    VOICE_REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Arc::downgrade(&shared));
    Box::new(P25RegeneratorHandle {
        regen: Regenerator::new(DemodConfig::new(rate, Modulation::Cqpsk)),
        tdma: TdmaRegenerator::new(DemodConfig::phase2(rate), None),
        phase2: false,
        slot: 0,
        crypto: EncryptionTracker::default(),
        crypto_pending: None,
        rate,
        shared,
        events: Vec::new(),
        tdma_events: Vec::new(),
    })
}

impl P25RegeneratorHandle {
    pub(crate) fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        out.clear();
        let mut snap = self
            .shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.phase2 {
            self.tdma_events.clear();
            self.tdma.process(iq, out, &mut self.tdma_events);
            snap.phase2 = self.tdma.stats();
            for e in &self.tdma_events {
                if e.message.channel != self.slot {
                    continue;
                }
                if let BurstContent::Voice2 { encryption, .. } = &e.message.content {
                    Self::track(
                        &mut self.crypto,
                        &mut self.crypto_pending,
                        encryption.as_ref().map(|c| &c.value),
                    );
                }
            }
        } else {
            self.events.clear();
            self.regen.process(iq, out, &mut self.events);
            snap.phase1 = self.regen.stats();
            for e in &self.events {
                if let VoiceEvent::Ldu2 { encryption, .. } = &e.message {
                    Self::track(
                        &mut self.crypto,
                        &mut self.crypto_pending,
                        encryption.as_ref().map(|c| &c.value),
                    );
                }
            }
        }
    }

    /// One superframe's ESS (`None`: it did not decode).
    fn track(
        tracker: &mut EncryptionTracker,
        pending: &mut Option<Encryption>,
        es: Option<&trunk_p25::voice::EncryptionSync>,
    ) {
        match es {
            Some(es) => {
                if let Some(e) = tracker.observe(es) {
                    *pending = Some(e);
                }
            }
            None => tracker.missed(),
        }
    }

    /// Encryption confirmed on this call's slot since the last take.
    pub(crate) const fn take_crypto(&mut self) -> Option<Encryption> {
        self.crypto_pending.take()
    }

    /// A new call. A phase 2 call needs the system's 2160-dibit scrambling
    /// mask; without one, only its unscrambled bursts are regenerated.
    pub(crate) fn start_call(
        &mut self,
        qpsk: bool,
        tdma: bool,
        xor_mask: &[u8],
        system: &str,
        primary: bool,
        slot: u8,
    ) {
        self.phase2 = tdma;
        self.slot = slot;
        self.crypto.reset();
        self.crypto_pending = None;
        if tdma {
            self.tdma.start_call(
                DemodConfig::phase2(self.rate),
                Scrambler::from_dibits(xor_mask),
            );
        } else {
            let modulation = if qpsk {
                Modulation::Cqpsk
            } else {
                Modulation::C4fm
            };
            self.regen
                .start_call(DemodConfig::new(self.rate, modulation));
        }
        let mut snap = self
            .shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        system.clone_into(&mut snap.system);
        snap.role = Role::of(primary);
    }

    pub(crate) const fn take_call_counts(&mut self) -> ffi::TrCallCounts {
        let c = if self.phase2 {
            self.tdma.take_call_counts()
        } else {
            self.regen.take_call_counts()
        };
        ffi::TrCallCounts {
            errors: c.errors,
            spikes: c.spikes,
            algid: match self.crypto.confirmed() {
                Some(e) => e.algid,
                None => trunk_p25::voice::signalling::ALGID_CLEAR,
            },
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
