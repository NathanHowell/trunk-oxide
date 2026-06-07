//! The one hardware-touching seam in gain calibration.
//!
//! Everything above this trait (measurement, selection, orchestration) is pure
//! and shared. The two acquisition strategies — a throwaway pre-flight capture
//! and (later) a tap on the live source — are just two implementations of
//! [`IqProbe`]. Adding runtime calibration means writing a second impl and
//! nothing else.

use anyhow::Result;
use num_complex::Complex32;

/// Acquires IQ from a device at a chosen gain. The sole boundary between the
/// pure calibration logic and real hardware.
pub trait IqProbe {
    /// Supported tuner gain steps, in dB. Order is not assumed (the selector
    /// sorts). Returned once and swept in full.
    fn supported_gains(&mut self) -> Result<Vec<f64>>;

    /// Set `gain_db`, let the front end settle, and return a block of complex
    /// baseband samples. The block must be large enough for several FFT
    /// segments (the orchestrator sizes its request via the policy).
    fn capture(&mut self, gain_db: f64) -> Result<Vec<Complex32>>;
}
