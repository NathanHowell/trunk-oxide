//! op25's software IMBE synthesiser (`software_imbe_decoder`, after
//! TIA-102.BABA): a frame's harmonic model to 160 samples, voiced harmonics
//! as sinusoids continued from the frame before and unvoiced bands as
//! shaped noise, overlapped and added across frames.
//!
//! Both phases synthesise with it; phase 2 fills a frame's model from its
//! AMBE+2 parameters where phase 1 decodes it from IMBE's.
//!
//! The arithmetic is op25's operation for operation, in its precision
//! (`float`, or `double` where C promotes), so the samples are op25's;
//! op25's own build must not fuse multiply-adds either.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops,
    clippy::manual_midpoint,
    clippy::needless_range_loop,
    clippy::many_single_char_names,
    clippy::unreadable_literal,
    clippy::suspicious_operation_groupings,
    reason = "a port of op25: its equations' names, indexing and constants, its int-to-float \
              conversions, and no fused multiply-adds or other rewritten arithmetic"
)]

use std::f64::consts::PI;
use std::sync::LazyLock;

use super::tables::{OVERLAP_GAIN, SYNTHESIS_WINDOW};

/// Samples in one 20 ms frame (8 kHz).
pub const SAMPLES_PER_FRAME: usize = 160;

/// One frame's audio.
pub type Pcm = [i16; SAMPLES_PER_FRAME];

/// Harmonics a model can have, indexed from 1 as op25 does (L ≤ 56).
pub(super) const HARMONICS: usize = 57;

/// A frame's model. There are two, the current frame's and the previous
/// one's, and they trade places each frame (op25's `[ell][New]` and
/// `[ell][Old]`): entries past a frame's own L keep what earlier frames
/// left there, and op25 reads some of them, so they are kept too.
#[derive(Clone, Copy)]
pub(super) struct Model {
    pub voiced: [bool; HARMONICS],
    /// Spectral amplitudes as decoded.
    pub mu: [f32; HARMONICS],
    /// Enhanced and smoothed, as synthesised.
    pub m: [f32; HARMONICS],
    /// log2 of `mu`, one past L for the next frame's prediction (phase 1).
    pub log2_mu: [f32; HARMONICS + 1],
    /// Each harmonic's phase at the frame's end.
    phi: [f32; HARMONICS],
}

impl Model {
    const ZERO: Self = Self {
        voiced: [false; HARMONICS],
        mu: [0.0; HARMONICS],
        m: [0.0; HARMONICS],
        log2_mu: [0.0; HARMONICS + 1],
        phi: [0.0; HARMONICS],
    };
}

/// Noise samples drawn per frame: 160 new, 51 kept for the window's overlap.
const NOISE: usize = 211;
/// The time-domain block an unvoiced frame is synthesised in.
const BLOCK: usize = 256;

pub(super) struct Synthesizer {
    models: [Model; 2],
    /// Which of `models` is the current frame's.
    new: usize,
    /// The current frame's harmonics and fundamental (radians per sample),
    /// and the previous frame's.
    pub l: usize,
    pub old_l: usize,
    pub w0: f32,
    pub old_w0: f32,
    /// The fundamental's phase at the end of the last frame.
    psi1: f32,
    /// The last unvoiced block, for the overlap.
    old_uw: [f32; BLOCK],
    /// The white noise generator's window.
    noise: [u32; NOISE],
}

/// Bins of the 256-point spectrum the unvoiced bands can reach (0..=128).
const BINS: usize = 129;

/// One bin's (cos, sin) for each noise sample.
type Twiddles = [(f32, f32); NOISE];

/// The unvoiced DFT's cos and sin, at bin em and noise sample n, of
/// −2π·em·(n − 105)/256 as op25 rounds it: they depend on nothing else, so
/// they are computed once rather than per frame.
static DFT: LazyLock<Box<[Twiddles]>> = LazyLock::new(|| {
    (0..BINS)
        .map(|em| {
            std::array::from_fn(|n| {
                let arg = (-0.0078125 * PI * em as f64 * (n as f64 - 105.0)) as f32;
                let (s, c) = arg.sin_cos();
                (c, s)
            })
        })
        .collect()
});

/// op25's noise generator.
const fn next_noise(u: u32) -> u32 {
    (u * 171 + 11213) % 53125
}

impl Synthesizer {
    pub fn new() -> Self {
        let mut noise = [0u32; NOISE];
        noise[0] = 3147;
        for i in 1..NOISE {
            noise[i] = next_noise(noise[i - 1]);
        }
        Self {
            models: [Model::ZERO; 2],
            new: 0,
            l: 9,
            old_l: 0,
            w0: 0.0,
            old_w0: 0.0,
            psi1: 0.0,
            old_uw: [0.0; BLOCK],
            noise,
        }
    }

    pub const fn current(&mut self) -> &mut Model {
        &mut self.models[self.new]
    }

    pub const fn previous(&mut self) -> &mut Model {
        &mut self.models[1 - self.new]
    }

    /// The current frame's model and the previous one's.
    const fn both(&mut self) -> (&mut Model, &Model) {
        let [a, b] = &mut self.models;
        if self.new == 0 { (a, b) } else { (b, a) }
    }

    /// The next frame: this one becomes the previous.
    pub const fn advance(&mut self) {
        self.old_l = self.l;
        self.old_w0 = self.w0;
        self.new = 1 - self.new;
    }

    /// Enhances the current frame's amplitudes (`mu` to `m`); returns the
    /// frame's energy as the smoothing takes it (op25's SE, which starts
    /// from zero each frame).
    pub fn enhance(&mut self) -> f32 {
        let (l, w0) = (self.l, self.w0);
        let cur = self.current();
        let (mut rm0, mut rm1) = (0f32, 0f32);
        for ell in 1..=l {
            let t = cur.mu[ell] * cur.mu[ell];
            rm0 += t;
            rm1 += t * (w0 * ell as f32).cos();
        }
        let k1 = (0.96 * PI / f64::from(w0 * rm0 * (rm0 * rm0 - rm1 * rm1))) as f32;
        let k2 = rm0 * rm0 + rm1 * rm1;
        let k3 = 2.0 * rm0 * rm1;
        let mut sum = 0f32;
        for ell in 1..=l {
            let w = cur.mu[ell].sqrt() * (k1 * (k2 - k3 * (w0 * ell as f32).cos())).powf(0.25);
            cur.m[ell] = if 8 * ell <= l {
                cur.mu[ell]
            } else if w > 1.2 {
                (f64::from(cur.mu[ell]) * 1.2) as f32
            } else if w < 0.5 {
                (f64::from(cur.mu[ell]) * 0.5) as f32
            } else {
                cur.mu[ell] * w
            };
            sum += cur.m[ell] * cur.m[ell];
        }
        let gain = (rm0 / sum).sqrt();
        for ell in 1..=l {
            cur.m[ell] *= gain;
        }
        let se = (0.05 * f64::from(rm0)) as f32;
        if se < 10000.0 { 10000.0 } else { se }
    }

    /// Adaptive smoothing: with error rate `er` and
    /// `et` bits corrected, bands louder than a threshold become voiced and
    /// the frame's total amplitude is capped.
    pub fn smooth(&mut self, se: f32, er: f32, et: f32) {
        let (er64, l) = (f64::from(er), self.l);
        let vm: f32 = if er64 <= 0.005 && et <= 4.0 {
            1e38
        } else if er64 <= 0.0125 && et == 0.0 {
            (45.255 * f64::from(se.powf(0.375)) / (277.6 * er64).exp()) as f32
        } else {
            (1.414 * f64::from(se.powf(0.375))) as f32
        };
        let cur = self.current();
        let mut am = 0f32;
        for ell in 1..=l {
            if cur.m[ell] > vm {
                cur.voiced[ell] = true;
            }
            am += cur.m[ell];
        }
        let tm: f32 = if er64 <= 0.005 && et <= 6.0 {
            20480.0
        } else {
            6000.0 - 300.0 * et
        };
        if tm <= am {
            let ym = tm / am;
            for ell in 1..=l {
                cur.m[ell] *= ym;
            }
        }
    }

    /// The current frame's audio.
    pub fn synthesize(&mut self) -> Pcm {
        let unvoiced = self.unvoiced();
        let voiced = self.voiced();
        std::array::from_fn(|n| to_pcm(unvoiced[n] + voiced[n] * 4.0))
    }

    /// Bands `al..bl` of harmonic ell, in the 256-point spectrum.
    fn band_edge(&self, ell: usize) -> usize {
        let edge = 128.0 / PI * (ell as f64 + 0.5) * f64::from(self.w0);
        let edge = (edge as f32).ceil() as usize;
        debug_assert!(edge <= BINS, "band edge {edge}");
        edge
    }

    /// The unvoiced bands: white noise, windowed and transformed, kept in
    /// the unvoiced bands at each one's amplitude, and transformed back.
    fn unvoiced(&mut self) -> [f32; SAMPLES_PER_FRAME] {
        self.noise.copy_within(160.., 0);
        for n in NOISE - 160..NOISE {
            self.noise[n] = next_noise(self.noise[n - 1]);
        }
        let windowed: [f32; NOISE] =
            std::array::from_fn(|n| self.noise[n] as f32 * SYNTHESIS_WINDOW[n]);
        // Voiced bands and those above the last stay zero.
        let mut re = [0f32; BLOCK];
        let mut im = [0f32; BLOCK];
        let mut bl = self.band_edge(0);
        for ell in 1..=self.l {
            let al = bl;
            bl = self.band_edge(ell);
            let cur = &self.models[self.new];
            if cur.voiced[ell] {
                continue;
            }
            for em in al..bl {
                let (mut i, mut q) = (0f32, 0f32);
                for (&x, &(c, s)) in windowed.iter().zip(&DFT[em]) {
                    i += x * c;
                    q += x * s;
                }
                re[em] = i;
                im[em] = q;
            }
            let mut energy = 0f32;
            for em in al..bl {
                energy += re[em] * re[em] + im[em] * im[em];
            }
            let scale = (0.91652 * f64::from(cur.m[ell])
                / f64::from((energy / (bl - al) as f32).sqrt())) as f32;
            for em in al..bl {
                re[em] *= scale;
                im[em] *= scale;
            }
        }
        let uw = ifft(&mut re, &mut im);

        // Overlap and add with the last block over n = 56..=104.
        let mut out = [0f32; SAMPLES_PER_FRAME];
        out[..=55].copy_from_slice(&self.old_uw[128..=183]);
        for n in 56..=104 {
            out[n] = (SYNTHESIS_WINDOW[n + 105] * self.old_uw[n + 128]
                + SYNTHESIS_WINDOW[n - 55] * uw[n - 32])
                * OVERLAP_GAIN[n];
        }
        out[105..].copy_from_slice(&uw[73..128]);
        self.old_uw = uw;
        out
    }

    /// The voiced harmonics: each a sinusoid, its amplitude and phase
    /// interpolated from the previous frame where both frames voice it and
    /// the pitch held, else the previous frame's faded out under the
    /// current one's faded in.
    #[allow(clippy::similar_names)]
    fn voiced(&mut self) -> [f32; SAMPLES_PER_FRAME] {
        let (l, old_l, w0, old_w0) = (self.l, self.old_l, self.w0, self.old_w0);
        let max_l = l.max(old_l);
        self.psi1 = remainder(self.psi1 + (old_w0 + w0) * 80.0, (2.0 * PI) as f32);
        let psi1 = self.psi1;
        let (cur, old) = self.both();
        for ell in 1..=max_l {
            cur.phi[ell] = psi1 * ell as f32;
        }
        let ws = &SYNTHESIS_WINDOW;
        let mut sv = [0f32; SAMPLES_PER_FRAME];
        for ell in 1..=max_l {
            let el = ell as f32;
            let m_new = if ell > l { 0.0 } else { cur.m[ell] };
            let m_old = if ell > old_l { 0.0 } else { old.m[ell] };
            let fade_out = |sv: &mut [f32], n: usize| {
                let x = n as f32;
                sv[n] += ws[n + 105] * m_old * (old_w0 * x * el + old.phi[ell]).cos();
            };
            let fade_in = |sv: &mut [f32], n: usize| {
                let x = n as f32 - 160.0;
                sv[n] += ws[n - 55] * m_new * (w0 * x * el + cur.phi[ell]).cos();
            };
            match (cur.voiced[ell], old.voiced[ell]) {
                (true, true) if ell < 8 && f64::from((w0 - old_w0).abs()) < 0.1 * f64::from(w0) => {
                    let dpl = cur.phi[ell] - old.phi[ell] - (old_w0 + w0) * el * 80.0;
                    let wraps = ((f64::from(dpl) + PI) / (2.0 * PI)) as f32;
                    let dwl =
                        (0.00625 * (f64::from(dpl) - 2.0 * PI * f64::from(wraps.floor()))) as f32;
                    let tha = old_w0 * el + dwl;
                    let thb = (f64::from((w0 - old_w0) * el) * 0.003125) as f32;
                    let mb = (0.00625 * f64::from(m_new - m_old)) as f32;
                    for (n, s) in sv.iter_mut().enumerate() {
                        let x = n as f32;
                        *s += (m_old + x * mb) * (old.phi[ell] + (tha + thb * x) * x).cos();
                    }
                }
                (true, true) => {
                    for n in 0..=55 {
                        fade_out(&mut sv, n);
                    }
                    for n in 56..=105 {
                        fade_out(&mut sv, n);
                        fade_in(&mut sv, n);
                    }
                    for n in 106..SAMPLES_PER_FRAME {
                        fade_in(&mut sv, n);
                    }
                }
                (true, false) => {
                    for n in 56..SAMPLES_PER_FRAME {
                        fade_in(&mut sv, n);
                    }
                }
                (false, true) => {
                    for n in 0..=105 {
                        fade_out(&mut sv, n);
                    }
                }
                (false, false) => {}
            }
        }
        sv
    }
}

/// A sample, clipped to ±32767 as op25 clips it. One beyond i32 (only a
/// degenerate model makes one) is silent, as op25's conversion leaves it on
/// x86.
fn to_pcm(sample: f32) -> i16 {
    if sample.abs() < 2_147_483_648.0 {
        sample.clamp(-32767.0, 32767.0) as i16
    } else {
        0
    }
}

/// IEEE remainder (C's `remainderf`): x − ny for the integer n nearest x/y,
/// ties to even. Exact, so this matches any correct implementation.
#[allow(clippy::float_cmp, reason = "the tie test is exact")]
fn remainder(x: f32, y: f32) -> f32 {
    let (x, y) = (f64::from(x), f64::from(y));
    // x/y is a few turns at most, so n·y and x − n·y are exact in f64.
    let mut n = (x / y).round();
    let mut r = x - n * y;
    let twice = 2.0 * r.abs();
    if twice > y || (twice == y && n % 2.0 != 0.0) {
        let s = r.signum();
        n += s;
        r -= s * y;
    }
    debug_assert!(n.abs() < 1e6);
    r as f32
}

/// The 129-point spectrum `re`, `im` (bins 0..=128) to 256 real samples,
/// as op25 computes it (a 128-point complex FFT and an unpacking pass).
/// Uses bins 129..256 as scratch.
fn ifft(re: &mut [f32; BLOCK], im: &mut [f32; BLOCK]) -> [f32; BLOCK] {
    let mut ai = [0f32; 128];
    let mut aq = [0f32; 128];
    for i in 0..=63 {
        let (j, k, h) = (i + 64, i * 2, 128 - i * 2);
        ai[i] = re[k] + im[k];
        aq[i] = re[k + 1] + im[k + 1];
        ai[j] = re[h] - im[h];
        aq[j] = re[h - 1] - im[h - 1];
    }
    fft(&mut ai, &mut aq);
    for i in 1..=63 {
        let j = 128 - i;
        let (k, h) = (i + 128, j + 128);
        re[k] = (aq[i] + aq[j]) / 2.0;
        re[h] = re[k];
        im[k] = -(ai[i] - ai[j]) / 2.0;
        im[h] = -im[k];
        re[i] = (ai[i] + ai[j]) / 2.0;
        re[j] = re[i];
        im[i] = (aq[i] - aq[j]) / 2.0;
        im[j] = -im[i];
    }
    re[192] = aq[64];
    im[192] = 0.0;
    re[64] = ai[64];
    im[64] = 0.0;
    re[128] = aq[0];
    im[128] = 0.0;
    re[0] = ai[0];
    im[0] = 0.0;

    let mut td = [0f32; BLOCK];
    let (mut ui, mut uq) = (1f32, 0f32);
    let si = (PI / 128.0).cos() as f32;
    let sq = -(PI / 128.0).sin() as f32;
    for i in 0..128 {
        let j = i + 128;
        let ti = re[j] * ui - im[j] * uq;
        let tq = re[j] * uq + im[j] * ui;
        let (xi, xq) = (re[i], im[i]);
        td[i + 128] = (xi + ti) - (xq + tq);
        td[i] = (xi - ti) - (xq - tq);
        let t = ui;
        ui = t * si - uq * sq;
        uq = t * sq + uq * si;
    }
    td
}

/// op25's in-place 128-point radix-2 FFT.
fn fft(re: &mut [f32; 128], im: &mut [f32; 128]) {
    let mut j = 64;
    for i in 1..=126 {
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
        let mut k = 64;
        while k <= j {
            j -= k;
            k /= 2;
        }
        j += k;
    }
    let mut h = 1usize;
    while h < 128 {
        let (mut ui, mut uq) = (1f32, 0f32);
        let theta = (PI / h as f64) as f32;
        let (s, c) = theta.sin_cos();
        let (si, sq) = (c, -s);
        for j in 1..=h {
            let mut k = j - 1;
            while k <= 127 {
                let kh = k + h;
                let ti = re[kh] * ui - im[kh] * uq;
                let tq = re[kh] * uq + im[kh] * ui;
                let (xi, xq) = (re[k], im[k]);
                re[kh] = xi - ti;
                re[k] = xi + ti;
                im[kh] = xq - tq;
                im[k] = xq + tq;
                k += 2 * h;
            }
            let t = ui;
            ui = t * si - uq * sq;
            uq = t * sq + uq * si;
        }
        h *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remainder_is_ieee() {
        // Halfway cases go to the even multiple.
        for (x, want) in [
            (1.0, 1.0),
            (3.0, -1.0),
            (5.0, 1.0),
            (7.0, -1.0),
            (-5.0, -1.0),
            (4.5, 0.5),
        ] {
            assert_eq!(remainder(x, 2.0), want, "{x}");
        }
        let y = (2.0 * PI) as f32;
        assert_eq!(remainder(4.0, y), 4.0 - y);
        assert_eq!(remainder(-4.0, y), y - 4.0);
    }

    /// op25's FFT is a DFT.
    #[test]
    fn fft_is_a_dft() {
        let x: Vec<(f32, f32)> = (0..128)
            .map(|n| ((n as f32 * 0.37).sin(), (n as f32 * 0.11).cos()))
            .collect();
        let mut re: [f32; 128] = std::array::from_fn(|n| x[n].0);
        let mut im: [f32; 128] = std::array::from_fn(|n| x[n].1);
        fft(&mut re, &mut im);
        for k in 0..128 {
            let (mut a, mut b) = (0f64, 0f64);
            for (n, &(xr, xi)) in x.iter().enumerate() {
                let t = -2.0 * PI * (k * n) as f64 / 128.0;
                a += f64::from(xr) * t.cos() - f64::from(xi) * t.sin();
                b += f64::from(xr) * t.sin() + f64::from(xi) * t.cos();
            }
            assert!(
                (f64::from(re[k]) - a).abs() < 1e-3 && (f64::from(im[k]) - b).abs() < 1e-3,
                "bin {k}"
            );
        }
    }

    #[test]
    fn samples_clip_as_op25s() {
        assert_eq!(to_pcm(40000.0), 32767);
        assert_eq!(to_pcm(-40000.0), -32767);
        assert_eq!(to_pcm(-1.9), -1);
        assert_eq!(to_pcm(f32::NAN), 0);
        assert_eq!(to_pcm(3e9), 0);
    }
}
