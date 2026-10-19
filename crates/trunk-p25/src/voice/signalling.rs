//! Signalling carried by voice channels.
//!
//! Link control (LDU1, TDULC), encryption sync (LDU2) and the header data
//! unit are each a Reed–Solomon word of hexbits whose symbols are themselves
//! protected by a short binary code.
//!
//! Inner symbols are soft-decoded and their margins become the outer
//! decoder's reliabilities, so a hexbit the inner code was unsure of is
//! erased rather than guessed (generalised minimum-distance decoding).

use crate::fec::{
    BlockCode, GOLAY_18, GOLAY_24, HAMMING_10, RS_24_12, RS_24_16, RS_36_20, ReedSolomon,
};
use crate::gf64::Gf64;
use crate::trunking::osp::{mfid, service_options};

/// A decoded word and what it took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checked<T> {
    pub value: T,
    /// Reed–Solomon symbols corrected (errors and erasures).
    pub corrected: u8,
    /// Symbols erased on the way.
    pub erased: u8,
}

/// Group voice addressing from a link-control word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GroupVoice {
    pub talkgroup: u16,
    pub source: u32,
    pub emergency: bool,
    pub encrypted: bool,
}

/// What a link-control word says (the cases trunk-recorder uses).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LcMessage {
    /// LCO 0: who is talking to which group.
    GroupVoice(GroupVoice),
    /// LCO 2 (implicit MFID): other groups' channels.
    GroupVoiceUpdate {
        channel_a: u16,
        talkgroup_a: u16,
        channel_b: u16,
        talkgroup_b: u16,
    },
    /// LCO 4 (implicit MFID): a group's transmit and receive channels.
    GroupVoiceUpdateExplicit {
        service_options: u8,
        talkgroup: u16,
        transmit: u16,
        receive: u16,
    },
    /// Motorola talker alias (MFID 0x90): header (LCO 0x15) or block (0x17).
    MotorolaAlias {
        header: bool,
    },
    /// Harris talker alias (MFID 0xa4, LCO 0x32–0x35).
    HarrisAlias {
        part_b: bool,
        even: bool,
    },
    Other {
        opcode: u8,
        mfid: u8,
    },
}

/// Standard link control opcodes, by their TIA-102.AABF-A aliases (§7.3,
/// LCO table).
pub mod lco {
    /// Group Voice Channel User (§7.3.1; explicit MFID).
    pub const LC_GRP_V_CH_USR: u8 = 0x00;
    /// Group Voice Channel Update (§7.3.2; implicit MFID).
    pub const LC_GRP_V_CH_UPDT: u8 = 0x02;
    /// Group Voice Channel Update – Explicit (§7.3.4; implicit MFID). The
    /// alias is the standard's, without its `_V`.
    pub const LC_GRP_CH_UPDT_EXP: u8 = 0x04;
}

/// A 72-bit link-control word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LinkControl(pub [u8; 9]);

/// Manufacturer IDs whose LCO 0 is the standard group voice user (the
/// standard two, and Motorola and Harris as op25 reads them).
const STANDARD_MFIDS: [u8; 4] = [
    mfid::STANDARD,
    mfid::STANDARD_ALT,
    mfid::MOTOROLA,
    mfid::HARRIS,
];

impl LinkControl {
    /// Protected flag: octet 0, bit 7 (TIA-102.AABF-A §7.1).
    pub const P: u8 = 0x80;
    /// Implicit/Explicit MFID Format flag: octet 0, bit 6 (TIA-102.AABF-A §7.1).
    pub const SF: u8 = 0x40;
    /// Link Control Opcode: octet 0, bits 5–0 (TIA-102.AABF-A §7.1).
    pub const LCO: u8 = 0x3f;

    /// The word itself is encrypted.
    pub const fn protected(&self) -> bool {
        self.0[0] & Self::P != 0
    }

    /// The manufacturer ID is implicit (standard) rather than in byte 1.
    pub const fn implicit_mfid(&self) -> bool {
        self.0[0] & Self::SF != 0
    }

    pub const fn opcode(&self) -> u8 {
        self.0[0] & Self::LCO
    }

    pub const fn mfid(&self) -> u8 {
        self.0[1]
    }

    const fn u16_at(&self, i: usize) -> u16 {
        u16::from_be_bytes([self.0[i], self.0[i + 1]])
    }

    pub const fn message(&self) -> Option<LcMessage> {
        if self.protected() {
            return None;
        }
        let b = &self.0;
        Some(if self.implicit_mfid() {
            match self.opcode() {
                lco::LC_GRP_V_CH_UPDT => LcMessage::GroupVoiceUpdate {
                    channel_a: self.u16_at(1),
                    talkgroup_a: self.u16_at(3),
                    channel_b: self.u16_at(5),
                    talkgroup_b: self.u16_at(7),
                },
                lco::LC_GRP_CH_UPDT_EXP => LcMessage::GroupVoiceUpdateExplicit {
                    service_options: b[2],
                    talkgroup: self.u16_at(3),
                    transmit: self.u16_at(5),
                    receive: self.u16_at(7),
                },
                opcode => LcMessage::Other { opcode, mfid: 0 },
            }
        } else {
            match (self.opcode(), b[1]) {
                (lco::LC_GRP_V_CH_USR, mfid) if is_standard(mfid) => {
                    LcMessage::GroupVoice(GroupVoice {
                        talkgroup: self.u16_at(4),
                        source: u32::from_be_bytes([0, b[6], b[7], b[8]]),
                        emergency: b[2] & service_options::EMERGENCY != 0,
                        encrypted: b[2] & service_options::PROTECTED != 0,
                    })
                }
                (0x15, mfid::MOTOROLA) => LcMessage::MotorolaAlias { header: true },
                (0x17, mfid::MOTOROLA) => LcMessage::MotorolaAlias { header: false },
                (op @ 0x32..=0x35, mfid::HARRIS) => LcMessage::HarrisAlias {
                    part_b: op & 1 == 1,
                    even: op >= 0x34,
                },
                (opcode, mfid) => LcMessage::Other { opcode, mfid },
            }
        })
    }
}

const fn is_standard(mfid: u8) -> bool {
    let mut i = 0;
    while i < STANDARD_MFIDS.len() {
        if STANDARD_MFIDS[i] == mfid {
            return true;
        }
        i += 1;
    }
    false
}

/// Algorithm ID meaning "not encrypted" (TIA-102.BAAC-A §2.8).
pub const ALGID_CLEAR: u8 = 0x80;

/// Encryption sync (LDU2): message indicator, algorithm and key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EncryptionSync {
    pub mi: [u8; 9],
    pub algid: u8,
    pub keyid: u16,
}

impl EncryptionSync {
    pub const fn encrypted(&self) -> bool {
        self.algid != ALGID_CLEAR
    }
}

/// The header data unit sent before voice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Header {
    pub mi: [u8; 9],
    pub mfid: u8,
    pub algid: u8,
    pub keyid: u16,
    pub talkgroup: u16,
}

/// Pack the first `8B/6` hexbits into `B` bytes, most significant first.
pub const fn pack<const B: usize>(h: &[Gf64]) -> [u8; B] {
    let mut out = [0u8; B];
    let mut bit = 0;
    while bit < B * 8 {
        let v = (h[bit / 6].get() >> (5 - bit % 6)) & 1;
        out[bit / 8] |= v << (7 - bit % 8);
        bit += 1;
    }
    out
}

/// Hexbits and their reliabilities from consecutive inner codewords; a
/// codeword carries `per` hexbits in its information bits.
fn hexbits<const H: usize>(code: &dyn BlockCode, llr: &[f32], per: usize) -> ([Gf64; H], [f32; H]) {
    let n = code.n() as usize;
    let mut sym = [Gf64::ZERO; H];
    let mut rel = [0f32; H];
    for (c, bits) in llr.chunks_exact(n).take(H / per).enumerate() {
        let d = code.decode_soft(bits);
        for p in 0..per {
            #[allow(clippy::cast_possible_truncation)]
            {
                sym[c * per + p] = Gf64::new((d.info >> (6 * (per - 1 - p))) as u8);
            }
            rel[c * per + p] = d.margin;
        }
    }
    (sym, rel)
}

/// The outer Reed–Solomon decode (generalised minimum distance).
fn outer<const N: usize, const K: usize>(
    rs: &ReedSolomon<N, K>,
    mut word: [Gf64; N],
    rel: &[f32; N],
) -> Option<([Gf64; N], u8, u8)> {
    let d = rs.decode_gmd(&mut word, rel)?;
    #[allow(clippy::cast_possible_truncation)]
    Some((word, d.changed as u8, d.erased as u8))
}

/// Link control from an LDU1's 240 signalling bits (24 Hamming(10,6) words).
pub fn ldu1_link_control(llr: &[f32]) -> Option<Checked<LinkControl>> {
    let (word, rel) = hexbits::<24>(&HAMMING_10, llr, 1);
    let (w, corrected, erased) = outer(&RS_24_12, word, &rel)?;
    Some(Checked {
        value: LinkControl(pack(&w[..12])),
        corrected,
        erased,
    })
}

/// Encryption sync from an LDU2's 240 signalling bits.
pub fn ldu2_encryption_sync(llr: &[f32]) -> Option<Checked<EncryptionSync>> {
    let (word, rel) = hexbits::<24>(&HAMMING_10, llr, 1);
    let (w, corrected, erased) = outer(&RS_24_16, word, &rel)?;
    let b: [u8; 12] = pack(&w[..16]);
    let mut mi = [0u8; 9];
    mi.copy_from_slice(&b[..9]);
    let value = EncryptionSync {
        mi,
        algid: b[9],
        keyid: u16::from_be_bytes([b[10], b[11]]),
    };
    Some(Checked {
        value,
        corrected,
        erased,
    })
}

/// Link control from a TDULC's 288 payload bits (12 Golay(24,12) words).
pub fn tdulc_link_control(llr: &[f32]) -> Option<Checked<LinkControl>> {
    let (word, rel) = hexbits::<24>(&GOLAY_24, llr, 2);
    let (w, corrected, erased) = outer(&RS_24_12, word, &rel)?;
    Some(Checked {
        value: LinkControl(pack(&w[..12])),
        corrected,
        erased,
    })
}

/// The header from an HDU's 648 payload bits (36 Golay(18,6) words).
pub fn header(llr: &[f32]) -> Option<Checked<Header>> {
    let (word, rel) = hexbits::<36>(&GOLAY_18, llr, 1);
    let (w, corrected, erased) = outer(&RS_36_20, word, &rel)?;
    let b: [u8; 15] = pack(&w[..20]);
    let mut mi = [0u8; 9];
    mi.copy_from_slice(&b[..9]);
    let value = Header {
        mi,
        mfid: b[9],
        algid: b[10],
        keyid: u16::from_be_bytes([b[11], b[12]]),
        talkgroup: u16::from_be_bytes([b[13], b[14]]),
    };
    Some(Checked {
        value,
        corrected,
        erased,
    })
}

/// Encoders, for tests and signal generation.
pub mod encode {
    use super::{BlockCode, GOLAY_18, GOLAY_24, Gf64, HAMMING_10, RS_24_12, RS_24_16, RS_36_20};

    fn unpack<const H: usize>(bytes: &[u8]) -> [Gf64; H] {
        std::array::from_fn(|i| {
            let mut v = 0u8;
            for b in 0..6 {
                let bit = i * 6 + b;
                v = (v << 1) | ((bytes[bit / 8] >> (7 - bit % 8)) & 1);
            }
            Gf64::new(v)
        })
    }

    fn bits(code: &dyn BlockCode, symbols: &[Gf64], per: usize) -> Vec<bool> {
        let n = code.n();
        symbols
            .chunks(per)
            .flat_map(|c| {
                let info = c.iter().fold(0u32, |a, s| (a << 6) | u32::from(s.get()));
                let cw = code.encode(info);
                (0..n).rev().map(move |b| (cw >> b) & 1 == 1)
            })
            .collect()
    }

    pub fn link_control_ldu1(lc: &[u8; 9]) -> Vec<bool> {
        bits(&HAMMING_10, &RS_24_12.encode(&unpack(lc)), 1)
    }

    pub fn encryption_sync(mi: &[u8; 9], algid: u8, keyid: u16) -> Vec<bool> {
        let mut b = [0u8; 12];
        b[..9].copy_from_slice(mi);
        b[9] = algid;
        b[10..].copy_from_slice(&keyid.to_be_bytes());
        bits(&HAMMING_10, &RS_24_16.encode(&unpack(&b)), 1)
    }

    pub fn link_control_tdulc(lc: &[u8; 9]) -> Vec<bool> {
        bits(&GOLAY_24, &RS_24_12.encode(&unpack(lc)), 2)
    }

    pub fn header(mi: &[u8; 9], mfid: u8, algid: u8, keyid: u16, talkgroup: u16) -> Vec<bool> {
        let mut b = [0u8; 15];
        b[..9].copy_from_slice(mi);
        b[9] = mfid;
        b[10] = algid;
        b[11..13].copy_from_slice(&keyid.to_be_bytes());
        b[13..].copy_from_slice(&talkgroup.to_be_bytes());
        bits(&GOLAY_18, &RS_36_20.encode(&unpack(&b)), 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::Rng;

    const LC: [u8; 9] = [0x00, 0x00, 0x40, 0x00, 0x0d, 0xba, 0x1a, 0x2b, 0x3c];

    fn soft(bits: &[bool], sigma: f64, rng: &mut Rng) -> Vec<f32> {
        #[allow(clippy::cast_possible_truncation)]
        bits.iter()
            .map(|&b| sigma.mul_add(rng.gauss(), if b { -1.0 } else { 1.0 }) as f32)
            .collect()
    }

    #[test]
    fn group_voice_link_control_reads_like_op25() {
        let lc = LinkControl(LC);
        assert_eq!(
            lc.message(),
            Some(LcMessage::GroupVoice(GroupVoice {
                talkgroup: 3514,
                source: 0x1a_2b3c,
                emergency: false,
                encrypted: true
            }))
        );
        assert_eq!(LinkControl([0x80, 0, 0, 0, 0, 0, 0, 0, 0]).message(), None);
        assert_eq!(
            LinkControl([0x15, 0x90, 0, 0, 0, 0, 0, 0, 0]).message(),
            Some(LcMessage::MotorolaAlias { header: true })
        );
        assert!(matches!(
            LinkControl([0x44, 0, 0x12, 0, 7, 0, 1, 0, 2]).message(),
            Some(LcMessage::GroupVoiceUpdateExplicit {
                talkgroup: 7,
                transmit: 1,
                receive: 2,
                ..
            })
        ));
    }

    #[test]
    fn every_word_round_trips() {
        let mut rng = Rng::new(71);
        let lc = ldu1_link_control(&soft(&encode::link_control_ldu1(&LC), 0.0, &mut rng)).unwrap();
        assert_eq!((lc.value, lc.corrected), (LinkControl(LC), 0));
        let lc =
            tdulc_link_control(&soft(&encode::link_control_tdulc(&LC), 0.0, &mut rng)).unwrap();
        assert_eq!(lc.value, LinkControl(LC));
        let mi = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        let es = ldu2_encryption_sync(&soft(
            &encode::encryption_sync(&mi, 0x84, 0x1234),
            0.0,
            &mut rng,
        ))
        .unwrap();
        assert_eq!(
            es.value,
            EncryptionSync {
                mi,
                algid: 0x84,
                keyid: 0x1234
            }
        );
        assert!(es.value.encrypted());
        let h = header(&soft(
            &encode::header(&mi, 0x90, 0x80, 0, 3514),
            0.0,
            &mut rng,
        ))
        .unwrap();
        assert_eq!(
            h.value,
            Header {
                mi,
                mfid: 0x90,
                algid: 0x80,
                keyid: 0,
                talkgroup: 3514
            }
        );
    }

    /// op25: hard Hamming(10,6) per hexbit, then RS bounded-distance with
    /// at most `limit` corrections.
    fn op25_link_control(llr: &[f32]) -> Option<[u8; 9]> {
        let mut word = [Gf64::ZERO; 24];
        for (i, bits) in llr.as_chunks::<10>().0.iter().enumerate() {
            let w = bits
                .iter()
                .fold(0u32, |a, &l| (a << 1) | u32::from(l < 0.0));
            #[allow(clippy::cast_possible_truncation)]
            {
                word[i] = Gf64::new(HAMMING_10.decode_hard(w).info as u8);
            }
        }
        RS_24_12.decode(&mut word, &[]).ok()?;
        Some(pack(&word[..12]))
    }

    #[test]
    fn soft_gmd_recovers_link_control_op25_loses_without_false_words() {
        let mut rng = Rng::new(72);
        let clean = encode::link_control_ldu1(&LC);
        let (mut ours, mut op25, mut wrong) = (0, 0, 0);
        let trials = 1000;
        for _ in 0..trials {
            let llr = soft(&clean, 0.9, &mut rng);
            match ldu1_link_control(&llr) {
                Some(c) if c.value == LinkControl(LC) => ours += 1,
                Some(_) => wrong += 1,
                None => {}
            }
            op25 += usize::from(op25_link_control(&llr) == Some(LC));
        }
        eprintln!("link control at sigma 0.9: ours {ours}, op25 {op25}, wrong {wrong} of {trials}");
        assert!(ours > op25 + trials / 10, "ours {ours} op25 {op25}");
        assert!(wrong * 200 < trials, "wrong {wrong}");
    }

    #[test]
    fn noise_rarely_yields_a_word() {
        let mut rng = Rng::new(73);
        let mut accepted = 0;
        for _ in 0..2000 {
            #[allow(clippy::cast_possible_truncation)]
            let llr: Vec<f32> = (0..240).map(|_| rng.gauss() as f32).collect();
            accepted += usize::from(ldu1_link_control(&llr).is_some());
        }
        eprintln!("noise: {accepted} of 2000 decoded");
        assert!(accepted < 10, "{accepted} of 2000 noise words decoded");
    }
}
