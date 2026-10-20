//! What gain each source ended up with, for the metrics.
//!
//! The calibration runs before the meter exists, so it leaves its result
//! here and `trunk.oxide.source.gain_db` reads it. The live floor
//! ([`super::live`]) leaves its latest measurement here too.

use std::sync::{Mutex, PoisonError};

use super::select::NoiseModel;

/// One source's gain as it was opened.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceGain {
    /// The device string as configured (`rtl=13360853`).
    pub device: String,
    /// The source's index in the config.
    pub source: usize,
    pub gain_db: f64,
    /// The noise floor the calibration measured at that gain, if it ran
    /// (the sweep's relative dB scale).
    pub noise_floor_db: Option<f64>,
    /// `budget`, `saturation_limited`, `adc_limited`, `fallback`, `cached`
    /// (the last sweep's pick re-checked and kept) or `configured` (the
    /// config pins `gain`, or the source has no tuner).
    pub reason: &'static str,
    /// The noise model fitted to the device's sweep, if one fits.
    pub model: Option<NoiseModel>,
}

/// The floor measured on a source's running stream at its gain.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveFloor {
    pub device: String,
    /// The source's index in the config.
    pub source: usize,
    /// On the sweep's relative dB scale.
    pub floor_db: f64,
    /// How far it moved from the sweep's floor at this gain.
    pub drift_db: Option<f64>,
    /// The SNR the ADC's noise takes now, by the sweep's model with the
    /// front end moved to this floor.
    pub penalty_db: Option<f64>,
}

static GAINS: Mutex<Vec<SourceGain>> = Mutex::new(Vec::new());
static LIVE: Mutex<Vec<LiveFloor>> = Mutex::new(Vec::new());

pub fn record(gains: Vec<SourceGain>) {
    *GAINS.lock().unwrap_or_else(PoisonError::into_inner) = gains;
}

pub fn gains() -> Vec<SourceGain> {
    GAINS.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Keep `floor` as its source's latest.
pub fn record_live(floor: LiveFloor) {
    let mut live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
    live.retain(|f| f.device != floor.device);
    live.push(floor);
}

pub fn live() -> Vec<LiveFloor> {
    LIVE.lock().unwrap_or_else(PoisonError::into_inner).clone()
}
