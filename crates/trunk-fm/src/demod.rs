//! FM demodulation and de-emphasis, arithmetic as in the GNU Radio blocks the
//! analog recorder used (`quadrature_demod_cf`, `iir_filter_ffd`).

use num_complex::Complex32;

use crate::atan::fast_atan2;

/// Quadrature demodulator: the phase advance between consecutive samples,
/// scaled so `max_dev` Hz of deviation is 1.0.
#[derive(Clone, Debug)]
pub struct QuadratureDemod {
    gain: f32,
    prev: Complex32,
}

impl QuadratureDemod {
    /// For `rate` Hz input and `max_dev` Hz peak deviation.
    #[must_use]
    pub fn new(rate: f64, max_dev: f64) -> Self {
        Self {
            gain: (rate / (2.0 * std::f64::consts::PI * max_dev)) as f32,
            prev: Complex32::new(0.0, 0.0),
        }
    }

    /// Demodulate `iq`, appending one sample per input to `out`.
    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        out.reserve(iq.len());
        for &x in iq {
            let d = x * self.prev.conj();
            out.push(self.gain * fast_atan2(d.im, d.re));
            self.prev = x;
        }
    }

    pub const fn reset(&mut self) {
        self.prev = Complex32::new(0.0, 0.0);
    }
}

/// First-order FM de-emphasis (`fm_emph.py`'s taps, bilinear transform of
/// `w_ca / (s + w_ca)`), run as `iir_filter_ffd` runs it: `f32` samples,
/// `f64` taps and accumulator.
#[derive(Clone, Debug)]
pub struct Deemphasis {
    b0: f64,
    b1: f64,
    /// The feedback tap, sign as the filter adds it.
    a1: f64,
    prev_in: f32,
    prev_out: f64,
}

impl Deemphasis {
    /// For `rate` Hz and time constant `tau` seconds (75 µs in the US).
    #[must_use]
    pub fn new(rate: f64, tau: f64) -> Self {
        let fs = rate;
        let w_c = 1.0 / tau;
        let w_ca = 2.0 * fs * (w_c / (2.0 * fs)).tan();
        let k = -w_ca / (2.0 * fs);
        let z1 = -1.0;
        let p1 = (1.0 + k) / (1.0 - k);
        let b0 = -k / (1.0 - k);
        Self {
            b0,
            b1: -z1 * b0,
            // fbtaps = [1, -p1]; the "new style" filter negates taps 1.. and
            // adds them, so the feedback is + p1 * y[n-1].
            a1: p1,
            prev_in: 0.0,
            prev_out: 0.0,
        }
    }

    /// Filter `input` in place.
    pub fn process(&mut self, input: &mut [f32]) {
        for x in input {
            let acc = self.b0 * f64::from(*x)
                + self.b1 * f64::from(self.prev_in)
                + self.a1 * self.prev_out;
            self.prev_in = *x;
            self.prev_out = acc;
            *x = acc as f32;
        }
    }

    pub const fn reset(&mut self) {
        self.prev_in = 0.0;
        self.prev_out = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn constant_frequency_offset_demodulates_to_its_deviation() {
        // A 1 kHz tone at 96 kS/s with max_dev 5 kHz → 0.2.
        let mut d = QuadratureDemod::new(96_000.0, 5_000.0);
        let iq: Vec<Complex32> = (0..200)
            .map(|n| Complex32::from_polar(1.0, TAU * 1_000.0 * n as f32 / 96_000.0))
            .collect();
        let mut out = Vec::new();
        d.process(&iq, &mut out);
        for &y in &out[1..] {
            assert!((y - 0.2).abs() < 1e-4, "{y}");
        }
        assert_eq!(out[0], 0.0, "no previous sample yet");
    }

    #[test]
    fn deemphasis_passes_dc_and_attenuates_high_frequencies() {
        let mut de = Deemphasis::new(96_000.0, 75e-6);
        let mut dc = vec![1.0f32; 2000];
        de.process(&mut dc);
        assert!((dc[1999] - 1.0).abs() < 1e-4, "dc {}", dc[1999]);
        let mut de = Deemphasis::new(96_000.0, 75e-6);
        let mut hf: Vec<f32> = (0..2000)
            .map(|n| (TAU * 10_000.0 * n as f32 / 96_000.0).sin())
            .collect();
        de.process(&mut hf);
        let peak = hf[1000..].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        // 10 kHz is ~2.1 kHz corner × 4.7 → about -14 dB.
        assert!(peak < 0.25 && peak > 0.15, "peak {peak}");
    }
}
