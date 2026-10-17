//! Per-channel conditioning for digital voice/control channels: power probe,
//! RMS AGC and band-edge FLL, ported from the GNU Radio chain that
//! `xlat_channelizer` used to build (`probe_avg_mag_sqrd_c`, `rms_agc`,
//! `fll_band_edge_cc` on the legacy 3.10 loop filter). Arithmetic follows the
//! GR blocks (f32 loop state, f64 AGC/probe averages) so the output can be
//! compared sample-for-sample against recorded baselines.

use std::f64::consts::{PI, TAU};

use num_complex::Complex32;

use crate::resampler::{ArbResampler, xlat_arb_taps};

/// Power squelch behaviour (`xlat_channelizer::set_analog_squelch`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SquelchMode {
    /// Power average smoothing factor.
    pub alpha: f64,
    /// Raised-cosine ramp length in samples on open/close (0 = hard).
    pub ramp: u32,
    /// Drop muted samples (true) or output zeros (false).
    pub gate: bool,
}

impl SquelchMode {
    /// Analog FM: fast average, 10-sample ramps, zeros while muted.
    pub const ANALOG: Self = Self {
        alpha: 0.01,
        ramp: 10,
        gate: false,
    };
    /// Digital (conventional P25): slow average, muted samples dropped.
    pub const DIGITAL: Self = Self {
        alpha: 0.0001,
        ramp: 0,
        gate: true,
    };
}

/// A squelch transition, as `callback_pwr_squelch_cc` publishes them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SquelchEvent {
    pub muted: bool,
    /// Squelch power average at the transition.
    pub pwr_db: f64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SqState {
    Muted,
    Attack,
    Unmuted,
    Decay,
}

/// Port of trunk-recorder's `callback_pwr_squelch_cc`.
struct Squelch {
    mode: SquelchMode,
    threshold: f64,
    pwr: f64,
    state: SqState,
    ramped: u32,
    envelope: f64,
    tag_next_unmuted: bool,
}

impl Squelch {
    fn new(mode: SquelchMode, db: f64) -> Self {
        Self {
            mode,
            threshold: 10f64.powf(db / 10.0),
            pwr: 0.0,
            state: SqState::Muted,
            ramped: 0,
            envelope: if mode.ramp > 0 { 0.0 } else { 1.0 },
            tag_next_unmuted: true,
        }
    }

    fn pwr_db(&self) -> f64 {
        10.0 * self.pwr.log10()
    }

    const fn unmuted(&self) -> bool {
        matches!(self.state, SqState::Unmuted | SqState::Attack)
    }

    /// Squelch `buf` in place; returns how many samples remain (all of them
    /// unless gating).
    fn process(&mut self, buf: &mut [Complex32], events: &mut Vec<SquelchEvent>) -> usize {
        let a = self.mode.alpha;
        let ramp = self.mode.ramp;
        let mut j = 0;
        for i in 0..buf.len() {
            let x = buf[i];
            let mag2 = f64::from(x.re * x.re + x.im * x.im);
            self.pwr = a * mag2 + (1.0 - a) * self.pwr;
            let mute = self.pwr < self.threshold;
            match self.state {
                SqState::Muted => {
                    if !mute {
                        self.state = if ramp > 0 {
                            SqState::Attack
                        } else {
                            SqState::Unmuted
                        };
                        if self.state == SqState::Unmuted {
                            self.tag_next_unmuted = true;
                        }
                    }
                }
                SqState::Unmuted => {
                    if self.tag_next_unmuted {
                        self.tag_next_unmuted = false;
                        events.push(SquelchEvent {
                            muted: false,
                            pwr_db: self.pwr_db(),
                        });
                    }
                    if mute {
                        self.state = if ramp > 0 {
                            SqState::Decay
                        } else {
                            SqState::Muted
                        };
                        if self.state == SqState::Muted {
                            events.push(SquelchEvent {
                                muted: true,
                                pwr_db: self.pwr_db(),
                            });
                        }
                    }
                }
                SqState::Attack => {
                    self.ramped += 1;
                    self.envelope =
                        0.5 - (PI * f64::from(self.ramped) / f64::from(ramp)).cos() / 2.0;
                    if self.ramped >= ramp {
                        self.state = SqState::Unmuted;
                        self.tag_next_unmuted = true;
                        self.envelope = 1.0;
                    }
                }
                SqState::Decay => {
                    self.ramped = self.ramped.saturating_sub(1);
                    self.envelope =
                        0.5 - (PI * f64::from(self.ramped) / f64::from(ramp)).cos() / 2.0;
                    if self.ramped == 0 {
                        self.state = SqState::Muted;
                        events.push(SquelchEvent {
                            muted: true,
                            pwr_db: self.pwr_db(),
                        });
                    }
                }
            }
            if self.state != SqState::Muted {
                // gr_complex(envelope, 0) is float: envelope rounds to f32
                buf[j] = x * (self.envelope as f32);
                j += 1;
            } else if !self.mode.gate {
                buf[j] = Complex32::ZERO;
                j += 1;
            }
        }
        j
    }
}

/// Tunables. Defaults reproduce the GR chain.
#[derive(Clone, Copy, Debug)]
pub struct ConditionerParams {
    /// `rms_agc` smoothing factor.
    pub agc_alpha: f64,
    /// `rms_agc` output RMS target.
    pub agc_reference: f64,
    /// FLL loop bandwidth scale: 1.0 = GR's `2*pi/sps/250`.
    pub fll_bw_scale: f64,
    /// Power-probe smoothing factor.
    pub probe_alpha: f64,
}

impl ConditionerParams {
    pub const DEFAULT: Self = Self {
        agc_alpha: 0.45,
        agc_reference: 0.85,
        fll_bw_scale: 1.0,
        probe_alpha: 0.0001,
    };
}

impl Default for ConditionerParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Smallest value `fll_band_edge_cc` loops on: frequency is clamped to
/// `+-2*pi*2/sps` rad/sample.
fn freq_limit(sps: f32) -> f32 {
    (TAU * (2.0 / f64::from(sps))) as f32
}

fn sinc(x: f32) -> f32 {
    let arg = std::f32::consts::PI * x;
    if x == 0.0 { 1.0 } else { arg.sin() / arg }
}

/// The band-edge pair as `fll_band_edge_cc::design_filter` builds them.
/// Returns `(lower, upper)` in convolution order (`y[n] = sum T[m] x[n-m]`).
fn design_taps(sps: f32, rolloff: f32, filter_size: usize) -> (Vec<Complex32>, Vec<Complex32>) {
    let m = (filter_size as f32 / sps).round_ties_even();
    let half_sps_inv = 2.0 / sps;
    let mut power = 0.0f32;
    let mut bb = Vec::with_capacity(filter_size);
    for i in 0..filter_size {
        let k = -m + i as f32 * half_sps_inv;
        let position = rolloff * k;
        let tap = sinc(position - 0.5) + sinc(position + 0.5);
        power += tap * tap;
        bb.push(tap);
    }
    let n = (filter_size as isize - 1) / 2;
    let invpower = 1.0 / power;
    let inv_twice_sps = 0.5 / sps;
    let mut lower = vec![Complex32::ZERO; filter_size];
    let mut upper = vec![Complex32::ZERO; filter_size];
    for (i, b) in bb.iter().enumerate() {
        let tap = b * invpower;
        let k = (i as isize - n) as f32 * inv_twice_sps;
        let index = filter_size - i - 1;
        let ph = -TAU * f64::from(1.0 + rolloff) * f64::from(k);
        let (s, c) = (ph.sin() as f32, ph.cos() as f32);
        lower[index] = Complex32::new(tap * c, tap * s);
        upper[index] = lower[index].conj();
    }
    (lower, upper)
}

/// Direct-form complex FIR with a sample history.
struct Fir {
    taps: Vec<Complex32>,
    /// Newest sample at `pos`, then older ones going backwards (ring).
    hist: Vec<Complex32>,
    pos: usize,
}

impl Fir {
    fn new(taps: Vec<Complex32>) -> Self {
        let n = taps.len();
        Self {
            taps,
            hist: vec![Complex32::ZERO; n],
            pos: 0,
        }
    }

    fn push(&mut self, x: Complex32) -> Complex32 {
        let n = self.taps.len();
        self.pos = if self.pos == 0 { n - 1 } else { self.pos - 1 };
        self.hist[self.pos] = x;
        // hist[pos + m] (wrapping) is x[n - m].
        let mut acc = Complex32::ZERO;
        let (head, tail) = self.hist.split_at(self.pos);
        for (t, h) in self.taps.iter().zip(tail.iter().chain(head)) {
            acc += t * h;
        }
        acc
    }
}

/// AGC -> FLL conditioner for one digital channel, plus an input power probe.
pub struct Conditioner {
    p: ConditionerParams,
    sps: f32,
    rolloff: f32,
    filter_size: usize,
    /// `rms_cf` running mean of |x|^2.
    agc_avg: f64,
    probe: f64,
    // control_loop state (all f32 like GR)
    phase: f32,
    freq: f32,
    alpha: f32,
    beta: f32,
    max_freq: f32,
    upper: Fir,
    lower: Fir,
    resampler: Option<ArbResampler>,
    squelch: Option<Squelch>,
    events: Vec<SquelchEvent>,
    scratch: Vec<Complex32>,
}

impl Conditioner {
    pub fn new(sps: u32, excess_bw: f32, params: ConditionerParams) -> Self {
        let sps = sps as f32;
        let filter_size = 2 * sps as usize + 1;
        let (lower, upper) = design_taps(sps, excess_bw, filter_size);
        let mut c = Self {
            p: params,
            sps,
            rolloff: excess_bw,
            filter_size,
            agc_avg: 0.0,
            probe: 0.0,
            phase: 0.0,
            freq: 0.0,
            alpha: 0.0,
            beta: 0.0,
            max_freq: freq_limit(sps),
            upper: Fir::new(upper),
            lower: Fir::new(lower),
            resampler: None,
            squelch: None,
            events: Vec::new(),
            scratch: Vec::new(),
        };
        c.set_gains();
        c
    }

    /// Resample by `rate` (< 1) first, with `xlat_channelizer`'s arbitrary
    /// resampler.
    #[must_use]
    pub fn with_resampler(mut self, rate: f32) -> Self {
        self.resampler = (rate < 1.0).then(|| ArbResampler::new(rate, &xlat_arb_taps(rate), 32));
        self
    }

    /// Power squelch between the resampler and the AGC.
    #[must_use]
    pub fn with_squelch(mut self, mode: SquelchMode, threshold_db: f64) -> Self {
        self.squelch = Some(Squelch::new(mode, threshold_db));
        self
    }

    pub fn has_squelch(&self) -> bool {
        self.squelch.is_some()
    }

    pub fn set_squelch_db(&mut self, db: f64) {
        if let Some(sq) = &mut self.squelch {
            sq.threshold = 10f64.powf(db / 10.0);
        }
    }

    pub fn set_squelch_mode(&mut self, mode: SquelchMode) {
        if let Some(sq) = &mut self.squelch {
            sq.mode = mode;
        }
    }

    /// True while the squelch is open (or there is none).
    pub fn squelch_open(&self) -> bool {
        self.squelch.as_ref().is_none_or(Squelch::unmuted)
    }

    /// Squelch transitions since the last call.
    pub fn take_events(&mut self) -> std::vec::Drain<'_, SquelchEvent> {
        self.events.drain(..)
    }

    fn set_gains(&mut self) {
        // fll_band_edge_cc is constructed with 2*pi/sps/250 on GR < 3.10.13.
        let bw = (TAU / f64::from(self.sps) / 250.0 * self.p.fll_bw_scale) as f32;
        let damping = (2.0f32).sqrt() / 2.0;
        let (d, b) = (f64::from(damping), f64::from(bw));
        let denom = 1.0 + 2.0 * d * b + b * b;
        self.alpha = ((4.0 * d * b) / denom) as f32;
        self.beta = ((4.0 * b * b) / denom) as f32;
    }

    /// Clear all adaptive state (AGC, FLL, probe), as for a newly assigned
    /// channel. The filters' history is cleared too.
    pub fn reset(&mut self) {
        self.agc_avg = 0.0;
        self.probe = 0.0;
        self.phase = 0.0;
        self.freq = 0.0;
        self.upper = Fir::new(self.upper.taps.clone());
        self.lower = Fir::new(self.lower.taps.clone());
    }

    /// Change samples per symbol (P25 phase 1 <-> 2). The FLL state carries on.
    pub fn set_samples_per_symbol(&mut self, sps: u32) {
        let sps = sps as f32;
        if (sps - self.sps).abs() < f32::EPSILON {
            return;
        }
        self.sps = sps;
        self.filter_size = 2 * sps as usize + 1;
        let (lower, upper) = design_taps(sps, self.rolloff, self.filter_size);
        self.upper = Fir::new(upper);
        self.lower = Fir::new(lower);
        self.max_freq = freq_limit(sps);
        self.freq = self.freq.clamp(-self.max_freq, self.max_freq);
        self.set_gains();
    }

    /// FLL frequency estimate in rad/sample.
    pub const fn frequency(&self) -> f32 {
        self.freq
    }

    /// FLL frequency error in Hz at `rate`, truncated like
    /// `xlat_channelizer::get_freq_error`.
    pub fn freq_error_hz(&self, rate: f64) -> i32 {
        ((f64::from(self.freq) / TAU) * rate) as i32
    }

    /// Smoothed input power |x|^2 (linear).
    pub const fn power(&self) -> f64 {
        self.probe
    }

    /// Smoothed input power in dB, or `None` before any signal.
    pub fn power_db(&self) -> Option<f64> {
        (self.probe > 0.0).then(|| 10.0 * self.probe.log10())
    }

    fn probe(&mut self, input: &[Complex32]) {
        let pa = self.p.probe_alpha;
        for s in input {
            let mag2 = f64::from(s.re) * f64::from(s.re) + f64::from(s.im) * f64::from(s.im);
            self.probe = pa * mag2 + (1.0 - pa) * self.probe;
        }
    }

    /// Condition `input` into `out`, returning the samples written. Without a
    /// resampler or a gating squelch that is `input.len()`; otherwise fewer.
    ///
    /// # Panics
    /// If `out` is shorter than `input`.
    pub fn process_into(&mut self, input: &[Complex32], out: &mut [Complex32]) -> usize {
        assert!(out.len() >= input.len(), "output shorter than input");
        if self.resampler.is_none() && self.squelch.is_none() {
            out[..input.len()].copy_from_slice(input);
            self.process(&mut out[..input.len()]);
            return input.len();
        }
        self.probe(input);
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        match &mut self.resampler {
            Some(r) => r.process(input, &mut buf),
            None => buf.extend_from_slice(input),
        }
        let n = match &mut self.squelch {
            Some(sq) => sq.process(&mut buf, &mut self.events),
            None => buf.len(),
        };
        let n = n.min(out.len());
        out[..n].copy_from_slice(&buf[..n]);
        self.scratch = buf;
        self.agc_fll(&mut out[..n]);
        n
    }

    /// Condition `buf` in place (no resampler or squelch).
    pub fn process(&mut self, buf: &mut [Complex32]) {
        self.probe(buf);
        self.agc_fll(buf);
    }

    fn agc_fll(&mut self, buf: &mut [Complex32]) {
        let two_pi = (2.0 * PI) as f32;
        let inv_ref = (1.0 / self.p.agc_reference) as f32;
        let aa = self.p.agc_alpha;
        for s in buf {
            let mag2 = f64::from(s.re) * f64::from(s.re) + f64::from(s.im) * f64::from(s.im);

            // rms_agc: x / (rms / ref + 1e-18)
            self.agc_avg = (1.0 - aa) * self.agc_avg + aa * mag2;
            let g = (self.agc_avg.sqrt() as f32) * inv_ref + 1e-18;
            let x = Complex32::new(s.re / g, s.im / g);

            // fll_band_edge_cc
            let (sn, cs) = self.phase.sin_cos();
            let y = x * Complex32::new(cs, sn);
            let out_upper = self.lower.push(y);
            let out_lower = self.upper.push(y);
            let error = out_lower.norm_sqr() - out_upper.norm_sqr();
            self.freq += self.beta * error;
            self.phase += self.freq + self.alpha * error;
            while self.phase > two_pi {
                self.phase -= two_pi;
            }
            while self.phase < -two_pi {
                self.phase += two_pi;
            }
            self.freq = self.freq.clamp(-self.max_freq, self.max_freq);
            *s = y;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f64, rate: f64, n: usize, amp: f32) -> Vec<Complex32> {
        (0..n)
            .map(|i| Complex32::from_polar(amp, (TAU * f * i as f64 / rate) as f32))
            .collect()
    }

    #[test]
    fn agc_settles_to_reference_level() {
        let mut c = Conditioner::new(5, 0.2, ConditionerParams::default());
        let mut x = tone(0.0, 24_000.0, 2_000, 0.03);
        c.process(&mut x);
        let rms = (x[1_000..].iter().map(|v| v.norm_sqr()).sum::<f32>() / 1_000.0).sqrt();
        assert!((rms - 0.85).abs() < 0.02, "rms {rms}");
    }

    #[test]
    fn fll_pulls_a_carrier_offset_toward_zero() {
        // A pure offset carrier has no band-edge asymmetry for the FLL to
        // lock on, so use a modulated one: random QPSK at 4.8 kbaud, 5 sps.
        let mut s = 12345u64;
        let mut next = move || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            (s >> 33) & 3
        };
        let mut sym = Vec::new();
        for _ in 0..8_000 {
            let q = next();
            let ph = PI / 4.0 + q as f64 * PI / 2.0;
            let v = Complex32::new(ph.cos() as f32, ph.sin() as f32);
            sym.extend(std::iter::repeat_n(v, 5));
        }
        // smooth with a short moving average, then offset by 250 Hz
        let mut x: Vec<Complex32> = (0..sym.len())
            .map(|i| {
                let lo = i.saturating_sub(2);
                let hi = (i + 3).min(sym.len());
                let a: Complex32 = sym[lo..hi].iter().sum::<Complex32>() / (hi - lo) as f32;
                a * Complex32::from_polar(1.0, (TAU * 250.0 * i as f64 / 24_000.0) as f32)
            })
            .collect();
        let mut c = Conditioner::new(5, 0.2, ConditionerParams::default());
        c.process(&mut x);
        let e = c.freq_error_hz(24_000.0);
        assert!((-300..=-150).contains(&e), "FLL freq {e} Hz");
    }

    #[test]
    fn smartnet_channel_resamples_and_tracks_an_offset() {
        // Smartnet: 3600 baud FSK2, 5 sps (18 kHz) out of a 24 kHz channel.
        let (fin, fout) = (24_000.0, 18_000.0);
        let mut s = 777u64;
        let mut dev = 0.0f64;
        let mut ph = 0.0f64;
        let x: Vec<Complex32> = (0..240_000)
            .map(|i| {
                if i % 20 == 0 {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                    dev = if (s >> 40) & 1 == 0 { 700.0 } else { -700.0 };
                }
                ph += TAU * (dev + 250.0) / fin;
                Complex32::from_polar(0.05, ph as f32)
            })
            .collect();
        #[allow(clippy::cast_possible_truncation)]
        let mut c = Conditioner::new(5, 0.35, ConditionerParams::default())
            .with_resampler((fout / fin) as f32);
        let mut out = vec![Complex32::ZERO; x.len()];
        let mut n = 0;
        for chunk in x.chunks(1_000) {
            n += c.process_into(chunk, &mut out[n..]);
        }
        assert!((n as i64 - 180_000).abs() < 40, "{n}");
        // reported against the hard-coded 24 kHz like xlat_channelizer:
        // 250 Hz at 18 kHz reads as 333 Hz
        let e = c.freq_error_hz(24_000.0);
        assert!((-450..=-200).contains(&e), "FLL freq {e} Hz");
        let rms = (out[n - 10_000..n].iter().map(|v| v.norm_sqr()).sum::<f32>() / 10_000.0).sqrt();
        assert!((rms - 0.85).abs() < 0.05, "rms {rms}");
    }

    #[test]
    fn probe_tracks_power_in_db() {
        let mut c = Conditioner::new(5, 0.2, ConditionerParams::default());
        let mut x = tone(100.0, 24_000.0, 50_000, 0.1);
        c.process(&mut x);
        let db = c.power_db().unwrap();
        assert!((db - -20.0).abs() < 0.3, "{db}");
    }
}
