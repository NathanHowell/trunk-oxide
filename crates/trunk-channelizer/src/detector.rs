//! Wideband signal detector: trunk-recorder's `signal_detector_cvf` (from
//! gr-inspector), as it ran on every conventional source's raw IQ.
//!
//! Every `interval` of samples it takes one FFT of `fft_len` contiguous
//! samples (Hamming window), turns it into a dB periodogram, smooths each
//! bin with a single-pole IIR, picks a threshold (given, or found as the
//! first large jump in the sorted spectrum above the median) and reports
//! each run of adjacent bins above it as a signal: its centre relative to
//! the source centre, its quantised bandwidth, its strongest bin. The
//! state machine turns a signal on a conventional channel's frequency into
//! "enable that channel's recorder".

// Plain multiply-adds, as the C++ wrote them.
#![allow(clippy::suboptimal_flops)]

use num_complex::Complex32;

use crate::fft::{Direction, Plan};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetectorConfig {
    pub fft_len: usize,
    /// A fixed threshold in dB, or `None` to find one per spectrum.
    pub threshold_db: Option<f32>,
    /// Auto threshold: a jump between sorted neighbours larger than
    /// `1 - sensitivity` of the spectrum's range marks the threshold.
    pub sensitivity: f32,
    /// IIR smoothing weight of the newest periodogram (1 = none).
    pub average: f64,
    /// Bandwidth quantum as a fraction of the sample rate.
    pub quantization: f32,
    /// Quantised bandwidth a signal must have, in Hz.
    pub min_bw_hz: f32,
    pub max_bw_hz: f32,
    /// How often to look, in seconds of input.
    pub interval_s: f64,
}

impl Default for DetectorConfig {
    /// trunk-recorder's settings.
    fn default() -> Self {
        Self {
            fft_len: 1024,
            threshold_db: None,
            sensitivity: 0.9,
            average: 0.8,
            quantization: 0.01,
            min_bw_hz: 0.0,
            max_bw_hz: 50_000.0,
            interval_s: 0.1,
        }
    }
}

/// A run of bins above the threshold.
#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    /// Centre relative to the source centre, Hz.
    pub center_hz: f64,
    /// Quantised width, Hz.
    pub bandwidth_hz: f64,
    /// The strongest bin, dB (the periodogram's relative scale).
    pub max_rssi_db: f32,
    /// The threshold it cleared, dB.
    pub threshold_db: f32,
}

pub struct SignalDetector {
    cfg: DetectorConfig,
    rate: f64,
    window: Vec<f32>,
    plan: Plan,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    /// Smoothed spectrum, fft-shifted (bin 0 = -rate/2), dB.
    smoothed: Vec<f64>,
    primed: bool,
    /// Bin centre frequencies, Hz relative to the source centre.
    freq: Vec<f32>,
    sorted: Vec<f32>,
    /// Samples seen since the last look.
    since: usize,
    interval: usize,
}

impl std::fmt::Debug for SignalDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalDetector")
            .field("cfg", &self.cfg)
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

impl SignalDetector {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn new(rate: f64, cfg: DetectorConfig) -> Self {
        let n = cfg.fft_len.max(2);
        let m = (n - 1) as f32;
        let window = (0..n)
            .map(|i| 0.54 - 0.46 * (std::f32::consts::TAU * i as f32 / m).cos())
            .collect();
        let freq = (0..n)
            .map(|i| (-rate / 2.0 + i as f64 * rate / n as f64) as f32)
            .collect();
        let plan = Plan::new(n, Direction::Forward);
        Self {
            cfg,
            rate,
            window,
            scratch: plan.scratch(),
            plan,
            buf: vec![Complex32::ZERO; n],
            smoothed: vec![0.0; n],
            primed: false,
            freq,
            sorted: vec![0.0; n],
            since: 0,
            interval: (cfg.interval_s * rate).round().max(1.0) as usize,
        }
    }

    /// The smoothed spectrum of the last look, in dB, fft-shifted (index 0
    /// is -rate/2), for diagnostics.
    pub fn smoothed_db(&self) -> &[f64] {
        &self.smoothed
    }

    pub const fn config(&self) -> &DetectorConfig {
        &self.cfg
    }

    pub const fn set_threshold(&mut self, threshold_db: Option<f32>) {
        self.cfg.threshold_db = threshold_db;
    }

    /// Feed one block of source IQ. When a look is due and the block is at
    /// least `fft_len` long, append what it finds to `out`.
    pub fn push(&mut self, block: &[Complex32], out: &mut Vec<Detection>) {
        self.since += block.len();
        if self.since < self.interval || block.len() < self.cfg.fft_len {
            return;
        }
        self.since = 0;
        self.periodogram(&block[..self.cfg.fft_len]);
        let threshold = self
            .cfg
            .threshold_db
            .unwrap_or_else(|| self.auto_threshold());
        self.edges(threshold, out);
    }

    /// `smoothed` = IIR(10 log10(|FFT(window * x)|^2 / N^2)), fft-shifted.
    /// The C++ passed the FFT length to volk as the *normalisation factor*
    /// (amplitude divisor) with a resolution bandwidth of 1, hence N^2;
    /// configured `signal_detector_threshold` values are on that scale.
    #[allow(clippy::cast_precision_loss)]
    fn periodogram(&mut self, x: &[Complex32]) {
        let n = self.cfg.fft_len;
        for ((b, s), w) in self.buf.iter_mut().zip(x).zip(&self.window) {
            *b = s * w;
        }
        self.plan.execute(&mut self.buf, &mut self.scratch);
        let scale = 1.0 / (n as f32 * n as f32);
        let half = n / 2;
        let (a, one_minus_a) = (self.cfg.average, 1.0 - self.cfg.average);
        for k in 0..n {
            // shifted index: bin k of the FFT lands at (k + n/2) % n
            let dst = (k + n - half) % n;
            let p = self.buf[k].norm_sqr() * scale;
            let db = f64::from(10.0 * p.max(1e-30).log10());
            self.smoothed[dst] = if self.primed {
                a * db + one_minus_a * self.smoothed[dst]
            } else {
                a * db
            };
        }
        self.primed = true;
    }

    /// The first jump above the median larger than `1 - sensitivity` of the
    /// range, else the maximum (so nothing is above it).
    #[allow(clippy::cast_possible_truncation)]
    fn auto_threshold(&mut self) -> f32 {
        let n = self.cfg.fft_len;
        for (s, v) in self.sorted.iter_mut().zip(&self.smoothed) {
            *s = *v as f32;
        }
        self.sorted.sort_by(f32::total_cmp);
        let range = self.sorted[n - 1] - self.sorted[0];
        for i in n / 2..n - 1 {
            if (self.sorted[i + 1] - self.sorted[i]) / range > 1.0 - self.cfg.sensitivity {
                return self.sorted[i];
            }
        }
        self.sorted[n - 1]
    }

    /// Runs of adjacent bins above `threshold`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn edges(&self, threshold: f32, out: &mut Vec<Detection>) {
        let quantum = (f64::from(self.cfg.quantization) * self.rate).floor();
        let mut run: Option<(usize, usize, f32)> = None;
        let finish =
            |run: &mut Option<(usize, usize, f32)>, out: &mut Vec<Detection>, last: bool| {
                let Some((start, end, max)) = run.take() else {
                    return;
                };
                let bandwidth = f64::from(self.freq[end] - self.freq[start]);
                let quantized = quantum * (bandwidth / quantum).round();
                // The C++ skipped the width check for a run touching the top bin.
                if last
                    || (quantized >= f64::from(self.cfg.min_bw_hz)
                        && quantized <= f64::from(self.cfg.max_bw_hz))
                {
                    out.push(Detection {
                        center_hz: f64::from(self.freq[start] + self.freq[end]) / 2.0,
                        bandwidth_hz: quantized,
                        max_rssi_db: max,
                        threshold_db: threshold,
                    });
                }
            };
        for (i, &v) in self.smoothed.iter().enumerate() {
            let v = v as f32;
            if v > threshold {
                match &mut run {
                    Some((_, end, max)) => {
                        *end = i;
                        *max = max.max(v);
                    }
                    None => run = Some((i, i, v)),
                }
            } else {
                finish(&mut run, out, false);
            }
        }
        finish(&mut run, out, true);
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use rand::{Rng as _, SeedableRng as _};
    use rand_chacha::ChaCha8Rng;
    use std::f64::consts::TAU;

    fn noise(n: usize, amp: f32, seed: u64) -> Vec<Complex32> {
        // Uniform in [-1, 1) with no DC.
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut next = || rng.random_range(-1.0f32..1.0);
        (0..n)
            .map(|_| Complex32::new(next() * amp, next() * amp))
            .collect()
    }

    fn with_tone(mut iq: Vec<Complex32>, rate: f64, hz: f64, amp: f32) -> Vec<Complex32> {
        for (i, s) in iq.iter_mut().enumerate() {
            let ph = (TAU * hz * i as f64 / rate) as f32;
            *s += Complex32::new(amp * ph.cos(), amp * ph.sin());
        }
        iq
    }

    #[test]
    fn looks_once_per_interval_and_finds_a_carrier() {
        let rate = 912_000.0;
        let mut d = SignalDetector::new(rate, DetectorConfig::default());
        let block = with_tone(noise(8208, 0.01, 3), rate, 100_000.0, 0.3);
        let mut out = Vec::new();
        // 0.1 s = 91 200 samples = 12 blocks of 8208 before the first look
        for _ in 0..11 {
            d.push(&block, &mut out);
        }
        assert_eq!(out, Vec::new());
        d.push(&block, &mut out);
        assert_eq!(out.len(), 1, "{out:?}");
        let det = &out[0];
        assert!((det.center_hz - 100_000.0).abs() < rate / 1024.0, "{det:?}");
        assert!(det.max_rssi_db > det.threshold_db);
        assert!(det.bandwidth_hz <= 9_120.0);
    }

    #[test]
    fn a_fixed_threshold_is_used_as_given_and_quiet_spectra_report_nothing() {
        let rate = 912_000.0;
        let cfg = DetectorConfig {
            threshold_db: Some(-40.0),
            interval_s: 0.0,
            ..DetectorConfig::default()
        };
        let mut d = SignalDetector::new(rate, cfg);
        let mut out = Vec::new();
        d.push(&noise(2048, 0.01, 5), &mut out);
        assert!(out.is_empty(), "{out:?}");
        d.push(
            &with_tone(noise(2048, 0.01, 5), rate, -200_000.0, 0.5),
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].threshold_db, -40.0);
        assert!((out[0].center_hz + 200_000.0).abs() < rate / 1024.0);

        // Auto threshold on noise alone: the search starts at the median,
        // so whatever it reports (the C++ did report noise peaks) sits in
        // the top half and clears its own threshold.
        let mut d = SignalDetector::new(
            rate,
            DetectorConfig {
                interval_s: 0.0,
                ..DetectorConfig::default()
            },
        );
        out.clear();
        d.push(&noise(2048, 0.01, 9), &mut out);
        let median = {
            let mut s: Vec<f64> = d.smoothed.clone();
            s.sort_by(f64::total_cmp);
            s[s.len() / 2] as f32
        };
        for det in &out {
            assert!(
                det.threshold_db >= median && det.max_rssi_db > det.threshold_db,
                "{det:?}"
            );
        }
    }

    #[test]
    fn two_carriers_are_two_signals_and_wide_ones_are_rejected() {
        let rate = 912_000.0;
        let cfg = DetectorConfig {
            interval_s: 0.0,
            ..DetectorConfig::default()
        };
        let mut d = SignalDetector::new(rate, cfg);
        let iq = with_tone(
            with_tone(noise(2048, 0.01, 7), rate, 50_000.0, 0.3),
            rate,
            300_000.0,
            0.3,
        );
        let mut out = Vec::new();
        d.push(&iq, &mut out);
        let mut centres: Vec<f64> = out.iter().map(|d| d.center_hz).collect();
        centres.sort_by(f64::total_cmp);
        assert_eq!(centres.len(), 2, "{out:?}");
        assert!(
            (centres[0] - 50_000.0).abs() < 1_000.0 && (centres[1] - 300_000.0).abs() < 1_000.0
        );

        let cfg = DetectorConfig {
            interval_s: 0.0,
            max_bw_hz: 100.0,
            ..DetectorConfig::default()
        };
        let mut d = SignalDetector::new(rate, cfg);
        out.clear();
        // a 60 kHz wide block of tones: wider than max_bw, dropped
        let mut wide = noise(2048, 0.01, 7);
        for k in 0..7 {
            wide = with_tone(wide, rate, 100_000.0 + 10_000.0 * f64::from(k), 0.2);
        }
        d.push(&wide, &mut out);
        assert!(out.iter().all(|d| d.bandwidth_hz <= 100.0), "{out:?}");
    }
}
