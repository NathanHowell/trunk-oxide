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
//! outcome to the metrics, and [`live`] measures the floor again on the
//! running stream.

pub mod cache;
pub mod live;
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
    /// The SNR (dB) the ADC's noise may take at the chosen gain
    /// ([`select::NoiseModel::penalty_db`]).
    pub penalty_budget_db: f64,
    /// The worst rms residual (dB) of the noise model still trusted.
    pub max_fit_rms_db: f64,
    /// Complex samples to capture per gain step. Must comfortably exceed
    /// `fft_size` so Welch averaging has several segments.
    pub capture_samples: usize,
    /// How far (dB) past the budget a cached pick's penalty may drift (the
    /// front end warming, say) before the device is swept again.
    pub recheck_hysteresis_db: f64,
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
            // The front end's noise 9 dB over the ADC's: sensitivity within
            // half a dB of the front end's, the rest of the range kept as
            // overload headroom.
            penalty_budget_db: 0.5,
            // 2026-10-19's four dongles fitted to 0.3-0.5 dB rms.
            max_fit_rms_db: 1.5,
            // ~0.1 s at 2.4 Msps; ~64 Welch segments at fft_size 4096.
            capture_samples: 1 << 18,
            // Up to a 1 dB penalty before re-sweeping: a warm afternoon
            // moves the floor ~2 dB, which must not walk the gain about
            // (73121177 went 40.2 -> 36.4 -> 29.7 dB across two deploys on
            // 2026-10-19 under a 2 dB floor-moved rule).
            recheck_hysteresis_db: 0.5,
            // ~5 ms of settle at typical rates; the probe also re-runs the
            // flowgraph per step which adds its own warm-up.
            settle_samples: 1 << 14,
        }
    }
}

impl CalibrationPolicy {
    const fn measure_params(&self) -> MeasureParams {
        MeasureParams {
            fft_size: self.fft_size,
            noise_floor_percentile: self.noise_floor_percentile,
            sat_threshold: self.sat_threshold,
        }
    }

    const fn select_params(&self) -> SelectParams {
        SelectParams {
            sat_limit: self.sat_limit,
            penalty_budget_db: self.penalty_budget_db,
            max_fit_rms_db: self.max_fit_rms_db,
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

/// A re-check of a device's last pick.
#[derive(Debug, Clone, Copy)]
pub struct Recheck {
    /// The pick stands.
    pub keep: bool,
    /// How far the floor at that gain moved since the sweep, dB.
    pub moved_db: f64,
    /// The SNR the ADC costs at that gain now, dB (`NaN` if unknown).
    pub penalty_db: f64,
}

/// Whether a device's last pick still stands.
///
/// One capture at that gain re-estimates the front end against the noise
/// model fitted to the cached sweep (the ADC's floor is the converter's own
/// and holds): the pick stands while its penalty stays within the budget
/// plus [`CalibrationPolicy::recheck_hysteresis_db`] and it does not
/// saturate. A front end that drifted a little keeps its gain; one that
/// drifted far, or a sweep the model cannot describe, is swept again.
pub fn recheck(
    probe: &mut dyn IqProbe,
    policy: &CalibrationPolicy,
    cached: &cache::Cached,
) -> Result<Recheck> {
    let unknown = Recheck {
        keep: false,
        moved_db: f64::NAN,
        penalty_db: f64::NAN,
    };
    if !probe.supported_gains()?.contains(&cached.gain_db) {
        return Ok(unknown);
    }
    let curve = unsaturated(policy, cached);
    let Some(model) = model(policy, cached) else {
        return Ok(unknown);
    };
    let m = measure(&probe.capture(cached.gain_db)?, &policy.measure_params());
    let moved_db = m.noise_floor_db - cached.noise_floor_db;
    let Some(now) = model.with_floor_at(cached.gain_db, m.noise_floor_db) else {
        return Ok(Recheck {
            moved_db,
            ..unknown
        });
    };
    let penalty_db = now.penalty_db(cached.gain_db);
    // What a sweep would now pick from the same steps: the lowest within
    // budget, else the highest (ADC-limited). If that is the cached gain, a
    // sweep cannot do better; otherwise the cached gain stands while it is
    // within the hysteresis.
    let gains: Vec<f64> = curve.iter().map(|&(g, _)| g).collect();
    let would_pick = gains
        .iter()
        .copied()
        .find(|&g| now.penalty_db(g) <= policy.penalty_budget_db)
        .or_else(|| gains.last().copied());
    let still_best = would_pick.is_some_and(|g| (g - cached.gain_db).abs() < 1e-9);
    Ok(Recheck {
        keep: (still_best || penalty_db <= policy.penalty_budget_db + policy.recheck_hysteresis_db)
            && m.sat_frac <= policy.sat_limit,
        moved_db,
        penalty_db,
    })
}

/// A cached sweep's steps that did not saturate, as (gain, floor dB).
fn unsaturated(policy: &CalibrationPolicy, cached: &cache::Cached) -> Vec<(f64, f64)> {
    cached
        .sweep
        .iter()
        .filter(|p| p.sat_frac <= policy.sat_limit)
        .map(|p| (p.gain_db, p.noise_floor_db))
        .collect()
}

/// The noise model fitted to a cached sweep, if it fits well enough to
/// trust.
#[must_use]
pub fn model(policy: &CalibrationPolicy, cached: &cache::Cached) -> Option<select::NoiseModel> {
    select::NoiseModel::fit(&unsaturated(policy, cached))
        .filter(|m| m.rms_db <= policy.max_fit_rms_db)
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
                penalty_db: f64::NAN,
                reason: SelectReason::Fallback,
            },
            points: Vec::new(),
        });
    }

    let mparams = policy.measure_params();
    let mut points = Vec::with_capacity(gains.len());
    let mut failed = 0usize;
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
                tracing::warn!(gain_db = gain, error = %e, "gain sweep capture failed");
                failed += 1;
            }
        }
    }

    // A device that failed mid-sweep (on 2026-10-19 a hot dongle dropped off
    // the bus, and the steps before it picked 29.7 dB at a 9 dB penalty) has
    // not shown its curve: keep the configured gain and cache nothing.
    let selection = if failed > 0 {
        tracing::warn!(failed, "gain sweep incomplete; keeping the configured gain");
        GainSelection {
            gain_db: fallback_gain,
            noise_floor_db: f64::NAN,
            penalty_db: f64::NAN,
            reason: SelectReason::Fallback,
        }
    } else {
        select_gain(&points, &policy.select_params(), fallback_gain)
    };
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
            ((s >> 11) as f32 / (1u64 << 52) as f32).mul_add(2.0, -1.0)
        };
        (0..n)
            .map(|_| Complex32::new(next() * amp, next() * amp))
            .collect()
    }

    #[test]
    fn end_to_end_picks_the_lowest_gain_within_budget() {
        // ADC noise plus a front end that equals it at 20 dB and tracks gain
        // 1:1; the budget (0.5 dB) needs the front end 9.1 dB over the ADC.
        let mut probe = MockProbe {
            gains: vec![0.0, 10.0, 20.0, 26.0, 32.0, 38.0],
            make_iq: Box::new(|g| {
                let power = 1.0 + 10f64.powf((g - 20.0) / 10.0);
                let amp = (0.02 * power.sqrt()) as f32;
                // seed varies with gain so captures aren't identical
                noise(1 << 16, amp, (g as u64) + 1)
            }),
        };
        let sweep = calibrate(&mut probe, &CalibrationPolicy::default(), 99.0).unwrap();
        let sel = sweep.selection;
        assert_eq!(sel.reason, SelectReason::Budget, "{sel:?}");
        assert_eq!(sel.gain_db, 32.0, "{sel:?}");
        assert_eq!(sweep.points.len(), 6);
        let curve = sweep.curve(1e-3);
        assert!(
            curve.starts_with("0.0:") && curve.contains(" 32.0:"),
            "{curve}"
        );
    }

    #[test]
    fn a_sweep_with_failed_captures_falls_back() {
        struct Dying(usize);
        impl IqProbe for Dying {
            fn supported_gains(&mut self) -> Result<Vec<f64>> {
                Ok(vec![0.0, 10.0, 20.0, 26.0, 32.0, 38.0])
            }
            fn capture(&mut self, gain_db: f64) -> Result<Vec<Complex32>> {
                self.0 += 1;
                anyhow::ensure!(self.0 <= 4, "i2c write failed");
                let power = 1.0 + 10f64.powf((gain_db - 20.0) / 10.0);
                Ok(noise(1 << 16, (0.02 * power.sqrt()) as f32, self.0 as u64))
            }
        }
        let sweep = calibrate(&mut Dying(0), &CalibrationPolicy::default(), 40.0).unwrap();
        assert_eq!(sweep.selection.reason, SelectReason::Fallback);
        assert_eq!(sweep.selection.gain_db, 40.0);
        assert_eq!(sweep.points.len(), 4);
    }

    /// A dongle whose front end equals its ADC at `equal_at` dB (slope 1),
    /// as a probe and as the cached sweep it would have produced.
    fn dongle(equal_at: f64) -> (MockProbe, cache::Cached) {
        const GAINS: [f64; 7] = [0.0, 10.0, 20.0, 26.0, 32.0, 38.0, 44.0];
        let power = move |g: f64| 1.0 + 10f64.powf((g - equal_at) / 10.0);
        let mut probe = MockProbe {
            gains: GAINS.to_vec(),
            make_iq: Box::new(move |g| noise(1 << 16, (0.02 * power(g).sqrt()) as f32, 7)),
        };
        let policy = CalibrationPolicy::default();
        let sweep = calibrate(&mut probe, &policy, 0.0).unwrap();
        let cached = cache::Cached {
            gain_db: sweep.selection.gain_db,
            noise_floor_db: sweep.selection.noise_floor_db,
            reason: sweep.selection.reason.label().into(),
            at: 0,
            algorithm: cache::ALGORITHM,
            sweep: sweep
                .points
                .iter()
                .map(|p| cache::SweepPoint {
                    gain_db: p.gain_db,
                    noise_floor_db: p.measurement.noise_floor_db,
                    sat_frac: p.measurement.sat_frac,
                })
                .collect(),
        };
        (probe, cached)
    }

    #[test]
    fn a_recheck_keeps_what_a_sweep_would_pick_again() {
        let policy = CalibrationPolicy::default();
        // within budget at 32 dB; unchanged, it stands
        let (mut probe, cached) = dongle(20.0);
        assert_eq!(cached.gain_db, 32.0);
        assert!(recheck(&mut probe, &policy, &cached).unwrap().keep);
        // ADC-limited (the front end equals the ADC only at 44 dB): the top
        // gain costs more than the budget, but a sweep would pick it again
        let (mut probe, cached) = dongle(44.0);
        assert_eq!(
            (cached.gain_db, cached.reason.as_str()),
            (44.0, "adc_limited")
        );
        let r = recheck(&mut probe, &policy, &cached).unwrap();
        assert!(r.keep && r.penalty_db > 1.0, "{r:?}");
    }

    #[test]
    fn a_recheck_sweeps_again_once_the_front_end_drifted_far() {
        let policy = CalibrationPolicy::default();
        let (_, cached) = dongle(20.0);
        // the same dongle with 8 dB less front-end gain: at 32 dB the front
        // end is only 4 dB over the ADC, a 1.5 dB penalty
        let (mut warmer, _) = dongle(28.0);
        let r = recheck(&mut warmer, &policy, &cached).unwrap();
        assert!(!r.keep && r.penalty_db > 1.0, "{r:?}");
        // 1 dB less: within the hysteresis
        let (mut warm, _) = dongle(21.0);
        assert!(recheck(&mut warm, &policy, &cached).unwrap().keep);
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
