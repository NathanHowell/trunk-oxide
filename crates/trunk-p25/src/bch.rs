//! BCH(63,16,23) code protecting the NID, over GF(2^6) with x^6 + x + 1.
//!
//! A codeword is a `u64` whose bit `i` is the coefficient of `x^i`; bit 62 is
//! transmitted first. The 16 information bits (NAC then DUID) occupy bits
//! 62..47, the 47 parity bits 46..0.

use std::sync::LazyLock;

pub const N: u32 = 63;
pub const K: u32 = 16;
/// Designed error-correcting capability.
pub const T: usize = 11;

struct Gf {
    exp: [u8; 126],
    log: [u8; 64],
}

static GF: LazyLock<Gf> = LazyLock::new(|| {
    let mut exp = [0u8; 126];
    let mut log = [0u8; 64];
    let mut x: u8 = 1;
    for i in 0..63 {
        exp[i] = x;
        exp[i + 63] = x;
        #[allow(clippy::cast_possible_truncation)]
        {
            log[usize::from(x)] = i as u8;
        }
        x <<= 1;
        if x & 0x40 != 0 {
            x ^= 0x43; // x^6 = x + 1
        }
    }
    Gf { exp, log }
});

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let gf = &*GF;
    gf.exp[usize::from(gf.log[usize::from(a)]) + usize::from(gf.log[usize::from(b)])]
}

fn div(a: u8, b: u8) -> u8 {
    debug_assert!(b != 0);
    if a == 0 {
        return 0;
    }
    let gf = &*GF;
    gf.exp[usize::from(gf.log[usize::from(a)]) + 63 - usize::from(gf.log[usize::from(b)])]
}

fn alpha_pow(e: usize) -> u8 {
    GF.exp[e % 63]
}

/// Generator polynomial: the product of the minimal polynomials of
/// α^1 … α^22, as a bitmask (degree 47, so bit 47 is set).
pub static GENERATOR: LazyLock<u64> = LazyLock::new(|| {
    let mut covered = [false; 63];
    // Polynomial with GF(64) coefficients, lowest degree first.
    let mut g: Vec<u8> = vec![1];
    for i in 1..=2 * T {
        if covered[i] {
            continue;
        }
        let mut j = i;
        loop {
            covered[j] = true;
            // g *= (x + α^j)
            let root = alpha_pow(j);
            let mut next = vec![0u8; g.len() + 1];
            for (d, &c) in g.iter().enumerate() {
                next[d + 1] ^= c;
                next[d] ^= mul(c, root);
            }
            g = next;
            j = (j * 2) % 63;
            if j == i {
                break;
            }
        }
    }
    g.iter().enumerate().fold(0u64, |acc, (d, &c)| {
        assert!(c <= 1, "generator must be binary");
        acc | (u64::from(c) << d)
    })
});

/// Systematic encode of 16 information bits.
pub fn encode(info: u16) -> u64 {
    let g = *GENERATOR;
    let shifted = u64::from(info) << (N - K);
    let mut r = shifted;
    for bit in (N - K..N).rev() {
        if (r >> bit) & 1 == 1 {
            r ^= g << (bit - (N - K));
        }
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
    let mut syn = [0u8; 2 * T];
    let mut any = false;
    for (j, s) in syn.iter_mut().enumerate() {
        let mut acc = 0u8;
        let mut w = word;
        while w != 0 {
            let i = w.trailing_zeros() as usize;
            acc ^= alpha_pow(i * (j + 1));
            w &= w - 1;
        }
        *s = acc;
        any |= acc != 0;
    }
    if !any {
        return Some((word, 0));
    }

    // Berlekamp–Massey.
    let mut c = [0u8; 2 * T + 1];
    let mut b = [0u8; 2 * T + 1];
    c[0] = 1;
    b[0] = 1;
    let mut l = 0usize;
    let mut m = 1usize;
    let mut bb = 1u8;
    for n in 0..2 * T {
        let mut d = syn[n];
        for i in 1..=l {
            d ^= mul(c[i], syn[n - i]);
        }
        if d == 0 {
            m += 1;
            continue;
        }
        let coef = div(d, bb);
        let prev = c;
        for i in m..=2 * T {
            c[i] ^= mul(coef, b[i - m]);
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
        let inv = (63 - i) % 63;
        let mut v = 0u8;
        for (k, &ck) in c.iter().enumerate().take(l + 1) {
            v ^= mul(ck, alpha_pow(inv * k));
        }
        if v == 0 {
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
        let g = *GENERATOR;
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
