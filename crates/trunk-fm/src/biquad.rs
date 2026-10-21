//! The filters the audio-rate decoders put ahead of their tone energies.

/// A second-order section.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    /// A high-pass at `hz` with the given Q.
    fn high_pass(hz: f64, q: f64) -> Self {
        let w = std::f64::consts::TAU * hz / crate::AUDIO_RATE;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b: [
                f64::midpoint(1.0, cos) / a0,
                -(1.0 + cos) / a0,
                f64::midpoint(1.0, cos) / a0,
            ],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// A fourth-order Butterworth high-pass at [`crate::AUDIO_RATE`].
#[derive(Clone, Copy)]
pub struct HighPass {
    hz: f64,
    sections: [Biquad; 2],
}

impl HighPass {
    /// With the corner at `hz`.
    #[must_use]
    pub fn new(hz: f64) -> Self {
        Self {
            hz,
            // the Qs of a fourth-order Butterworth
            sections: [0.541_196_1, 1.306_563].map(|q| Biquad::high_pass(hz, q)),
        }
    }

    pub fn run(&mut self, x: f64) -> f64 {
        self.sections.iter_mut().fold(x, |x, s| s.run(x))
    }

    /// Forget the audio so far.
    pub fn reset(&mut self) {
        *self = Self::new(self.hz);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gain of the filter on a sine at `hz`, once settled.
    fn gain(hz: f64, corner: f64) -> f64 {
        let mut f = HighPass::new(corner);
        let rate = crate::AUDIO_RATE;
        let mut peak = 0.0f64;
        for n in 0..16_000 {
            let y = f.run((std::f64::consts::TAU * hz * f64::from(n) / rate).sin());
            if n > 8_000 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn it_passes_above_the_corner_and_stops_below() {
        assert!((gain(1_200.0, 700.0) - 1.0).abs() < 0.1);
        // -3 dB at the corner, and a fourth-order slope under it
        assert!((gain(700.0, 700.0) - 0.707).abs() < 0.05);
        assert!(gain(162.2, 700.0) < 0.01);
    }

    #[test]
    fn a_reset_forgets_the_audio() {
        let mut f = HighPass::new(500.0);
        for _ in 0..100 {
            f.run(1.0);
        }
        f.reset();
        let fresh = HighPass::new(500.0).run(0.25);
        assert_eq!(f.run(0.25), fresh);
    }
}
