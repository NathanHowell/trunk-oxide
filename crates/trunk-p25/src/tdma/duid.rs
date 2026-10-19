//! The burst's data unit ID: four bits in an (8,4,4) extended Hamming code,
//! sent unscrambled as dibits 20, 57, 142 and 179 of the burst.
//!
//! op25 decodes it with a 256-entry table on hard bits (ambiguous words fail);
//! here it is maximum likelihood over the burst types a voice channel uses.

use crate::fec::{BinaryCode, Decision};

/// The parity half of the (8,4,4) generator matrix, information bit 3
/// first: bit 3 contributes 1101, bit 2 1011, bit 1 1110, bit 0 0111
/// (BBAC §5.4.1 Table 5-2).
pub const DUID_PARITY_ROWS: [u32; 4] = [0xd, 0xb, 0xe, 0x7];

/// Systematic: the four information bits, then parity.
pub static DUID_CODE: BinaryCode<4, 16> = BinaryCode::from_parity_rows(8, DUID_PARITY_ROWS);

/// DUID information values on a voice channel (BBAC §5.4.1 Table 5-3).
pub const DUID_4V: u8 = 0b0000;
pub const DUID_SACCH_SCRAMBLED: u8 = 0b0011;
pub const DUID_2V: u8 = 0b0110;
pub const DUID_FACCH_SCRAMBLED: u8 = 0b1001;
pub const DUID_SACCH_UNSCRAMBLED: u8 = 0b1100;
pub const DUID_FACCH_UNSCRAMBLED: u8 = 0b1111;
/// Reserved in BBAC Table 5-3; op25 reads these as the TDMA control
/// channel's LCCH.
const LCCH_SCRAMBLED: u8 = 4;
const LCCH_UNSCRAMBLED: u8 = 13;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BurstType {
    /// Four voice codewords and a quarter of the encryption sync.
    Voice4,
    /// Two voice codewords and the encryption sync parity.
    Voice2,
    /// Slow associated control channel.
    Sacch { scrambled: bool },
    /// Fast associated control channel (in place of voice).
    Facch { scrambled: bool },
    /// Logical control channel (TDMA control channels).
    Lcch { scrambled: bool },
    /// A DUID with no defined use.
    Reserved(u8),
}

impl BurstType {
    pub const fn from_duid(v: u8) -> Self {
        match v & 15 {
            DUID_4V => Self::Voice4,
            DUID_2V => Self::Voice2,
            DUID_SACCH_SCRAMBLED => Self::Sacch { scrambled: true },
            DUID_SACCH_UNSCRAMBLED => Self::Sacch { scrambled: false },
            DUID_FACCH_SCRAMBLED => Self::Facch { scrambled: true },
            DUID_FACCH_UNSCRAMBLED => Self::Facch { scrambled: false },
            LCCH_SCRAMBLED => Self::Lcch { scrambled: true },
            LCCH_UNSCRAMBLED => Self::Lcch { scrambled: false },
            v => Self::Reserved(v),
        }
    }

    pub const fn duid(self) -> u8 {
        match self {
            Self::Voice4 => DUID_4V,
            Self::Voice2 => DUID_2V,
            Self::Sacch { scrambled: true } => DUID_SACCH_SCRAMBLED,
            Self::Sacch { scrambled: false } => DUID_SACCH_UNSCRAMBLED,
            Self::Facch { scrambled: true } => DUID_FACCH_SCRAMBLED,
            Self::Facch { scrambled: false } => DUID_FACCH_UNSCRAMBLED,
            Self::Lcch { scrambled: true } => LCCH_SCRAMBLED,
            Self::Lcch { scrambled: false } => LCCH_UNSCRAMBLED,
            Self::Reserved(v) => v,
        }
    }

    /// The types with a meaning.
    pub const DEFINED: [Self; 8] = [
        Self::Voice4,
        Self::Voice2,
        Self::Sacch { scrambled: true },
        Self::Sacch { scrambled: false },
        Self::Facch { scrambled: true },
        Self::Facch { scrambled: false },
        Self::Lcch { scrambled: true },
        Self::Lcch { scrambled: false },
    ];

    pub const fn scrambled(self) -> bool {
        match self {
            Self::Voice4 | Self::Voice2 => true,
            Self::Sacch { scrambled } | Self::Facch { scrambled } | Self::Lcch { scrambled } => {
                scrambled
            }
            Self::Reserved(_) => false,
        }
    }

    pub const fn codeword(self) -> u32 {
        DUID_CODE.encode(self.duid() as u32)
    }
}

/// Maximum-likelihood burst type from the DUID's 8 bit LLRs.
pub fn decode(llr: &[f32; 8]) -> (BurstType, Decision) {
    let mut best: Option<(BurstType, f32)> = None;
    let mut second = f32::INFINITY;
    for t in BurstType::DEFINED {
        let c = DUID_CODE.cost_of(llr, u32::from(t.duid()));
        match best {
            Some((_, b)) if c >= b => second = second.min(c),
            _ => {
                if let Some((_, b)) = best {
                    second = b;
                }
                best = Some((t, c));
            }
        }
    }
    let (t, cost) = best.unwrap_or((BurstType::Voice4, f32::INFINITY));
    let hard = llr.iter().fold(0, |w, &l| (w << 1) | u32::from(l < 0.0));
    let codeword = t.codeword();
    (
        t,
        Decision {
            info: u32::from(t.duid()),
            codeword,
            flips: (hard ^ codeword).count_ones(),
            margin: second - cost,
            runner_up: None,
            cost,
        },
    )
}

/// op25's `_duid_lookup`: the nearest codeword on hard bits, failing on ties.
pub fn decode_hard(word: u8) -> Option<BurstType> {
    let d = DUID_CODE.decode_hard(u32::from(word));
    #[allow(clippy::cast_possible_truncation)]
    (d.flips <= 1).then(|| BurstType::from_duid(d.info as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// op25's `_duid_lookup` table (`p25p2_duid.cc`).
    #[rustfmt::skip]
    const OP25: [i8; 256] = [
        0,0,0,-1,0,-1,-1,1,0,-1,-1,4,-1,8,2,-1,
        0,-1,-1,1,-1,1,1,1,-1,3,9,-1,5,-1,-1,1,
        0,-1,-1,10,-1,6,2,-1,-1,3,2,-1,2,-1,2,2,
        -1,3,7,-1,11,-1,-1,1,3,3,-1,3,-1,3,2,-1,
        0,-1,-1,4,-1,6,12,-1,-1,4,4,4,5,-1,-1,4,
        -1,13,7,-1,5,-1,-1,1,5,-1,-1,4,5,5,5,-1,
        -1,6,7,-1,6,6,-1,6,14,-1,-1,4,-1,6,2,-1,
        7,-1,7,7,-1,6,7,-1,-1,3,7,-1,5,-1,-1,15,
        0,-1,-1,10,-1,8,12,-1,-1,8,9,-1,8,8,-1,8,
        -1,13,9,-1,11,-1,-1,1,9,-1,9,9,-1,8,9,-1,
        -1,10,10,10,11,-1,-1,10,14,-1,-1,10,-1,8,2,-1,
        11,-1,-1,10,11,11,11,-1,-1,3,9,-1,11,-1,-1,15,
        -1,13,12,-1,12,-1,12,12,14,-1,-1,4,-1,8,12,-1,
        13,13,-1,13,-1,13,12,-1,-1,13,9,-1,5,-1,-1,15,
        14,-1,-1,10,-1,6,12,-1,14,14,14,-1,14,-1,-1,15,
        -1,13,7,-1,11,-1,-1,15,14,-1,-1,15,-1,15,15,15,
    ];

    #[test]
    fn hard_decoding_is_op25s_table() {
        for (w, &want) in OP25.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let got = decode_hard(w as u8).map(BurstType::duid);
            #[allow(clippy::cast_sign_loss)]
            let want = (want >= 0).then_some(want as u8);
            assert_eq!(got, want, "{w:#04x}");
        }
    }

    #[test]
    fn types_round_trip() {
        for v in 0..16 {
            assert_eq!(BurstType::from_duid(v).duid(), v);
        }
        for t in BurstType::DEFINED {
            let llr: [f32; 8] = std::array::from_fn(|i| {
                if (t.codeword() >> (7 - i)) & 1 == 1 {
                    -1.0
                } else {
                    1.0
                }
            });
            assert_eq!(decode(&llr).0, t);
        }
    }
}
