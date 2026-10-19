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
//! Pre-flight acquisition lives in [`rtl`]; it opens each dongle
//! standalone before the sources are built, so nothing is perturbed. [`cache`] keeps each device's last sweep across restarts and
//! [`recheck`] decides whether it still stands; [`report`] hands the
//! outcome to the metrics.

pub mod cache;
pub mod measure;
pub mod probe;
pub mod report;
pub mod rtl;
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
    /// The least gain (dB) that slope is measured across.
    pub knee_span_db: f64,
    /// How far (dB) above the knee the pick sits.
    pub margin_db: f64,
    /// Complex samples to capture per gain step. Must comfortably exceed
    /// `fft_size` so Welch averaging has several segments.
    pub capture_samples: usize,
    /// How far (dB) the noise floor at a cached gain may have moved before
    /// the device is swept again rather than keeping the cached pick.
    pub recheck_tolerance_db: f64,
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
            // RTL steps run 0.5–4 dB apart and a 0.1 s floor estimate
            // scatters a few tenths of a dB, so a slope over one small step
            // is noise; 2.5 dB spans at least two of the fine steps.
            knee_span_db: 2.5,
            // At the knee the front end's noise only equals the ADC's (a
            // ~3 dB noise-figure penalty); 6 dB above it the penalty is ~1.
            margin_db: 6.0,
            // ~0.1 s at 2.4 Msps; ~64 Welch segments at fft_size 4096.
            capture_samples: 1 << 18,
            // A gain step is ~1–3 dB on an RTL tuner; floor drift within
            // that is noise, beyond it the band changed.
            recheck_tolerance_db: 2.0,
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
            knee_span_db: self.knee_span_db,
            margin_db: self.margin_db,
        }
    }
}

/// A full sweep: the pick and the curve it was picked from.
#[derive(Debug, Clone)]
pub struct Sweep {
    pub selection: GainSelection,
    /// Every step that produced a measurement, in gain order.
    pub points: Vec<GainPoint>,
}

impl Sweep {
    /// The curve as one log field: `gain:floor` per step, `!` after a step
    /// that saturated. E.g. `0.0:-60.1 0.9:-60.0 … 49.6:-31.2!`.
    pub fn curve(&self, sat_limit: f64) -> String {
        self.points
            .iter()
            .map(|p| {
                let m = p.measurement;
                let sat = if m.sat_frac > sat_limit { "!" } else { "" };
                format!("{:.1}:{:.1}{sat}", p.gain_db, m.noise_floor_db)
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Whether a device's last pick still stands.
///
/// One capture at that gain: its noise floor must be within
/// [`CalibrationPolicy::recheck_tolerance_db`] of the floor measured then,
/// and not saturating. How far the floor moved is returned with the verdict
/// for the log.
pub fn recheck(
    probe: &mut dyn IqProbe,
    policy: &CalibrationPolicy,
    cached: &cache::Cached,
) -> Result<(bool, f64)> {
    if !probe.supported_gains()?.contains(&cached.gain_db) {
        return Ok((false, f64::NAN));
    }
    let m = measure(&probe.capture(cached.gain_db)?, &policy.measure_params());
    let moved = m.noise_floor_db - cached.noise_floor_db;
    Ok((
        moved.abs() <= policy.recheck_tolerance_db && m.sat_frac <= policy.sat_limit,
        moved,
    ))
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
) -> Result<Sweep> {
    let gains = probe.supported_gains()?;
    if gains.is_empty() {
        tracing::warn!("device reported no gain steps; keeping configured gain");
        return Ok(Sweep {
            selection: GainSelection {
                gain_db: fallback_gain,
                noise_floor_db: f64::NAN,
                reason: SelectReason::Fallback,
            },
            points: Vec::new(),
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
    Ok(Sweep { selection, points })
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
        let sweep = calibrate(&mut probe, &CalibrationPolicy::default(), 99.0).unwrap();
        let sel = sweep.selection;
        assert_eq!(sel.reason, SelectReason::Knee);
        // Knee confirmed at 26 (by 32); the margin asks for ≥ 32.
        assert_eq!(sel.gain_db, 32.0);
        assert_eq!(sweep.points.len(), 5);
        let curve = sweep.curve(1e-3);
        assert!(
            curve.starts_with("0.0:") && curve.contains(" 32.0:"),
            "{curve}"
        );
    }

    #[test]
    fn no_gain_steps_falls_back() {
        let mut probe = MockProbe {
            gains: vec![],
            make_iq: Box::new(|_| Vec::new()),
        };
        let sel = calibrate(&mut probe, &CalibrationPolicy::default(), 33.0)
            .unwrap()
            .selection;
        assert_eq!(sel.reason, SelectReason::Fallback);
        assert_eq!(sel.gain_db, 33.0);
    }
}
