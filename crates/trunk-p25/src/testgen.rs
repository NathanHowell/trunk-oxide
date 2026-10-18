//! Synthetic P25 signals for tests and benches: frame encoders, CQPSK and
//! C4FM modulators, and channel impairments (noise, frequency offset, timing).

use std::f64::consts::PI;

use num_complex::Complex32;

use crate::bch;
use crate::crc;
use crate::framer::{Duid, nid_word};
use crate::symbols::{LEVELS, STATUS_PERIOD, sync_dibits};
use crate::trellis;
use crate::types::Nac;

/// xorshift64* with Box–Muller Gaussians: deterministic, dependency free.
pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub const fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn below(&mut self, n: usize) -> usize {
        ((self.uniform() * n as f64) as usize).min(n - 1)
    }

    pub fn gauss(&mut self) -> f64 {
        let u = self.uniform().max(1e-300);
        let v = self.uniform();
        (-2.0 * u.ln()).sqrt() * (2.0 * PI * v).cos()
    }
}

/// A TSBK payload (10 bytes) with its CRC appended.
pub fn tsbk_block(payload: &[u8; 10], last: bool) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[..10].copy_from_slice(payload);
    if last {
        b[0] |= 0x80;
    } else {
        b[0] &= 0x7f;
    }
    let c = crc::crc16(&b[..10]);
    b[10..].copy_from_slice(&c.to_be_bytes());
    b
}

/// Insert status dibits (every 36th, counting from the first sync dibit) into
/// a frame's data dibits, padding with idle status to `total` dibits.
pub fn with_status(data: &[u8], total: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(total);
    let mut it = data.iter();
    while out.len() < total {
        if (out.len() + 1) % STATUS_PERIOD == 0 {
            out.push(1);
        } else {
            out.push(it.next().copied().unwrap_or(0));
        }
    }
    out
}

fn push_u64_dibits(out: &mut Vec<u8>, word: u64, ndibits: usize) {
    for i in (0..ndibits).rev() {
        #[allow(clippy::cast_possible_truncation)]
        out.push(((word >> (2 * i)) & 3) as u8);
    }
}

/// Sync + NID dibits (data only, no status).
pub fn header(nac: Nac, duid: Duid) -> Vec<u8> {
    let mut d = sync_dibits().to_vec();
    push_u64_dibits(&mut d, nid_word(nac, duid), 32);
    d
}

/// A TSDU of 360 dibits carrying `tsbks` (1–3); the last is flagged.
pub fn tsdu(nac: Nac, tsbks: &[[u8; 10]]) -> Vec<u8> {
    let mut d = header(nac, Duid::Tsdu);
    for (i, t) in tsbks.iter().enumerate() {
        d.extend_from_slice(&trellis::encode(&tsbk_block(t, i + 1 == tsbks.len())));
    }
    with_status(&d, 360)
}

/// A frame with only a header (TDU: 72 dibits).
pub fn tdu(nac: Nac) -> Vec<u8> {
    with_status(&header(nac, Duid::Tdu), 72)
}

/// Random TSBK payloads.
pub fn random_tsbks(rng: &mut Rng, n: usize) -> Vec<[u8; 10]> {
    (0..n)
        .map(|_| {
            let mut p = [0u8; 10];
            for x in &mut p {
                #[allow(clippy::cast_possible_truncation)]
                {
                    *x = rng.next_u64() as u8;
                }
            }
            p[0] &= 0x3f;
            p
        })
        .collect()
}

/// Raised-cosine (`root == false`) or root-raised-cosine pulse, `span`
/// symbols long, unit energy-ish (peak 1 for RC).
#[allow(clippy::cast_precision_loss)]
pub fn pulse(sps: usize, alpha: f64, span: usize, root: bool) -> Vec<f64> {
    let n = sps * span + 1;
    let mid = (n / 2) as f64;
    let taps: Vec<f64> = (0..n)
        .map(|i| {
            let t = (i as f64 - mid) / sps as f64;
            if root { rrc(t, alpha) } else { rc(t, alpha) }
        })
        .collect();
    let norm: f64 = if root {
        taps.iter().map(|x| x * x).sum::<f64>().sqrt() / (sps as f64).sqrt()
    } else {
        1.0
    };
    taps.iter().map(|x| x / norm).collect()
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

fn rc(t: f64, a: f64) -> f64 {
    let d = (2.0 * a * t).mul_add(-(2.0 * a * t), 1.0);
    if d.abs() < 1e-9 {
        PI / 4.0 * sinc(1.0 / (2.0 * a))
    } else {
        sinc(t) * (PI * a * t).cos() / d
    }
}

fn rrc(t: f64, a: f64) -> f64 {
    if t.abs() < 1e-12 {
        return 1.0 - a + 4.0 * a / PI;
    }
    if (t.abs() - 1.0 / (4.0 * a)).abs() < 1e-9 {
        return a / 2f64.sqrt()
            * (1.0 - 2.0 / PI).mul_add(
                (PI / (4.0 * a)).cos(),
                (1.0 + 2.0 / PI) * (PI / (4.0 * a)).sin(),
            );
    }
    let num = (4.0 * a * t).mul_add((PI * t * (1.0 + a)).cos(), (PI * t * (1.0 - a)).sin());
    let den = PI * t * (4.0 * a * t).mul_add(-(4.0 * a * t), 1.0);
    num / den
}

fn convolve(x: &[Complex32], taps: &[f64]) -> Vec<Complex32> {
    let half = taps.len() / 2;
    (0..x.len())
        .map(|n| {
            let mut acc = Complex32::new(0.0, 0.0);
            for (k, &t) in taps.iter().enumerate() {
                if let Some(i) = (n + half).checked_sub(k)
                    && i < x.len()
                {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        acc += x[i] * t as f32;
                    }
                }
            }
            acc
        })
        .collect()
}

/// CQPSK (π/4-DQPSK) at `sps` samples per symbol: each dibit is a phase step
/// of its level × 45°, shaped with a root-raised-cosine (α = 0.2).
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn modulate_cqpsk(dibits: &[u8], sps: usize) -> Vec<Complex32> {
    let mut x = vec![Complex32::new(0.0, 0.0); dibits.len() * sps];
    let mut phase = 0.0f64;
    for (k, &d) in dibits.iter().enumerate() {
        phase += f64::from(LEVELS[usize::from(d)]) * PI / 4.0;
        x[k * sps] = Complex32::from_polar(1.0, phase as f32) * sps as f32;
    }
    let shaped = convolve(&x, &pulse(sps, 0.2, 8, true));
    normalize(shaped)
}

/// C4FM: deviation level × 600 Hz, raised-cosine shaped frequency pulse.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn modulate_c4fm(dibits: &[u8], sps: usize) -> Vec<Complex32> {
    let fs = crate::symbols::SYMBOL_RATE * sps as f64;
    let mut f = vec![Complex32::new(0.0, 0.0); dibits.len() * sps];
    for (k, &d) in dibits.iter().enumerate() {
        f[k * sps] = Complex32::new(LEVELS[usize::from(d)] * 600.0, 0.0);
    }
    let freq = convolve(&f, &c4fm_pulse(sps, 0.2, 16));
    let mut phase = 0.0f64;
    freq.iter()
        .map(|v| {
            phase += 2.0 * PI * f64::from(v.re) / fs;
            Complex32::from_polar(1.0, phase as f32)
        })
        .collect()
}

/// The C4FM frequency pulse (TIA-102.BAAA): raised cosine cascaded with the
/// πfT/sin(πfT) shaping filter, so that integrating the frequency over one
/// symbol period recovers the deviation without inter-symbol interference.
/// Built by frequency sampling.
#[allow(clippy::cast_precision_loss)]
fn c4fm_pulse(sps: usize, alpha: f64, span: usize) -> Vec<f64> {
    let n = sps * span + 1;
    let mid = (n / 2) as f64;
    let len = 4096usize;
    // Spectrum in cycles per symbol, sampled finely; real and even, so the
    // impulse response is a cosine sum.
    let spectrum = |f: f64| -> f64 {
        let f = f.abs();
        let (lo, hi) = ((1.0 - alpha) / 2.0, f64::midpoint(1.0, alpha));
        let rc = if f <= lo {
            1.0
        } else if f <= hi {
            f64::midpoint(1.0, (PI / alpha * (f - lo)).cos())
        } else {
            0.0
        };
        let x = PI * f;
        if x < 1e-9 { rc } else { rc * x / x.sin() }
    };
    let df = sps as f64 / len as f64;
    (0..n)
        .map(|i| {
            let t = (i as f64 - mid) / sps as f64;
            let mut acc = spectrum(0.0);
            for k in 1..len / 2 {
                let f = k as f64 * df;
                acc = f64::mul_add(2.0 * spectrum(f), (2.0 * PI * f * t).cos(), acc);
            }
            acc * df
        })
        .collect()
}

fn normalize(x: Vec<Complex32>) -> Vec<Complex32> {
    #[allow(clippy::cast_precision_loss)]
    let rms = (x.iter().map(Complex32::norm_sqr).sum::<f32>() / x.len().max(1) as f32).sqrt();
    x.into_iter().map(|v| v / rms.max(1e-12)).collect()
}

/// Channel impairments applied to a unit-power signal.
pub struct Channel {
    /// Symbol energy to noise density, dB.
    pub es_n0_db: f64,
    pub freq_offset_hz: f64,
    /// Fractional-sample delay at the start, in samples.
    pub delay: f64,
    /// Sample clock error, parts per million.
    pub clock_ppm: f64,
    pub seed: u64,
}

impl Channel {
    pub const fn clean() -> Self {
        Self {
            es_n0_db: 200.0,
            freq_offset_hz: 0.0,
            delay: 0.0,
            clock_ppm: 0.0,
            seed: 1,
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::many_single_char_names
    )]
    pub fn apply(&self, x: &[Complex32], sps: usize) -> Vec<Complex32> {
        let fs = crate::symbols::SYMBOL_RATE * sps as f64;
        let mut rng = Rng::new(self.seed);
        // Noise per complex sample: Es/N0 with Es = sps × unit sample power.
        let n0 = sps as f64 / 10f64.powf(self.es_n0_db / 10.0);
        let sigma = (n0 / 2.0).sqrt();
        let step = self.clock_ppm.mul_add(1e-6, 1.0);
        let mut out = Vec::with_capacity(x.len());
        let count = ((x.len() as f64 - 2.0 - self.delay) / step).ceil().max(0.0) as usize;
        for n in 0..count {
            let t = (n as f64).mul_add(step, self.delay);
            let i = t.floor() as usize;
            let mu = (t - t.floor()) as f32;
            let y = [x[i.saturating_sub(1)], x[i], x[i + 1], x[i + 2]];
            let v = crate::demod::cubic(&y, mu);
            let rot = 2.0 * PI * self.freq_offset_hz * n as f64 / fs;
            let noise = Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32);
            out.push(v * Complex32::from_polar(1.0, rot as f32) + noise);
        }
        out
    }
}

/// Info word of a NID, for tests that poke at BCH directly.
pub fn nid_info(nac: Nac, duid: Duid) -> u16 {
    bch::info(nid_word(nac, duid) >> 1)
}
