//! IMBE voice frames: 88 parameter bits in a 144-bit FEC codeword
//! (TIA-102.BABA).
//!
//! The parameters are eight vectors: u0–u3 of 12 bits, each in a Golay(23,12)
//! codeword; u4–u6 of 11 bits, each in a Hamming(15,11) codeword; u7's 7 bits
//! unprotected. Codewords c1–c6 are scrambled with a pseudo-random mask
//! seeded by u0, so an error in u0 garbles the rest of the frame.
//!
//! op25 decodes each vector on hard bits, u0 first. Here every vector is
//! soft-decoded, and u0 is chosen jointly: both the best and the runner-up
//! u0 are tried, and the one whose descrambled c1–c6 decode most cheaply
//! wins.

use crate::fec::{Decision, GOLAY_23, HAMMING_15};

pub const CODEWORD_BITS: usize = 144;

/// Width of each vector's codeword, and where it starts in the 144 bits.
const CODE_BITS: [usize; 8] = [23, 23, 23, 23, 15, 15, 15, 7];
const OFFSET: [usize; 8] = [0, 23, 46, 69, 92, 107, 122, 137];

/// u0–u7 of one 20 ms frame: 12, 12, 12, 12, 11, 11, 11 and 7 bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ImbeParams(pub [u16; 8]);

/// The 144-bit codeword, one word per vector (c0…c7), first-sent bit high.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ImbeCodeword(pub [u32; 8]);

/// The scrambling masks of c1–c6 for a given u0 (index 0 and 7 are zero):
/// a 16-bit congruential generator p ← 173p + 13849 seeded with 16·u0,
/// whose top bit, 23 or 15 times over, is each mask.
pub const fn pn_masks(u0: u16) -> [u32; 8] {
    let mut masks = [0u32; 8];
    let mut p = (u0 as u32) << 4;
    let mut v = 1;
    while v <= 6 {
        let mut m = 0;
        let mut i = 0;
        while i < CODE_BITS[v] {
            p = (173 * p + 13849) & 0xffff;
            m = (m << 1) | ((p >> 15) & 1);
            i += 1;
        }
        masks[v] = m;
        v += 1;
    }
    masks
}

impl ImbeParams {
    /// From op25's `u[0..8]`, whose u7 is shifted left one bit.
    pub const fn from_op25(u: [u32; 8]) -> Self {
        let mut p = [0u16; 8];
        let mut i = 0;
        while i < 8 {
            #[allow(clippy::cast_possible_truncation)]
            {
                p[i] = if i == 7 {
                    (u[i] >> 1) as u16
                } else {
                    u[i] as u16
                };
            }
            i += 1;
        }
        Self(p)
    }

    /// op25's `u[0..8]`, as its vocoder takes them (u7 shifted left).
    pub const fn to_op25(self) -> [u32; 8] {
        let mut u = [0u32; 8];
        let mut i = 0;
        while i < 8 {
            u[i] = self.0[i] as u32;
            i += 1;
        }
        u[7] <<= 1;
        u
    }

    pub const fn encode(self) -> ImbeCodeword {
        let masks = pn_masks(self.0[0]);
        let mut c = [0u32; 8];
        let mut i = 0;
        while i < 4 {
            c[i] = GOLAY_23.encode(self.0[i] as u32) ^ masks[i];
            i += 1;
        }
        while i < 7 {
            c[i] = HAMMING_15.encode(self.0[i] as u32) ^ masks[i];
            i += 1;
        }
        c[7] = self.0[7] as u32;
        ImbeCodeword(c)
    }
}

impl ImbeCodeword {
    /// Bit `j` of the 144, in transmission order.
    pub const fn bit(&self, j: usize) -> bool {
        let mut v = 7;
        while OFFSET[v] > j {
            v -= 1;
        }
        (self.0[v] >> (CODE_BITS[v] - 1 - (j - OFFSET[v]))) & 1 == 1
    }
}

/// A decoded frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImbeFrame {
    pub params: ImbeParams,
    /// Bits corrected in c0–c6 (op25's per-frame error count).
    pub errors: u32,
    /// Bits corrected in c0 alone.
    pub errors_c0: u32,
    /// The weakest vector decision's margin: low means the frame is suspect.
    pub reliability: f32,
    /// The least |LLR| among the unprotected bits (u7).
    pub unprotected: f32,
}

/// One u0 hypothesis carried through c1–c6.
struct Hypothesis {
    params: ImbeParams,
    cost: f32,
    errors: u32,
    reliability: f32,
}

fn vector(llr: &[f32; CODEWORD_BITS], v: usize) -> &[f32] {
    &llr[OFFSET[v]..OFFSET[v] + CODE_BITS[v]]
}

fn descrambled(llr: &[f32], mask: u32) -> [f32; 23] {
    let n = llr.len();
    let mut out = [0f32; 23];
    for (i, (o, &l)) in out.iter_mut().zip(llr).enumerate() {
        *o = if (mask >> (n - 1 - i)) & 1 == 1 {
            -l
        } else {
            l
        };
    }
    out
}

fn follow(llr: &[f32; CODEWORD_BITS], u0: u16, c0: &Decision) -> Hypothesis {
    let masks = pn_masks(u0);
    let mut params = [0u16; 8];
    params[0] = u0;
    let flips0 = (GOLAY_23.encode(u32::from(u0)) ^ hard(vector(llr, 0))).count_ones();
    let (mut cost, mut errors) = (GOLAY_23.cost_of(vector(llr, 0), u32::from(u0)), flips0);
    let mut reliability = c0.margin;
    for v in 1..7 {
        let bits = vector(llr, v);
        let clear = descrambled(bits, masks[v]);
        let clear = &clear[..bits.len()];
        let d = if v < 4 {
            GOLAY_23.decode_soft(clear)
        } else {
            HAMMING_15.decode_soft(clear)
        };
        #[allow(clippy::cast_possible_truncation)]
        {
            params[v] = d.info as u16;
        }
        cost += d.cost;
        errors += d.flips;
        reliability = reliability.min(d.margin);
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        params[7] = hard(vector(llr, 7)) as u16;
    }
    Hypothesis {
        params: ImbeParams(params),
        cost,
        errors,
        reliability,
    }
}

fn hard(llr: &[f32]) -> u32 {
    llr.iter().fold(0, |w, &l| (w << 1) | u32::from(l < 0.0))
}

/// Decode one frame from its 144 bit LLRs (positive favours 0).
pub fn decode(llr: &[f32; CODEWORD_BITS]) -> ImbeFrame {
    let c0 = GOLAY_23.decode_soft(vector(llr, 0));
    #[allow(clippy::cast_possible_truncation)]
    let best = follow(llr, c0.info as u16, &c0);
    #[allow(clippy::cast_possible_truncation)]
    let chosen = match c0.runner_up.map(|u| follow(llr, u as u16, &c0)) {
        Some(alt) if alt.cost < best.cost => alt,
        _ => best,
    };
    let errors_c0 =
        (GOLAY_23.encode(u32::from(chosen.params.0[0])) ^ hard(vector(llr, 0))).count_ones();
    ImbeFrame {
        params: chosen.params,
        errors: chosen.errors,
        errors_c0,
        reliability: chosen.reliability,
        unprotected: vector(llr, 7)
            .iter()
            .fold(f32::INFINITY, |m, &l| m.min(l.abs())),
    }
}

/// op25's decoder (`imbe_header_decode`): hard decisions, u0 first.
pub fn decode_hard(llr: &[f32; CODEWORD_BITS]) -> ImbeParams {
    #[allow(clippy::cast_possible_truncation)]
    let u0 = GOLAY_23.decode_hard(hard(vector(llr, 0))).info as u16;
    let masks = pn_masks(u0);
    let mut p = [0u16; 8];
    p[0] = u0;
    for (v, slot) in p.iter_mut().enumerate().take(7).skip(1) {
        let w = hard(vector(llr, v)) ^ masks[v];
        let d = if v < 4 {
            GOLAY_23.decode_hard(w)
        } else {
            HAMMING_15.decode_hard(w)
        };
        #[allow(clippy::cast_possible_truncation)]
        {
            *slot = d.info as u16;
        }
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        p[7] = hard(vector(llr, 7)) as u16;
    }
    ImbeParams(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::Rng;

    fn bits(s: &str) -> Vec<bool> {
        s.bytes().map(|b| b == b'1').collect()
    }

    /// op25's `imbe_header_encode`, from a harness linked against op25.
    #[test]
    fn encoder_matches_op25() {
        let vectors = [
            (
                [3762, 3894, 873, 153, 283, 1222, 1512, 78],
                "111010110010101001101001101001001111100011001110011000111101001001100110101010000001111111110000101110001100010100110010101100010100011110100111",
            ),
            (
                [4028, 2304, 3886, 212, 1378, 1216, 1254, 188],
                "111110111100001110110011100111011111011000101001101001110000101101011111000000011111110101111100000110010100110100010100101001001011001001011110",
            ),
            (
                [3480, 674, 2942, 1599, 171, 769, 544, 102],
                "110110011000000010000000111111111000011101010100010011101100101011111011000001111000110010111000001000000000011010110100000010101010111100110011",
            ),
            (
                [4007, 4090, 3769, 1976, 88, 361, 501, 16],
                "111110100111110011000111111011111001101010011100101011001010000101011001000101110111001101010001111110000111000111010000110110010011100110001000",
            ),
        ];
        for (u, want) in vectors {
            let cw = ImbeParams::from_op25(u).encode();
            let got: Vec<bool> = (0..CODEWORD_BITS).map(|j| cw.bit(j)).collect();
            assert_eq!(got, bits(want), "{u:?}");
        }
    }

    #[test]
    fn pn_matches_op25() {
        // op25 pngen23 twice from seed 0x123 << 4 (the full encoder vectors
        // above cover the rest of the sequence).
        let m = pn_masks(0x123);
        assert_eq!((m[1], m[2]), (6_237_377, 2_353_311));
    }

    fn random_params(rng: &mut Rng) -> ImbeParams {
        let widths = [12, 12, 12, 12, 11, 11, 11, 7];
        #[allow(clippy::cast_possible_truncation)]
        ImbeParams(std::array::from_fn(|i| {
            (rng.next_u64() & ((1 << widths[i]) - 1)) as u16
        }))
    }

    fn llrs(cw: &ImbeCodeword, sigma: f64, rng: &mut Rng) -> [f32; CODEWORD_BITS] {
        #[allow(clippy::cast_possible_truncation)]
        std::array::from_fn(|j| {
            let x = if cw.bit(j) { -1.0 } else { 1.0 };
            sigma.mul_add(rng.gauss(), x) as f32
        })
    }

    #[test]
    fn clean_frames_round_trip_both_ways() {
        let mut rng = Rng::new(61);
        for _ in 0..200 {
            let p = random_params(&mut rng);
            let llr = llrs(&p.encode(), 0.0, &mut rng);
            let f = decode(&llr);
            assert_eq!((f.params, f.errors), (p, 0));
            assert_eq!(decode_hard(&llr), p);
            assert_eq!(ImbeParams::from_op25(p.to_op25()), p);
        }
    }

    #[test]
    fn soft_joint_decoding_recovers_more_frames_than_op25() {
        let mut rng = Rng::new(62);
        let (mut soft, mut op25) = (0, 0);
        let trials = 2000;
        for _ in 0..trials {
            let p = random_params(&mut rng);
            let llr = llrs(&p.encode(), 0.65, &mut rng);
            // u7 is unprotected; compare the protected vectors.
            soft += usize::from(decode(&llr).params.0[..7] == p.0[..7]);
            op25 += usize::from(decode_hard(&llr).0[..7] == p.0[..7]);
        }
        assert!(soft > op25 + trials / 20, "soft {soft} op25 {op25}");
    }

    #[test]
    fn reliability_flags_bad_frames() {
        let mut rng = Rng::new(63);
        let (mut good, mut bad) = (Vec::new(), Vec::new());
        for _ in 0..2000 {
            let p = random_params(&mut rng);
            let f = decode(&llrs(&p.encode(), 0.8, &mut rng));
            if f.params.0[..7] == p.0[..7] {
                good.push(f.reliability);
            } else {
                bad.push(f.reliability);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let median = |v: &mut Vec<f32>| {
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        assert!(
            median(&mut good) > median(&mut bad),
            "good {} bad {}",
            median(&mut good),
            median(&mut bad)
        );
    }
}
