//! What gain each source ended up with, for the metrics: the calibration
//! runs before the meter exists, so it leaves its result here and
//! `trunk.oxide.source.gain_db` reads it.

use std::sync::{Mutex, PoisonError};

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
    /// (the last sweep's pick re-checked and kept) or `configured` (no
    /// `auto_gain`).
    pub reason: &'static str,
}

static GAINS: Mutex<Vec<SourceGain>> = Mutex::new(Vec::new());

pub fn record(gains: Vec<SourceGain>) {
    *GAINS.lock().unwrap_or_else(PoisonError::into_inner) = gains;
}

pub fn gains() -> Vec<SourceGain> {
    GAINS.lock().unwrap_or_else(PoisonError::into_inner).clone()
}
