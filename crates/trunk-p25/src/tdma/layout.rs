//! Where things are in a Phase 2 burst (TIA-102.BBAC).
//!
//! The outbound channel is a continuous stream of 180-dibit bursts, twelve to
//! a 360 ms superframe, the two logical channels taking turns. Each burst
//! starts with the 20-dibit inter-slot signalling channel (ISCH), then four
//! unscrambled DUID dibits placed around 156 scrambled payload dibits.
//!
//! Positions are burst dibits, 0 at the first ISCH dibit; op25 counts from
//! the eleventh (its `burstp`), which is why the scrambling mask is offset.

use std::ops::Range;

pub const BURST_DIBITS: usize = 180;
pub const ISCH_DIBITS: usize = 20;
pub const BURSTS_PER_SUPERFRAME: usize = 12;
pub const SUPERFRAME_DIBITS: usize = BURST_DIBITS * BURSTS_PER_SUPERFRAME;

/// The DUID's four dibits, first-sent first.
pub const DUID_AT: [usize; 4] = [20, 57, 142, 179];

pub const PAYLOAD_DIBITS: usize = 156;
pub const PAYLOAD_BITS: usize = 2 * PAYLOAD_DIBITS;

/// Payload dibit `p`'s place in the burst: the dibits after the first DUID
/// dibit, stepping over the other three.
pub const fn payload_dibit(p: usize) -> usize {
    ISCH_DIBITS + 1 + p + (p >= 36) as usize + (p >= 120) as usize
}

/// Index into the superframe's scrambling sequence of payload dibit `p` in
/// superframe slot `slot`.
pub const fn mask_index(slot: usize, p: usize) -> usize {
    slot * BURST_DIBITS + payload_dibit(p) - 10
}

/// Which logical channel (0 or 1) each superframe slot carries: they
/// alternate, except that the two SACCH slots at the end are swapped.
pub const CHANNEL_OF_SLOT: [u8; BURSTS_PER_SUPERFRAME] = [0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 1, 0];

/// Slots whose ISCH is the sync word rather than information.
pub const fn is_sync_slot(slot: usize) -> bool {
    slot % 4 >= 2
}

/// A voice codeword is 36 payload dibits.
pub const VOICE_DIBITS: usize = 36;
/// Voice codewords of a 4V burst, at these payload dibits.
pub const VOICE_4V: [usize; 4] = [0, 36, 84, 120];
/// Voice codewords of a 2V burst.
pub const VOICE_2V: [usize; 2] = [0, 36];
/// A 4V burst's share of the encryption sync data (ESS-B): 4 hexbits.
pub const ESS_B: Range<usize> = 72..84;
/// A 2V burst's encryption sync parity (ESS-A): 28 hexbits.
pub const ESS_A: Range<usize> = 72..156;

/// FACCH carries no data in these payload dibits.
pub const FACCH_GAP: Range<usize> = 67..88;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_matches_op25s_offsets() {
        // op25 (`p25p2_tdma.cc`): voice at burstp 11, 48, 96, 133; DUID at 10,
        // 47, 132, 169; FACCH reads 11..47, 48..79, 100..132, 133..169.
        let burstp = |p| payload_dibit(p) - 10;
        assert_eq!(VOICE_4V.map(burstp), [11, 48, 96, 133]);
        assert_eq!(DUID_AT.map(|d| d - 10), [10, 47, 132, 169]);
        assert_eq!(burstp(ESS_B.start), 84);
        assert_eq!(burstp(FACCH_GAP.start - 1), 78);
        assert_eq!(burstp(FACCH_GAP.end), 100);
        assert_eq!(payload_dibit(PAYLOAD_DIBITS - 1), BURST_DIBITS - 2);
        let all: Vec<usize> = (0..PAYLOAD_DIBITS).map(payload_dibit).collect();
        assert!(all.iter().all(|d| !DUID_AT.contains(d)));
        assert!(all.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(mask_index(1, 0), 180 + 11);
    }

    #[test]
    fn sync_slots_are_op25s() {
        let expected = [
            false, false, true, true, false, false, true, true, false, false, true, true,
        ];
        assert_eq!(std::array::from_fn(is_sync_slot), expected);
    }
}
