//! P25 Phase 1 voice channels: header, voice frames (LDU1/LDU2) and
//! terminators, decoded down to IMBE parameters and signalling words.

pub mod crypto;
pub mod imbe;
pub mod layout;
pub mod regenerate;
pub mod signalling;

use num_complex::Complex32;

pub use crypto::{Encryption, EncryptionTracker};
pub use imbe::{ImbeCodeword, ImbeFrame, ImbeParams};
pub use regenerate::{CallCounts, Regenerator};
pub use signalling::{Checked, EncryptionSync, GroupVoice, Header, LcMessage, LinkControl};

use crate::control::Received;
use crate::demod::{DemodConfig, DemodStats, Demodulator};
use crate::framer::{Duid, Frame, Framer, FramerStats};
use crate::symbols::bit_llrs;
use crate::types::Nac;
use layout::{
    IMBE_BASE, IMBE_BITS, IMBE_PER_LDU, PAYLOAD, SIGNALLING_BITS, imbe_bit, raw_bit, signalling_bit,
};

/// How a voice transmission ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminator {
    /// TDU: no payload.
    Simple,
    /// TDULC: carries link control (if it decoded).
    LinkControl(Option<Checked<LinkControl>>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum VoiceEvent {
    Header {
        nac: Nac,
        header: Option<Checked<Header>>,
    },
    Ldu1 {
        nac: Nac,
        frames: [ImbeFrame; IMBE_PER_LDU],
        link_control: Option<Checked<LinkControl>>,
    },
    Ldu2 {
        nac: Nac,
        frames: [ImbeFrame; IMBE_PER_LDU],
        encryption: Option<Checked<EncryptionSync>>,
    },
    Terminator {
        nac: Nac,
        terminator: Terminator,
    },
}

impl VoiceEvent {
    pub const fn nac(&self) -> Nac {
        match self {
            Self::Header { nac, .. }
            | Self::Ldu1 { nac, .. }
            | Self::Ldu2 { nac, .. }
            | Self::Terminator { nac, .. } => *nac,
        }
    }

    /// The voice frames, for LDUs.
    pub const fn frames(&self) -> Option<&[ImbeFrame; IMBE_PER_LDU]> {
        match self {
            Self::Ldu1 { frames, .. } | Self::Ldu2 { frames, .. } => Some(frames),
            _ => None,
        }
    }
}

/// Bit LLRs of a frame, addressed in data-bit coordinates. Bits past the end
/// of a truncated frame read as 0 (no information).
pub struct FrameBits(Vec<f32>);

impl FrameBits {
    pub fn new(frame: &Frame) -> Self {
        Self(frame.raw.iter().flat_map(|&s| bit_llrs(s)).collect())
    }

    pub fn data(&self, p: usize) -> f32 {
        self.0.get(raw_bit(p)).copied().unwrap_or(0.0)
    }

    pub fn imbe(&self, i: usize) -> [f32; IMBE_BITS] {
        std::array::from_fn(|j| self.data(imbe_bit(IMBE_BASE[i], j)))
    }

    pub fn signalling(&self) -> [f32; SIGNALLING_BITS] {
        std::array::from_fn(|j| self.data(signalling_bit(j)))
    }

    /// `len` consecutive payload bits (HDU, TDULC).
    pub fn payload(&self, len: usize) -> Vec<f32> {
        (PAYLOAD..PAYLOAD + len).map(|p| self.data(p)).collect()
    }
}

/// The header code word: 36 Golay(18,6) words, 648 bits (BAAA-A §5.2).
const HDU_PAYLOAD_BITS: usize = 36 * 18;
/// The terminator's link control: 12 Golay(24,12) words, 288 bits (BAAA-A
/// §5.5.1).
const TDULC_PAYLOAD_BITS: usize = 12 * 24;

/// Decode one framed unit; `None` for frames that are not voice-channel
/// traffic (TSDU, PDU).
pub fn decode_frame(frame: &Frame) -> Option<VoiceEvent> {
    let bits = FrameBits::new(frame);
    let nac = frame.nac;
    let frames = || std::array::from_fn(|i| imbe::decode(&bits.imbe(i)));
    Some(match frame.duid {
        Duid::Hdu => VoiceEvent::Header {
            nac,
            header: signalling::header(&bits.payload(HDU_PAYLOAD_BITS)),
        },
        Duid::Ldu1 => VoiceEvent::Ldu1 {
            nac,
            frames: frames(),
            link_control: signalling::ldu1_link_control(&bits.signalling()),
        },
        Duid::Ldu2 => VoiceEvent::Ldu2 {
            nac,
            frames: frames(),
            encryption: signalling::ldu2_encryption_sync(&bits.signalling()),
        },
        Duid::Tdu => VoiceEvent::Terminator {
            nac,
            terminator: Terminator::Simple,
        },
        Duid::TduLc => VoiceEvent::Terminator {
            nac,
            terminator: Terminator::LinkControl(signalling::tdulc_link_control(
                &bits.payload(TDULC_PAYLOAD_BITS),
            )),
        },
        Duid::Tsdu | Duid::Pdu => return None,
    })
}

/// Cumulative counters (never reset).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VoiceStats {
    pub demod: DemodStats,
    pub framer: FramerStats,
    pub headers: u64,
    pub headers_failed: u64,
    pub ldu1: u64,
    pub ldu2: u64,
    pub imbe_frames: u64,
    /// Bits corrected over all IMBE frames (op25's call error count).
    pub imbe_errors: u64,
    pub link_control: u64,
    pub link_control_failed: u64,
    pub encryption_sync: u64,
    pub encryption_sync_failed: u64,
    pub terminators: u64,
}

impl VoiceStats {
    fn count(&mut self, e: &VoiceEvent) {
        match e {
            VoiceEvent::Header { header, .. } => {
                self.headers += 1;
                self.headers_failed += u64::from(header.is_none());
            }
            VoiceEvent::Ldu1 { link_control, .. } => {
                self.ldu1 += 1;
                self.link_control += u64::from(link_control.is_some());
                self.link_control_failed += u64::from(link_control.is_none());
            }
            VoiceEvent::Ldu2 { encryption, .. } => {
                self.ldu2 += 1;
                self.encryption_sync += u64::from(encryption.is_some());
                self.encryption_sync_failed += u64::from(encryption.is_none());
            }
            VoiceEvent::Terminator { terminator, .. } => {
                self.terminators += 1;
                if let Terminator::LinkControl(lc) = terminator {
                    self.link_control += u64::from(lc.is_some());
                    self.link_control_failed += u64::from(lc.is_none());
                }
            }
        }
        if let Some(frames) = e.frames() {
            self.imbe_frames += frames.len() as u64;
            self.imbe_errors += frames.iter().map(|f| u64::from(f.errors)).sum::<u64>();
        }
    }
}

/// A Phase 1 voice channel: conditioned IQ in, voice events out.
pub struct VoiceDecoder {
    demod: Demodulator,
    framer: Framer,
    soft: Vec<f32>,
    frames: Vec<Frame>,
    symbol: u64,
    stats: VoiceStats,
}

impl VoiceDecoder {
    pub fn new(demod: DemodConfig) -> Self {
        Self {
            demod: Demodulator::new(demod),
            framer: Framer::new(),
            soft: Vec::new(),
            frames: Vec::new(),
            symbol: 0,
            stats: VoiceStats::default(),
        }
    }

    /// Fix the NAC instead of learning it (the system's, when known).
    pub const fn set_nac(&mut self, nac: Option<Nac>) {
        self.framer.set_nac(nac);
    }

    /// Start over for a new call: timing, phase and NAC. Counters persist.
    pub fn reset(&mut self) {
        self.demod.reset();
        self.framer.reset();
    }

    pub fn stats(&self) -> VoiceStats {
        VoiceStats {
            demod: self.demod.stats(),
            framer: self.framer.stats,
            ..self.stats
        }
    }

    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<Received<VoiceEvent>>) {
        let mut soft = std::mem::take(&mut self.soft);
        soft.clear();
        self.demod.process(iq, &mut soft);
        self.process_symbols(&soft, out);
        self.soft = soft;
    }

    pub fn process_symbols(&mut self, soft: &[f32], out: &mut Vec<Received<VoiceEvent>>) {
        for &s in soft {
            self.symbol += 1;
            self.framer.push(s, &mut self.frames);
            for frame in std::mem::take(&mut self.frames) {
                if let Some(event) = decode_frame(&frame) {
                    self.stats.count(&event);
                    out.push(Received {
                        symbol: self.symbol,
                        message: event,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demod::Modulation;
    use crate::testgen::{self, Channel, Rng};

    const NAC: Nac = Nac::new(0x38d).unwrap();
    const LC: [u8; 9] = [0x00, 0x00, 0x00, 0x00, 0x03, 0x61, 0x12, 0x34, 0x56];

    fn superframe(rng: &mut Rng, sent: &mut Vec<ImbeParams>) -> Vec<u8> {
        let mut d = Vec::new();
        for duid in [Duid::Ldu1, Duid::Ldu2] {
            let params: [ImbeParams; 9] = std::array::from_fn(|_| testgen::random_imbe(rng));
            sent.extend(params);
            let signalling = if duid == Duid::Ldu1 {
                signalling::encode::link_control_ldu1(&LC)
            } else {
                signalling::encode::encryption_sync(&[0; 9], 0x80, 0)
            };
            d.extend(testgen::ldu(NAC, duid, &params, &signalling));
        }
        d
    }

    fn call(rng: &mut Rng, superframes: usize) -> (Vec<u8>, Vec<ImbeParams>) {
        let mut sent = Vec::new();
        #[allow(clippy::cast_possible_truncation)]
        let mut d: Vec<u8> = (0..300).map(|_| (rng.next_u64() & 3) as u8).collect();
        d.extend(testgen::hdu(NAC, &[0; 9], 0, 0x80, 0, 865));
        for _ in 0..superframes {
            d.extend(superframe(rng, &mut sent));
        }
        d.extend(testgen::tdu(NAC));
        d.extend(vec![0u8; 200]);
        (d, sent)
    }

    #[test]
    fn a_clean_call_decodes_completely() {
        let mut rng = Rng::new(81);
        let (dibits, sent) = call(&mut rng, 3);
        let mut dec = VoiceDecoder::new(DemodConfig::new(24000.0, Modulation::C4fm));
        let mut out = Vec::new();
        dec.process_symbols(&crate::framer::ideal(&dibits), &mut out);
        let events: Vec<_> = out.iter().map(|r| &r.message).collect();
        assert!(
            matches!(events[0], VoiceEvent::Header { header: Some(h), .. } if h.value.talkgroup == 865)
        );
        let got: Vec<ImbeParams> = events
            .iter()
            .filter_map(|e| e.frames())
            .flatten()
            .map(|f| f.params)
            .collect();
        assert_eq!(got, sent);
        let lc = events.iter().find_map(|e| match e {
            VoiceEvent::Ldu1 {
                link_control: Some(lc),
                ..
            } => lc.value.message(),
            _ => None,
        });
        assert_eq!(
            lc,
            Some(LcMessage::GroupVoice(GroupVoice {
                talkgroup: 865,
                source: 0x12_3456,
                emergency: false,
                encrypted: false
            }))
        );
        assert!(matches!(
            events.last(),
            Some(VoiceEvent::Terminator {
                terminator: Terminator::Simple,
                ..
            })
        ));
        let s = dec.stats();
        assert_eq!(
            (s.ldu1, s.ldu2, s.imbe_frames, s.imbe_errors, s.link_control),
            (3, 3, 54, 0, 3)
        );
    }

    #[test]
    fn noisy_cqpsk_call_over_the_air() {
        let mut rng = Rng::new(82);
        let (dibits, sent) = call(&mut rng, 6);
        let iq = Channel {
            es_n0_db: 14.0,
            freq_offset_hz: 40.0,
            delay: 0.6,
            clock_ppm: 30.0,
            seed: 4,
        }
        .apply(&testgen::modulate_cqpsk(&dibits, 5), 5);
        let cfg = DemodConfig {
            matched_filter: true,
            ..DemodConfig::new(24000.0, Modulation::Cqpsk)
        };
        let mut dec = VoiceDecoder::new(cfg);
        let mut out = Vec::new();
        for chunk in iq.chunks(960) {
            dec.process(chunk, &mut out);
        }
        let got: Vec<ImbeParams> = out
            .iter()
            .filter_map(|r| r.message.frames())
            .flatten()
            .map(|f| f.params)
            .collect();
        let right = got.iter().zip(&sent).filter(|(a, b)| a == b).count();
        assert_eq!(got.len(), sent.len());
        assert!(right * 100 >= sent.len() * 97, "{right} of {}", sent.len());
        assert_eq!(dec.stats().link_control_failed, 0);
    }
}
