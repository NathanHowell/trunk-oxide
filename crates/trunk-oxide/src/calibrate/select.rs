//! Pure gain selection from a swept set of [`Measurement`]s.
//!
//! Given the noise floor and saturation measured at each supported gain step,
//! pick the gain with the best noise figure that stays clear of overload. The
//! logic is deterministic and unit-testable — no device, no FFI.
//!
//! Method (see the design discussion):
//! 1. **Saturation veto** — drop any gain whose clipping fraction exceeds the
//!    limit. These are hard ADC overload and never acceptable.
//! 2. **Knee detection** — as gain rises, the noise floor is flat while the ADC
//!    dominates (ADC-limited) then tracks gain ~1:1 once the front end
//!    dominates (front-end-limited, best NF). The slope is measured over at
//!    least [`SelectParams::knee_span_db`] of gain, not one tuner step (RTL
//!    steps are as small as 0.5 dB, and a 0.1 s floor estimate scatters by
//!    that much), and the knee is only declared once the next window agrees.
//! 3. **Margin** — at the knee the front end's noise merely equals the ADC's,
//!    which still costs ~3 dB of noise figure. Pick the lowest usable gain at
//!    least [`SelectParams::margin_db`] above the knee: there the ADC's
//!    contribution is a fraction of a dB, with the least overload headroom
//!    spent to get it.
//! 4. **Spur hook** — [`spur_overload_gain`] is a reserved seam for detecting
//!    discrete intermod tones (which a raised broadband floor can miss). Stubbed
//!    for now; wired so it can only ever *lower* the chosen gain.

use super::measure::Measurement;

/// One swept data point: a gain step and what we measured there.
#[derive(Debug, Clone, Copy)]
pub struct GainPoint {
    pub gain_db: f64,
    pub measurement: Measurement,
}

/// Why a particular gain was chosen — surfaced in logs so an operator can tell
/// a healthy calibration from a compromised RF environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectReason {
    /// Picked from the knee: the lowest usable gain a margin above the onset
    /// of the front-end-limited regime. The good outcome — best noise figure
    /// with minimal overload risk.
    Knee,
    /// Overload cut the sweep off below the knee; picked the highest gain that
    /// stays clear of saturation. The front end can't reach optimal NF without
    /// clipping — usually means the antenna is too hot; add attenuation.
    SaturationLimited,
    /// The floor never tracked gain (ADC-limited across the whole sweep); picked
    /// the highest usable gain for maximum sensitivity. Usually a weak antenna.
    AdcLimited,
    /// Not enough usable points to decide; caller should keep its configured
    /// gain.
    Fallback,
}

impl SelectReason {
    /// The `reason` label of `trunk.oxide.source.gain_db`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Knee => "knee",
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
    pub reason: SelectReason,
}

/// Tunables for selection. Defaults live in
/// [`CalibrationPolicy`](super::CalibrationPolicy).
#[derive(Debug, Clone, Copy)]
pub struct SelectParams {
    /// Saturation fraction above which a gain is vetoed as overloaded.
    pub sat_limit: f64,
    /// Noise-floor slope (dB of floor per dB of gain) at or above which we
    /// consider the front end to dominate. ~`1.0` is the ideal asymptote;
    /// `0.6` leaves margin for measurement noise.
    pub knee_slope: f64,
    /// The least gain (dB) a slope is measured across. Adjacent tuner steps
    /// closer than this are bridged to an earlier one.
    pub knee_span_db: f64,
    /// How far (dB) above the knee to sit. `6.0` puts the front end's noise
    /// 6 dB over the ADC's, a ~1 dB noise-figure penalty instead of ~3.
    pub margin_db: f64,
}

/// The floor's slope (dB/dB) at `valid[i]`, measured back to the nearest
/// lower point at least `span_db` below it. `None` when no point is that far
/// below.
fn slope_at(valid: &[GainPoint], i: usize, span_db: f64) -> Option<f64> {
    let cur = valid[i];
    let prev = valid[..i]
        .iter()
        .rev()
        .find(|p| cur.gain_db - p.gain_db >= span_db)?;
    Some(
        (cur.measurement.noise_floor_db - prev.measurement.noise_floor_db)
            / (cur.gain_db - prev.gain_db),
    )
}

/// The first point whose slope and the next point's both clear
/// `params.knee_slope`: the onset of the front-end-limited regime, confirmed.
fn find_knee(valid: &[GainPoint], params: &SelectParams) -> Option<GainPoint> {
    let steep =
        |i: usize| slope_at(valid, i, params.knee_span_db).is_some_and(|s| s >= params.knee_slope);
    (0..valid.len().saturating_sub(1))
        .find(|&i| steep(i) && steep(i + 1))
        .map(|i| valid[i])
}

/// Reserved hook for discrete-intermod (spur) overload detection.
///
/// The noise-floor knee already penalizes *broadband* front-end overload (it
/// lifts the floor super-linearly). What it can miss is a single intermod
/// *tone* that doesn't move the broadband floor much. The intended
/// implementation watches specific PSD peaks grow faster than 1:1 with gain
/// (2:1 for 2nd-order, 3:1 for 3rd-order) across the sweep and returns the
/// highest gain still free of such growth.
///
/// This requires per-gain peak tracking we don't yet retain, so it is a stub:
/// it never constrains the result today. It is wired in [`select_gain`] such
/// that, once implemented, it can only ever *lower* the selected gain.
#[must_use]
pub fn spur_overload_gain(_points: &[GainPoint]) -> Option<f64> {
    None
}

/// Pick the best gain from a swept set of measurements.
///
/// `fallback_gain` is returned (with [`SelectReason::Fallback`]) when there are
/// too few usable points to decide.
#[must_use]
pub fn select_gain(
    points: &[GainPoint],
    params: &SelectParams,
    fallback_gain: f64,
) -> GainSelection {
    // Sort by gain ascending; drop points whose floor we couldn't measure.
    let mut pts: Vec<GainPoint> = points
        .iter()
        .copied()
        .filter(|p| p.measurement.noise_floor_db.is_finite())
        .collect();
    pts.sort_by(|a, b| {
        a.gain_db
            .partial_cmp(&b.gain_db)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if pts.is_empty() {
        return GainSelection {
            gain_db: fallback_gain,
            noise_floor_db: f64::NAN,
            reason: SelectReason::Fallback,
        };
    }

    // Saturation veto.
    let valid: Vec<GainPoint> = pts
        .iter()
        .copied()
        .filter(|p| p.measurement.sat_frac <= params.sat_limit)
        .collect();
    let any_saturated = valid.len() < pts.len();

    // Every gain overloads — can't avoid clipping. Pick the lowest gain (least
    // bad) and flag it loudly.
    let Some(&lowest_valid) = valid.first() else {
        let lowest = pts[0];
        return GainSelection {
            gain_db: lowest.gain_db,
            noise_floor_db: lowest.measurement.noise_floor_db,
            reason: SelectReason::SaturationLimited,
        };
    };

    let mut selection = match find_knee(&valid, params) {
        Some(knee) => {
            // The lowest usable gain a margin above the knee. If overload
            // cuts the sweep off before that, the highest usable gain is
            // still past the knee, just with less margin than wanted.
            let want = knee.gain_db + params.margin_db;
            let (point, reason) = valid.iter().find(|p| p.gain_db >= want).map_or_else(
                || {
                    let top = valid.last().copied().unwrap_or(knee);
                    let reason = if any_saturated {
                        SelectReason::SaturationLimited
                    } else {
                        SelectReason::Knee
                    };
                    (top, reason)
                },
                |p| (*p, SelectReason::Knee),
            );
            GainSelection {
                gain_db: point.gain_db,
                noise_floor_db: point.measurement.noise_floor_db,
                reason,
            }
        }
        None => {
            // No front-end-limited regime among usable gains. Either overload
            // truncated the sweep before the knee (SaturationLimited) or the
            // floor was flat throughout (AdcLimited). Either way the best we can
            // do is the highest usable gain.
            let top = *valid.last().unwrap_or(&lowest_valid);
            GainSelection {
                gain_db: top.gain_db,
                noise_floor_db: top.measurement.noise_floor_db,
                reason: if any_saturated {
                    SelectReason::SaturationLimited
                } else {
                    SelectReason::AdcLimited
                },
            }
        }
    };

    // Spur hook (currently a no-op): may only lower the chosen gain.
    if let Some(spur_cap) = spur_overload_gain(&valid)
        && spur_cap < selection.gain_db
    {
        // Re-resolve to the highest valid point at or below the cap.
        if let Some(capped) = valid.iter().rev().find(|p| p.gain_db <= spur_cap) {
            selection = GainSelection {
                gain_db: capped.gain_db,
                noise_floor_db: capped.measurement.noise_floor_db,
                reason: SelectReason::SaturationLimited,
            };
        }
    }

    selection
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> SelectParams {
        SelectParams {
            sat_limit: 1e-3,
            knee_slope: 0.6,
            knee_span_db: 2.5,
            margin_db: 6.0,
        }
    }

    /// An RTL-SDR-like ladder: flat floor to `knee`, then tracking gain 1:1,
    /// with `scatter` dB of alternating measurement noise.
    fn ladder(knee: f64, scatter: f64) -> Vec<GainPoint> {
        const STEPS: [f64; 29] = [
            0.0, 0.9, 1.4, 2.7, 3.7, 7.7, 8.7, 12.5, 14.4, 15.7, 16.6, 19.7, 20.7, 22.9, 25.4,
            28.0, 29.7, 32.8, 33.8, 36.4, 37.2, 38.6, 40.2, 42.1, 43.4, 43.9, 44.5, 48.0, 49.6,
        ];
        STEPS
            .iter()
            .enumerate()
            .map(|(i, &g)| {
                let floor = -60.0 + (g - knee).max(0.0);
                let wobble = if i % 2 == 0 { scatter } else { -scatter };
                point(g, floor + wobble, 0.0)
            })
            .collect()
    }

    fn point(gain: f64, floor: f64, sat: f64) -> GainPoint {
        GainPoint {
            gain_db: gain,
            measurement: Measurement {
                noise_floor_db: floor,
                peak_db: floor + 30.0,
                sat_frac: sat,
            },
        }
    }

    #[test]
    fn sits_a_margin_above_the_knee() {
        // Floor flat (ADC-limited) up to 20 dB, then tracks gain 1:1.
        let pts = vec![
            point(0.0, -60.0, 0.0),
            point(10.0, -59.5, 0.0),
            point(20.0, -59.0, 0.0),
            point(30.0, -49.0, 0.0), // +10 dB floor for +10 dB gain → slope 1
            point(35.0, -44.0, 0.0),
            point(40.0, -39.0, 0.0),
        ];
        let sel = select_gain(&pts, &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::Knee);
        assert_eq!(sel.gain_db, 40.0, "knee at 30, first step ≥ 36");
    }

    #[test]
    fn knee_on_a_real_ladder_lands_where_the_floor_lifts() {
        let sel = select_gain(&ladder(25.4, 0.0), &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::Knee);
        // Slope first clears 0.6 at 29.7 (vs 25.4: 4.3/4.3), confirmed at
        // 32.8; want ≥ 35.7 → 36.4.
        assert_eq!(sel.gain_db, 36.4);
    }

    #[test]
    fn one_noisy_step_is_not_a_knee() {
        // 0.5 dB of scatter across sub-dB steps makes single-step slopes
        // swing past 1 dB/dB in the flat region; the span and the
        // confirmation window ignore that.
        let sel = select_gain(&ladder(25.4, 0.5), &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::Knee);
        assert!((36.4..=38.6).contains(&sel.gain_db), "got {}", sel.gain_db);
    }

    #[test]
    fn knee_needs_the_next_window_to_agree() {
        // One step jumps (a burst on air during that capture) and the floor
        // then settles back: flat sweep, not a knee.
        let mut pts = ladder(99.0, 0.0);
        pts[10].measurement.noise_floor_db += 4.0;
        let sel = select_gain(&pts, &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::AdcLimited);
    }

    #[test]
    fn vetoes_saturated_high_gains() {
        // Knee at 30, but 40 and up clip; best usable is 35, past the knee
        // but short of the margin.
        let pts = vec![
            point(0.0, -60.0, 0.0),
            point(10.0, -59.5, 0.0),
            point(20.0, -59.0, 0.0),
            point(30.0, -49.0, 0.0),
            point(35.0, -44.0, 0.0),
            point(40.0, -39.0, 0.01), // clips
            point(45.0, -34.0, 0.2),  // clips hard
        ];
        let sel = select_gain(&pts, &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::SaturationLimited);
        assert_eq!(sel.gain_db, 35.0);
    }

    #[test]
    fn overload_below_the_knee_takes_the_highest_clean_gain() {
        let pts = vec![
            point(0.0, -60.0, 0.0),
            point(10.0, -59.5, 0.0),
            point(20.0, -59.0, 0.0),
            point(30.0, -49.0, 0.01),
            point(40.0, -39.0, 0.2),
        ];
        let sel = select_gain(&pts, &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::SaturationLimited);
        assert_eq!(sel.gain_db, 20.0);
    }

    #[test]
    fn adc_limited_when_floor_never_tracks_gain() {
        // Weak antenna: floor barely moves across the whole sweep.
        let pts = vec![
            point(0.0, -60.0, 0.0),
            point(20.0, -59.6, 0.0),
            point(40.0, -59.0, 0.0),
        ];
        let sel = select_gain(&pts, &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::AdcLimited);
        assert_eq!(sel.gain_db, 40.0, "max sensitivity when ADC-limited");
    }

    #[test]
    fn all_saturated_picks_lowest_and_flags() {
        let pts = vec![point(0.0, -60.0, 0.05), point(20.0, -50.0, 0.2)];
        let sel = select_gain(&pts, &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::SaturationLimited);
        assert_eq!(sel.gain_db, 0.0);
    }

    #[test]
    fn fallback_when_no_finite_points() {
        let pts = vec![point(0.0, f64::NAN, 0.0), point(20.0, f64::NAN, 0.0)];
        let sel = select_gain(&pts, &params(), 42.0);
        assert_eq!(sel.reason, SelectReason::Fallback);
        assert_eq!(sel.gain_db, 42.0);
    }

    #[test]
    fn empty_input_falls_back() {
        let sel = select_gain(&[], &params(), 42.0);
        assert_eq!(sel.reason, SelectReason::Fallback);
        assert_eq!(sel.gain_db, 42.0);
    }
}
