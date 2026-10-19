//! A source's software clock correction: what is added to every channel
//! tuned on it.
//!
//! The dongle's crystal is off by a fraction of a ppm that the integer
//! hardware correction cannot express; within one dongle's band a fixed
//! offset in Hz is a good enough model of it. It starts where the
//! configuration puts it (`freq_correction_hz`) and, when adaptive, follows
//! the FLL errors the source's channels close with, so it tracks the
//! crystal's thermal and ageing drift: a slow average, a small gain and a
//! dead band, so it follows drift without chasing per-call noise.

/// EMA smoothing (~10 calls of memory).
const ALPHA: f64 = 0.2;
/// The share of the smoothed error applied in one step.
const GAIN: f64 = 0.3;
/// Errors this small are noise, not drift.
const DEADBAND_HZ: f64 = 30.0;
/// The largest single step.
const MAX_STEP_HZ: f64 = 50.0;
/// The largest correction believable.
const MAX_CORRECTION_HZ: f64 = 5_000.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Correction {
    hz: f64,
    adaptive: bool,
    /// The smoothed FLL error, Hz.
    ema: f64,
}

impl Correction {
    #[must_use]
    pub const fn new(hz: f64, adaptive: bool) -> Self {
        Self {
            hz,
            adaptive,
            ema: 0.0,
        }
    }

    /// The correction now, Hz.
    #[must_use]
    pub const fn hz(&self) -> f64 {
        self.hz
    }

    /// A channel on the source closed with the FLL `freq_error_hz` off; true
    /// if the correction moved. Empirically d(error)/d(correction) > 0, so
    /// a negative error calls for a larger correction.
    pub fn observe(&mut self, freq_error_hz: f64) -> bool {
        if !self.adaptive {
            return false;
        }
        self.ema = ALPHA.mul_add(freq_error_hz, (1.0 - ALPHA) * self.ema);
        if self.ema.abs() <= DEADBAND_HZ {
            return false;
        }
        let step = (-GAIN * self.ema).clamp(-MAX_STEP_HZ, MAX_STEP_HZ);
        self.hz = (self.hz + step).clamp(-MAX_CORRECTION_HZ, MAX_CORRECTION_HZ);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_correction_never_moves() {
        let mut c = Correction::new(185.0, false);
        assert!(!c.observe(-400.0));
        assert!((c.hz() - 185.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_adaptive_one_ignores_noise_and_follows_drift_in_bounded_steps() {
        let mut c = Correction::new(0.0, true);
        // within the dead band once smoothed: no step
        assert!(!c.observe(100.0), "ema 20 Hz");
        // a sustained +200 Hz error: the correction goes down, 50 Hz at most
        let mut last = c.hz();
        for _ in 0..10 {
            if c.observe(200.0) {
                assert!(c.hz() < last && last - c.hz() <= MAX_STEP_HZ + 1e-9);
                last = c.hz();
            }
        }
        assert!(c.hz() < -100.0, "{}", c.hz());
        for _ in 0..1000 {
            c.observe(1e6);
        }
        assert!((c.hz() + MAX_CORRECTION_HZ).abs() < 1e-9, "bounded");
    }
}
