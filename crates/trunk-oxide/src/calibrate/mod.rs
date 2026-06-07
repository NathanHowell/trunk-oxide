//! Automatic SDR gain calibration.
//!
//! Sweeps a dongle's supported gain steps, measures the noise floor and
//! overload at each, and picks the gain with the best noise figure that stays
//! clear of clipping. Designed so a later *runtime* sweep reuses all of this:
//! the only hardware-specific piece is the [`IqProbe`] implementation.
//!
//! Layering:
//! - [`measure`] — IQ → [`Measurement`] (pure, tested)
//! - [`select`] — `&[GainPoint]` → [`GainSelection`] (pure, tested)
//! - [`probe`] — the [`IqProbe`] acquisition seam (hardware)
//! - [`calibrate`] — the orchestrator that ties them together
//!
//! Pre-flight acquisition lives in [`preflight`]; it opens each device
//! standalone before trunk-recorder builds its flowgraph, so nothing is
//! perturbed.

pub mod measure;
pub mod preflight;
pub mod probe;
pub mod select;

use anyhow::Result;

pub use probe::IqProbe;
pub use select::{GainSelection, SelectReason};

use measure::{MeasureParams, measure};
use select::{GainPoint, SelectParams, select_gain};

/// All tunables for one calibration run, with hardware-sane defaults.
#[derive(Debug, Clone, Copy)]
pub struct CalibrationPolicy {
    /// FFT length for the noise-floor periodogram.
    pub fft_size: usize,
    /// Percentile of the PSD taken as the noise floor (`0.0..=1.0`).
    pub noise_floor_percentile: f64,
    /// |I|/|Q| magnitude counted as saturated (normalized full-scale ~`1.0`).
    pub sat_threshold: f32,
    /// Clipping fraction above which a gain is vetoed as overloaded.
    pub sat_limit: f64,
    /// Floor-vs-gain slope marking the front-end-limited regime (dB/dB).
    pub knee_slope: f64,
    /// Complex samples to capture per gain step. Must comfortably exceed
    /// `fft_size` so Welch averaging has several segments.
    pub capture_samples: usize,
    /// Samples to discard after each gain change, letting the tuner settle.
    pub settle_samples: usize,
}

impl Default for CalibrationPolicy {
    fn default() -> Self {
        Self {
            fft_size: 4096,
            // Noise lives in the quiet bins; a low percentile tracks it even
            // with several in-band carriers.
            noise_floor_percentile: 0.25,
            // RTL output is ~[-1, 1]; the top codes land near 0.99.
            sat_threshold: 0.95,
            // 0.1% of samples clipping is already audible overload.
            sat_limit: 1e-3,
            // Allow margin below the ideal 1.0 dB/dB asymptote.
            knee_slope: 0.6,
            // ~0.1 s at 2.4 Msps; ~64 Welch segments at fft_size 4096.
            capture_samples: 1 << 18,
            // ~5 ms of settle at typical rates; the probe also re-runs the
            // flowgraph per step which adds its own warm-up.
            settle_samples: 1 << 14,
        }
    }
}

impl CalibrationPolicy {
    fn measure_params(&self) -> MeasureParams {
        MeasureParams {
            fft_size: self.fft_size,
            noise_floor_percentile: self.noise_floor_percentile,
            sat_threshold: self.sat_threshold,
        }
    }

    fn select_params(&self) -> SelectParams {
        SelectParams {
            sat_limit: self.sat_limit,
            knee_slope: self.knee_slope,
        }
    }
}

/// Sweep every supported gain on `probe`, measure each, and select the best.
///
/// `fallback_gain` is returned if the sweep yields too few usable points (e.g.
/// the device produced no samples). Capture errors on individual gain steps are
/// logged and skipped rather than aborting the whole sweep.
pub fn calibrate(
    probe: &mut dyn IqProbe,
    policy: &CalibrationPolicy,
    fallback_gain: f64,
) -> Result<GainSelection> {
    let gains = probe.supported_gains()?;
    if gains.is_empty() {
        tracing::warn!("device reported no gain steps; keeping configured gain");
        return Ok(GainSelection {
            gain_db: fallback_gain,
            noise_floor_db: f64::NAN,
            reason: SelectReason::Fallback,
        });
    }

    let mparams = policy.measure_params();
    let mut points = Vec::with_capacity(gains.len());
    for gain in gains {
        match probe.capture(gain) {
            Ok(iq) => {
                let measurement = measure(&iq, &mparams);
                tracing::debug!(
                    gain_db = gain,
                    noise_floor_db = measurement.noise_floor_db,
                    sat_frac = measurement.sat_frac,
                    "gain sweep step"
                );
                points.push(GainPoint {
                    gain_db: gain,
                    measurement,
                });
            }
            Err(e) => {
                tracing::warn!(gain_db = gain, error = %e, "gain sweep capture failed; skipping step");
            }
        }
    }

    let selection = select_gain(&points, &policy.select_params(), fallback_gain);
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;

    /// A probe backed by synthetic IQ generators, one per gain step.
    struct MockProbe {
        gains: Vec<f64>,
        make_iq: Box<dyn Fn(f64) -> Vec<Complex32>>,
    }

    impl IqProbe for MockProbe {
        fn supported_gains(&mut self) -> Result<Vec<f64>> {
            Ok(self.gains.clone())
        }
        fn capture(&mut self, gain_db: f64) -> Result<Vec<Complex32>> {
            Ok((self.make_iq)(gain_db))
        }
    }

    fn noise(n: usize, amp: f32, seed: u64) -> Vec<Complex32> {
        let mut s = seed | 1;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f32 / (1u64 << 52) as f32 * 2.0 - 1.0
        };
        (0..n)
            .map(|_| Complex32::new(next() * amp, next() * amp))
            .collect()
    }

    #[test]
    fn end_to_end_picks_frontend_knee() {
        // Amplitude flat until 20 dB (ADC-limited), then doubling per 6 dB
        // (front-end-limited). Knee should land at the first step past 20.
        let mut probe = MockProbe {
            gains: vec![0.0, 10.0, 20.0, 26.0, 32.0],
            make_iq: Box::new(|g| {
                let amp = if g <= 20.0 {
                    0.03
                } else {
                    0.03 * 2.0f32.powf((g as f32 - 20.0) / 6.0)
                };
                // seed varies with gain so captures aren't identical
                noise(1 << 16, amp, (g as u64) + 1)
            }),
        };
        let sel = calibrate(&mut probe, &CalibrationPolicy::default(), 99.0).unwrap();
        assert_eq!(sel.reason, SelectReason::Knee);
        assert_eq!(sel.gain_db, 26.0);
    }

    #[test]
    fn no_gain_steps_falls_back() {
        let mut probe = MockProbe {
            gains: vec![],
            make_iq: Box::new(|_| Vec::new()),
        };
        let sel = calibrate(&mut probe, &CalibrationPolicy::default(), 33.0).unwrap();
        assert_eq!(sel.reason, SelectReason::Fallback);
        assert_eq!(sel.gain_db, 33.0);
    }
}
