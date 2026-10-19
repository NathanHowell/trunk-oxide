//! Pre-flight [`IqProbe`] on an RTL-SDR that trunk-oxide opens itself.
//!
//! The dongle is held open for the probe's lifetime and released on drop,
//! before the same dongle is opened again for streaming.

use anyhow::{Context, Result};
use num_complex::Complex32;
use trunk_sdr::{DeviceSpec, Gain, Rtl, Tuning};

use super::probe::IqProbe;

pub struct RtlProbe {
    rtl: Rtl,
    capture_samples: usize,
    settle_samples: usize,
}

impl RtlProbe {
    /// Open `spec` tuned as `tuning` says (its gain is replaced per capture).
    pub fn open(
        spec: &DeviceSpec,
        tuning: &Tuning,
        capture_samples: usize,
        settle_samples: usize,
    ) -> Result<Self> {
        let rtl = Rtl::open(spec, tuning)
            .with_context(|| format!("opening RTL-SDR {spec:?} for gain calibration"))?;
        Ok(Self {
            rtl,
            capture_samples,
            settle_samples,
        })
    }
}

impl IqProbe for RtlProbe {
    fn supported_gains(&mut self) -> Result<Vec<f64>> {
        Ok(self.rtl.gain_steps_db())
    }

    fn capture(&mut self, gain_db: f64) -> Result<Vec<Complex32>> {
        self.rtl.set_gain(Gain::Manual(gain_db))?;
        self.rtl
            .capture(self.settle_samples, self.capture_samples)
            .with_context(|| format!("IQ capture at gain {gain_db} dB"))
    }
}
