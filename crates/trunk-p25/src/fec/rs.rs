//! Reed–Solomon codes over GF(64), shortened from length 63.
//!
//! P25 uses RS(24,12,13) for link control, RS(24,16,9) for encryption sync and
//! RS(36,20,17) for the header. The generator is g(x) = ∏_{i=1}^{n−k} (x − αⁱ);
//! symbol `j` of a word is the coefficient of x^(n−1−j), so data come first
//! and parity last (as op25's ezpwd codecs lay them out).
//!
//! Decoding corrects errors and erasures together: e errors and f erasures
//! are recoverable while 2e + f ≤ n − k. Erasures come from the inner codes:
//! a hexbit whose Hamming or Golay decision was close is better erased than
//! guessed (see [`ReedSolomon::decode_gmd`]).

use crate::gf64::{Gf64, Poly};

/// An (N, K) Reed–Solomon code; N − K ≤ 63.
#[derive(Clone, Debug)]
pub struct ReedSolomon<const N: usize, const K: usize> {
    /// g(x), lowest degree first; degree N − K.
    generator: [Gf64; 64],
}

/// Link control: RS(24,12,13).
pub const RS_24_12: ReedSolomon<24, 12> = ReedSolomon::new();
/// Encryption sync: RS(24,16,9).
pub const RS_24_16: ReedSolomon<24, 16> = ReedSolomon::new();
/// Header data unit: RS(36,20,17).
pub const RS_36_20: ReedSolomon<36, 20> = ReedSolomon::new();

/// Why a word did not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RsFailure {
    /// More erasures than parity symbols.
    TooManyErasures,
    /// The errata locator has no consistent set of roots in the word.
    Uncorrectable,
}

impl<const N: usize, const K: usize> Default for ReedSolomon<N, K> {
    fn default() -> Self {
        Self::new()
    }
}

/// The generator's first consecutive root, α¹: g(x) = ∏ (x − αⁱ) for
/// i = 1 … N − K (BBAC Annex A eq. 13; BAAA).
pub const FIRST_ROOT: usize = 1;

impl<const N: usize, const K: usize> ReedSolomon<N, K> {
    const R: usize = N - K;

    pub const fn new() -> Self {
        assert!(K < N && N <= 63);
        let mut g = [Gf64::ZERO; 64];
        g[0] = Gf64::ONE;
        let mut i = 1;
        while i <= Self::R {
            // g ← g · (x + α^(FIRST_ROOT + i − 1))
            let root = Gf64::alpha(FIRST_ROOT + i - 1);
            let mut d = i;
            while d > 0 {
                g[d] = g[d - 1].plus(g[d].times(root));
                d -= 1;
            }
            g[0] = g[0].times(root);
            i += 1;
        }
        Self { generator: g }
    }

    pub const fn parity(&self) -> usize {
        Self::R
    }

    /// Position `j`'s locator, X = α^(N−1−j).
    const fn locator_of(j: usize) -> Gf64 {
        Gf64::alpha(N - 1 - j)
    }

    fn polynomial(word: &[Gf64]) -> Poly {
        word.iter().rev().copied().collect()
    }

    /// Systematic encode: the K data symbols, then N − K parity symbols (the
    /// remainder of d(x)·x^(N−K) by g(x), computed by the usual LFSR).
    pub const fn encode(&self, data: &[Gf64; K]) -> [Gf64; N] {
        let mut word = [Gf64::ZERO; N];
        // parity register: word[K..N], highest degree first
        let mut i = 0;
        while i < K {
            word[i] = data[i];
            let feedback = data[i].plus(word[K]);
            let mut j = 0;
            while j < Self::R {
                let next = if j + 1 < Self::R {
                    word[K + j + 1]
                } else {
                    Gf64::ZERO
                };
                word[K + j] = next.plus(feedback.times(self.generator[Self::R - 1 - j]));
                j += 1;
            }
            i += 1;
        }
        word
    }

    /// S(x) = Σ c(α^(`FIRST_ROOT` + i)) xⁱ; zero for a codeword.
    fn syndromes(word: &[Gf64]) -> Poly {
        let c = Self::polynomial(word);
        (FIRST_ROOT..FIRST_ROOT + Self::R)
            .map(|i| c.eval(Gf64::alpha(i)))
            .collect()
    }

    pub fn is_codeword(&self, word: &[Gf64; N]) -> bool {
        Self::syndromes(word) == Poly::default()
    }

    /// Correct `word` in place, treating the positions in `erasures` as
    /// unknown. Returns the number of symbols changed.
    #[allow(clippy::many_single_char_names)] // the textbook's Λ, B, L, S, r
    pub fn decode(&self, word: &mut [Gf64; N], erasures: &[usize]) -> Result<usize, RsFailure> {
        let r = Self::R;
        if erasures.len() > r {
            return Err(RsFailure::TooManyErasures);
        }
        let s = Self::syndromes(word);
        if s == Poly::default() {
            return Ok(0);
        }

        // Berlekamp–Massey, seeded with the erasure locator Γ(x).
        let gamma = erasures.iter().fold(Poly::one(), |g, &j| {
            &g * &Poly::locator(Self::locator_of(j))
        });
        let f = erasures.len();
        let mut lambda = gamma.clone();
        let mut b = gamma;
        let mut l = f;
        for step in f..r {
            // Δ = [x^step] Λ(x)S(x)
            let delta = (0..=l.min(step))
                .fold(Gf64::ZERO, |acc, i| acc + lambda.coef(i) * s.coef(step - i));
            if delta.is_zero() {
                b = b.shift(1);
                continue;
            }
            let next = &lambda + &b.shift(1).scale(delta);
            if 2 * l <= step + f {
                l = step + f + 1 - l;
                b = lambda.scale(delta.inv());
            } else {
                b = b.shift(1);
            }
            lambda = next;
        }
        let degree = lambda.degree().unwrap_or(0);
        if degree != l || 2 * (l - f) + f > r {
            return Err(RsFailure::Uncorrectable);
        }

        // Chien search over the (shortened) positions, then Forney.
        let omega = (&s * &lambda).truncate(r);
        let dlambda = lambda.derivative();
        let mut changed = 0;
        let mut roots = 0;
        for (j, symbol) in word.iter_mut().enumerate() {
            let x_inv = Self::locator_of(j).inv();
            if !lambda.eval(x_inv).is_zero() {
                continue;
            }
            roots += 1;
            let denominator = dlambda.eval(x_inv);
            if denominator.is_zero() {
                return Err(RsFailure::Uncorrectable);
            }
            let magnitude = omega.eval(x_inv) / denominator;
            if !magnitude.is_zero() {
                *symbol += magnitude;
                changed += 1;
            }
        }
        if roots != degree || !self.is_codeword(word) {
            return Err(RsFailure::Uncorrectable);
        }
        Ok(changed)
    }

    /// Generalised minimum-distance decoding (Forney). `reliability[j]` is how
    /// sure the inner decoder was of symbol j (any non-negative scale). The
    /// least reliable symbols are erased two at a time, 0 … N − K of them;
    /// each successful decode is a candidate, and the candidate closest in
    /// generalised distance wins, provided that distance is below `d_min` —
    /// which makes it the only codeword that close, however many erasures it
    /// took to find.
    pub fn decode_gmd(&self, word: &mut [Gf64; N], reliability: &[f32; N]) -> Option<Decoded> {
        let alpha = normalised(reliability);
        let mut order: [usize; N] = std::array::from_fn(|j| j);
        order.sort_by(|&a, &b| alpha[a].total_cmp(&alpha[b]));
        let mut best: Option<([Gf64; N], Decoded)> = None;
        for erased in (0..=Self::R).step_by(2) {
            let mut trial = *word;
            if let Ok(changed) = self.decode(&mut trial, &order[..erased]) {
                let distance = generalised_distance(word, &trial, &alpha);
                #[allow(clippy::cast_precision_loss)] // R ≤ 62
                let d_min = (Self::R + 1) as f32;
                if distance < d_min && best.as_ref().is_none_or(|(_, b)| distance < b.distance) {
                    best = Some((
                        trial,
                        Decoded {
                            changed,
                            erased,
                            distance,
                        },
                    ));
                }
            }
        }
        let (decoded, d) = best?;
        *word = decoded;
        Some(d)
    }
}

/// Reliabilities scaled to [0, 1] against the word's median, so a symbol is
/// fully trusted when it is at least as sure as a typical one.
fn normalised<const N: usize>(reliability: &[f32; N]) -> [f32; N] {
    let mut sorted = *reliability;
    sorted.sort_by(f32::total_cmp);
    let median = sorted[N / 2].max(f32::MIN_POSITIVE);
    reliability.map(|r| (r / median).clamp(0.0, 1.0))
}

/// Forney's generalised distance Σ (1 − αⱼsⱼ), sⱼ = +1 where the codeword
/// keeps symbol j and −1 where it changes it: a kept symbol costs 1 − α, a
/// changed one 1 + α. With every α = 1 (hard decisions) it is twice the
/// Hamming distance, and "< `d_min`" is bounded-distance decoding.
fn generalised_distance<const N: usize>(
    received: &[Gf64; N],
    codeword: &[Gf64; N],
    alpha: &[f32; N],
) -> f32 {
    received
        .iter()
        .zip(codeword)
        .zip(alpha)
        .map(|((r, c), a)| if r == c { 1.0 - a } else { 1.0 + a })
        .sum()
}

/// What [`ReedSolomon::decode_gmd`] did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decoded {
    pub changed: usize,
    pub erased: usize,
    /// Generalised distance from the received word (< `d_min`).
    pub distance: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::Rng;

    fn hex<const M: usize>(v: &[u8]) -> [Gf64; M] {
        std::array::from_fn(|i| Gf64::new(v[i]))
    }

    /// Parity from op25's ezpwd codecs (`RS<63,55>` etc., data in the last n
    /// symbols of the 63), generated by a harness linked against op25.
    #[test]
    fn parity_matches_op25() {
        let w = [
            10, 42, 63, 32, 8, 24, 4, 60, 31, 16, 48, 23, 27, 46, 47, 46, 35, 15, 9, 21, 36, 61, 0,
            6,
        ];
        assert_eq!(RS_24_16.encode(&hex(&w)), hex::<24>(&w));
        let w = [
            39, 42, 11, 44, 38, 5, 57, 61, 14, 58, 30, 30, 35, 37, 15, 56, 8, 20, 8, 52, 15, 22,
            17, 0,
        ];
        assert_eq!(RS_24_12.encode(&hex(&w)), hex::<24>(&w));
        let w = [
            11, 47, 13, 42, 42, 34, 24, 56, 28, 43, 38, 51, 48, 37, 34, 21, 48, 46, 57, 26, 32, 23,
            63, 53, 16, 57, 15, 14, 44, 54, 20, 7, 36, 26, 32, 48,
        ];
        assert_eq!(RS_36_20.encode(&hex(&w)), hex::<36>(&w));
        assert!(RS_36_20.is_codeword(&hex(&w)));
    }

    #[test]
    fn encoding_is_usable_at_compile_time() {
        const W: [Gf64; 24] = RS_24_12.encode(&[Gf64::ONE; 12]);
        assert!(RS_24_12.is_codeword(&W));
    }

    fn random_word<const N: usize, const K: usize>(
        rs: &ReedSolomon<N, K>,
        rng: &mut Rng,
    ) -> [Gf64; N] {
        #[allow(clippy::cast_possible_truncation)]
        let data: [Gf64; K] = std::array::from_fn(|_| Gf64::new(rng.next_u64() as u8));
        rs.encode(&data)
    }

    fn corrupt(word: &mut [Gf64], rng: &mut Rng, positions: &[usize]) {
        for &p in positions {
            #[allow(clippy::cast_possible_truncation)]
            let e = Gf64::new((rng.next_u64() % 63 + 1) as u8);
            word[p] += e;
        }
    }

    fn distinct(rng: &mut Rng, n: usize, count: usize) -> Vec<usize> {
        let mut v: Vec<usize> = (0..n).collect();
        for i in 0..count {
            let j = i + rng.below(n - i);
            v.swap(i, j);
        }
        v.truncate(count);
        v
    }

    fn every_errata_pattern<const N: usize, const K: usize>(rs: &ReedSolomon<N, K>, rng: &mut Rng) {
        let r = N - K;
        for f in 0..=r {
            for e in 0..=(r - f) / 2 {
                let clean = random_word(rs, rng);
                let mut word = clean;
                let pos = distinct(rng, N, e + f);
                corrupt(&mut word, rng, &pos);
                assert_eq!(
                    rs.decode(&mut word, &pos[e..]),
                    Ok(e + f),
                    "RS({N},{K}) e={e} f={f}"
                );
                assert_eq!(word, clean);
            }
        }
    }

    #[test]
    fn corrects_every_errata_pattern_within_the_bound() {
        let mut rng = Rng::new(41);
        every_errata_pattern(&RS_24_12, &mut rng);
        every_errata_pattern(&RS_24_16, &mut rng);
        every_errata_pattern(&RS_36_20, &mut rng);
    }

    #[test]
    fn beyond_the_bound_is_reported_not_miscorrected_silently() {
        let mut rng = Rng::new(42);
        let (mut failed, mut wrong) = (0, 0);
        for _ in 0..300 {
            let clean = random_word(&RS_24_12, &mut rng);
            let mut word = clean;
            let pos = distinct(&mut rng, 24, 9);
            corrupt(&mut word, &mut rng, &pos);
            if RS_24_12.decode(&mut word, &[]).is_ok() {
                assert!(RS_24_12.is_codeword(&word));
                wrong += usize::from(word != clean);
            } else {
                failed += 1;
            }
        }
        assert!(failed > 250, "failed {failed} wrong {wrong}");
    }

    #[test]
    fn gmd_uses_reliability_to_go_past_half_the_distance() {
        let mut rng = Rng::new(43);
        let clean = random_word(&RS_24_12, &mut rng);
        let mut word = clean;
        // Eight errors: beyond plain decoding (6), but the inner decoder
        // flagged them as unreliable.
        let pos = distinct(&mut rng, 24, 8);
        corrupt(&mut word, &mut rng, &pos);
        let mut reliability = [10.0f32; 24];
        for &p in &pos {
            reliability[p] = 0.5;
        }
        assert!(RS_24_12.decode(&mut word.clone(), &[]).is_err());
        let d = RS_24_12.decode_gmd(&mut word, &reliability).unwrap();
        assert_eq!(word, clean);
        assert_eq!(d.changed, 8);
    }
}
