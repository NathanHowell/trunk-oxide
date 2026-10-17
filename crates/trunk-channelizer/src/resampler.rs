//! Polyphase arbitrary resampler, ported from GNU Radio's
//! `pfb_arb_resampler_ccf` (32 arms with linear interpolation between arms
//! through a derivative filter bank), with the prototype filter
//! `xlat_channelizer` designed for it.

use std::f64::consts::PI;

use num_complex::Complex32;

/// `firdes::low_pass_2` with a 4-term (92 dB) Blackman-Harris window.
fn low_pass_2_bh(gain: f64, fs: f64, cutoff: f64, transition: f64, atten_db: f64) -> Vec<f32> {
    let mut ntaps = (atten_db * fs / (22.0 * transition)) as usize;
    if ntaps.is_multiple_of(2) {
        ntaps += 1;
    }
    // window::coswindow (f32 like GR)
    let m = (ntaps - 1) as f32;
    let w: Vec<f32> = (0..ntaps)
        .map(|n| {
            let n = n as f32;
            let two_pi = (2.0 * PI) as f32;
            0.35875 - 0.48829 * (two_pi * n / m).cos() + 0.14128 * (2.0 * two_pi * n / m).cos()
                - 0.01168 * (3.0 * two_pi * n / m).cos()
        })
        .collect();
    let half = (ntaps - 1) / 2;
    let fw_t0 = 2.0 * PI * cutoff / fs;
    let mut taps = vec![0.0f32; ntaps];
    for (i, t) in taps.iter_mut().enumerate() {
        let n = i as f64 - half as f64;
        let v = if i == half {
            fw_t0 / PI
        } else {
            (n * fw_t0).sin() / (n * PI)
        };
        *t = (v * f64::from(w[i])) as f32;
    }
    let mut fmax = f64::from(taps[half]);
    for t in &taps[half + 1..] {
        fmax += 2.0 * f64::from(*t);
    }
    let g = gain / fmax;
    for t in &mut taps {
        *t = (f64::from(*t) * g) as f32;
    }
    taps
}

/// The prototype filter `xlat_channelizer` gives its arbitrary resampler for
/// `rate < 1`: 32 arms, pass 80 % of the output half-band, 30 dB.
pub fn xlat_arb_taps(rate: f32) -> Vec<f32> {
    let halfband = 0.5 * f64::from(rate);
    let bw = 0.8 * halfband;
    let tb = 0.4 * halfband;
    low_pass_2_bh(32.0, 32.0, bw, tb, 30.0)
}

pub struct ArbResampler {
    nfilt: usize,
    tpf: usize,
    /// `arms[i][k]` multiplies the k-th newest... see `dot`.
    arms: Vec<Vec<f32>>,
    darms: Vec<Vec<f32>>,
    dec_rate: usize,
    flt_rate: f32,
    acc: f32,
    last_filter: usize,
    /// Input not yet fully consumed, preceded by `tpf - 1` samples of history.
    buf: Vec<Complex32>,
    /// Read position in `buf` (GR's `i_in`).
    pos: usize,
}

impl ArbResampler {
    pub fn new(rate: f32, taps: &[f32], nfilt: usize) -> Self {
        let mut dtaps: Vec<f32> = taps.windows(2).map(|w| w[1] - w[0]).collect();
        dtaps.push(0.0);
        let tpf = taps.len().div_ceil(nfilt);
        let split = |t: &[f32]| -> Vec<Vec<f32>> {
            (0..nfilt)
                .map(|i| {
                    (0..tpf)
                        .map(|j| t.get(i + j * nfilt).copied().unwrap_or(0.0))
                        .collect()
                })
                .collect()
        };
        let ratio = nfilt as f32 / rate;
        let dec_rate = ratio.floor() as usize;
        Self {
            nfilt,
            tpf,
            arms: split(taps),
            darms: split(&dtaps),
            dec_rate,
            flt_rate: ratio - dec_rate as f32,
            acc: 0.0,
            last_filter: (taps.len() / 2) % nfilt,
            buf: vec![Complex32::ZERO; tpf - 1],
            pos: 0,
        }
    }

    /// `fir_filter_ccf::filter`: taps reversed against `x[0..tpf]`.
    fn dot(taps: &[f32], x: &[Complex32]) -> Complex32 {
        let mut acc = Complex32::ZERO;
        for (t, v) in taps.iter().rev().zip(x) {
            acc += v * *t;
        }
        acc
    }

    /// Resample `input`, appending to `out`.
    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        self.buf.extend_from_slice(input);
        let mut j = self.last_filter;
        while self.pos + self.tpf <= self.buf.len() {
            while j < self.nfilt {
                let x = &self.buf[self.pos..self.pos + self.tpf];
                let o0 = Self::dot(&self.arms[j], x);
                let o1 = Self::dot(&self.darms[j], x);
                out.push(o0 + o1 * self.acc);
                self.acc += self.flt_rate;
                j += self.dec_rate + self.acc.floor() as usize;
                self.acc %= 1.0;
            }
            self.pos += j / self.nfilt;
            j %= self.nfilt;
        }
        self.last_filter = j;
        // Keep the history the next call needs.
        let keep_from = self.pos.min(self.buf.len());
        self.buf.drain(..keep_from);
        self.pos -= keep_from;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    #[test]
    fn resamples_a_tone_at_the_requested_rate_and_unity_gain() {
        let (fin, fout) = (101_333.333_f64, 96_000.0_f64);
        let rate = (fout / fin) as f32;
        let mut r = ArbResampler::new(rate, &xlat_arb_taps(rate), 32);
        let n = 101_333;
        let x: Vec<Complex32> = (0..n)
            .map(|i| Complex32::from_polar(1.0, (TAU * 2_000.0 * i as f64 / fin) as f32))
            .collect();
        let mut out = Vec::new();
        for c in x.chunks(777) {
            r.process(c, &mut out);
        }
        assert!((out.len() as i64 - 96_000).abs() < 40, "{}", out.len());
        let mid = &out[10_000..90_000];
        let mean = mid.iter().map(|c| c.norm()).sum::<f32>() / mid.len() as f32;
        assert!((mean - 1.0).abs() < 0.02, "gain {mean}");
        // frequency is preserved: phase advance per output sample
        let ph = (mid[1] * mid[0].conj()).arg() as f64;
        assert!((ph - TAU * 2_000.0 / fout).abs() < 2e-3, "{ph}");
    }
}
