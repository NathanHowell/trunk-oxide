//! Systematic binary block codes up to 32 bits, and the P25 instances.
//!
//! A word is a `u32` whose bit n−1 is sent first; the top k bits carry the
//! information and the low n−k the parity. A code is its generator matrix
//! G = [I | P], stored as the parity contribution of each information bit;
//! cyclic codes derive P from their generator polynomial, extended and
//! shortened codes derive it from their parent. Everything up to the
//! coset-leader table is computed at compile time.
//!
//! Hard decoding is syndrome decoding against the coset leaders (exact
//! maximum likelihood for the perfect Golay and Hamming codes). Soft decoding
//! takes per-bit log-likelihood ratios and is exact maximum likelihood by
//! enumeration when 2^k is small, Chase-II otherwise.

/// Soft input: one LLR per bit, first-sent bit first; positive favours 0.
pub type Llrs = [f32];

/// A decoding decision.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decision {
    pub info: u32,
    pub codeword: u32,
    /// Bits where the codeword differs from the hard decisions.
    pub flips: u32,
    /// Soft cost of the runner-up minus that of the decision (∞ if none was
    /// examined): how sure the decoder is.
    pub margin: f32,
    /// The runner-up's information bits, when one was examined.
    pub runner_up: Option<u32>,
    /// Soft cost: total |LLR| of the bits the decision overrules (0 for a
    /// hard decision).
    pub cost: f32,
}

/// What every block code here offers, whatever its dimensions.
pub trait BlockCode: Sync {
    fn n(&self) -> u32;
    fn k(&self) -> u32;
    fn encode(&self, info: u32) -> u32;
    fn decode_hard(&self, word: u32) -> Decision;
    fn decode_soft(&self, llr: &Llrs) -> Decision;
    fn cost_of(&self, llr: &Llrs, info: u32) -> f32;
}

/// A systematic (n, K) code; `L` = 2^(n−K) is its number of cosets.
#[derive(Clone, Debug)]
pub struct BinaryCode<const K: usize, const L: usize> {
    n: u32,
    /// Parity of each information bit, most significant first.
    rows: [u32; K],
    /// Syndrome → minimum-weight error pattern.
    leaders: [u32; L],
}

/// Positions Chase-II flips: 2^4 test patterns.
const CHASE_BITS: usize = 4;
/// Enumerate every codeword (exact ML) up to this dimension.
const ML_MAX_K: usize = 8;

const fn mask(n: u32) -> u32 {
    if n >= 32 { u32::MAX } else { (1 << n) - 1 }
}

/// Next larger integer with the same number of set bits (Gosper's hack).
const fn next_combination(x: u64) -> u64 {
    let c = x.isolate_lowest_one();
    let r = x + c;
    (((r ^ x) >> 2) / c) | r
}

#[allow(clippy::cast_possible_truncation)]
const fn k32(k: usize) -> u32 {
    k as u32
}

impl<const K: usize, const L: usize> BinaryCode<K, L> {
    /// From the generator matrix's parity part (one row per information bit,
    /// most significant first).
    pub const fn from_parity_rows(n: u32, rows: [u32; K]) -> Self {
        assert!(k32(K) < n && n <= 32, "need 0 < k < n ≤ 32");
        assert!(L == 1 << (n - k32(K)), "L must be 2^(n−k)");
        let mut code = Self {
            n,
            rows,
            leaders: [u32::MAX; L],
        };
        code.leaders = code.coset_leaders();
        code
    }

    /// Cyclic code with generator polynomial `g` (bit i = coefficient of xⁱ):
    /// information bit j (counting from the top) contributes x^(n−1−j) mod g.
    #[allow(clippy::many_single_char_names)]
    pub const fn cyclic(n: u32, g: u32) -> Self {
        let r = g.ilog2();
        assert!(n - r == k32(K), "deg g must be n − k");
        let mut rows = [0u32; K];
        let mut j = 0;
        while j < K {
            let mut v = 1u64 << (n - 1 - k32(j));
            let mut bit = n - 1;
            while bit >= r {
                if (v >> bit) & 1 == 1 {
                    v ^= (g as u64) << (bit - r);
                }
                bit -= 1;
            }
            #[allow(clippy::cast_possible_truncation)]
            {
                rows[j] = v as u32;
            }
            j += 1;
        }
        Self::from_parity_rows(n, rows)
    }

    /// Append an overall even-parity bit (the new last bit).
    pub const fn extended<const L2: usize>(&self) -> BinaryCode<K, L2> {
        let mut rows = [0u32; K];
        let mut j = 0;
        while j < K {
            let p = self.rows[j];
            rows[j] = (p << 1) | ((p.count_ones() + 1) & 1);
            j += 1;
        }
        BinaryCode::from_parity_rows(self.n + 1, rows)
    }

    /// Drop the top K − K2 information bits (sent as zeros, so not sent).
    pub const fn shortened<const K2: usize, const L2: usize>(&self) -> BinaryCode<K2, L2> {
        assert!(K2 < K);
        let s = K - K2;
        let mut rows = [0u32; K2];
        let mut j = 0;
        while j < K2 {
            rows[j] = self.rows[s + j];
            j += 1;
        }
        BinaryCode::from_parity_rows(self.n - k32(s), rows)
    }

    const fn r(&self) -> u32 {
        self.n - k32(K)
    }

    const fn parity_of(&self, info: u32) -> u32 {
        let mut p = 0;
        let mut j = 0;
        while j < K {
            if (info >> (k32(K) - 1 - k32(j))) & 1 == 1 {
                p ^= self.rows[j];
            }
            j += 1;
        }
        p
    }

    pub const fn syndrome(&self, word: u32) -> u32 {
        self.parity_of(word >> self.r()) ^ (word & mask(self.r()))
    }

    /// Every syndrome's lightest error pattern: enumerate patterns weight by
    /// weight until each coset has its leader.
    const fn coset_leaders(&self) -> [u32; L] {
        let mut leaders = [u32::MAX; L];
        leaders[0] = 0;
        let mut filled = 1;
        let mut w = 1;
        while filled < L && w <= self.n {
            let mut x: u64 = (1 << w) - 1;
            while x < 1 << self.n {
                #[allow(clippy::cast_possible_truncation)]
                let e = x as u32;
                let s = self.syndrome(e) as usize;
                if leaders[s] == u32::MAX {
                    leaders[s] = e;
                    filled += 1;
                }
                x = next_combination(x);
            }
            w += 1;
        }
        leaders
    }

    pub const fn encode(&self, info: u32) -> u32 {
        (info << self.r()) | self.parity_of(info)
    }

    pub const fn decode_hard(&self, word: u32) -> Decision {
        let e = self.leaders[self.syndrome(word) as usize];
        let codeword = word ^ e;
        Decision {
            info: codeword >> self.r(),
            codeword,
            flips: e.count_ones(),
            margin: f32::INFINITY,
            runner_up: None,
            cost: 0.0,
        }
    }

    fn hard_word(llr: &Llrs) -> u32 {
        llr.iter().fold(0, |w, &l| (w << 1) | u32::from(l < 0.0))
    }

    /// Soft cost of the codeword carrying `info` against `llr`.
    pub fn cost_of(&self, llr: &Llrs, info: u32) -> f32 {
        self.cost(llr, Self::hard_word(llr), self.encode(info))
    }

    /// Soft cost of `codeword`: the total |LLR| of the bits it overrules.
    fn cost(&self, llr: &Llrs, hard: u32, codeword: u32) -> f32 {
        let mut d = hard ^ codeword;
        let mut c = 0.0;
        while d != 0 {
            c += llr[(self.n - 1 - d.trailing_zeros()) as usize].abs();
            d &= d - 1;
        }
        c
    }

    /// Soft-decision decode: maximum likelihood for small k, Chase-II above.
    /// Allocation free.
    pub fn decode_soft(&self, llr: &Llrs) -> Decision {
        assert_eq!(llr.len(), self.n as usize);
        let hard = Self::hard_word(llr);
        let mut best = Best::new(hard);
        if K <= ML_MAX_K {
            for info in 0..1 << K {
                let cw = self.encode(info);
                best.offer(cw, self.cost(llr, hard, cw));
            }
        } else {
            // The CHASE_BITS least reliable positions, by insertion.
            let mut weak = [(f32::INFINITY, 0u32); CHASE_BITS];
            for (pos, l) in llr.iter().enumerate() {
                let m = l.abs();
                if m < weak[CHASE_BITS - 1].0 {
                    let mut i = CHASE_BITS - 1;
                    while i > 0 && weak[i - 1].0 > m {
                        weak[i] = weak[i - 1];
                        i -= 1;
                    }
                    weak[i] = (m, u32::try_from(pos).unwrap_or(0));
                }
            }
            for pattern in 0..1u32 << CHASE_BITS {
                let test = weak
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| (pattern >> i) & 1 == 1)
                    .fold(hard, |w, (_, &(_, pos))| w ^ (1 << (self.n - 1 - pos)));
                let cw = self.decode_hard(test).codeword;
                best.offer(cw, self.cost(llr, hard, cw));
            }
        }
        best.decision(self.r())
    }
}

/// Running best and runner-up of a candidate search.
struct Best {
    hard: u32,
    codeword: u32,
    cost: f32,
    second: Option<(u32, f32)>,
}

impl Best {
    const fn new(hard: u32) -> Self {
        Self {
            hard,
            codeword: hard,
            cost: f32::INFINITY,
            second: None,
        }
    }

    fn offer(&mut self, codeword: u32, cost: f32) {
        if codeword == self.codeword {
            self.cost = self.cost.min(cost);
        } else if cost < self.cost {
            if self.cost.is_finite() {
                self.second = Some((self.codeword, self.cost));
            }
            self.codeword = codeword;
            self.cost = cost;
        } else if self.second.is_none_or(|(_, c)| cost < c) {
            self.second = Some((codeword, cost));
        }
    }

    fn decision(&self, r: u32) -> Decision {
        Decision {
            info: self.codeword >> r,
            codeword: self.codeword,
            flips: (self.codeword ^ self.hard).count_ones(),
            margin: self.second.map_or(f32::INFINITY, |(_, c)| c - self.cost),
            runner_up: self.second.map(|(cw, _)| cw >> r),
            cost: self.cost,
        }
    }
}

impl<const K: usize, const L: usize> BlockCode for BinaryCode<K, L> {
    fn n(&self) -> u32 {
        self.n
    }
    fn k(&self) -> u32 {
        k32(K)
    }
    fn encode(&self, info: u32) -> u32 {
        Self::encode(self, info)
    }
    fn decode_hard(&self, word: u32) -> Decision {
        Self::decode_hard(self, word)
    }
    fn decode_soft(&self, llr: &Llrs) -> Decision {
        Self::decode_soft(self, llr)
    }
    fn cost_of(&self, llr: &Llrs, info: u32) -> f32 {
        Self::cost_of(self, llr, info)
    }
}

/// Golay generator g(x) = x¹¹ + x¹⁰ + x⁶ + x⁵ + x⁴ + x² + 1, octal 6165
/// (BAAA-A §5.7; BABA §7.3).
pub const GOLAY_GENERATOR: u32 = 0o6165;
/// Parity rows of the shortened Hamming(10,6,3) code (BAAA-A §5.8, Table
/// 5-4): chosen rows of the (15,11) code, not simply its last six.
pub const HAMMING_10_PARITY: [u32; 6] = [0b1110, 0b1101, 0b1011, 0b0111, 0b0011, 0b1100];

/// Hamming(15,11) parity columns: every 4-bit value of weight ≥ 2,
/// descending (BAAA-A §5.8, Table 5-4; BABA §7.3).
const fn hamming_columns() -> [u32; 11] {
    let mut rows = [0u32; 11];
    let (mut c, mut i) = (15u32, 0);
    while i < 11 {
        if c.count_ones() >= 2 {
            rows[i] = c;
            i += 1;
        }
        c -= 1;
    }
    rows
}

/// Golay(23,12,7): cyclic, [`GOLAY_GENERATOR`]. Perfect, t = 3.
/// Protects IMBE vectors u0–u3.
pub static GOLAY_23: BinaryCode<12, 2048> = BinaryCode::cyclic(23, GOLAY_GENERATOR);
/// Extended Golay(24,12,8), for TDULC link control: Golay(23,12) plus an
/// even-parity bit (BAAA-A §5.7, Table 5-3).
pub static GOLAY_24: BinaryCode<12, 4096> = GOLAY_23.extended();
/// Shortened Golay(18,6,8), for the header data unit: the (24,12) code less
/// its left six bits (BAAA-A §5.7, Table 5-3).
pub static GOLAY_18: BinaryCode<6, 4096> = GOLAY_24.shortened();
/// Hamming(15,11,3), protecting IMBE vectors u4–u6.
pub static HAMMING_15: BinaryCode<11, 16> = BinaryCode::from_parity_rows(15, hamming_columns());
/// Shortened Hamming(10,6,3): P25's parity columns for the hexbits of link
/// control and encryption sync in voice frames.
pub static HAMMING_10: BinaryCode<6, 16> = BinaryCode::from_parity_rows(10, HAMMING_10_PARITY);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::Rng;

    const CODES: [(&dyn BlockCode, u32, u32); 5] = [
        (&GOLAY_23, 7, 3),
        (&GOLAY_24, 8, 3),
        (&GOLAY_18, 8, 3),
        (&HAMMING_15, 3, 1),
        (&HAMMING_10, 3, 1),
    ];

    /// op25's encoders (`golay_23_encode`, `hamming_15_encode`, hmg1063EncTbl),
    /// from a harness linked against op25.
    #[test]
    fn encoders_match_op25() {
        const HMG1063: [u32; 64] = [
            0, 12, 3, 15, 7, 11, 4, 8, 11, 7, 8, 4, 12, 0, 15, 3, 13, 1, 14, 2, 10, 6, 9, 5, 6, 10,
            5, 9, 1, 13, 2, 14, 14, 2, 13, 1, 9, 5, 10, 6, 5, 9, 6, 10, 2, 14, 1, 13, 3, 15, 0, 12,
            4, 8, 7, 11, 8, 4, 11, 7, 15, 3, 12, 0,
        ];
        for (v, cw) in [
            (3094, 6_337_856),
            (1063, 2_178_522),
            (1308, 2_679_516),
            (3868, 7_922_514),
        ] {
            assert_eq!(GOLAY_23.encode(v), cw);
        }
        for (v, cw) in [(1487, 23794), (685, 10970), (631, 10102), (943, 15090)] {
            assert_eq!(HAMMING_15.encode(v), cw);
        }
        for (d, &p) in HMG1063.iter().enumerate() {
            let d = u32::try_from(d).unwrap();
            assert_eq!(HAMMING_10.encode(d), (d << 4) | p);
        }
        // Golay(24) is Golay(23) plus even parity; op25's gly24128Dec drops it.
        let cw = GOLAY_24.encode(3094);
        assert_eq!(cw >> 1, 6_337_856);
        assert_eq!(cw.count_ones() % 2, 0);
    }

    #[test]
    fn encoding_is_usable_at_compile_time() {
        const CW: u32 = GOLAY_23.encode(3094);
        const D: Decision = GOLAY_23.decode_hard(CW ^ 0b101);
        assert!(D.runner_up.is_none());
        assert_eq!((CW, D.info, D.flips), (6_337_856, 3094, 2));
    }

    #[test]
    fn minimum_distances() {
        for (code, d, _) in CODES {
            let min = (1..1u32 << code.k())
                .map(|i| code.encode(i).count_ones())
                .min()
                .unwrap();
            assert_eq!(min, d, "n = {}", code.n());
        }
    }

    #[test]
    fn hard_decoding_corrects_up_to_t() {
        let mut rng = Rng::new(51);
        for (code, _, t) in CODES {
            for _ in 0..300 {
                #[allow(clippy::cast_possible_truncation)]
                let info = rng.next_u64() as u32 & mask(code.k());
                let cw = code.encode(info);
                let mut word = cw;
                while (word ^ cw).count_ones() < t {
                    word ^= 1 << rng.below(code.n() as usize);
                }
                let d = code.decode_hard(word);
                assert_eq!(
                    (d.info, d.codeword, d.flips),
                    (info, cw, t),
                    "n = {}",
                    code.n()
                );
            }
        }
    }

    fn noisy(code: &dyn BlockCode, cw: u32, sigma: f64, rng: &mut Rng) -> Vec<f32> {
        (0..code.n())
            .rev()
            .map(|b| {
                let x = if (cw >> b) & 1 == 1 { -1.0 } else { 1.0 };
                #[allow(clippy::cast_possible_truncation)]
                let v = sigma.mul_add(rng.gauss(), x) as f32;
                v
            })
            .collect()
    }

    fn hard_word(llr: &[f32]) -> u32 {
        llr.iter().fold(0, |w, &l| (w << 1) | u32::from(l < 0.0))
    }

    #[test]
    fn soft_decoding_beats_hard_decoding() {
        let mut rng = Rng::new(52);
        for (code, _, _) in CODES {
            let (mut soft, mut hard) = (0, 0);
            for _ in 0..2000 {
                #[allow(clippy::cast_possible_truncation)]
                let info = rng.next_u64() as u32 & mask(code.k());
                let llr = noisy(code, code.encode(info), 0.6, &mut rng);
                soft += usize::from(code.decode_soft(&llr).info == info);
                hard += usize::from(code.decode_hard(hard_word(&llr)).info == info);
            }
            assert!(soft > hard, "n = {}: soft {soft} hard {hard}", code.n());
        }
    }

    #[test]
    fn margin_ranks_reliability() {
        let mut rng = Rng::new(53);
        let code = &HAMMING_10;
        let (mut right, mut wrong) = (Vec::new(), Vec::new());
        for _ in 0..3000 {
            #[allow(clippy::cast_possible_truncation)]
            let info = rng.next_u64() as u32 & 63;
            let d = code.decode_soft(&noisy(code, code.encode(info), 0.9, &mut rng));
            if d.info == info {
                right.push(d.margin);
            } else {
                wrong.push(d.margin);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
        assert!(
            mean(&right) > 2.0 * mean(&wrong),
            "right {} wrong {}",
            mean(&right),
            mean(&wrong)
        );
    }
}
