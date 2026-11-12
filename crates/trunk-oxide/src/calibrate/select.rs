//! Pure gain selection from a swept set of [`Measurement`]s.
//!
//! What a sweep measures at each gain is the ADC's own noise plus the front
//! end's, brought up by the gain:
//!
//! ```text
//! floor(g) = 10 log10(10^(A/10) + 10^((B + s·g)/10))
//! ```
//!
//! `A` is the ADC's floor, `B + s·g` the front end's (`s` near 1 in theory;
//! an R820T's steps are not exact dB, and on 2026-10-19 four dongles fitted
//! `s` 0.65–0.79 to within 0.3–0.5 dB rms). What a gain costs is the SNR the
//! ADC's noise takes from the front end's:
//!
//! ```text
//! penalty(g) = 10 log10(1 + 10^((A − B − s·g)/10)) dB
//! ```
//!
//! and the pick is the lowest gain whose penalty is within a budget: the
//! sensitivity is all but the front end's, with the most overload headroom
//! kept. The fit uses every point, so it is steady where a slope test on a
//! few adjacent steps (the knee rule this replaced) scattered by a 6 dB pick
//! between sweeps of one dongle, and it says when a dongle is truly
//! ADC-limited rather than guessing it from a flat-looking curve.
//!
//! 1. **Saturation veto**: gains whose clipping fraction exceeds the limit
//!    are dropped; hard ADC overload is never acceptable.
//! 2. **Fit** the model to what is left (a scan of `s`, each a linear least
//!    squares in power).
//! 3. **Budget**: the lowest gain whose penalty is within it.
//! 4. **Spur hook**: [`spur_overload_gain`] is a reserved seam for discrete
//!    intermod tones, which can only ever *lower* the pick.

use super::measure::Measurement;

/// One swept data point: a gain step and what we measured there.
#[derive(Debug, Clone, Copy)]
pub struct GainPoint {
    pub gain_db: f64,
    pub measurement: Measurement,
}

/// Why a particular gain was chosen, surfaced in logs so an operator can tell
/// a healthy calibration from a compromised RF environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectReason {
    /// The lowest gain within the SNR-loss budget. The good outcome.
    Budget,
    /// Overload cut the sweep off before the budget was met; picked the
    /// highest gain clear of saturation. The antenna is too hot for full
    /// sensitivity: add attenuation.
    SaturationLimited,
    /// Even the highest gain loses more than the budget to the ADC; picked
    /// it. Usually a weak antenna or a quiet front end.
    AdcLimited,
    /// Too few usable points, or a curve the model does not describe; the
    /// caller keeps its configured gain.
    Fallback,
}

impl SelectReason {
    /// The `reason` label of `trunk.oxide.source.gain_db`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::SaturationLimited => "saturation_limited",
            Self::AdcLimited => "adc_limited",
            Self::Fallback => "fallback",
        }
    }
}

/// The outcome of [`select_gain`].
#[derive(Debug, Clone, Copy)]
pub struct GainSelection {
    pub gain_db: f64,
    /// Noise floor measured at the chosen gain (relative dB), or `NaN` on
    /// fallback. The same relative scale as [`Measurement::noise_floor_db`].
    pub noise_floor_db: f64,
    /// The SNR the ADC costs at that gain, dB ([`NoiseModel::penalty_db`]);
    /// `NaN` on fallback.
    pub penalty_db: f64,
    pub reason: SelectReason,
}

/// Tunables for selection. Defaults live in
/// [`CalibrationPolicy`](super::CalibrationPolicy).
#[derive(Debug, Clone, Copy)]
pub struct SelectParams {
    /// Saturation fraction above which a gain is vetoed as overloaded.
    pub sat_limit: f64,
    /// The SNR (dB) the ADC's noise may take at the chosen gain.
    pub penalty_budget_db: f64,
    /// The worst rms fit residual (dB) still trusted.
    pub max_fit_rms_db: f64,
}

/// A dongle's noise against gain (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoiseModel {
    /// The ADC's floor, linear power.
    adc: f64,
    /// The front end's at 0 dB of gain, linear power.
    front: f64,
    /// dB of front-end noise per dB of gain.
    slope: f64,
    /// How well it fits the points it was fitted to, dB rms.
    pub rms_db: f64,
}

/// Fewer points than this cannot pin three parameters down.
const MIN_FIT_POINTS: usize = 4;
/// The slopes scanned, in thousandths of a dB per dB.
const SLOPES: std::ops::RangeInclusive<u32> = 300..=1500;

fn lin(db: f64) -> f64 {
    10f64.powf(db / 10.0)
}

fn db(lin: f64) -> f64 {
    10.0 * lin.log10()
}

impl NoiseModel {
    /// The model that best fits `points` (gain, floor dB), or `None` if
    /// there are too few or no slope gives positive noise terms.
    ///
    /// For a fixed slope the floor is linear in its two powers,
    /// `a + b·x(g)` with `x = 10^(s·g/10)`; weighting each point by its own
    /// power makes the least squares one of relative error, which is dB
    /// error to first order. Scanning the slope finishes the fit.
    #[must_use]
    pub fn fit(points: &[(f64, f64)]) -> Option<Self> {
        if points.len() < MIN_FIT_POINTS {
            return None;
        }
        SLOPES
            .filter_map(|milli| {
                let slope = f64::from(milli) / 1000.0;
                // normal equations of sum(((a + b x) / y - 1)^2)
                let (mut s00, mut s01, mut s11, mut r0, mut r1) = (0.0, 0.0, 0.0, 0.0, 0.0);
                for &(g, floor) in points {
                    let (u, v) = (1.0 / lin(floor), lin(slope * g) / lin(floor));
                    s00 = f64::mul_add(u, u, s00);
                    s01 = f64::mul_add(u, v, s01);
                    s11 = f64::mul_add(v, v, s11);
                    r0 += u;
                    r1 += v;
                }
                let det = s00.mul_add(s11, -(s01 * s01));
                let adc = r0.mul_add(s11, -(r1 * s01)) / det;
                let front = s00.mul_add(r1, -(s01 * r0)) / det;
                (adc > 0.0 && front > 0.0).then(|| {
                    let mut m = Self {
                        adc,
                        front,
                        slope,
                        rms_db: 0.0,
                    };
                    m.rms_db = m.rms_over(points);
                    m
                })
            })
            .min_by(|a, b| a.rms_db.total_cmp(&b.rms_db))
    }

    fn rms_over(&self, points: &[(f64, f64)]) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let n = points.len() as f64;
        (points
            .iter()
            .map(|&(g, floor)| (self.floor_db(g) - floor).powi(2))
            .sum::<f64>()
            / n)
            .sqrt()
    }

    /// The floor the model expects at `gain_db`.
    #[must_use]
    pub fn floor_db(&self, gain_db: f64) -> f64 {
        db(self.adc + self.front_at(gain_db))
    }

    fn front_at(&self, gain_db: f64) -> f64 {
        self.front * lin(self.slope * gain_db)
    }

    /// The SNR (dB) the ADC's noise takes at `gain_db`.
    #[must_use]
    pub fn penalty_db(&self, gain_db: f64) -> f64 {
        db(1.0 + self.adc / self.front_at(gain_db))
    }

    /// The same dongle after its front end drifted (warmed, say), from one
    /// floor measured at `gain_db`: the ADC's floor is the converter's own
    /// and holds, so what moved is the front end. `None` if the floor is at
    /// or below the ADC's, which the model cannot explain.
    #[must_use]
    pub fn with_floor_at(&self, gain_db: f64, floor_db: f64) -> Option<Self> {
        let front_now = lin(floor_db) - self.adc;
        (front_now > 0.0).then(|| Self {
            front: front_now / lin(self.slope * gain_db),
            ..*self
        })
    }
}

/// Reserved hook for discrete-intermod (spur) overload detection.
///
/// A broadband floor can miss a single intermod *tone*. The intended
/// implementation watches specific PSD peaks grow faster than 1:1 with gain
/// (2:1 for 2nd-order, 3:1 for 3rd-order) across the sweep and returns the
/// highest gain still free of such growth. It needs per-gain peak tracking
/// not yet retained, so it never constrains the result today; it is wired in
/// [`select_gain`] such that it can only ever *lower* the selected gain.
#[must_use]
pub const fn spur_overload_gain(_points: &[GainPoint]) -> Option<f64> {
    None
}

/// Pick the best gain from a swept set of measurements.
///
/// `fallback_gain` is returned (with [`SelectReason::Fallback`]) when the
/// points cannot be trusted to decide.
#[must_use]
pub fn select_gain(
    points: &[GainPoint],
    params: &SelectParams,
    fallback_gain: f64,
) -> GainSelection {
    let fallback = GainSelection {
        gain_db: fallback_gain,
        noise_floor_db: f64::NAN,
        penalty_db: f64::NAN,
        reason: SelectReason::Fallback,
    };
    let mut pts: Vec<GainPoint> = points
        .iter()
        .copied()
        .filter(|p| p.measurement.noise_floor_db.is_finite())
        .collect();
    pts.sort_by(|a, b| a.gain_db.total_cmp(&b.gain_db));
    let Some(&lowest) = pts.first() else {
        return fallback;
    };

    let valid: Vec<GainPoint> = pts
        .iter()
        .copied()
        .filter(|p| p.measurement.sat_frac <= params.sat_limit)
        .collect();
    let any_saturated = valid.len() < pts.len();
    // Every gain overloads: the least bad, flagged loudly.
    if valid.is_empty() {
        return GainSelection {
            gain_db: lowest.gain_db,
            noise_floor_db: lowest.measurement.noise_floor_db,
            penalty_db: f64::NAN,
            reason: SelectReason::SaturationLimited,
        };
    }

    let curve: Vec<(f64, f64)> = valid
        .iter()
        .map(|p| (p.gain_db, p.measurement.noise_floor_db))
        .collect();
    let Some(model) = NoiseModel::fit(&curve).filter(|m| m.rms_db <= params.max_fit_rms_db) else {
        return fallback;
    };
    let pick = |p: &GainPoint, reason| GainSelection {
        gain_db: p.gain_db,
        noise_floor_db: p.measurement.noise_floor_db,
        penalty_db: model.penalty_db(p.gain_db),
        reason,
    };

    let top = valid[valid.len() - 1];
    let mut selection = valid
        .iter()
        .find(|p| model.penalty_db(p.gain_db) <= params.penalty_budget_db)
        .map_or_else(
            || {
                let reason = if any_saturated {
                    SelectReason::SaturationLimited
                } else {
                    SelectReason::AdcLimited
                };
                pick(&top, reason)
            },
            |p| pick(p, SelectReason::Budget),
        );

    // Spur hook (currently a no-op): may only lower the chosen gain.
    if let Some(cap) = spur_overload_gain(&valid)
        && cap < selection.gain_db
        && let Some(capped) = valid.iter().rev().find(|p| p.gain_db <= cap)
    {
        selection = pick(capped, SelectReason::SaturationLimited);
    }
    selection
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> SelectParams {
        SelectParams {
            sat_limit: 1e-3,
            penalty_budget_db: 0.5,
            max_fit_rms_db: 1.5,
        }
    }

    /// The R820T's gain steps.
    const STEPS: [f64; 29] = [
        0.0, 0.9, 1.4, 2.7, 3.7, 7.7, 8.7, 12.5, 14.4, 15.7, 16.6, 19.7, 20.7, 22.9, 25.4, 28.0,
        29.7, 32.8, 33.8, 36.4, 37.2, 38.6, 40.2, 42.1, 43.4, 43.9, 44.5, 48.0, 49.6,
    ];

    fn point(gain: f64, floor: f64, sat: f64) -> GainPoint {
        GainPoint {
            gain_db: gain,
            measurement: Measurement {
                noise_floor_db: floor,
                peak_db: floor + 10.0,
                sat_frac: sat,
            },
        }
    }

    fn sweep(floors: [f64; 29]) -> Vec<GainPoint> {
        STEPS
            .iter()
            .zip(floors)
            .map(|(&g, f)| point(g, f, 0.0))
            .collect()
    }

    /// The floors of 2026-10-19's sweeps, one per R820T step.
    const S73121177_A: [f64; 29] = [
        -46.2, -46.2, -46.1, -46.2, -46.2, -46.3, -46.4, -46.6, -46.6, -46.5, -46.4, -45.8, -45.3,
        -44.2, -43.3, -41.4, -40.1, -38.3, -37.1, -36.7, -34.7, -34.3, -33.0, -32.2, -31.0, -29.7,
        -29.7, -27.5, -27.1,
    ];
    const S73121177_B: [f64; 29] = [
        -46.2, -46.2, -46.1, -46.1, -46.2, -46.2, -46.2, -46.2, -46.1, -46.0, -45.7, -45.1, -44.2,
        -42.2, -40.4, -39.4, -38.5, -36.5, -35.2, -34.6, -33.2, -32.6, -31.4, -30.4, -29.4, -28.1,
        -27.9, -25.7, -24.9,
    ];
    const S01494467: [f64; 29] = [
        -45.5, -45.5, -45.5, -45.5, -45.5, -45.5, -45.4, -45.4, -45.3, -45.3, -45.2, -44.9, -44.7,
        -44.2, -43.6, -42.6, -41.4, -40.4, -39.1, -38.9, -37.7, -36.9, -35.6, -35.0, -33.8, -33.2,
        -33.1, -30.8, -30.4,
    ];
    const S13360853: [f64; 29] = [
        -46.2, -46.2, -46.2, -46.2, -46.2, -46.1, -46.1, -46.1, -46.1, -46.1, -46.1, -46.1, -46.1,
        -46.0, -46.0, -45.9, -45.7, -45.6, -45.5, -45.1, -44.6, -44.2, -43.6, -42.8, -42.6, -42.0,
        -41.2, -41.1, -40.5,
    ];

    /// A dongle exactly as the model says: ADC floor `a` dB, the front end
    /// equal to it at `equal_at` dB of gain, slope `s`.
    fn modelled(a: f64, equal_at: f64, s: f64) -> Vec<GainPoint> {
        STEPS
            .iter()
            .map(|&g| point(g, db(lin(a) + lin(s.mul_add(g - equal_at, a))), 0.0))
            .collect()
    }

    #[test]
    fn the_fit_recovers_a_modelled_dongle() {
        let pts = modelled(-46.0, 30.0, 0.8);
        let curve: Vec<_> = pts
            .iter()
            .map(|p| (p.gain_db, p.measurement.noise_floor_db))
            .collect();
        let m = NoiseModel::fit(&curve).unwrap();
        assert!(m.rms_db < 0.01, "{m:?}");
        assert!((m.slope - 0.8).abs() < 0.01, "{m:?}");
        assert!((db(m.adc) + 46.0).abs() < 0.05, "{m:?}");
        // equal noise: 3 dB lost
        assert!((m.penalty_db(30.0) - 3.01).abs() < 0.05);
    }

    #[test]
    fn picks_the_lowest_gain_within_the_budget() {
        // penalty <= 0.5 dB once the front end is 9.1 dB over the ADC, i.e.
        // 9.1 / 0.8 = 11.4 dB of gain past where they are equal
        let sel = select_gain(&modelled(-46.0, 25.0, 0.8), &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::Budget);
        assert_eq!(sel.gain_db, 37.2);
        assert!(sel.penalty_db <= 0.5, "{sel:?}");
    }

    #[test]
    fn todays_sweeps() {
        // 2026-10-19's dongles: 73121177 tracks gain and gets a mid pick;
        // 01494467, which the knee rule called adc_limited and sent to the
        // top, tracks gain too and is within budget well below it; 13360853
        // really is ADC-limited.
        let pick = |floors| select_gain(&sweep(floors), &params(), 0.0);
        let a = pick(S73121177_A);
        assert_eq!((a.reason, a.gain_db), (SelectReason::Budget, 36.4), "{a:?}");
        let b = pick(S73121177_B);
        assert_eq!((b.reason, b.gain_db), (SelectReason::Budget, 32.8), "{b:?}");
        let c = pick(S01494467);
        assert_eq!((c.reason, c.gain_db), (SelectReason::Budget, 40.2), "{c:?}");
        let d = pick(S13360853);
        assert_eq!(
            (d.reason, d.gain_db),
            (SelectReason::AdcLimited, 49.6),
            "{d:?}"
        );
        assert!(d.penalty_db > 1.0, "{d:?}");
    }

    #[test]
    fn a_warmer_front_end_is_re_estimated_from_one_floor() {
        let m = NoiseModel::fit(
            &modelled(-46.0, 25.0, 0.8)
                .iter()
                .map(|p| (p.gain_db, p.measurement.noise_floor_db))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let g = 37.2;
        // the front end lost 3 dB of gain: its noise is 3 dB lower
        let warmer = db(m.adc + m.front_at(g) / 2.0);
        let w = m.with_floor_at(g, warmer).unwrap();
        assert!(w.penalty_db(g) > m.penalty_db(g));
        assert!((w.penalty_db(g) - db(1.0 + 2.0 * m.adc / m.front_at(g))).abs() < 1e-9);
        // a floor at the ADC's leaves no front end to explain
        assert!(m.with_floor_at(g, db(m.adc) - 0.1).is_none());
    }

    #[test]
    fn vetoes_saturated_high_gains() {
        let mut pts = modelled(-46.0, 25.0, 0.8);
        for p in pts.iter_mut().filter(|p| p.gain_db >= 33.0) {
            p.measurement.sat_frac = 0.01;
        }
        let sel = select_gain(&pts, &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::SaturationLimited);
        assert_eq!(sel.gain_db, 32.8);
    }

    #[test]
    fn all_saturated_picks_lowest_and_flags() {
        let pts: Vec<_> = STEPS.iter().map(|&g| point(g, -40.0 + g, 0.5)).collect();
        let sel = select_gain(&pts, &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::SaturationLimited);
        assert_eq!(sel.gain_db, 0.0);
    }

    #[test]
    fn falls_back_on_too_little_or_an_unexplained_curve() {
        assert_eq!(
            select_gain(&[], &params(), 33.0).reason,
            SelectReason::Fallback
        );
        let few = [point(0.0, -46.0, 0.0), point(20.0, -40.0, 0.0)];
        assert_eq!(select_gain(&few, &params(), 33.0).gain_db, 33.0);
        let nan: Vec<_> = STEPS.iter().map(|&g| point(g, f64::NAN, 0.0)).collect();
        assert_eq!(
            select_gain(&nan, &params(), 33.0).reason,
            SelectReason::Fallback
        );
        // a floor that falls with gain is nothing the model describes
        let falling: Vec<_> = STEPS
            .iter()
            .enumerate()
            .map(|(i, &g)| point(g, if i % 2 == 0 { -30.0 } else { -50.0 } - g, 0.0))
            .collect();
        assert_eq!(
            select_gain(&falling, &params(), 33.0).reason,
            SelectReason::Fallback
        );
    }
}
