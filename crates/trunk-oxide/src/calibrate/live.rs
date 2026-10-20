//! The noise floor again, on the running stream.
//!
//! The sweep runs once at startup, but a dongle's front end drifts with
//! temperature (on 2026-10-19 a room ~10 C warmer took signal and noise down
//! together on all three). Every [`EVERY`] each source hands over a snapshot
//! of its samples ([`trunk_radio::snapshot`]), measured as the sweep measured
//! each step, against the sweep's floor and noise model at the gain the
//! source runs.

use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Duration;

use trunk_radio::snapshot::{Snapshot, Snapshots};

use super::CalibrationPolicy;
use super::measure::measure;
use super::report::{self, LiveFloor, SourceGain};

/// Stream time between measurements of a source.
pub const EVERY: Duration = Duration::from_secs(60);

/// Start measuring; the sources take snapshots for the returned
/// [`Snapshots`].
///
/// # Errors
/// If the thread cannot be started.
pub fn start(policy: CalibrationPolicy) -> std::io::Result<Snapshots> {
    let (tx, rx) = sync_channel(1);
    std::thread::Builder::new()
        .name("live-floor".into())
        .spawn(move || run(&policy, &rx))?;
    Ok(Snapshots {
        every: EVERY,
        len: policy.capture_samples,
        tx,
    })
}

fn run(policy: &CalibrationPolicy, rx: &Receiver<Snapshot>) {
    let params = policy.measure_params();
    for snap in rx {
        let gains = report::gains();
        let Some(gain) = gains.iter().find(|g| g.device == snap.device) else {
            continue;
        };
        let m = measure(&snap.iq, &params);
        if !m.noise_floor_db.is_finite() {
            continue;
        }
        let live = compare(gain, m.noise_floor_db);
        tracing::info!(
            device = %live.device,
            gain_db = gain.gain_db,
            floor_db = live.floor_db,
            drift_db = live.drift_db,
            penalty_db = live.penalty_db,
            sat_frac = m.sat_frac,
            at_s = snap.at.as_secs(),
            "live noise floor"
        );
        report::record_live(live);
    }
}

/// `floor_db`, measured at `gain`'s gain, against its calibration.
fn compare(gain: &SourceGain, floor_db: f64) -> LiveFloor {
    LiveFloor {
        device: gain.device.clone(),
        source: gain.source,
        floor_db,
        drift_db: gain.noise_floor_db.map(|c| floor_db - c),
        penalty_db: gain
            .model
            .and_then(|m| m.with_floor_at(gain.gain_db, floor_db))
            .map(|now| now.penalty_db(gain.gain_db)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibrate::select::NoiseModel;

    fn gain(model: Option<NoiseModel>) -> SourceGain {
        SourceGain {
            device: "rtl=1".into(),
            source: 3,
            gain_db: 32.0,
            noise_floor_db: Some(-40.0),
            reason: "budget",
            model,
        }
    }

    /// ADC at -50 dB; a front end 1:1 with gain, equal to the ADC at 20 dB.
    fn model() -> NoiseModel {
        let points: Vec<(f64, f64)> = [0.0, 10.0, 20.0, 26.0, 32.0, 38.0, 44.0]
            .iter()
            .map(|&g| {
                (
                    g,
                    10.0f64.mul_add((1.0 + 10f64.powf((g - 20.0) / 10.0)).log10(), -50.0),
                )
            })
            .collect();
        NoiseModel::fit(&points).expect("fits")
    }

    #[test]
    fn a_floor_that_fell_costs_snr() {
        let m = model();
        let at = m.floor_db(32.0);
        let g = SourceGain {
            noise_floor_db: Some(at),
            ..gain(Some(m))
        };
        let same = compare(&g, at);
        assert!(same.drift_db.is_some_and(|d| d.abs() < 1e-9), "{same:?}");
        let warm = compare(&g, at - 3.0);
        assert!(warm.drift_db.is_some_and(|d| (d + 3.0).abs() < 1e-9));
        let (before, after) = (same.penalty_db.unwrap(), warm.penalty_db.unwrap());
        assert!(after > before + 0.2, "{before} -> {after}");
    }

    #[test]
    fn without_a_calibration_only_the_floor_is_known() {
        let g = SourceGain {
            noise_floor_db: None,
            ..gain(None)
        };
        let l = compare(&g, -41.0);
        assert_eq!((l.floor_db, l.drift_db, l.penalty_db), (-41.0, None, None));
        assert_eq!((l.device.as_str(), l.source), ("rtl=1", 3));
    }
}
