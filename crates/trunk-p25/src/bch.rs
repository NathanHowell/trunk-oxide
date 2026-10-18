//! BCH(63,16,23) code protecting the NID, over GF(2^6) with x^6 + x + 1.
//!
//! A codeword is a `u64` whose bit `i` is the coefficient of `x^i`; bit 62 is
//! transmitted first. The 16 information bits (NAC then DUID) occupy bits
//! 62..47, the 47 parity bits 46..0.

use crate::gf64::{self, Gf64};

pub const N: u32 = 63;
pub const K: u32 = 16;
/// Designed error-correcting capability.
pub const T: usize = 11;

/// Generator polynomial: ∏ (x − αʲ) over every j in the cyclotomic cosets of
/// 1 … 2T, i.e. the product of their minimal polynomials. As a bitmask
/// (degree 47, so bit 47 is set).
pub const GENERATOR: u64 = {
    let mut covered = [false; 63];
    // GF(64) coefficients, lowest degree first.
    let mut g = [Gf64::ZERO; 64];
    g[0] = Gf64::ONE;
    let mut degree = 0;
    let mut i = 1;
    while i <= 2 * T {
        let mut j = i;
        while !covered[j] {
            covered[j] = true;
            // g ← g · (x + αʲ)
            let root = Gf64::alpha(j);
            let mut d = degree + 1;
            while d > 0 {
                g[d] = g[d - 1].plus(g[d].times(root));
                d -= 1;
            }
            g[0] = g[0].times(root);
            degree += 1;
            j = (j * 2) % 63;
        }
        i += 1;
    }
    let mut mask = 0u64;
    let mut d = 0;
    while d <= degree {
        assert!(g[d].get() <= 1, "generator must be binary");
        mask |= (g[d].get() as u64) << d;
        d += 1;
    }
    mask
};

/// Systematic encode of 16 information bits.
pub const fn encode(info: u16) -> u64 {
    let shifted = (info as u64) << (N - K);
    let mut r = shifted;
    let mut bit = N - 1;
    while bit >= N - K {
        if (r >> bit) & 1 == 1 {
            r ^= GENERATOR << (bit - (N - K));
        }
        bit -= 1;
    }
    shifted | r
}

pub const fn info(codeword: u64) -> u16 {
    #[allow(clippy::cast_possible_truncation)]
    let v = ((codeword >> (N - K)) & 0xffff) as u16;
    v
}

/// Hard-decision decode (Berlekamp–Massey + Chien search). Returns the
/// corrected codeword and the number of bits corrected, or `None` when the
/// word is more than [`T`] errors from any codeword.
#[allow(clippy::many_single_char_names)] // textbook Berlekamp–Massey names
pub fn decode(word: u64) -> Option<(u64, u32)> {
    let word = word & ((1u64 << N) - 1);
    let mut syn = [Gf64::ZERO; 2 * T];
    let mut any = false;
    for (j, s) in syn.iter_mut().enumerate() {
        let mut acc = Gf64::ZERO;
        let mut w = word;
        while w != 0 {
            let i = w.trailing_zeros() as usize;
            acc += Gf64::alpha(i * (j + 1));
            w &= w - 1;
        }
        *s = acc;
        any |= !acc.is_zero();
    }
    if !any {
        return Some((word, 0));
    }

    // Berlekamp–Massey.
    let mut c = [Gf64::ZERO; 2 * T + 1];
    let mut b = [Gf64::ZERO; 2 * T + 1];
    c[0] = Gf64::ONE;
    b[0] = Gf64::ONE;
    let mut l = 0usize;
    let mut m = 1usize;
    let mut bb = Gf64::ONE;
    for n in 0..2 * T {
        let mut d = syn[n];
        for i in 1..=l {
            d += c[i] * syn[n - i];
        }
        if d.is_zero() {
            m += 1;
            continue;
        }
        let coef = d / bb;
        let prev = c;
        for i in m..=2 * T {
            c[i] += coef * b[i - m];
        }
        if 2 * l <= n {
            l = n + 1 - l;
            b = prev;
            bb = d;
            m = 1;
        } else {
            m += 1;
        }
    }
    if l > T {
        return None;
    }

    // Chien search: error at position i when Λ(α^-i) = 0.
    let mut fixed = word;
    let mut found = 0usize;
    for i in 0..N as usize {
        if gf64::eval(&c[..=l], Gf64::alpha(63 - i)).is_zero() {
            fixed ^= 1u64 << i;
            found += 1;
        }
    }
    if found != l {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some((fixed, found as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::Rng;

    #[test]
    fn generator_matches_op25_table() {
        // op25 bch.cc bchG, highest degree first.
        const BCH_G: [u8; 48] = [
            1, 1, 0, 1, 0, 1, 0, 0, 1, 1, 0, 1, 1, 1, 0, 0, 1, 0, 1, 1, 1, 0, 1, 1, 1, 1, 0, 1, 0,
            0, 0, 0, 1, 1, 0, 0, 1, 0, 0, 1, 1, 0, 1, 1, 0, 0, 1, 1,
        ];
        let g = GENERATOR;
        assert_eq!(g.ilog2(), 47);
        let ours: Vec<u8> = (0..48).rev().map(|d| u8::from((g >> d) & 1 == 1)).collect();
        let reversed: Vec<u8> = BCH_G.iter().rev().copied().collect();
        assert!(ours == BCH_G || ours == reversed, "{ours:?}");
    }

    #[test]
    fn codewords_decode_cleanly() {
        for info in [0u16, 1, 0x38d7, 0xffff, 0x2935] {
            let cw = encode(info);
            assert_eq!(decode(cw), Some((cw, 0)));
            assert_eq!(super::info(cw), info);
        }
    }

    #[test]
    fn corrects_up_to_eleven_errors() {
        let mut rng = Rng::new(7);
        for trial in 0..400 {
            #[allow(clippy::cast_possible_truncation)]
            let info = rng.next_u64() as u16;
            let cw = encode(info);
            let nerr = trial % (T + 1);
            let mut word = cw;
            let mut flipped = 0;
            while flipped < nerr {
                let bit = rng.below(63);
                if (word ^ cw) >> bit & 1 == 0 {
                    word ^= 1 << bit;
                    flipped += 1;
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            let expect = Some((cw, nerr as u32));
            assert_eq!(decode(word), expect, "trial {trial}, {nerr} errors");
        }
    }

    #[test]
    fn far_words_are_rejected_or_land_on_a_codeword() {
        let mut rng = Rng::new(9);
        for _ in 0..200 {
            let word = rng.next_u64() & ((1 << 63) - 1);
            if let Some((cw, n)) = decode(word) {
                assert!(n as usize <= T);
                assert_eq!(decode(cw), Some((cw, 0)));
                assert_eq!((cw ^ word).count_ones(), n);
            }
        }
    }
}
