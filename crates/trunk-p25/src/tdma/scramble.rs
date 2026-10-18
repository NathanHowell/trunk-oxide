//! The superframe scrambling sequence.
//!
//! Payload dibits are `XOR`ed with a 4320-bit sequence that repeats every
//! superframe and depends only on the system: a Galois LFSR with
//! p(x) = x⁴⁴ + x⁴⁰ + x³⁵ + x²⁹ + x²⁴ + x¹⁰ + 1, seeded from WACN (20 bits),
//! system ID (12) and NAC (12). This is op25's `lfsr.py` (and trunk-recorder's
//! `p25p2_lfsr`), whose six shifted sub-registers are this LFSR's taps and
//! whose seeding matrix is the same polynomial's shift-and-XOR.

use super::layout::{SUPERFRAME_DIBITS, mask_index};
use crate::types::Nac;

const WIDTH: u32 = 44;
const MASK: u64 = (1 << WIDTH) - 1;
/// The taps of p(x) below x⁴⁴.
const TAPS: [u32; 6] = [40, 35, 29, 24, 10, 0];

const fn feedback() -> u64 {
    let mut f = 0;
    let mut i = 0;
    while i < TAPS.len() {
        f |= 1 << TAPS[i];
        i += 1;
    }
    f
}

/// The 2160 scrambling dibits of a superframe.
#[derive(Clone, PartialEq, Eq)]
pub struct Scrambler([u8; SUPERFRAME_DIBITS]);

impl std::fmt::Debug for Scrambler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Scrambler({:?}…)", &self.0[..8])
    }
}

impl Scrambler {
    pub const fn new(wacn: u32, system: u16, nac: Nac) -> Self {
        let id = ((wacn as u64) << 24) | ((system as u64 & 0xfff) << 12) | nac.get() as u64;
        // The seed is the identity times (1 + Σ x^-(44−t)) over the taps.
        let mut reg = id;
        let mut i = 0;
        while i < TAPS.len() - 1 {
            reg ^= id >> (WIDTH - TAPS[i]);
            i += 1;
        }
        let mut out = [0u8; SUPERFRAME_DIBITS];
        let mut d = 0;
        while d < SUPERFRAME_DIBITS {
            let mut two = 0;
            let mut b = 0;
            while b < 2 {
                let top = (reg >> (WIDTH - 1)) & 1;
                two = (two << 1) | top as u8;
                reg = (reg << 1) & MASK;
                if top == 1 {
                    reg ^= feedback();
                }
                b += 1;
            }
            out[d] = two;
            d += 1;
        }
        Self(out)
    }

    /// From the 2160-dibit mask trunk-recorder hands op25.
    pub fn from_dibits(mask: &[u8]) -> Option<Self> {
        let mut out = [0u8; SUPERFRAME_DIBITS];
        if mask.len() != SUPERFRAME_DIBITS {
            return None;
        }
        for (o, &m) in out.iter_mut().zip(mask) {
            *o = m & 3;
        }
        Some(Self(out))
    }

    /// The mask dibit for payload dibit `p` of superframe slot `slot`.
    pub const fn payload(&self, slot: usize, p: usize) -> u8 {
        self.0[mask_index(slot, p)]
    }

    pub const fn dibits(&self) -> &[u8; SUPERFRAME_DIBITS] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_trunk_recorders_lfsr() {
        // trunk-recorder's p25p2_lfsr(nac 0x293, sysid 0x001, wacn 0xBEE00).
        const S: Scrambler = Scrambler::new(0xbee00, 0x001, Nac::new(0x293).unwrap());
        #[rustfmt::skip]
        let expected: [u8; 128] = [
            2,3,3,2,3,2,0,0,0,0,0,0,0,0,0,1,0,2,2,1,0,3,1,2,1,1,0,0,3,1,2,2,
            1,0,1,3,2,3,3,2,0,1,0,2,1,3,2,2,3,0,2,0,1,3,2,2,1,1,2,3,2,1,2,0,
            3,0,1,3,0,2,2,3,0,3,1,1,2,2,1,3,3,3,3,0,1,2,2,3,2,3,1,2,1,3,1,3,
            0,3,2,2,3,1,3,0,2,0,2,3,3,0,2,3,0,2,0,1,1,2,2,0,1,0,3,3,0,2,2,1,
        ];
        assert_eq!(S.dibits()[..128], expected);
        assert_eq!(Scrambler::from_dibits(S.dibits()), Some(S));
    }

    #[test]
    fn the_sequence_does_not_repeat_within_a_superframe() {
        let s = Scrambler::new(0xbee00, 0x5e4, Nac::new(0x5e1).unwrap());
        let d = s.dibits();
        assert!((1..SUPERFRAME_DIBITS / 2).all(|lag| d[lag..] != d[..SUPERFRAME_DIBITS - lag]));
    }
}
