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

/// The LFSR's length (BBAC §7.2.5).
const WIDTH: u32 = 44;
const MASK: u64 = (1 << WIDTH) - 1;
/// The taps of G(x) = x⁴⁴ + x⁴⁰ + x³⁵ + x²⁹ + x²⁴ + x¹⁰ + 1 below x⁴⁴
/// (BBAC §7.2.5 eq. 4).
const TAPS: [u32; 6] = [40, 35, 29, 24, 10, 0];
/// `seed_external` = WACN·2²⁴ + System ID·2¹² + CC (BBAC §7.2.5 eq. 5).
/// Eq. 5 replaces an all-zero seed with 2⁴⁴ − 1, but the color code is the
/// NAC, which is never 0 ([`Nac::new`]), so the seed never is.
const WACN_SHIFT: u32 = 24;
const SYSTEM_ID_SHIFT: u32 = 12;
/// WACN is 20 bits and System ID 12 (BBAC §7.2.5).
const WACN_MASK: u64 = 0xf_ffff;
const SYSTEM_ID_MASK: u64 = 0xfff;

const fn feedback() -> u64 {
    let mut f = 0;
    let mut i = 0;
    while i < TAPS.len() {
        f |= 1 << TAPS[i];
        i += 1;
    }
    f
}

/// The 2160 scrambling dibits of a superframe, and the color code (NAC)
/// they were seeded with.
#[derive(Clone, PartialEq, Eq)]
pub struct Scrambler {
    dibits: [u8; SUPERFRAME_DIBITS],
    nac: Nac,
}

impl std::fmt::Debug for Scrambler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Scrambler({:03x}: {:?}…)",
            self.nac.get(),
            &self.dibits[..8]
        )
    }
}

impl Scrambler {
    pub const fn new(wacn: u32, system: u16, nac: Nac) -> Self {
        let id = ((wacn as u64 & WACN_MASK) << WACN_SHIFT)
            | ((system as u64 & SYSTEM_ID_MASK) << SYSTEM_ID_SHIFT)
            | nac.get() as u64;
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
        Self { dibits: out, nac }
    }

    /// The mask dibit for payload dibit `p` of superframe slot `slot`.
    pub const fn payload(&self, slot: usize, p: usize) -> u8 {
        self.dibits[mask_index(slot, p)]
    }

    pub const fn dibits(&self) -> &[u8; SUPERFRAME_DIBITS] {
        &self.dibits
    }

    /// The color code: the system's NAC, which a phase 2 channel's
    /// `MAC_END_PTT` repeats (BBAC §7.3.3).
    pub const fn color_code(&self) -> Nac {
        self.nac
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
    }

    /// Only the WACN's 20 bits and the System ID's 12 seed the sequence.
    #[test]
    fn the_seed_takes_the_fields_widths() {
        let nac = Nac::new(0x5e1).unwrap();
        let s = Scrambler::new(0xbee00, 0x5e4, nac);
        assert_eq!(Scrambler::new(0xfffb_ee00, 0xf5e4, nac), s);
    }

    #[test]
    fn the_sequence_does_not_repeat_within_a_superframe() {
        let s = Scrambler::new(0xbee00, 0x5e4, Nac::new(0x5e1).unwrap());
        let d = s.dibits();
        assert!((1..SUPERFRAME_DIBITS / 2).all(|lag| d[lag..] != d[..SUPERFRAME_DIBITS - lag]));
    }
}
