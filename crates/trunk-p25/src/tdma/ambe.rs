//! AMBE+2 half-rate voice codewords: 49 parameter bits in 72 (TIA-102.BBAC).
//!
//! Four vectors: u0 (12 bits) in an extended Golay(24,12) word, u1 (12) in a
//! Golay(23,12) word scrambled with the 23-bit mask seeded by u0 (the same
//! generator IMBE uses), u2 (11) and u3 (14) unprotected. The 72 bits are
//! sent as an 18 × 4 block interleaver: the vectors' bits, most significant
//! first and one after another, fill the four columns in turn and are sent
//! row by row.
//!
//! The hard decision is TIA-102.BABA-1's (§5.3–5.5): u0 and then u1 on
//! hard bits, with the bits each code corrected or detected. The soft one
//! decodes both on soft bits and chooses u0 jointly with u1.

use crate::fec::{Decision, GOLAY_23, GOLAY_24};
use crate::voice::imbe::pn_masks;

pub const CODEWORD_BITS: usize = 72;

const WIDTH: [usize; 4] = [24, 23, 11, 14];
const OFFSET: [usize; 4] = [0, 24, 47, 58];
const ROWS: usize = 18;

/// u0–u3 of one 20 ms frame: 12, 12, 11 and 14 bits (op25's `u[0..4]`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AmbeParams(pub [u16; 4]);

/// The codeword vectors c0–c3, first-sent bit highest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AmbeCodeword(pub [u32; 4]);

/// Sent bit `j`'s place in the c0‖c1‖c2‖c3 stream.
pub const fn stream_index(j: usize) -> usize {
    ROWS * (j % 4) + j / 4
}

impl AmbeParams {
    pub const fn encode(self) -> AmbeCodeword {
        let u = self.0;
        AmbeCodeword([
            GOLAY_24.encode(u[0] as u32),
            GOLAY_23.encode(u[1] as u32) ^ pn_masks(u[0])[1],
            u[2] as u32,
            u[3] as u32,
        ])
    }
}

impl AmbeCodeword {
    /// Bit `i` of the c0‖c1‖c2‖c3 stream.
    const fn stream_bit(&self, i: usize) -> bool {
        let mut v = 3;
        while OFFSET[v] > i {
            v -= 1;
        }
        (self.0[v] >> (WIDTH[v] - 1 - (i - OFFSET[v]))) & 1 == 1
    }

    /// Sent bit `j` of the 72.
    pub const fn bit(&self, j: usize) -> bool {
        self.stream_bit(stream_index(j))
    }

    /// From a frame's 36 dibits as sent.
    pub fn from_dibits(dibits: &[u8; 36]) -> Self {
        let mut c = [0u32; 4];
        for j in 0..CODEWORD_BITS {
            let i = stream_index(j);
            let v = (0..4).rposition(|v| OFFSET[v] <= i).unwrap_or(0);
            let bit = u32::from((dibits[j / 2] >> (1 - j % 2)) & 1);
            c[v] |= bit << (WIDTH[v] - 1 - (i - OFFSET[v]));
        }
        Self(c)
    }

    /// The hard decision (BABA-1 §5.3–5.5): c0 as the [24,12] code, then
    /// c1 unmodulated by u0's mask. The parameters and ε0, ε1: the bits each
    /// code corrected, or 4 for c0 when it detects four it cannot correct
    /// (then u0, and so u1, are a guess, and the frame is repeated).
    ///
    /// op25 (`p25p2_vf::process_vcw`) decodes c0 as Golay(23) without its
    /// parity bit, so four errors pass undetected, and counts an
    /// information bit error twice and two parity bit errors once.
    pub const fn decode(self) -> (AmbeParams, [u32; 2]) {
        let c = self.0;
        let d0 = GOLAY_24.decode_hard(c[0]);
        #[allow(clippy::cast_possible_truncation)]
        let d1 = GOLAY_23.decode_hard(c[1] ^ pn_masks(d0.info as u16)[1]);
        #[allow(clippy::cast_possible_truncation)]
        (
            AmbeParams([d0.info as u16, d1.info as u16, c[2] as u16, c[3] as u16]),
            [d0.flips, d1.flips],
        )
    }
}

/// A decoded frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbeFrame {
    pub params: AmbeParams,
    /// Bits corrected in c0 and c1.
    pub errors: u32,
    /// The weaker of the two decisions' margins.
    pub reliability: f32,
    /// The least |LLR| among the unprotected bits (u2, u3): how sure the
    /// least sure of them is.
    pub unprotected: f32,
}

/// Least decision margin (total |LLR|) of a frame worth re-sending.
pub const TRUST_MARGIN: f32 = 6.0;
/// Most corrected bits of a frame worth re-sending.
pub const TRUST_ERRORS: u32 = 5;

impl AmbeFrame {
    /// Whether the decision is sure enough to re-send in place of what was
    /// received. On pure noise 0.5% of frames pass; at a bit LLR noise of
    /// σ = 0.6 (several bit errors a frame) 74% do and 0.03% of those are
    /// wrong (`examples/tdma_noise.rs`).
    pub const fn trusted(&self) -> bool {
        self.reliability >= TRUST_MARGIN && self.errors <= TRUST_ERRORS
    }
}

/// The four vectors' LLRs, de-interleaved.
fn vectors(llr: &[f32; CODEWORD_BITS]) -> [f32; CODEWORD_BITS] {
    let mut s = [0f32; CODEWORD_BITS];
    for (j, &l) in llr.iter().enumerate() {
        s[stream_index(j)] = l;
    }
    s
}

fn hard(llr: &[f32]) -> u32 {
    llr.iter().fold(0, |w, &l| (w << 1) | u32::from(l < 0.0))
}

fn vector(s: &[f32; CODEWORD_BITS], v: usize) -> &[f32] {
    &s[OFFSET[v]..OFFSET[v] + WIDTH[v]]
}

/// c1 given u0: (u1 decision, its cost including c0's).
fn follow(s: &[f32; CODEWORD_BITS], u0: u32) -> (Decision, f32) {
    #[allow(clippy::cast_possible_truncation)]
    let mask = pn_masks(u0 as u16)[1];
    let c1 = vector(s, 1);
    let clear: [f32; 23] = std::array::from_fn(|i| {
        if (mask >> (22 - i)) & 1 == 1 {
            -c1[i]
        } else {
            c1[i]
        }
    });
    let d = GOLAY_23.decode_soft(&clear);
    let cost = GOLAY_24.cost_of(vector(s, 0), u0) + d.cost;
    (d, cost)
}

/// Decode one frame from its 72 bit LLRs (positive favours 0), in sent order.
pub fn decode(llr: &[f32; CODEWORD_BITS]) -> AmbeFrame {
    let s = vectors(llr);
    let c0 = GOLAY_24.decode_soft(vector(&s, 0));
    let mut chosen = (c0.info, follow(&s, c0.info));
    if let Some(alt) = c0.runner_up {
        let f = follow(&s, alt);
        if f.1 < chosen.1.1 {
            chosen = (alt, f);
        }
    }
    let (u0, (d1, _)) = chosen;
    let errors = (GOLAY_24.encode(u0) ^ hard(vector(&s, 0))).count_ones() + d1.flips;
    #[allow(clippy::cast_possible_truncation)]
    AmbeFrame {
        params: AmbeParams([
            u0 as u16,
            d1.info as u16,
            hard(vector(&s, 2)) as u16,
            hard(vector(&s, 3)) as u16,
        ]),
        errors,
        reliability: c0.margin.min(d1.margin),
        unprotected: least(vector(&s, 2)).min(least(vector(&s, 3))),
    }
}

/// The least |LLR| of a vector.
fn least(llr: &[f32]) -> f32 {
    llr.iter().fold(f32::INFINITY, |m, &l| m.min(l.abs()))
}

/// The hard decision ([`AmbeCodeword::decode`]) on the LLRs' signs: the
/// parameters and ε<sub>T</sub>.
pub fn decode_hard(llr: &[f32; CODEWORD_BITS]) -> (AmbeParams, u32) {
    let s = vectors(llr);
    let (params, [e0, e1]) = AmbeCodeword(std::array::from_fn(|v| hard(vector(&s, v)))).decode();
    (params, e0 + e1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::Rng;

    /// op25's `p25p2_vf::encode_vcw`, from a harness linked against op25:
    /// u0–u3 and the 36 dibits.
    #[test]
    fn encoder_matches_op25() {
        let vectors = [
            (
                [2748, 291, 1445, 12047],
                "301332112102232133230212100233111311",
            ),
            (
                [1, 4095, 2047, 16383],
                "130111031111111111010133330303033323",
            ),
            (
                [2273, 1207, 195, 4660],
                "221201032023201203001220313102030000",
            ),
            ([0, 0, 0, 0], "020200000002001000020200100010100000"),
        ];
        for (u, want) in vectors {
            let cw = AmbeParams(u).encode();
            let got: String = (0..36)
                .map(|d| {
                    char::from(b'0' + u8::from(cw.bit(2 * d)) * 2 + u8::from(cw.bit(2 * d + 1)))
                })
                .collect();
            assert_eq!(got, want, "{u:?}");
        }
    }

    fn random_params(rng: &mut Rng) -> AmbeParams {
        #[allow(clippy::cast_possible_truncation)]
        AmbeParams(std::array::from_fn(|i| {
            (rng.next_u64() & ((1 << [12, 12, 11, 14][i]) - 1)) as u16
        }))
    }

    fn llrs(cw: &AmbeCodeword, sigma: f64, rng: &mut Rng) -> [f32; CODEWORD_BITS] {
        #[allow(clippy::cast_possible_truncation)]
        std::array::from_fn(|j| {
            let x = if cw.bit(j) { -1.0 } else { 1.0 };
            sigma.mul_add(rng.gauss(), x) as f32
        })
    }

    #[test]
    fn clean_frames_round_trip() {
        let mut rng = Rng::new(71);
        for _ in 0..300 {
            let p = random_params(&mut rng);
            let llr = llrs(&p.encode(), 0.0, &mut rng);
            let f = decode(&llr);
            assert_eq!((f.params, f.errors), (p, 0));
            assert_eq!(decode_hard(&llr), (p, 0));
        }
    }

    #[test]
    fn dibits_are_the_sent_bits() {
        let mut rng = Rng::new(73);
        for _ in 0..300 {
            let cw = random_params(&mut rng).encode();
            let dibits: [u8; 36] =
                std::array::from_fn(|d| u8::from(cw.bit(2 * d)) * 2 + u8::from(cw.bit(2 * d + 1)));
            assert_eq!(AmbeCodeword::from_dibits(&dibits), cw);
        }
    }

    /// ε0 and ε1 are the bits corrected, c0's parity bit included.
    #[test]
    fn the_errors_are_the_bits_corrected() {
        let p = AmbeParams([2748, 291, 1445, 12047]);
        let mut cw = p.encode();
        cw.0[0] ^= 1;
        assert_eq!(cw.decode(), (p, [1, 0]));
        // Two more in c0's parity, one in c1's information bits.
        cw.0[0] ^= 0b110;
        cw.0[1] ^= 1 << 22;
        assert_eq!(cw.decode(), (p, [3, 1]));
        // And two more in c1.
        cw.0[1] ^= 0b11 << 20;
        assert_eq!(cw.decode(), (p, [3, 3]));
    }

    /// Four errors in c0 are detected, whichever bits they fall on (BABA-1
    /// §5.6 then repeats the frame), where op25 corrects three of them
    /// into a wrong u0 when the fourth is the parity bit.
    #[test]
    fn four_errors_in_c0_are_detected() {
        let mut rng = Rng::new(74);
        for _ in 0..300 {
            let p = random_params(&mut rng);
            let mut cw = p.encode();
            let mut flipped = 0;
            while flipped < 4 {
                let bit = 1 << (rng.next_u64() % 24);
                if cw.0[0] & bit == p.encode().0[0] & bit {
                    cw.0[0] ^= bit;
                    flipped += 1;
                }
            }
            assert_eq!(cw.decode().1[0], 4);
        }
    }

    #[test]
    fn soft_joint_decoding_recovers_more_frames_than_hard() {
        let mut rng = Rng::new(72);
        let (mut soft, mut hard) = (0, 0);
        let trials = 3000;
        for _ in 0..trials {
            let p = random_params(&mut rng);
            let llr = llrs(&p.encode(), 0.65, &mut rng);
            soft += usize::from(decode(&llr).params.0[..2] == p.0[..2]);
            hard += usize::from(decode_hard(&llr).0.0[..2] == p.0[..2]);
        }
        assert!(soft > hard + trials / 20, "soft {soft} hard {hard}");
    }
}
