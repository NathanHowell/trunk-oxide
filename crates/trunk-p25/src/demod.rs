//! Conditioned IQ to soft symbols.
//!
//! The channelizer delivers each control channel at 24 kS/s (5 samples per
//! symbol), level-controlled and frequency-locked. From there:
//!
//! 1. For CQPSK, a low-pass receive filter ([`RECEIVE_CUTOFF_HZ`]) that
//!    takes out the noise the channel filter lets in beyond the signal.
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

/// The CQPSK receive filter's 6 dB point and transition width.
///
/// Not the 2880 Hz band edge of TIA-102.BAAA-A §9.3's raised cosine: the
/// sites heard here transmit wider (phase 1 is -6.6 dB at 2880 Hz and -14 dB
/// at 4.5 kHz, phase 2's H-DQPSK -6 dB at 4 kHz), and a 2.9 kHz filter made
/// every score worse. The best cut-off moves with the capture: 4.5 kHz led on
/// the two 10-17 tap sets but cost phase 2 2 % of its good MAC PDUs on the
/// 10-19 spare-dongle capture. 5 kHz never lost to it: phase 1's corrected
/// bits down 8 %, phase 2's good MAC PDUs up 3.5 % on the 10-17 sets and
/// level on 10-19, control channels best on all three (bead a4p.2). Tuning
/// it per channel is 71l.5. The root-raised cosine this replaced nearly
/// doubled phase 1's corrected bits.
pub const RECEIVE_CUTOFF_HZ: f64 = 5_000.0;
pub const RECEIVE_TRANSITION_HZ: f64 = 2_000.0;
/// Damping of the timing loop (critically damped is 1; 1/√2 settles fastest
/// without ringing).
const TIMING_DAMPING: f64 = 0.707;
/// Default for [`DemodConfig::timing_bandwidth`].
const DEFAULT_TIMING_BANDWIDTH: f32 = 0.0025;
/// Default for [`DemodConfig::bias_gain`].
const DEFAULT_BIAS_GAIN: f32 = 0.02;

/// Timing-loop correction limit, as a fraction of the nominal symbol period.
const MAX_PERIOD_ADJUST: f64 = 0.01;
/// The phase bias tracker never follows more than this: a quarter of the
/// spacing between decision levels.
const MAX_PHASE_BIAS: f32 = PI / 8.0;
/// Weight of each new symbol in the running signal power and eye noise
/// estimates (exponential averages).
const POWER_SMOOTHING: f32 = 0.01;
const NOISE_SMOOTHING: f32 = 0.005;
/// Keeps divisions by a signal power or magnitude finite.
const POWER_FLOOR: f32 = 1e-9;
/// Noise variance floor of [`DemodStats::symbol_snr_db`] (60 dB).
const NOISE_VAR_FLOOR: f32 = 1e-6;
/// Phase change of the outermost symbols (±3 deviation units); a C4FM
/// timing error is normalised by its square.
const OUTER_SYMBOL_PHASE: f32 = 3.0 * FRAC_PI_4;
/// Slack, in samples, kept behind the two-symbol look-back for the
/// interpolator's taps.
const HISTORY_MARGIN_SAMPLES: f64 = 3.0;

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
    /// The low-pass receive filter's 6 dB point and transition width, Hz:
    /// [`RECEIVE_CUTOFF_HZ`] for CQPSK, none for C4FM, whose
    /// discriminator-and-integrate receiver (BAAA-A §9.6) filters itself.
    pub lowpass: Option<(f64, f64)>,
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
            lowpass: match modulation {
                Modulation::Cqpsk => Some((RECEIVE_CUTOFF_HZ, RECEIVE_TRANSITION_HZ)),
                Modulation::C4fm => None,
            },
            timing_bandwidth: DEFAULT_TIMING_BANDWIDTH,
            reference_memory: 0.0,
            bias_gain: DEFAULT_BIAS_GAIN,
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
        -10.0 * self.noise_var.max(NOISE_VAR_FLOOR).log10()
    }

    /// Symbol clock error against the nominal rate, ppm.
    pub fn clock_ppm(&self) -> f64 {
        (self.nominal_sps / self.sps - 1.0) * 1e6
    }
}

pub struct Demodulator {
    cfg: DemodConfig,
    sps: f64,
    /// The receive filter, last tap first: output `i` is the dot product of
    /// these with the `taps - 1` inputs before it and input `i`.
    taps: Vec<f32>,
    /// The last `taps - 1` inputs, which the next block's first outputs need.
    hist: Vec<Complex32>,
    /// `hist` then the block being filtered.
    work: Vec<Complex32>,
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
        let mut taps = if let Some((cutoff, transition)) = cfg.lowpass {
            lowpass_taps(cfg.sample_rate, cutoff, transition)
        } else {
            vec![1.0]
        };
        taps.reverse();
        let (kp, ki) = loop_gains(f64::from(cfg.timing_bandwidth), TIMING_DAMPING);
        Self {
            sps,
            hist: vec![Complex32::new(0.0, 0.0); taps.len() - 1],
            work: Vec::new(),
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

    /// Appends `input` through the receive filter to `buf`, with anything
    /// not finite taken as zero.
    fn filter(&mut self, input: &[Complex32]) {
        let finite = |y: Complex32| {
            if y.re.is_finite() && y.im.is_finite() {
                y
            } else {
                Complex32::new(0.0, 0.0)
            }
        };
        let n = self.taps.len();
        if n == 1 {
            self.buf.extend(input.iter().map(|&x| finite(x)));
            return;
        }
        self.work.clear();
        self.work.extend_from_slice(&self.hist);
        self.work.extend_from_slice(input);
        self.buf
            .extend(self.work.windows(n).map(|w| finite(dot(w, &self.taps))));
        let keep = self.work.len() - (n - 1);
        self.hist.copy_from_slice(&self.work[keep..]);
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
        self.filter(input);
        let half = self.sps / 2.0;
        while self.t.floor() as usize + 2 < self.buf.len() {
            if self.t - half < 1.0 {
                self.t += self.sps;
                continue;
            }
            let z = self.at(self.t);
            let mid = self.at(self.t - f64::midpoint(self.sps, self.period_adj));
            self.power =
                (1.0 - POWER_SMOOTHING).mul_add(self.power, POWER_SMOOTHING * z.norm_sqr());

            let step = z * self.prev.conj();
            let error = match self.cfg.modulation {
                Modulation::Cqpsk => {
                    ((self.prev - z) * mid.conj()).re / self.power.max(POWER_FLOOR)
                }
                Modulation::C4fm => {
                    let phase = step.arg();
                    let mid_phase = (mid * self.prev_mid.conj()).arg();
                    let e = (self.prev_phase - phase) * mid_phase / OUTER_SYMBOL_PHASE.powi(2);
                    self.prev_phase = phase;
                    e
                }
            };
            self.prev_mid = mid;
            let error = f64::from(error.clamp(-1.0, 1.0));
            self.period_adj = self
                .ki
                .mul_add(error, self.period_adj)
                .clamp(-MAX_PERIOD_ADJUST * self.sps, MAX_PERIOD_ADJUST * self.sps);

            out.push(self.decide(z));
            self.prev = z;
            self.t += self.kp.mul_add(error, self.sps + self.period_adj);
        }
        // Keep enough history for the interpolator and the mid-point.
        let keep_from = (2.0f64.mul_add(-self.sps, self.t) - HISTORY_MARGIN_SAMPLES)
            .floor()
            .max(0.0) as usize;
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
            .clamp(-MAX_PHASE_BIAS, MAX_PHASE_BIAS);
        self.noise = (1.0 - NOISE_SMOOTHING).mul_add(self.noise, NOISE_SMOOTHING * err * err);
        if self.cfg.reference_memory > 0.0 {
            let rot = Complex32::from_polar(1.0, decided.mul_add(FRAC_PI_4, self.bias));
            let norm = z.norm().max(POWER_FLOOR);
            self.reference = z / norm + self.reference * rot * self.cfg.reference_memory;
        }
        s
    }
}

/// Accumulators the filter sums in, so its adds do not wait on one another.
const LANES: usize = 4;

/// `Σ x[i] · t[i]`, summed in [`LANES`] lanes: within rounding of the
/// sequential sum, and several times faster.
fn dot(x: &[Complex32], t: &[f32]) -> Complex32 {
    let mut lanes = [Complex32::new(0.0, 0.0); LANES];
    let (xb, xr) = x.as_chunks::<LANES>();
    let (tb, tr) = t.as_chunks::<LANES>();
    for (x, t) in xb.iter().zip(tb) {
        for ((l, x), t) in lanes.iter_mut().zip(x).zip(t) {
            *l += x * t;
        }
    }
    let tail: Complex32 = xr.iter().zip(tr).map(|(x, t)| x * t).sum();
    lanes.iter().sum::<Complex32>() + tail
}

/// Proportional and integral gains of a second-order loop with noise
/// bandwidth `bn_t` (normalised) and damping `zeta`, for unit detector gain.
fn loop_gains(bn_t: f64, zeta: f64) -> (f64, f64) {
    let theta = bn_t / (zeta + 0.25 / zeta);
    let d = theta.mul_add(theta, (2.0 * zeta).mul_add(theta, 1.0));
    (4.0 * zeta * theta / d, 4.0 * theta * theta / d)
}

/// A windowed-sinc low-pass (Blackman-Harris, 60 dB), 6 dB down at
/// `cutoff`, unit gain at DC.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
#[must_use]
pub fn lowpass_taps(fs: f64, cutoff: f64, transition: f64) -> Vec<f32> {
    use std::f64::consts::PI;
    let n = (60.0 * fs / (22.0 * transition)) as usize | 1;
    let m = (n - 1) as f64;
    let half = (n - 1) / 2;
    let w0 = 2.0 * PI * cutoff / fs;
    let taps: Vec<f64> = (0..n)
        .map(|i| {
            let x = 2.0 * PI * i as f64 / m;
            let window = 0.01168f64.mul_add(
                -(3.0 * x).cos(),
                0.14128f64.mul_add((2.0 * x).cos(), 0.48829f64.mul_add(-x.cos(), 0.35875)),
            );
            let k = i as f64 - half as f64;
            let sinc = if i == half {
                w0 / PI
            } else {
                (k * w0).sin() / (k * PI)
            };
            sinc * window
        })
        .collect();
    let gain: f64 = taps.iter().sum();
    taps.iter().map(|&x| (x / gain) as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::{self, Channel, Rng};

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn the_filter_is_the_direct_sum_to_within_rounding_in_blocks_of_any_size() {
        let mut rng = Rng::new(7);
        let x: Vec<Complex32> = (0..700)
            .map(|_| Complex32::new(rng.gauss() as f32, rng.gauss() as f32))
            .collect();
        let mut d = Demodulator::new(DemodConfig::new(24_000.0, Modulation::Cqpsk));
        let taps: Vec<f32> = d.taps.iter().rev().copied().collect();
        let mut at = 0;
        for size in [1usize, 5, 33, 64, 200, 397] {
            let end = (at + size).min(x.len());
            d.buf.clear();
            d.filter(&x[at..end]);
            for (i, y) in d.buf.iter().enumerate() {
                let n = at + i;
                let want =
                    taps.iter()
                        .enumerate()
                        .fold(Complex32::new(0.0, 0.0), |acc, (k, &t)| {
                            acc + n
                                .checked_sub(k)
                                .map_or(Complex32::new(0.0, 0.0), |m| x[m] * t)
                        });
                assert!((y - want).norm() < 2e-6, "sample {n}: {y} vs {want}");
            }
            at = end;
        }
    }

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
        let iq = channel.apply(&modulate(&dibits, 5), 5, crate::symbols::SYMBOL_RATE);
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
        let cfg = DemodConfig::new(24000.0, Modulation::Cqpsk);
        let r = ser(cfg, &impaired(16.0), testgen::modulate_cqpsk);
        assert!(r < 0.01, "SER {r}");
    }

    #[test]
    fn decision_feedback_reference_lowers_symbol_errors() {
        let plain = DemodConfig::new(24000.0, Modulation::Cqpsk);
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
            crate::symbols::SYMBOL_RATE,
        );
        let mut d = Demodulator::new(cfg);
        d.process(&iq, &mut Vec::new());
        let s = d.stats();
        assert!((s.bias_hz - 60.0).abs() < 10.0, "{s:?}");
        assert!(5.0f64.mul_add(-(1.0 - 80e-6), s.sps).abs() < 0.01, "{s:?}");
    }
}
