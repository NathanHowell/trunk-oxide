//! Dibit/symbol conventions shared by every stage.
//!
//! Soft symbols are in deviation units: the four ideal levels are ±1 and ±3,
//! which for differential QPSK are phase changes of ±45° and ±135° (one unit
//! is π/4). The scale is circular: +4 and −4 are the same phase, so distances
//! go through [`wrap`].

/// P25 Phase 1 symbol rate.
pub const SYMBOL_RATE: f64 = 4800.0;

/// The 48-bit frame sync, first transmitted dibit in the top bits.
pub const FRAME_SYNC: u64 = 0x5575_F5FF_77FF;
pub const SYNC_DIBITS: usize = 24;

/// Every 36th dibit of a frame (counting from the first sync dibit) is a
/// status symbol, not data.
pub const STATUS_PERIOD: usize = 36;

/// Dibit to symbol level (TIA-102.BAAA): 01 → +3, 00 → +1, 10 → −1, 11 → −3.
pub const LEVELS: [f32; 4] = [1.0, 3.0, -1.0, -3.0];

pub const fn is_status(frame_dibit: usize) -> bool {
    (frame_dibit + 1).is_multiple_of(STATUS_PERIOD)
}

/// Fold a soft symbol onto the circle [−4, 4).
pub fn wrap(s: f32) -> f32 {
    (s + 4.0).rem_euclid(8.0) - 4.0
}

/// Nearest dibit on the circle.
pub fn slice(s: f32) -> u8 {
    let s = wrap(s);
    if s >= 2.0 {
        1
    } else if s >= 0.0 {
        0
    } else if s >= -2.0 {
        2
    } else {
        3
    }
}

/// Squared circular distance from a soft symbol to a dibit's level.
pub fn dist2(s: f32, dibit: u8) -> f32 {
    let d = wrap(s - LEVELS[usize::from(dibit)]);
    d * d
}

/// The dibits of [`FRAME_SYNC`] in transmission order.
pub const fn sync_dibits() -> [u8; SYNC_DIBITS] {
    let mut out = [0u8; SYNC_DIBITS];
    let mut i = 0;
    while i < SYNC_DIBITS {
        #[allow(clippy::cast_possible_truncation)]
        let d = ((FRAME_SYNC >> (46 - 2 * i)) & 3) as u8;
        out[i] = d;
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slicing_matches_op25_thresholds() {
        // op25 fsk4_slicer with levels {-2, 0, 2, 4}.
        for (s, d) in [
            (2.5, 1),
            (1.9, 0),
            (0.0, 0),
            (-0.1, 2),
            (-2.0, 2),
            (-2.1, 3),
        ] {
            assert_eq!(slice(s), d, "{s}");
        }
    }

    #[test]
    fn distance_wraps_around_the_circle() {
        // 180° sits between +135° and −135°.
        assert!((dist2(4.0, 1) - 1.0).abs() < 1e-6);
        assert!((dist2(4.0, 3) - 1.0).abs() < 1e-6);
        assert!((dist2(3.9, 3) - 1.21).abs() < 1e-5);
        assert_eq!(slice(4.2), 3);
    }

    #[test]
    fn sync_is_all_outer_symbols() {
        assert!(sync_dibits().iter().all(|&d| d == 1 || d == 3));
        assert!(is_status(35) && is_status(71) && !is_status(36));
    }
}
