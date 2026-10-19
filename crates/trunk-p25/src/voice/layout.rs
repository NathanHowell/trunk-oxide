//! Where things sit in Phase 1 voice frames (TIA-102.BAAA).
//!
//! Positions are counted two ways: *data* bits, with the status dibits taken
//! out (one after every 35 data dibits), and *raw* bits, as transmitted from
//! the first sync bit. Every field is a run of data bits; op25's bit tables
//! (`voice_codeword_bits`, `hdu_codeword_bits`, `imbe_ldu_ls_data_bits`) are
//! these maps evaluated and stored.

use crate::framer::NID_BITS;
use crate::symbols::{FRAME_SYNC_BITS, MICRO_SLOT_DATA_DIBITS};

/// Sync (48) and NID (64): the data bits before any frame's payload.
pub const PAYLOAD: usize = FRAME_SYNC_BITS + NID_BITS;

/// Raw bit of data bit `p`: every 35 data dibits (a micro-slot's 70 bits)
/// are followed by a status dibit (BAAA-A §8.2).
pub const fn raw_bit(p: usize) -> usize {
    let d = p / 2;
    2 * (d + d / MICRO_SLOT_DATA_DIBITS) + p % 2
}

/// IMBE codewords per voice frame (LDU) and their size (BAAA-A §5.1, §5.3).
pub const IMBE_PER_LDU: usize = 9;
pub const IMBE_BITS: usize = 144;

/// First data bit of each IMBE codeword in an LDU. Codewords 2–7 are each
/// preceded by a 40-bit link-control (LDU1) or encryption-sync (LDU2) group,
/// codeword 8 by the 32 bits of low-speed data.
pub const IMBE_BASE: [usize; IMBE_PER_LDU] = {
    let mut base = [0; IMBE_PER_LDU];
    let mut at = PAYLOAD;
    let mut i = 0;
    while i < IMBE_PER_LDU {
        if i >= 2 {
            at += if i == 8 { LSD_BITS } else { SIGNALLING_GROUP };
        }
        base[i] = at;
        at += IMBE_BITS;
        i += 1;
    }
    base
};

/// Data bits in an LDU: 1680, i.e. 864 dibits with status.
pub const LDU_DATA_BITS: usize = IMBE_BASE[IMBE_PER_LDU - 1] + IMBE_BITS;

/// Four Hamming(10,6) words of link control or encryption sync between
/// voice codewords (BAAA-A §8.2.2, Figures 8-3 and 8-4).
const SIGNALLING_GROUP: usize = 40;
/// Two (16,8) low-speed data words (BAAA-A §5.6, §8.2.2).
const LSD_BITS: usize = 32;

/// Data bit of bit `j` of an IMBE codeword starting at `base`. The 144 bits
/// are six rows of 24; row r takes pairs 12 bits apart, the pair's second
/// bit 7 (even rows) or 5 (odd rows) after its first.
pub const fn imbe_bit(base: usize, j: usize) -> usize {
    let (row, col) = (j / 24, j % 24);
    let second = if row % 2 == 0 { 7 } else { 5 };
    base + row + 12 * (col / 2) + (col % 2) * second
}

/// Data bit of bit `j` (0..240) of an LDU's link-control / encryption-sync
/// field: the 40-bit groups before IMBE codewords 2–7, in order.
pub const fn signalling_bit(j: usize) -> usize {
    IMBE_BASE[2 + j / SIGNALLING_GROUP] - SIGNALLING_GROUP + j % SIGNALLING_GROUP
}

/// The signalling field: 24 Hamming(10,6) codewords, one hexbit each
/// (BAAA-A §5.4, §5.5).
pub const SIGNALLING_BITS: usize = 6 * SIGNALLING_GROUP;

#[cfg(test)]
mod tests {
    use super::*;

    // Rows of op25's op25_imbe_frame.h tables (raw positions).
    const OP25_IMBE_0_ROW_0: [usize; 24] = [
        114, 121, 126, 133, 138, 147, 152, 159, 164, 171, 176, 183, 188, 195, 200, 207, 212, 221,
        226, 233, 238, 245, 250, 257,
    ];
    const OP25_IMBE_3_ROW_4: [usize; 24] = [
        644, 653, 658, 665, 670, 677, 682, 689, 694, 701, 706, 713, 720, 727, 732, 739, 744, 751,
        756, 763, 768, 775, 780, 787,
    ];
    const OP25_IMBE_8_ROW_5: [usize; 24] = [
        1585, 1590, 1597, 1602, 1609, 1614, 1621, 1626, 1633, 1638, 1645, 1650, 1659, 1664, 1671,
        1676, 1683, 1688, 1695, 1700, 1707, 1712, 1719, 1724,
    ];
    const OP25_IMBE_8_ROW_4: [usize; 24] = [
        1584, 1591, 1596, 1603, 1608, 1615, 1620, 1627, 1632, 1639, 1644, 1651, 1658, 1665, 1670,
        1677, 1682, 1689, 1694, 1701, 1706, 1713, 1718, 1725,
    ];

    #[test]
    fn imbe_interleave_matches_op25() {
        for (cw, row, table) in [
            (0, 0, OP25_IMBE_0_ROW_0),
            (3, 4, OP25_IMBE_3_ROW_4),
            (8, 5, OP25_IMBE_8_ROW_5),
            (8, 4, OP25_IMBE_8_ROW_4),
        ] {
            for (c, &raw) in table.iter().enumerate() {
                assert_eq!(
                    raw_bit(imbe_bit(IMBE_BASE[cw], row * 24 + c)),
                    raw,
                    "cw {cw} row {row} col {c}"
                );
            }
        }
    }

    #[test]
    fn hdu_and_signalling_tables_match_op25() {
        // hdu_codeword_bits: data bits from 112, status skipped.
        let hdu: Vec<usize> = (0..34).map(|i| raw_bit(PAYLOAD + i)).collect();
        assert_eq!(&hdu[26..30], &[140, 141, 144, 145]);
        assert_eq!(raw_bit(PAYLOAD + 657), 789); // its last entry
        // imbe_ldu_ls_data_bits: 410… with 430/431 skipped, 600…, last LC bit 1397.
        assert_eq!(raw_bit(signalling_bit(0)), 410);
        assert_eq!(raw_bit(signalling_bit(19)), 429);
        assert_eq!(raw_bit(signalling_bit(20)), 432);
        assert_eq!(raw_bit(signalling_bit(40)), 600);
        assert_eq!(raw_bit(signalling_bit(239)), 1397);
    }

    #[test]
    fn an_ldu_is_864_dibits() {
        assert_eq!(LDU_DATA_BITS, 1680);
        // + the final status dibit
        assert_eq!(
            raw_bit(LDU_DATA_BITS - 1) + 1 + 2,
            2 * crate::framer::LDU_DIBITS
        );
    }

    #[test]
    fn every_ldu_bit_is_used_once() {
        let mut used = vec![0u8; LDU_DATA_BITS];
        for &b in &IMBE_BASE {
            for j in 0..IMBE_BITS {
                used[imbe_bit(b, j)] += 1;
            }
        }
        for j in 0..SIGNALLING_BITS {
            used[signalling_bit(j)] += 1;
        }
        let lsd = IMBE_BASE[8] - 32;
        for u in &mut used[lsd..lsd + 32] {
            *u += 1;
        }
        assert!(used[PAYLOAD..].iter().all(|&u| u == 1));
    }
}
