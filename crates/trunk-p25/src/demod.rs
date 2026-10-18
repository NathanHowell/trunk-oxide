//! Conditioned IQ to soft symbols.
//!
//! The channelizer delivers each control channel at 24 kS/s (5 samples per
//! symbol), level-controlled and frequency-locked. From there:
//!
//! 1. Optional root-raised-cosine matched filter (α = 0.2).
//! 2. Gardner timing recovery with a proportional-integral loop and cubic
//!    interpolation. For CQPSK the detector runs on the complex samples (as
//!    op25's `gardner_cc`); for C4FM, whose envelope is constant, it runs on
//!    the per-symbol phase change.
//! 3. Differential detection: the phase step from the previous symbol, in
//!    deviation units (±1, ±3). With `reference_memory` > 0 the previous
//!    symbol is replaced by a decision-feedback reference averaged over
//!    several symbols, which takes most of the noise out of the reference.
//! 4. A decision-directed tracker removes the constant phase bias a residual
//!    frequency offset leaves on every step (op25 runs a Costas loop for this).
//!
//! Both P25 modulations are read the same way: a C4FM symbol's phase change
//! over one symbol period is its deviation × 2π × T, the same ±45°/±135° steps
//! CQPSK sends.

use std::f32::consts::{FRAC_PI_4, PI};

use num_complex::Complex32;

use crate::symbols::{LEVELS, PHASE2_SYMBOL_RATE, SYMBOL_RATE, slice, wrap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modulation {
    Cqpsk,
    C4fm,
}

#[derive(Clone, Copy, Debug)]
pub struct DemodConfig {
    pub sample_rate: f64,
    /// 4800 baud for Phase 1, 6000 for Phase 2 TDMA.
    pub symbol_rate: f64,
    pub modulation: Modulation,
    pub matched_filter: bool,
    /// Timing loop noise bandwidth, normalised to the symbol rate.
    pub timing_bandwidth: f32,
    /// Decision-feedback reference weight in [0, 1); 0 is plain differential
    /// detection.
    pub reference_memory: f32,
    /// Phase-bias tracker gain per symbol.
    pub bias_gain: f32,
}

impl DemodConfig {
    pub const fn new(sample_rate: f64, modulation: Modulation) -> Self {
        Self {
            sample_rate,
            symbol_rate: SYMBOL_RATE,
            modulation,
            matched_filter: false,
            timing_bandwidth: 0.0025,
            reference_memory: 0.0,
            bias_gain: 0.02,
        }
    }

    /// Phase 2 TDMA: H-DQPSK at 6000 baud, read like CQPSK.
    pub const fn phase2(sample_rate: f64) -> Self {
        Self {
            symbol_rate: PHASE2_SYMBOL_RATE,
            ..Self::new(sample_rate, Modulation::Cqpsk)
        }
    }
}

/// Four-point Lagrange interpolation between `y[1]` and `y[2]`.
pub fn cubic(y: &[Complex32; 4], mu: f32) -> Complex32 {
    let c0 = y[1];
    let c1 = y[2] - y[0] / 3.0 - y[1] / 2.0 - y[3] / 6.0;
    let c2 = (y[0] + y[2]) / 2.0 - y[1];
    let c3 = (y[3] - y[0]) / 6.0 + (y[1] - y[2]) / 2.0;
    ((c3 * mu + c2) * mu + c1) * mu + c0
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DemodStats {
    pub symbols: u64,
    /// Mean squared distance of soft symbols from their decisions.
    pub noise_var: f32,
    /// Residual frequency offset implied by the phase bias, Hz.
    pub bias_hz: f32,
    /// Current samples per symbol estimate.
    pub sps: f64,
    /// Samples per symbol at exactly the nominal symbol rate.
    pub nominal_sps: f64,
}

impl DemodStats {
    /// Eye quality as an SNR: ideal levels are 2 apart, so a noise variance
    /// of 1 (σ = 1) is 0 dB.
    pub fn symbol_snr_db(&self) -> f32 {
        -10.0 * self.noise_var.max(1e-6).log10()
    }

    /// Symbol clock error against the nominal rate, ppm.
    pub fn clock_ppm(&self) -> f64 {
        (self.nominal_sps / self.sps - 1.0) * 1e6
    }
}

pub struct Demodulator {
    cfg: DemodConfig,
    sps: f64,
    taps: Vec<f32>,
    hist: Vec<Complex32>,
    hist_pos: usize,
    buf: Vec<Complex32>,
    t: f64,
    period_adj: f64,
    kp: f64,
    ki: f64,
    prev: Complex32,
    prev_mid: Complex32,
    prev_phase: f32,
    reference: Complex32,
    power: f32,
    bias: f32,
    noise: f32,
    symbols: u64,
}

impl Demodulator {
    pub fn new(cfg: DemodConfig) -> Self {
        let sps = cfg.sample_rate / cfg.symbol_rate;
        assert!(sps >= 2.0, "need at least 2 samples per symbol");
        let taps = if cfg.matched_filter {
            rrc_taps(sps, 0.2, 8)
        } else {
            vec![1.0]
        };
        let (kp, ki) = loop_gains(f64::from(cfg.timing_bandwidth), 0.707);
        Self {
            sps,
            hist: vec![Complex32::new(0.0, 0.0); taps.len()],
            hist_pos: 0,
            taps,
            buf: Vec::new(),
            t: 2.0,
            period_adj: 0.0,
            kp: kp * sps,
            ki: ki * sps,
            prev: Complex32::new(0.0, 0.0),
            prev_mid: Complex32::new(0.0, 0.0),
            prev_phase: 0.0,
            reference: Complex32::new(0.0, 0.0),
            power: 1.0,
            bias: 0.0,
            noise: 1.0,
            symbols: 0,
            cfg,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.cfg);
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn stats(&self) -> DemodStats {
        DemodStats {
            symbols: self.symbols,
            noise_var: self.noise,
            bias_hz: (f64::from(self.bias) * self.cfg.symbol_rate / std::f64::consts::TAU) as f32,
            sps: self.sps + self.period_adj,
            nominal_sps: self.sps,
        }
    }

    fn filter(&mut self, x: Complex32) -> Complex32 {
        if self.taps.len() == 1 {
            return x;
        }
        let n = self.taps.len();
        self.hist[self.hist_pos] = x;
        self.hist_pos = (self.hist_pos + 1) % n;
        let mut acc = Complex32::new(0.0, 0.0);
        for (k, &tap) in self.taps.iter().enumerate() {
            acc += self.hist[(self.hist_pos + n - 1 - k) % n] * tap;
        }
        acc
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn at(&self, t: f64) -> Complex32 {
        let i = t.floor() as usize;
        let mu = (t - t.floor()) as f32;
        cubic(
            &[
                self.buf[i - 1],
                self.buf[i],
                self.buf[i + 1],
                self.buf[i + 2],
            ],
            mu,
        )
    }

    /// Demodulate `input`, appending one soft symbol per symbol period.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<f32>) {
        for &x in input {
            let y = self.filter(x);
            let y = if y.re.is_finite() && y.im.is_finite() {
                y
            } else {
                Complex32::new(0.0, 0.0)
            };
            self.buf.push(y);
        }
        let half = self.sps / 2.0;
        while self.t.floor() as usize + 2 < self.buf.len() {
            if self.t - half < 1.0 {
                self.t += self.sps;
                continue;
            }
            let z = self.at(self.t);
            let mid = self.at(self.t - f64::midpoint(self.sps, self.period_adj));
            self.power = 0.99f32.mul_add(self.power, 0.01 * z.norm_sqr());

            let step = z * self.prev.conj();
            let error = match self.cfg.modulation {
                Modulation::Cqpsk => ((self.prev - z) * mid.conj()).re / self.power.max(1e-9),
                Modulation::C4fm => {
                    let phase = step.arg();
                    let mid_phase = (mid * self.prev_mid.conj()).arg();
                    let e = (self.prev_phase - phase) * mid_phase / (3.0 * FRAC_PI_4).powi(2);
                    self.prev_phase = phase;
                    e
                }
            };
            self.prev_mid = mid;
            let error = f64::from(error.clamp(-1.0, 1.0));
            self.period_adj = self
                .ki
                .mul_add(error, self.period_adj)
                .clamp(-0.01 * self.sps, 0.01 * self.sps);

            out.push(self.decide(z));
            self.prev = z;
            self.t += self.kp.mul_add(error, self.sps + self.period_adj);
        }
        // Keep enough history for the interpolator and the mid-point.
        let keep_from = (2.0f64.mul_add(-self.sps, self.t) - 3.0).floor().max(0.0) as usize;
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.t -= keep_from as f64;
        }
    }

    fn decide(&mut self, z: Complex32) -> f32 {
        self.symbols += 1;
        let reference = if self.cfg.reference_memory > 0.0 {
            self.reference
        } else {
            self.prev
        };
        let phase = (z * reference.conj()).arg() - self.bias;
        let s = wrap(phase / FRAC_PI_4);
        let decided = LEVELS[usize::from(slice(s))];
        let err = s - decided;
        self.bias = (self.cfg.bias_gain * err)
            .mul_add(FRAC_PI_4, self.bias)
            .clamp(-PI / 8.0, PI / 8.0);
        self.noise = 0.995f32.mul_add(self.noise, 0.005 * err * err);
        if self.cfg.reference_memory > 0.0 {
            let rot = Complex32::from_polar(1.0, decided.mul_add(FRAC_PI_4, self.bias));
            let norm = z.norm().max(1e-9);
            self.reference = z / norm + self.reference * rot * self.cfg.reference_memory;
        }
        s
    }
}

/// Proportional and integral gains of a second-order loop with noise
/// bandwidth `bn_t` (normalised) and damping `zeta`, for unit detector gain.
fn loop_gains(bn_t: f64, zeta: f64) -> (f64, f64) {
    let theta = bn_t / (zeta + 0.25 / zeta);
    let d = theta.mul_add(theta, (2.0 * zeta).mul_add(theta, 1.0));
    (4.0 * zeta * theta / d, 4.0 * theta * theta / d)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn rrc_taps(sps: f64, alpha: f64, span: usize) -> Vec<f32> {
    let n = (sps * span as f64).round() as usize | 1;
    let mid = (n / 2) as f64;
    let taps: Vec<f64> = (0..n)
        .map(|i| {
            let t = (i as f64 - mid) / sps;
            let pi = std::f64::consts::PI;
            if t.abs() < 1e-12 {
                1.0 - alpha + 4.0 * alpha / pi
            } else if (t.abs() - 1.0 / (4.0 * alpha)).abs() < 1e-9 {
                alpha / 2f64.sqrt()
                    * (1.0 - 2.0 / pi).mul_add(
                        (pi / (4.0 * alpha)).cos(),
                        (1.0 + 2.0 / pi) * (pi / (4.0 * alpha)).sin(),
                    )
            } else {
                (4.0 * alpha * t).mul_add(
                    (pi * t * (1.0 + alpha)).cos(),
                    (pi * t * (1.0 - alpha)).sin(),
                ) / (pi * t * (4.0 * alpha * t).mul_add(-(4.0 * alpha * t), 1.0))
            }
        })
        .collect();
    let gain: f64 = taps.iter().sum();
    taps.iter().map(|&x| (x / gain) as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::{self, Channel, Rng};

    fn random_dibits(rng: &mut Rng, n: usize) -> Vec<u8> {
        #[allow(clippy::cast_possible_truncation)]
        (0..n).map(|_| (rng.next_u64() & 3) as u8).collect()
    }

    /// Symbol error rate after the loops settle, aligning output to input.
    fn ser(
        cfg: DemodConfig,
        channel: &Channel,
        modulate: fn(&[u8], usize) -> Vec<Complex32>,
    ) -> f64 {
        let mut rng = Rng::new(21);
        let dibits = random_dibits(&mut rng, 6000);
        let iq = channel.apply(&modulate(&dibits, 5), 5);
        let mut d = Demodulator::new(cfg);
        let mut soft = Vec::new();
        for chunk in iq.chunks(997) {
            d.process(chunk, &mut soft);
        }
        let hard: Vec<u8> = soft.iter().map(|&s| slice(s)).collect();
        // Find the alignment with the fewest errors over a window.
        let settle = 2000; // the narrow timing loop pulls in 80 ppm within ~2000 symbols
        let window = &dibits[settle..settle + 200];
        let at = |i: usize, lag: isize| i.checked_add_signed(lag).and_then(|j| hard.get(j));
        let (lag, _) = (-10..60)
            .map(|lag: isize| {
                let errs = window
                    .iter()
                    .enumerate()
                    .filter(|&(i, &d)| at(settle + i, lag).is_none_or(|&h| h != d))
                    .count();
                (lag, errs)
            })
            .min_by_key(|&(_, e)| e)
            .unwrap();
        let mut errs = 0;
        let mut n = 0;
        for (i, &d) in dibits.iter().enumerate().skip(settle).take(3500) {
            if let Some(&h) = at(i, lag) {
                n += 1;
                errs += usize::from(h != d);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let r = errs as f64 / f64::from(u32::try_from(n).unwrap());
        r
    }

    /// The synthetic channel's noise is white across the whole 24 kHz (the
    /// real channelizer has already band-limited it), so noise tests use the
    /// matched filter.
    fn filtered(modulation: Modulation) -> DemodConfig {
        DemodConfig {
            matched_filter: true,
            ..DemodConfig::new(24000.0, modulation)
        }
    }

    fn impaired(es_n0_db: f64) -> Channel {
        Channel {
            es_n0_db,
            freq_offset_hz: 60.0,
            delay: 2.4,
            clock_ppm: 80.0,
            seed: 5,
        }
    }

    #[test]
    fn cqpsk_clean_signal_is_error_free() {
        let cfg = DemodConfig::new(24000.0, Modulation::Cqpsk);
        assert_eq!(ser(cfg, &impaired(40.0), testgen::modulate_cqpsk), 0.0);
    }

    #[test]
    fn c4fm_clean_signal_is_error_free() {
        let cfg = DemodConfig::new(24000.0, Modulation::C4fm);
        assert_eq!(ser(cfg, &impaired(40.0), testgen::modulate_c4fm), 0.0);
    }

    #[test]
    fn cqpsk_tracks_timing_and_frequency_in_noise() {
        let cfg = filtered(Modulation::Cqpsk);
        let r = ser(cfg, &impaired(16.0), testgen::modulate_cqpsk);
        assert!(r < 0.01, "SER {r}");
    }

    #[test]
    fn decision_feedback_reference_lowers_symbol_errors() {
        let plain = filtered(Modulation::Cqpsk);
        let df = DemodConfig {
            reference_memory: 0.6,
            ..plain
        };
        let a = ser(plain, &impaired(11.0), testgen::modulate_cqpsk);
        let b = ser(df, &impaired(11.0), testgen::modulate_cqpsk);
        assert!(b < a * 0.8, "plain {a} df {b}");
    }

    #[test]
    fn residual_frequency_offset_is_reported() {
        let cfg = DemodConfig::new(24000.0, Modulation::Cqpsk);
        let mut rng = Rng::new(3);
        let iq = impaired(30.0).apply(
            &testgen::modulate_cqpsk(&random_dibits(&mut rng, 3000), 5),
            5,
        );
        let mut d = Demodulator::new(cfg);
        d.process(&iq, &mut Vec::new());
        let s = d.stats();
        assert!((s.bias_hz - 60.0).abs() < 10.0, "{s:?}");
        assert!(5.0f64.mul_add(-(1.0 - 80e-6), s.sps).abs() < 0.01, "{s:?}");
    }
}
