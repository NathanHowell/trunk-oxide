//! MAC signalling on a Phase 2 voice channel: the associated control
//! channels (FACCH, SACCH) and the encryption sync (ESS).
//!
//! All three are Reed–Solomon words of the RS(63,35,29) code over GF(64),
//! shortened at the front and, for the control channels, punctured at the
//! back: ESS is (44,16,29), FACCH (45,26,20) and SACCH (52,30,23). A MAC PDU
//! ends in a CRC-12.
//!
//! op25 decodes them by marking the shortened symbols — known to be zero —
//! as erasures alongside the punctured ones, which spends 9 (FACCH) or 5
//! (SACCH) symbols of the code's correcting power on positions it already
//! knows. Here shortened symbols are simply absent; punctured ones enter
//! generalised minimum-distance decoding with no reliability, so Forney's
//! acceptance rule (distance below the mother code's 29) becomes exactly the
//! punctured code's own bound. Symbol reliabilities come from the bit LLRs.

use std::ops::Range;

use super::layout::{ESS_A, ESS_B, FACCH_GAP, PAYLOAD_DIBITS};
use crate::crc::{crc12, crc12_ok};
use crate::fec::ReedSolomon;
use crate::gf64::Gf64;
use crate::types::Nac;
use crate::voice::{Checked, EncryptionSync};

/// The mother code, RS(63,35,29) over GF(64) (BBAC §5.6 Table 5-5).
pub const MOTHER_N: usize = 63;
pub const MOTHER_K: usize = 35;
/// Leading information hexbits shortened (known zero), per application
/// (BBAC §5.6 Table 5-5, S).
pub const ESS_SHORTENED: usize = 19;
pub const S_OEMI_SHORTENED: usize = 9;
pub const I_OEMI_SHORTENED: usize = 5;
/// Least significant parity hexbits punctured (not sent), per application
/// (BBAC §5.6 Table 5-5, U; ESS has none).
pub const S_OEMI_PUNCTURED: usize = 9;
pub const I_OEMI_PUNCTURED: usize = 6;
/// Information hexbits of each derived code (K′ = K − S).
pub const ESS_INFO: usize = MOTHER_K - ESS_SHORTENED;
pub const S_OEMI_INFO: usize = MOTHER_K - S_OEMI_SHORTENED;
pub const I_OEMI_INFO: usize = MOTHER_K - I_OEMI_SHORTENED;
/// The ESS code word's length (N′ = N − S − U).
pub const ESS_LEN: usize = MOTHER_N - ESS_SHORTENED;

/// Encryption sync, (44,16,29): 16 data hexbits (in the four 4V bursts), 28
/// parity (2V) (BBAC §5.6.2).
pub const RS_ESS: ReedSolomon<ESS_LEN, ESS_INFO> = ReedSolomon::new();
/// FACCH, the S-OEMI (45,26,20): 26 data hexbits, 19 parity sent, 9
/// punctured (BBAC §5.6.1.2).
pub const RS_FACCH: ReedSolomon<{ MOTHER_N - S_OEMI_SHORTENED }, S_OEMI_INFO> = ReedSolomon::new();
/// SACCH, the I-OEMI (52,30,23): 30 data hexbits, 22 parity sent, 6
/// punctured (BBAC §5.6.1.3).
pub const RS_SACCH: ReedSolomon<{ MOTHER_N - I_OEMI_SHORTENED }, I_OEMI_INFO> = ReedSolomon::new();

/// ESS-B hexbits in each 4V burst and ESS-A hexbits in the 2V (BBAC §5.6.2
/// Fig. 5-9).
pub const ESS_B_HEXBITS: usize = 4;
pub const ESS_A_HEXBITS: usize = 28;
/// The ESS information octets: ALGID, KID, MI (BBAC §5.6.2 Fig. 5-9).
pub const ESS_ALGID: usize = 0;
pub const ESS_KID: Range<usize> = 1..3;
pub const ESS_MI: Range<usize> = 3..12;

/// A MAC PDU's octets before its CRC-12: 19.5-octet S-OEMI (FACCH) and
/// 22.5-octet I-OEMI (SACCH) PDUs (BBAC §8.1.1 Fig. 8-2).
pub const S_OEMI_PDU_OCTETS: usize = 18;
pub const I_OEMI_PDU_OCTETS: usize = 21;

/// MAC header: 3-bit opcode, 3-bit offset, 2 reserved bits (BBAC §8.1).
const OPCODE_SHIFT: u32 = 5;
const OFFSET_SHIFT: u32 = 2;
const OFFSET_MASK: u8 = 0b111;

/// MAC PDU opcodes (BBAC §8.4.1 Table 8-6).
pub const MAC_PTT: u8 = 0b001;
pub const MAC_END_PTT: u8 = 0b010;
pub const MAC_IDLE: u8 = 0b011;
pub const MAC_ACTIVE: u8 = 0b100;
pub const MAC_HANGTIME: u8 = 0b110;

/// `MAC_PTT` fields, by octet (BBAC §8.2.1 Figs. 8-4 to 8-6).
pub const PTT_MI: Range<usize> = 1..10;
pub const PTT_ALGID: usize = 10;
pub const PTT_KID: Range<usize> = 11..13;
pub const PTT_SOURCE: Range<usize> = 13..16;
pub const PTT_GROUP: Range<usize> = 16..18;
/// `MAC_END_PTT`'s color code: the low 4 bits of its first octet after the
/// header, then the next (BBAC §8.2.2 Figs. 8-7 to 8-9).
pub const END_PTT_COLOR_CODE: Range<usize> = 1..3;
pub const END_PTT_COLOR_CODE_HIGH_MASK: u8 = 0x0f;
/// `MAC_END_PTT`'s group address octets (BBAC §8.2.2 Figs. 8-7 to 8-9).
pub const END_PTT_GROUP: Range<usize> = 16..18;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MacChannel {
    Facch,
    Sacch,
}

impl MacChannel {
    /// PDU length in bytes, before the CRC.
    pub const fn pdu_bytes(self) -> usize {
        match self {
            Self::Facch => S_OEMI_PDU_OCTETS,
            Self::Sacch => I_OEMI_PDU_OCTETS,
        }
    }

    /// Hexbits sent.
    pub const fn sent(self) -> usize {
        match self {
            Self::Facch => MOTHER_N - S_OEMI_SHORTENED - S_OEMI_PUNCTURED,
            Self::Sacch => MOTHER_N - I_OEMI_SHORTENED - I_OEMI_PUNCTURED,
        }
    }

    /// Whether payload dibit `p` carries this channel's data.
    pub const fn carries(self, p: usize) -> bool {
        match self {
            Self::Facch => p < FACCH_GAP.start || p >= FACCH_GAP.end,
            Self::Sacch => p < PAYLOAD_DIBITS,
        }
    }
}

/// The MAC PDU's opcode (top three bits of its first byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MacOpcode {
    Signal,
    Ptt,
    EndPtt,
    Idle,
    Active,
    Hangtime,
    Reserved(u8),
}

impl MacOpcode {
    pub const fn from_bits(v: u8) -> Self {
        match v & 7 {
            // Reserved in BBAC Table 8-6; op25's MAC_SIGNAL.
            0 => Self::Signal,
            MAC_PTT => Self::Ptt,
            MAC_END_PTT => Self::EndPtt,
            MAC_IDLE => Self::Idle,
            MAC_ACTIVE => Self::Active,
            MAC_HANGTIME => Self::Hangtime,
            v => Self::Reserved(v),
        }
    }

    /// The opcode's three bits.
    pub const fn bits(self) -> u8 {
        match self {
            Self::Signal => 0,
            Self::Ptt => MAC_PTT,
            Self::EndPtt => MAC_END_PTT,
            Self::Idle => MAC_IDLE,
            Self::Active => MAC_ACTIVE,
            Self::Hangtime => MAC_HANGTIME,
            Self::Reserved(v) => v,
        }
    }

    /// A name for metrics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Signal => "signal",
            Self::Ptt => "ptt",
            Self::EndPtt => "end_ptt",
            Self::Idle => "idle",
            Self::Active => "active",
            Self::Hangtime => "hangtime",
            Self::Reserved(_) => "reserved",
        }
    }
}

/// What a `MAC_PTT` says about the transmission starting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MacPtt {
    pub encryption: EncryptionSync,
    pub source: u32,
    pub group: u16,
}

/// A MAC PDU whose CRC checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MacPdu {
    pub channel: MacChannel,
    bytes: [u8; 21],
}

impl MacPdu {
    pub fn new(channel: MacChannel, bytes: &[u8]) -> Self {
        let mut b = [0u8; 21];
        let n = channel.pdu_bytes().min(bytes.len());
        b[..n].copy_from_slice(&bytes[..n]);
        Self { channel, bytes: b }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.channel.pdu_bytes()]
    }

    pub const fn opcode(&self) -> MacOpcode {
        MacOpcode::from_bits(self.bytes[0] >> OPCODE_SHIFT)
    }

    /// A `MAC_END_PTT`'s color code.
    pub const fn end_ptt_color_code(&self) -> Option<u16> {
        if !matches!(self.opcode(), MacOpcode::EndPtt) {
            return None;
        }
        let c = END_PTT_COLOR_CODE.start;
        Some(u16::from_be_bytes([
            self.bytes[c] & END_PTT_COLOR_CODE_HIGH_MASK,
            self.bytes[c + 1],
        ]))
    }

    /// A `MAC_END_PTT` naming a color code other than `ours` (when that is
    /// known): another system's, heard co-channel.
    pub const fn is_foreign_end_ptt(&self, ours: Option<Nac>) -> bool {
        matches!((self.end_ptt_color_code(), ours), (Some(c), Some(n)) if c != n.get())
    }

    /// The offset field: bursts until the first 4V of the voice that follows.
    pub const fn offset(&self) -> u8 {
        (self.bytes[0] >> OFFSET_SHIFT) & OFFSET_MASK
    }

    pub const fn ptt(&self) -> Option<MacPtt> {
        if !matches!(self.opcode(), MacOpcode::Ptt) {
            return None;
        }
        let b = &self.bytes;
        let mut mi = [0u8; 9];
        let mut i = 0;
        while i < mi.len() {
            mi[i] = b[PTT_MI.start + i];
            i += 1;
        }
        let (kid, src, grp) = (PTT_KID.start, PTT_SOURCE.start, PTT_GROUP.start);
        Some(MacPtt {
            encryption: EncryptionSync {
                mi,
                algid: b[PTT_ALGID],
                keyid: u16::from_be_bytes([b[kid], b[kid + 1]]),
            },
            source: u32::from_be_bytes([0, b[src], b[src + 1], b[src + 2]]),
            group: u16::from_be_bytes([b[grp], b[grp + 1]]),
        })
    }
}

/// A hexbit and how sure the bits made it: the smallest |LLR| of its six.
pub fn hexbit(llr: &[f32]) -> (Gf64, f32) {
    let v = llr.iter().fold(0u8, |a, &l| (a << 1) | u8::from(l < 0.0));
    let r = llr.iter().fold(f32::INFINITY, |m, l| m.min(l.abs()));
    (Gf64::new(v), r)
}

/// Bits of hexbits, most significant first, into bytes.
fn pack(symbols: &[Gf64]) -> [u8; 23] {
    let mut out = [0u8; 23];
    for (i, s) in symbols.iter().enumerate() {
        for b in 0..6 {
            let bit = i * 6 + b;
            out[bit / 8] |= ((s.get() >> (5 - b)) & 1) << (7 - bit % 8);
        }
    }
    out
}

fn unpack<const H: usize>(bytes: &[u8]) -> [Gf64; H] {
    std::array::from_fn(|i| {
        let mut v = 0u8;
        for b in 0..6 {
            let bit = i * 6 + b;
            v = (v << 1) | ((bytes.get(bit / 8).copied().unwrap_or(0) >> (7 - bit % 8)) & 1);
        }
        Gf64::new(v)
    })
}

/// GMD over the sent symbols; the rest (punctured) enter with no reliability.
fn outer<const N: usize, const K: usize>(
    rs: &ReedSolomon<N, K>,
    sent: &[(Gf64, f32)],
) -> Option<([Gf64; N], u8, u8)> {
    let mut word = [Gf64::ZERO; N];
    let mut rel = [0f32; N];
    for (j, &(s, r)) in sent.iter().enumerate().take(N) {
        word[j] = s;
        rel[j] = r;
    }
    let received = word;
    let d = rs.decode_gmd(&mut word, &rel)?;
    let punctured = N - sent.len().min(N);
    let changed = (0..sent.len().min(N))
        .filter(|&j| word[j] != received[j])
        .count();
    #[allow(clippy::cast_possible_truncation)]
    Some((
        word,
        changed as u8,
        d.erased.saturating_sub(punctured) as u8,
    ))
}

/// The bit LLRs a control channel's data occupies, from the burst's 312
/// payload bit LLRs.
pub fn acch_bits(channel: MacChannel, payload: &[f32]) -> Vec<f32> {
    (0..PAYLOAD_DIBITS)
        .filter(|&p| channel.carries(p))
        .flat_map(|p| [payload[2 * p], payload[2 * p + 1]])
        .collect()
}

/// Decode a FACCH or SACCH from the burst's 312 payload bit LLRs.
pub fn decode_acch(channel: MacChannel, payload: &[f32]) -> Option<Checked<MacPdu>> {
    let bits = acch_bits(channel, payload);
    let sent: Vec<(Gf64, f32)> = bits.as_chunks::<6>().0.iter().map(|c| hexbit(c)).collect();
    let (data, corrected, erased) = match channel {
        MacChannel::Facch => {
            let (w, c, e) = outer(&RS_FACCH, &sent)?;
            (pack(&w[..S_OEMI_INFO]), c, e)
        }
        MacChannel::Sacch => {
            let (w, c, e) = outer(&RS_SACCH, &sent)?;
            (pack(&w[..I_OEMI_INFO]), c, e)
        }
    };
    let n = channel.pdu_bytes() * 8;
    crc12_ok(&data, n).then(|| Checked {
        value: MacPdu::new(channel, &data),
        corrected,
        erased,
    })
}

/// The ESS-B hexbits of a 4V burst, with reliabilities.
pub fn ess_b(payload: &[f32]) -> [(Gf64, f32); ESS_B_HEXBITS] {
    std::array::from_fn(|h| {
        let at = 2 * ESS_B.start + 6 * h;
        hexbit(&payload[at..at + 6])
    })
}

/// The ESS-A hexbits of a 2V burst.
pub fn ess_a(payload: &[f32]) -> [(Gf64, f32); ESS_A_HEXBITS] {
    std::array::from_fn(|h| {
        let at = 2 * ESS_A.start + 6 * h;
        hexbit(&payload[at..at + 6])
    })
}

/// Encryption sync from the four 4V bursts' ESS-B (`None` for a burst that
/// was missed: its symbols are erased) and the 2V's ESS-A.
pub fn decode_ess(
    b: &[Option<[(Gf64, f32); ESS_B_HEXBITS]>; 4],
    a: &[(Gf64, f32); ESS_A_HEXBITS],
) -> Option<Checked<EncryptionSync>> {
    let mut sent = [(Gf64::ZERO, 0.0f32); ESS_LEN];
    for (i, part) in b.iter().enumerate() {
        if let Some(part) = part {
            sent[ESS_B_HEXBITS * i..ESS_B_HEXBITS * (i + 1)].copy_from_slice(part);
        }
    }
    sent[ESS_INFO..].copy_from_slice(a);
    let (w, corrected, erased) = outer(&RS_ESS, &sent)?;
    let bytes = pack(&w[..ESS_INFO]);
    let mut mi = [0u8; 9];
    mi.copy_from_slice(&bytes[ESS_MI]);
    Some(Checked {
        value: EncryptionSync {
            mi,
            algid: bytes[ESS_ALGID],
            keyid: u16::from_be_bytes([bytes[ESS_KID.start], bytes[ESS_KID.start + 1]]),
        },
        corrected,
        erased,
    })
}

/// op25's decoders (`p25p2_tdma.cc`), for comparison: hard hexbits, the
/// shortened and punctured symbols all erased, bounded-distance decoding.
pub mod op25 {
    use super::{
        ESS_A_HEXBITS, ESS_ALGID, ESS_INFO, ESS_KID, ESS_LEN, ESS_MI, EncryptionSync, Gf64,
        I_OEMI_INFO, I_OEMI_SHORTENED, MOTHER_K, MOTHER_N, MacChannel, MacPdu, ReedSolomon,
        S_OEMI_INFO, S_OEMI_SHORTENED, acch_bits, crc12_ok, hexbit, pack,
    };

    /// The full RS(63,35).
    const RS_63_35: ReedSolomon<MOTHER_N, MOTHER_K> = ReedSolomon::new();

    pub fn decode_acch(channel: MacChannel, payload: &[f32]) -> Option<MacPdu> {
        let bits = acch_bits(channel, payload);
        let (lead, data) = match channel {
            MacChannel::Facch => (S_OEMI_SHORTENED, S_OEMI_INFO),
            MacChannel::Sacch => (I_OEMI_SHORTENED, I_OEMI_INFO),
        };
        let mut word = [Gf64::ZERO; MOTHER_N];
        for (j, c) in bits.as_chunks::<6>().0.iter().enumerate() {
            word[lead + j] = hexbit(c).0;
        }
        let erasures: Vec<usize> = (0..lead).chain(lead + channel.sent()..MOTHER_N).collect();
        RS_63_35.decode(&mut word, &erasures).ok()?;
        let bytes = pack(&word[lead..lead + data]);
        crc12_ok(&bytes, channel.pdu_bytes() * 8).then(|| MacPdu::new(channel, &bytes))
    }

    /// op25 decodes ESS with up to 14 corrections, no erasures.
    pub fn decode_ess(b: &[Gf64; ESS_INFO], a: &[Gf64; ESS_A_HEXBITS]) -> Option<EncryptionSync> {
        let mut word = [Gf64::ZERO; ESS_LEN];
        word[..ESS_INFO].copy_from_slice(b);
        word[ESS_INFO..].copy_from_slice(a);
        super::RS_ESS.decode(&mut word, &[]).ok()?;
        let bytes = pack(&word[..ESS_INFO]);
        let mut mi = [0u8; 9];
        mi.copy_from_slice(&bytes[ESS_MI]);
        Some(EncryptionSync {
            mi,
            algid: bytes[ESS_ALGID],
            keyid: u16::from_be_bytes([bytes[ESS_KID.start], bytes[ESS_KID.start + 1]]),
        })
    }
}

/// Encoders, for regeneration and tests.
pub mod encode {
    use super::{
        ESS_ALGID, ESS_KID, ESS_LEN, ESS_MI, EncryptionSync, Gf64, MacChannel, PAYLOAD_DIBITS,
        RS_ESS, RS_FACCH, RS_SACCH,
    };
    use super::{crc12, unpack};

    fn bits(symbols: &[Gf64]) -> impl Iterator<Item = bool> + '_ {
        symbols
            .iter()
            .flat_map(|s| (0..6).rev().map(move |b| (s.get() >> b) & 1 == 1))
    }

    /// The PDU with its CRC, as the code's data hexbits.
    fn data_bytes(channel: MacChannel, pdu: &[u8]) -> [u8; 23] {
        let n = channel.pdu_bytes();
        let mut b = [0u8; 23];
        b[..n].copy_from_slice(&pdu[..n]);
        let crc = crc12(&b, n * 8);
        let [hi, lo] = (crc << 4).to_be_bytes();
        b[n] = hi;
        b[n + 1] = lo;
        b
    }

    /// The 312 payload bits of a FACCH or SACCH burst carrying `pdu` (bits
    /// outside the channel's data are false).
    pub fn acch(channel: MacChannel, pdu: &[u8]) -> Vec<bool> {
        let data = data_bytes(channel, pdu);
        let sent: Vec<bool> = match channel {
            MacChannel::Facch => bits(&RS_FACCH.encode(&unpack(&data))[..channel.sent()]).collect(),
            MacChannel::Sacch => bits(&RS_SACCH.encode(&unpack(&data))[..channel.sent()]).collect(),
        };
        let mut out = vec![false; 2 * PAYLOAD_DIBITS];
        let mut it = sent.into_iter();
        for p in (0..PAYLOAD_DIBITS).filter(|&p| channel.carries(p)) {
            out[2 * p] = it.next().unwrap_or(false);
            out[2 * p + 1] = it.next().unwrap_or(false);
        }
        out
    }

    /// The ESS codeword: 16 data hexbits (four per 4V), 28 parity (the 2V).
    pub fn ess(es: &EncryptionSync) -> [Gf64; ESS_LEN] {
        let mut b = [0u8; ESS_MI.end];
        b[ESS_ALGID] = es.algid;
        b[ESS_KID].copy_from_slice(&es.keyid.to_be_bytes());
        b[ESS_MI].copy_from_slice(&es.mi);
        RS_ESS.encode(&unpack(&b))
    }

    /// Bits of hexbits, most significant first.
    pub fn hexbit_bits(symbols: &[Gf64]) -> Vec<bool> {
        bits(symbols).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::Rng;

    #[test]
    fn end_ptt_names_its_color_code() {
        let mut b = [0u8; 21];
        b[0] = MAC_END_PTT << OPCODE_SHIFT;
        b[1] = 0xf5; // the high nibble is not the color code's
        b[2] = 0xe1;
        let end = MacPdu::new(MacChannel::Sacch, &b);
        assert_eq!(end.end_ptt_color_code(), Some(0x5e1));
        assert!(!end.is_foreign_end_ptt(Nac::new(0x5e1)));
        assert!(end.is_foreign_end_ptt(Nac::new(0x5e2)));
        assert!(!end.is_foreign_end_ptt(None), "unknown is not foreign");
        b[0] = MAC_HANGTIME << OPCODE_SHIFT;
        let hangtime = MacPdu::new(MacChannel::Sacch, &b);
        assert_eq!(hangtime.end_ptt_color_code(), None);
        assert!(!hangtime.is_foreign_end_ptt(Nac::new(0x5e2)));
    }

    fn hex<const M: usize>(v: &[u8]) -> [Gf64; M] {
        std::array::from_fn(|i| Gf64::new(v[i]))
    }

    /// ezpwd `RS<63,35>` (op25's codec) with 19 and 9 leading zeros, from a
    /// harness linked against op25.
    #[test]
    fn parity_matches_op25() {
        let w = [
            28, 4, 37, 42, 31, 45, 29, 26, 26, 37, 44, 27, 30, 31, 19, 48, 41, 8, 50, 12, 16, 10,
            22, 25, 13, 32, 7, 12, 10, 42, 8, 40, 15, 13, 53, 60, 55, 30, 55, 2, 57, 43, 29, 12,
        ];
        assert_eq!(RS_ESS.encode(&hex(&w)), hex::<44>(&w));
        let w = [
            57, 44, 61, 16, 63, 25, 47, 32, 29, 4, 44, 52, 29, 2, 43, 6, 56, 51, 58, 50, 31, 31,
            46, 55, 8, 25, 5, 2, 36, 12, 39, 28, 17, 20, 34, 56, 16, 3, 36, 29, 55, 46, 40, 34, 2,
            54, 14, 11, 45, 0, 59, 60, 54, 14,
        ];
        assert_eq!(RS_FACCH.encode(&hex(&w)), hex::<54>(&w));
    }

    fn noisy(bits: &[bool], sigma: f64, rng: &mut Rng) -> Vec<f32> {
        #[allow(clippy::cast_possible_truncation)]
        bits.iter()
            .map(|&b| sigma.mul_add(rng.gauss(), if b { -1.0 } else { 1.0 }) as f32)
            .collect()
    }

    fn random_pdu(rng: &mut Rng) -> [u8; 21] {
        #[allow(clippy::cast_possible_truncation)]
        std::array::from_fn(|_| rng.next_u64() as u8)
    }

    #[test]
    fn clean_pdus_round_trip_both_ways() {
        let mut rng = Rng::new(51);
        for channel in [MacChannel::Facch, MacChannel::Sacch] {
            for _ in 0..20 {
                let pdu = random_pdu(&mut rng);
                let llr = noisy(&encode::acch(channel, &pdu), 0.0, &mut rng);
                let got = decode_acch(channel, &llr).unwrap();
                assert_eq!(got.value.bytes(), &pdu[..channel.pdu_bytes()]);
                assert_eq!(got.corrected, 0);
                assert_eq!(op25::decode_acch(channel, &llr), Some(got.value));
            }
        }
    }

    #[test]
    fn knowing_the_shortened_symbols_corrects_more_than_op25() {
        let mut rng = Rng::new(52);
        for channel in [MacChannel::Facch, MacChannel::Sacch] {
            let (mut ours, mut theirs, mut wrong) = (0, 0, 0);
            let trials = 400;
            for _ in 0..trials {
                let pdu = random_pdu(&mut rng);
                let llr = noisy(&encode::acch(channel, &pdu), 0.55, &mut rng);
                match decode_acch(channel, &llr) {
                    Some(d) if d.value.bytes() == &pdu[..channel.pdu_bytes()] => ours += 1,
                    Some(_) => wrong += 1,
                    None => {}
                }
                theirs += usize::from(op25::decode_acch(channel, &llr).is_some());
            }
            assert_eq!(wrong, 0);
            assert!(
                ours > theirs + trials / 10,
                "{channel:?}: ours {ours} op25 {theirs}"
            );
        }
    }

    #[test]
    fn noise_does_not_decode() {
        let mut rng = Rng::new(53);
        let mut accepted = 0;
        for _ in 0..2000 {
            #[allow(clippy::cast_possible_truncation)]
            let llr: Vec<f32> = (0..312).map(|_| rng.gauss() as f32).collect();
            accepted += usize::from(decode_acch(MacChannel::Facch, &llr).is_some());
            accepted += usize::from(decode_acch(MacChannel::Sacch, &llr).is_some());
        }
        assert_eq!(accepted, 0);
    }

    #[test]
    fn ess_survives_a_missed_burst_and_errors_op25_cannot_correct() {
        let mut rng = Rng::new(54);
        let es = EncryptionSync {
            mi: [1, 2, 3, 4, 5, 6, 7, 8, 9],
            algid: 0x84,
            keyid: 0x1234,
        };
        let mut cw = encode::ess(&es);
        // Twelve confident symbol errors in the parity, and the second 4V
        // burst missed: 2·12 + 4 = 28 is within the code's reach only if the
        // missing symbols are known to be missing.
        for s in &mut cw[20..32] {
            *s += Gf64::ONE;
        }
        let llr = noisy(&encode::hexbit_bits(&cw), 0.0, &mut rng);
        let syms: Vec<(Gf64, f32)> = llr.as_chunks::<6>().0.iter().map(|c| hexbit(c)).collect();
        let b = [
            Some(std::array::from_fn(|i| syms[i])),
            None,
            Some(std::array::from_fn(|i| syms[8 + i])),
            Some(std::array::from_fn(|i| syms[12 + i])),
        ];
        let a: [(Gf64, f32); 28] = std::array::from_fn(|i| syms[16 + i]);
        let got = decode_ess(&b, &a).unwrap();
        assert_eq!(got.value, es);
        assert_eq!(got.erased, 4);
        assert!(got.corrected >= 12, "{got:?}");
        let hard_b: [Gf64; 16] = std::array::from_fn(|i| {
            if (4..8).contains(&i) {
                Gf64::ZERO
            } else {
                syms[i].0
            }
        });
        assert_eq!(op25::decode_ess(&hard_b, &a.map(|s| s.0)), None);
    }

    #[test]
    fn ptt_fields() {
        let mut b = [0u8; 21];
        b[0] = 0x20 | (2 << 2);
        b[1..10].copy_from_slice(&[9, 8, 7, 6, 5, 4, 3, 2, 1]);
        b[10] = 0x80;
        b[11..13].copy_from_slice(&[0, 0]);
        b[13..16].copy_from_slice(&[0x12, 0x34, 0x56]);
        b[16..18].copy_from_slice(&[0x0d, 0xba]);
        let pdu = MacPdu::new(MacChannel::Sacch, &b);
        assert_eq!(pdu.opcode(), MacOpcode::Ptt);
        assert_eq!(pdu.offset(), 2);
        let p = pdu.ptt().unwrap();
        assert_eq!(
            (p.source, p.group, p.encryption.algid),
            (0x12_3456, 3514, 0x80)
        );
    }
}
