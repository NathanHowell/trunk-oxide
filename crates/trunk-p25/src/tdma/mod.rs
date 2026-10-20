//! P25 Phase 2 (TDMA) voice channels: H-DQPSK at 6000 baud, two logical
//! channels in alternating 30 ms bursts.
//!
//! - [`framer`]: bursts and their superframe slots (ISCH, flywheel sync).
//! - [`duid`]: the burst type.
//! - [`scramble`]: the per-system scrambling of burst payloads.
//! - [`ambe`]: AMBE+2 voice codewords (4V and 2V bursts).
//! - [`mac`]: FACCH/SACCH MAC PDUs and the encryption sync.
//!
//! Every decision is soft: bit LLRs from the demodulator reach the ISCH,
//! DUID, Golay and Reed–Solomon decoders unrounded.

pub mod ambe;
pub mod duid;
pub mod framer;
pub mod isch;
pub mod layout;
pub mod mac;
pub mod regenerate;
pub mod scramble;

use num_complex::Complex32;

pub use ambe::{AmbeFrame, AmbeParams};
pub use duid::BurstType;
pub use framer::{Burst, BurstFramer, BurstFramerStats};
pub use isch::{Isch, IschDecision, IschInfo};
pub use mac::{MacChannel, MacOpcode, MacPdu, MacPtt};
pub use regenerate::TdmaRegenerator;
pub use scramble::Scrambler;

use crate::control::Received;
use crate::demod::{DemodConfig, DemodStats, Demodulator};
use crate::fec::Decision;
use crate::gf64::Gf64;
use crate::symbols::bit_llrs;
use crate::types::Nac;
use crate::voice::{Checked, EncryptionSync};
use layout::{
    CHANNEL_OF_SLOT, DUID_AT, PAYLOAD_BITS, VOICE_2V, VOICE_4V, VOICE_DIBITS, payload_dibit,
};

/// A burst's bit LLRs: the ISCH and DUID as sent, the payload descrambled.
pub struct BurstBits {
    raw: Vec<f32>,
    payload: [f32; PAYLOAD_BITS],
}

impl BurstBits {
    /// `scrambler` is applied when `descramble`; without one, scrambled
    /// payloads are left as received.
    pub fn new(burst: &Burst, scrambler: Option<&Scrambler>, descramble: bool) -> Self {
        let raw: Vec<f32> = burst.soft.iter().flat_map(|&s| bit_llrs(s)).collect();
        let payload = std::array::from_fn(|q| {
            let p = q / 2;
            let llr = raw[2 * payload_dibit(p) + q % 2];
            let m = match scrambler {
                Some(s) if descramble => s.payload(burst.slot, p),
                _ => 0,
            };
            if (m >> (1 - q % 2)) & 1 == 1 {
                -llr
            } else {
                llr
            }
        });
        Self { raw, payload }
    }

    pub fn duid(&self) -> [f32; 8] {
        std::array::from_fn(|i| self.raw[2 * DUID_AT[i / 2] + i % 2])
    }

    /// The 312 payload bit LLRs.
    pub const fn payload(&self) -> &[f32; PAYLOAD_BITS] {
        &self.payload
    }

    /// The voice codeword starting at payload dibit `p`.
    pub fn voice(&self, p: usize) -> [f32; ambe::CODEWORD_BITS] {
        std::array::from_fn(|j| self.payload[2 * p + j])
    }
}

/// What a burst carried.
#[derive(Clone, Debug, PartialEq)]
pub enum BurstContent {
    Voice4 {
        frames: [AmbeFrame; 4],
    },
    Voice2 {
        frames: [AmbeFrame; 2],
        /// Assembled with the four 4V bursts before it.
        encryption: Option<Checked<EncryptionSync>>,
    },
    Mac(Option<Checked<MacPdu>>),
    /// Not decoded: control-channel LCCH, a reserved type, or scrambled with
    /// no scrambler known.
    Unread,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TdmaEvent {
    /// Symbol index of the burst's first ISCH dibit.
    pub start: u64,
    pub slot: usize,
    /// Logical channel, 0 or 1.
    pub channel: u8,
    pub isch: IschDecision,
    pub burst_type: BurstType,
    pub duid: Decision,
    pub content: BurstContent,
}

impl TdmaEvent {
    pub const fn frames(&self) -> &[AmbeFrame] {
        match &self.content {
            BurstContent::Voice4 { frames } => frames,
            BurstContent::Voice2 { frames, .. } => frames,
            _ => &[],
        }
    }
}

/// Cumulative counters (never reset).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TdmaStats {
    pub demod: DemodStats,
    pub framer: BurstFramerStats,
    pub voice4: u64,
    pub voice2: u64,
    pub ambe_frames: u64,
    /// Bits corrected over all voice codewords (op25's call error count).
    pub ambe_errors: u64,
    /// Voice frames sure enough to regenerate ([`AmbeFrame::trusted`]).
    pub ambe_trusted: u64,
    pub mac: u64,
    pub mac_failed: u64,
    /// MAC PDUs decoded, by opcode ([`MacOpcode::bits`]).
    pub mac_by_opcode: [u64; 8],
    /// `MAC_END_PTT`s naming another system's color code.
    pub end_ptt_foreign: u64,
    pub encryption_sync: u64,
    pub encryption_sync_failed: u64,
    /// Bursts not read (LCCH, reserved, or scrambled with no scrambler).
    pub unread: u64,
}

impl TdmaStats {
    fn count(&mut self, e: &TdmaEvent, color_code: Option<Nac>) {
        match &e.content {
            BurstContent::Voice4 { .. } => self.voice4 += 1,
            BurstContent::Voice2 { encryption, .. } => {
                self.voice2 += 1;
                self.encryption_sync += u64::from(encryption.is_some());
                self.encryption_sync_failed += u64::from(encryption.is_none());
            }
            BurstContent::Mac(pdu) => {
                self.mac += u64::from(pdu.is_some());
                self.mac_failed += u64::from(pdu.is_none());
                if let Some(p) = pdu {
                    self.mac_by_opcode[usize::from(p.value.opcode().bits())] += 1;
                    self.end_ptt_foreign += u64::from(p.value.is_foreign_end_ptt(color_code));
                }
            }
            BurstContent::Unread => self.unread += 1,
        }
        self.ambe_frames += e.frames().len() as u64;
        self.ambe_errors += e.frames().iter().map(|f| u64::from(f.errors)).sum::<u64>();
        self.ambe_trusted += e.frames().iter().filter(|f| f.trusted()).count() as u64;
    }
}

/// A 4V burst's ESS-B share: four hexbits and their reliabilities.
type EssPart = [(Gf64, f32); 4];

/// The ESS-B shares of a logical channel's recent 4V bursts.
#[derive(Clone, Debug, Default)]
struct EssAssembly {
    /// By voice position (superframe slot / 2): the burst's sequence and its
    /// four hexbits.
    parts: [Option<(u64, EssPart)>; 5],
}

impl EssAssembly {
    fn add(&mut self, burst: &Burst, part: [(Gf64, f32); 4]) {
        if let Some(p) = self.parts.get_mut(burst.slot / 2) {
            *p = Some((burst.sequence, part));
        }
    }

    /// The four 4V shares before a 2V burst, in order (missing as `None`).
    fn before(&self, burst: &Burst) -> [Option<[(Gf64, f32); 4]>; 4] {
        let at = burst.slot / 2;
        std::array::from_fn(|i| {
            if at >= 5 {
                return None;
            }
            let (seq, part) = self.parts[(at + 1 + i) % 5]?;
            (burst.sequence.checked_sub(seq)? < 12).then_some(part)
        })
    }
}

/// Decode one burst. The ESS needs the bursts before it, so a 2V burst's
/// encryption sync is filled in by [`TdmaDecoder`].
pub fn decode_burst(burst: &Burst, scrambler: Option<&Scrambler>) -> TdmaEvent {
    let raw = BurstBits::new(burst, None, false);
    let (burst_type, duid) = duid::decode(&raw.duid());
    let readable = !burst_type.scrambled() || scrambler.is_some();
    let bits = BurstBits::new(burst, scrambler, burst_type.scrambled());
    let content = if readable {
        match burst_type {
            BurstType::Voice4 => BurstContent::Voice4 {
                frames: VOICE_4V.map(|p| ambe::decode(&bits.voice(p))),
            },
            BurstType::Voice2 => BurstContent::Voice2 {
                frames: VOICE_2V.map(|p| ambe::decode(&bits.voice(p))),
                encryption: None,
            },
            BurstType::Facch { .. } => {
                BurstContent::Mac(mac::decode_acch(MacChannel::Facch, bits.payload()))
            }
            BurstType::Sacch { .. } => {
                BurstContent::Mac(mac::decode_acch(MacChannel::Sacch, bits.payload()))
            }
            BurstType::Lcch { .. } | BurstType::Reserved(_) => BurstContent::Unread,
        }
    } else {
        BurstContent::Unread
    };
    debug_assert_eq!(VOICE_DIBITS * 2, ambe::CODEWORD_BITS);
    TdmaEvent {
        start: burst.start,
        slot: burst.slot,
        channel: CHANNEL_OF_SLOT[burst.slot],
        isch: burst.isch,
        burst_type,
        duid,
        content,
    }
}

/// A voice frame's 36 dibits at payload dibit `p`, descrambled, as
/// [`TdmaRegenerator`] sends them: re-encoded when the burst is rewritten
/// ([`regenerate::rewritten`]) and the frame trusted, else as received.
pub fn regenerated_voice(
    burst: &Burst,
    scrambler: Option<&Scrambler>,
    p: usize,
    reencode: Option<&AmbeFrame>,
) -> [u8; VOICE_DIBITS] {
    reencode.map_or_else(
        || {
            std::array::from_fn(|k| {
                let raw = burst
                    .soft
                    .get(payload_dibit(p + k))
                    .map_or(0, |&s| crate::symbols::slice(s));
                raw ^ scrambler.map_or(0, |s| s.payload(burst.slot, p + k))
            })
        },
        |frame| {
            let cw = frame.params.encode();
            std::array::from_fn(|k| u8::from(cw.bit(2 * k)) << 1 | u8::from(cw.bit(2 * k + 1)))
        },
    )
}

/// What op25 makes of a burst after [`TdmaRegenerator`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op25View {
    /// The burst type op25 reads: the one decided here if the burst is
    /// rewritten, else op25's hard decoding of the DUID as received (`None`
    /// when that fails).
    pub burst_type: Option<BurstType>,
    /// For a voice type, its frames' dibits as op25 receives them.
    pub voice: Vec<[u8; VOICE_DIBITS]>,
}

pub fn op25_view(burst: &Burst, scrambler: Option<&Scrambler>, event: &TdmaEvent) -> Op25View {
    let rewritten = regenerate::rewritten(event);
    let burst_type = if rewritten {
        Some(event.burst_type)
    } else {
        let word = DUID_AT.iter().fold(0u8, |w, &at| {
            (w << 2) | burst.soft.get(at).map_or(0, |&s| crate::symbols::slice(s))
        });
        duid::decode_hard(word)
    };
    let starts: &[usize] = match burst_type {
        Some(BurstType::Voice4) => &VOICE_4V,
        Some(BurstType::Voice2) => &VOICE_2V,
        _ => &[],
    };
    let frames = event.frames();
    let voice = starts
        .iter()
        .enumerate()
        .map(|(i, &p)| {
            let reencode = frames.get(i).filter(|f| rewritten && f.trusted());
            regenerated_voice(burst, scrambler, p, reencode)
        })
        .collect();
    Op25View { burst_type, voice }
}

/// A Phase 2 voice channel: conditioned IQ in, bursts out (both logical
/// channels; callers pick theirs).
pub struct TdmaDecoder {
    demod: Demodulator,
    framer: BurstFramer,
    scrambler: Option<Scrambler>,
    ess: [EssAssembly; 2],
    soft: Vec<f32>,
    bursts: Vec<Burst>,
    stats: TdmaStats,
}

impl TdmaDecoder {
    pub fn new(demod: DemodConfig, scrambler: Option<Scrambler>) -> Self {
        Self {
            demod: Demodulator::new(demod),
            framer: BurstFramer::new(),
            scrambler,
            ess: Default::default(),
            soft: Vec::new(),
            bursts: Vec::new(),
            stats: TdmaStats::default(),
        }
    }

    pub const fn set_scrambler(&mut self, scrambler: Option<Scrambler>) {
        self.scrambler = scrambler;
    }

    /// Start over for a new call. Counters persist.
    pub fn reset(&mut self) {
        self.demod.reset();
        self.framer.reset();
        self.ess = Default::default();
    }

    pub fn stats(&self) -> TdmaStats {
        TdmaStats {
            demod: self.demod.stats(),
            framer: self.framer.stats,
            ..self.stats
        }
    }

    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<Received<TdmaEvent>>) {
        let mut soft = std::mem::take(&mut self.soft);
        soft.clear();
        self.demod.process(iq, &mut soft);
        self.process_symbols(&soft, out);
        self.soft = soft;
    }

    pub fn process_symbols(&mut self, soft: &[f32], out: &mut Vec<Received<TdmaEvent>>) {
        self.process_symbols_with(soft, &mut |_, event, symbol| {
            out.push(Received {
                symbol,
                message: event,
            });
        });
    }

    /// [`Self::process`], handing each burst to `f` with its event and the
    /// symbol index it completed at.
    /// The soft symbols the last [`Self::process`] or [`Self::process_with`]
    /// demodulated.
    pub fn symbols(&self) -> &[f32] {
        &self.soft
    }

    pub fn process_with(&mut self, iq: &[Complex32], f: &mut dyn FnMut(&Burst, TdmaEvent, u64)) {
        let mut soft = std::mem::take(&mut self.soft);
        soft.clear();
        self.demod.process(iq, &mut soft);
        self.process_symbols_with(&soft, f);
        self.soft = soft;
    }

    pub fn process_symbols_with(
        &mut self,
        soft: &[f32],
        f: &mut dyn FnMut(&Burst, TdmaEvent, u64),
    ) {
        for &s in soft {
            self.framer.push(s, &mut self.bursts);
            for burst in std::mem::take(&mut self.bursts) {
                let event = self.decode(&burst);
                f(&burst, event, self.framer.position());
            }
        }
    }

    pub const fn scrambler(&self) -> Option<&Scrambler> {
        self.scrambler.as_ref()
    }

    /// Decode `burst`, assembling encryption sync across bursts.
    pub fn decode(&mut self, burst: &Burst) -> TdmaEvent {
        let mut event = decode_burst(burst, self.scrambler.as_ref());
        let ess = &mut self.ess[usize::from(event.channel)];
        if !matches!(event.content, BurstContent::Unread) {
            let bits = BurstBits::new(burst, self.scrambler.as_ref(), true);
            match &mut event.content {
                BurstContent::Voice4 { .. } => ess.add(burst, mac::ess_b(bits.payload())),
                BurstContent::Voice2 { encryption, .. } => {
                    *encryption = mac::decode_ess(&ess.before(burst), &mac::ess_a(bits.payload()));
                }
                _ => {}
            }
        }
        self.stats
            .count(&event, self.scrambler.as_ref().map(Scrambler::color_code));
        event
    }

    /// Counters of [`TdmaDecoder::decode`] alone (no demodulator or framer).
    pub const fn decode_stats(&self) -> TdmaStats {
        self.stats
    }
}

#[cfg(test)]
mod tests;
