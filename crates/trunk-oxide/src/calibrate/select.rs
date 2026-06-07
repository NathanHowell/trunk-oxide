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
//!    dominates (front-end-limited, best NF). Pick the *lowest* gain in the
//!    front-end-limited regime: full sensitivity, minimal overload headroom
//!    spent.
//! 3. **Spur hook** — [`spur_overload_gain`] is a reserved seam for detecting
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
    /// Picked the knee: lowest gain in the front-end-limited regime. The good
    /// outcome — best noise figure with minimal overload risk.
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

    // Knee: first valid gain whose floor rose ~1:1 versus the previous valid
    // gain. That point is the onset of the front-end-limited regime.
    let mut knee: Option<GainPoint> = None;
    for win in valid.windows(2) {
        let (prev, cur) = (win[0], win[1]);
        let dg = cur.gain_db - prev.gain_db;
        if dg <= 0.0 {
            continue;
        }
        let slope = (cur.measurement.noise_floor_db - prev.measurement.noise_floor_db) / dg;
        if slope >= params.knee_slope {
            knee = Some(cur);
            break;
        }
    }

    let mut selection = match knee {
        Some(point) => GainSelection {
            gain_db: point.gain_db,
            noise_floor_db: point.measurement.noise_floor_db,
            reason: SelectReason::Knee,
        },
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
        }
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
    fn picks_knee_at_onset_of_frontend_regime() {
        // Floor flat (ADC-limited) up to 20 dB, then tracks gain 1:1.
        let pts = vec![
            point(0.0, -60.0, 0.0),
            point(10.0, -59.5, 0.0),
            point(20.0, -59.0, 0.0),
            point(30.0, -49.0, 0.0), // +10 dB floor for +10 dB gain → slope 1
            point(40.0, -39.0, 0.0),
        ];
        let sel = select_gain(&pts, &params(), 99.0);
        assert_eq!(sel.reason, SelectReason::Knee);
        assert_eq!(
            sel.gain_db, 30.0,
            "knee is the first front-end-limited step"
        );
    }

    #[test]
    fn vetoes_saturated_high_gains() {
        // Knee at 30, but 30 and 40 clip; best usable is 20 (below the knee).
        let pts = vec![
            point(0.0, -60.0, 0.0),
            point(10.0, -59.5, 0.0),
            point(20.0, -59.0, 0.0),
            point(30.0, -49.0, 0.01), // clips
            point(40.0, -39.0, 0.2),  // clips hard
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
