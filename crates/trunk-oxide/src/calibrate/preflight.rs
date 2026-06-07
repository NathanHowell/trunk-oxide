//! Pre-flight [`IqProbe`]: opens a device standalone, before the main
//! flowgraph exists, and sweeps gain through the C++ `TrProbe` FFI.
//!
//! The device is held open for the probe's lifetime and released when the
//! `PreflightProbe` is dropped — which must happen before trunk-recorder opens
//! the same device for real.

use anyhow::{Context, Result};
use num_complex::Complex32;
use trunk_recorder_sys::ffi;

use super::probe::IqProbe;

/// An [`IqProbe`] backed by a standalone osmosdr source opened just for
/// calibration.
pub struct PreflightProbe {
    probe: cxx::UniquePtr<ffi::TrProbe>,
    capture_samples: usize,
    settle_samples: usize,
}

impl PreflightProbe {
    /// Open `cfg`'s device for calibration. Fails if the driver is unsupported
    /// or the device can't be opened (osmosdr only).
    pub fn open(
        cfg: &ffi::TrProbeConfig,
        capture_samples: usize,
        settle_samples: usize,
    ) -> Result<Self> {
        let probe = ffi::tr_probe_open(cfg)
            .with_context(|| format!("opening SDR '{}' for gain calibration", cfg.device))?;
        Ok(Self {
            probe,
            capture_samples,
            settle_samples,
        })
    }
}

impl IqProbe for PreflightProbe {
    fn supported_gains(&mut self) -> Result<Vec<f64>> {
        // &UniquePtr<TrProbe> derefs to &TrProbe for the FFI call.
        Ok(ffi::tr_probe_gain_steps(&self.probe))
    }

    fn capture(&mut self, gain_db: f64) -> Result<Vec<Complex32>> {
        let interleaved = ffi::tr_probe_capture(
            self.probe.pin_mut(),
            gain_db,
            self.capture_samples,
            self.settle_samples,
        )
        .map_err(|e| anyhow::anyhow!("IQ capture at gain {gain_db} dB: {e}"))?;
        // C++ returns interleaved [re, im, re, im, ...].
        Ok(interleaved
            .chunks_exact(2)
            .map(|c| Complex32::new(c[0], c[1]))
            .collect())
    }
}
