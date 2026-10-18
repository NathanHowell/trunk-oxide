//! The inter-slot signalling channel: 40 bits ahead of every burst.
//!
//! Sync slots carry the fixed S-ISCH word. The others carry an I-ISCH: seven
//! information bits (logical channel, location in the superframe, a flag and
//! a superframe count) in an affine (40, 7) code,
//! c(i) = c₀ ⊕ Σₖ iₖ·gₖ, whose words are at least 14 apart from each other
//! and from S-ISCH. op25 looks the word up and, failing that, takes the
//! nearest one within 7 bit errors on hard decisions; here the decision is
//! maximum likelihood on bit LLRs, and the framer narrows the candidates to
//! what the superframe position allows.

use super::layout::{BURSTS_PER_SUPERFRAME, is_sync_slot};

pub const BITS: usize = 40;

/// The synchronisation word, first-sent bit highest.
pub const S_ISCH: u64 = 0x57_5d57_f7ff;
/// c(0).
const OFFSET: u64 = 0x18_4229_d461;
/// gₖ for information bit k (least significant first).
const ROWS: [u64; 7] = [
    0x00_343d_8597,
    0x00_58cb_aa4e,
    0x00_9da3_a171,
    0x09_048d_9b72,
    0x02_0807_f7ff,
    0x0c_00de_d18e,
    0x10_0f4b_1758,
];

/// An I-ISCH's seven bits: channel (2), location (2), flag (1), count (2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IschInfo(u8);

impl IschInfo {
    pub const fn new(bits: u8) -> Self {
        Self(bits & 0x7f)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Logical channel of the burst that follows.
    pub const fn channel(self) -> u8 {
        self.0 >> 5
    }

    /// Which third of the superframe (0–2).
    pub const fn location(self) -> u8 {
        (self.0 >> 3) & 3
    }

    /// op25's "fr" bit.
    pub const fn flag(self) -> bool {
        (self.0 >> 2) & 1 == 1
    }

    /// Superframe count (mod 4).
    pub const fn count(self) -> u8 {
        self.0 & 3
    }

    /// The superframe slot this I-ISCH announces, if it is a valid one.
    pub const fn slot(self) -> Option<usize> {
        let slot = self.location() as usize * 4 + self.channel() as usize;
        if self.location() < 3 && self.channel() < 2 {
            Some(slot)
        } else {
            None
        }
    }

    pub const fn codeword(self) -> u64 {
        let mut c = OFFSET;
        let mut k = 0;
        while k < ROWS.len() {
            if (self.0 >> k) & 1 == 1 {
                c ^= ROWS[k];
            }
            k += 1;
        }
        c
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Isch {
    Sync,
    Info(IschInfo),
}

impl Isch {
    pub const fn codeword(self) -> u64 {
        match self {
            Self::Sync => S_ISCH,
            Self::Info(i) => i.codeword(),
        }
    }

    /// Whether this ISCH may precede superframe slot `slot`.
    pub const fn fits(self, slot: usize) -> bool {
        match self {
            Self::Sync => is_sync_slot(slot),
            Self::Info(i) => matches!(i.slot(), Some(s) if s == slot),
        }
    }

    /// Every word: S-ISCH, then the 128 I-ISCHs.
    pub const ALL: [Self; 129] = {
        let mut all = [Self::Sync; 129];
        let mut i = 0;
        while i < 128 {
            #[allow(clippy::cast_possible_truncation)] // i < 128
            {
                all[i + 1] = Self::Info(IschInfo::new(i as u8));
            }
            i += 1;
        }
        all
    };
}

/// A soft decision.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IschDecision {
    pub isch: Isch,
    /// Total |LLR| of the bits the decision overrules.
    pub cost: f32,
    /// Cost of the runner-up minus the decision's.
    pub margin: f32,
    /// Hard bit errors.
    pub errors: u32,
}

fn hard(llr: &[f32; BITS]) -> u64 {
    llr.iter().fold(0, |w, &l| (w << 1) | u64::from(l < 0.0))
}

/// The soft cost of `codeword` against `llr`.
pub fn cost(llr: &[f32; BITS], codeword: u64) -> f32 {
    let diff = hard(llr) ^ codeword;
    llr.iter()
        .enumerate()
        .filter(|&(i, _)| (diff >> (BITS - 1 - i)) & 1 == 1)
        .map(|(_, l)| l.abs())
        .sum()
}

/// The most likely of `candidates`.
pub fn decode_among(llr: &[f32; BITS], candidates: impl Iterator<Item = Isch>) -> IschDecision {
    let h = hard(llr);
    let (mut best, mut best_cost, mut second) = (Isch::Sync, f32::INFINITY, f32::INFINITY);
    for c in candidates {
        let k = cost(llr, c.codeword());
        if k < best_cost {
            second = best_cost;
            best = c;
            best_cost = k;
        } else if k < second {
            second = k;
        }
    }
    IschDecision {
        isch: best,
        cost: best_cost,
        margin: second - best_cost,
        errors: (h ^ best.codeword()).count_ones(),
    }
}

/// The most likely of all 129 words.
pub fn decode(llr: &[f32; BITS]) -> IschDecision {
    decode_among(llr, Isch::ALL.into_iter())
}

/// The most likely word for superframe slot `slot`.
pub fn decode_for_slot(llr: &[f32; BITS], slot: usize) -> IschDecision {
    debug_assert!(slot < BURSTS_PER_SUPERFRAME);
    decode_among(llr, Isch::ALL.into_iter().filter(|i| i.fits(slot)))
}

/// op25's decision (`p25p2_isch::isch_lookup`): exact match, else the nearest
/// word within 7 bit errors, on hard bits.
pub fn decode_hard(word: u64) -> Option<Isch> {
    let (best, d) = Isch::ALL
        .into_iter()
        .map(|i| (i, (i.codeword() ^ word).count_ones()))
        .min_by_key(|&(_, d)| d)?;
    (d <= 7).then_some(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::Rng;

    /// A few entries of op25's `isch_map`.
    #[test]
    fn codewords_match_op25() {
        for (bits, cw) in [
            (0, 0x18_4229_d461u64),
            (1, 0x18_7614_51f6),
            (9, 0x11_7299_ca84),
            (77, 0x01_e071_7cad),
            (127, 0x0f_b063_f092),
        ] {
            assert_eq!(IschInfo::new(bits).codeword(), cw, "{bits}");
        }
    }

    #[test]
    fn words_are_at_least_14_apart() {
        let words = Isch::ALL.map(Isch::codeword);
        let min = (0..words.len())
            .flat_map(|a| (a + 1..words.len()).map(move |b| (a, b)))
            .map(|(a, b)| (words[a] ^ words[b]).count_ones())
            .min();
        assert_eq!(min, Some(14));
    }

    #[test]
    fn slots_follow_op25s_sync_check() {
        // op25 checks loc·4 + chn against {0, 1, -, -, 4, 5, -, -, 8, 9, -, -}.
        for slot in 0..12 {
            let infos = Isch::ALL.iter().filter(|i| i.fits(slot)).count();
            assert_eq!(infos, if is_sync_slot(slot) { 1 } else { 8 }, "{slot}");
        }
    }

    fn llrs(word: u64, sigma: f64, rng: &mut Rng) -> [f32; BITS] {
        #[allow(clippy::cast_possible_truncation)]
        std::array::from_fn(|i| {
            let x = if (word >> (BITS - 1 - i)) & 1 == 1 {
                -1.0
            } else {
                1.0
            };
            sigma.mul_add(rng.gauss(), x) as f32
        })
    }

    #[test]
    fn soft_decoding_beats_op25s_lookup() {
        let mut rng = Rng::new(31);
        let (mut soft, mut slot, mut op25) = (0, 0, 0);
        let trials = 3000;
        for t in 0..trials {
            let s = t % 12;
            let truth = if is_sync_slot(s) {
                Isch::Sync
            } else {
                #[allow(clippy::cast_possible_truncation)]
                Isch::Info(IschInfo::new(
                    ((s & 3) << 5 | (s / 4) << 3) as u8 | (rng.next_u64() & 7) as u8,
                ))
            };
            let llr = llrs(truth.codeword(), 1.0, &mut rng);
            soft += usize::from(decode(&llr).isch == truth);
            slot += usize::from(decode_for_slot(&llr, s).isch == truth);
            op25 += usize::from(decode_hard(hard(&llr)) == Some(truth));
        }
        assert!(soft > op25 + trials / 50, "soft {soft} op25 {op25}");
        assert!(slot >= soft, "slot {slot} soft {soft}");
    }
}
