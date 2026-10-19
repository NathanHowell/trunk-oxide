//! How much of a sampled band carries a channel cleanly.
//!
//! An RTL-SDR's decimation filter rolls off toward Nyquist (half the sample
//! rate). Measured on 2026-10-19 at 1.872 and 2.4 MS/s: the noise floor sags
//! ~3 dB from 85% of Nyquist outward, and past ~93% noise folded in from
//! beyond the edge fills back in, so a channel out there loses signal faster
//! than noise and its symbols smear (a P25 phase 2 channel at 97% had eight
//! times the untrusted frames of its neighbours). Channels belong inside
//! [`USABLE`] of Nyquist; the radio prefers the source a channel sits
//! deepest in, and the solver plans with the same rule.

/// The share of Nyquist a channel's far edge may reach.
pub const USABLE: f64 = 0.85;

/// How far a 12.5 kHz channel reaches either side of its centre.
pub const CHANNEL_HALF_WIDTH_HZ: f64 = 6_250.0;

/// How deep in a band sampled at `rate_hz` a channel `offset_hz` from its
/// centre sits: the share of Nyquist its far edge reaches (0 at the centre,
/// 1 at the edge).
#[must_use]
pub fn depth(offset_hz: f64, rate_hz: f64) -> f64 {
    (offset_hz.abs() + CHANNEL_HALF_WIDTH_HZ) / (rate_hz / 2.0)
}

/// Whether that channel sits inside the usable band.
#[must_use]
pub fn usable(offset_hz: f64, rate_hz: f64) -> bool {
    depth(offset_hz, rate_hz) <= USABLE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channels_of_2026_10_19() {
        // source 2: 773.7755 MHz at 1.872 MS/s
        let off = |mhz: f64| (mhz - 773.7755) * 1e6;
        assert!(!usable(off(774.68125), 1.872e6), "97% of Nyquist");
        assert!((depth(off(774.68125), 1.872e6) - 0.974).abs() < 1e-3);
        assert!(usable(off(774.40625), 1.872e6));
        // at 2.4 MS/s the same channels clear the roll-off
        assert!(usable(off(774.68125), 2.4e6));
        assert!(usable(off(772.90625), 2.4e6));
        assert!(depth(0.0, 1e6) > 0.0, "a channel has width");
    }
}
