//! Shared by the vocoder policy examples: frames from the speech corpus or
//! at random, a noisy channel, and the log-spectral distance that scores
//! what a vocoder played against what was sent.
#![allow(
    dead_code,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]
use trunk_p25::symbols::{LEVELS, bit_llrs, slice};
use trunk_p25::tdma::ambe::{AmbeCodeword, AmbeParams};
use trunk_p25::testgen::{Rng, random_imbe, tdma::random_ambe};
use trunk_p25::vocoder::Spectrum;
use trunk_p25::voice::ImbeParams;

/// How a decoded frame compares with the one sent.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    Right,
    /// Only unprotected bits differ (IMBE u7; AMBE+2 u2, u3).
    Unprotected,
    Protected,
}

pub fn imbe_truth(decoded: &ImbeParams, sent: &ImbeParams) -> Truth {
    if decoded == sent {
        Truth::Right
    } else if decoded.0[..7] == sent.0[..7] {
        Truth::Unprotected
    } else {
        Truth::Protected
    }
}

pub fn ambe_truth(decoded: AmbeParams, sent: AmbeParams) -> Truth {
    if decoded == sent {
        Truth::Right
    } else if decoded.0[..2] == sent.0[..2] {
        Truth::Unprotected
    } else {
        Truth::Protected
    }
}

/// A frame's bits, sent order, as noisy symbols' LLRs and as the dibits a
/// hard slicer makes of them.
pub fn channel<const N: usize>(
    bit: impl Fn(usize) -> bool,
    sigma: f64,
    rng: &mut Rng,
) -> ([f32; N], Vec<u8>) {
    let mut llr = [0f32; N];
    let mut dibits = Vec::with_capacity(N / 2);
    for d in 0..N / 2 {
        let dibit = u8::from(bit(2 * d)) * 2 + u8::from(bit(2 * d + 1));
        let s = sigma.mul_add(rng.gauss(), f64::from(LEVELS[usize::from(dibit)])) as f32;
        llr[2 * d..2 * d + 2].copy_from_slice(&bit_llrs(s));
        dibits.push(slice(s));
    }
    (llr, dibits)
}

pub const fn ambe_b0(p: AmbeParams) -> u16 {
    ((p.0[0] >> 5) & 0x78) + ((p.0[3] >> 9) & 0x7)
}

pub const fn imbe_b0(p: &ImbeParams) -> u32 {
    let u = p.to_op25();
    ((u[0] >> 4) & 0xfc) | ((u[7] >> 1) & 0x3)
}

/// Random AMBE+2 voice parameters (b0 < 120: a pitch, not a tone, silence
/// or erasure).
pub fn ambe_voice(rng: &mut Rng) -> AmbeParams {
    loop {
        let p = random_ambe(rng);
        if ambe_b0(p) < 120 {
            return p;
        }
    }
}

/// Random IMBE parameters with a pitch (b̂0 ≤ 207).
pub fn imbe_voice(rng: &mut Rng) -> ImbeParams {
    loop {
        let p = random_imbe(rng);
        if imbe_b0(&p) <= 207 {
            return p;
        }
    }
}

/// The corpus `VOCODER_CORPUS=<dir>` names (`imbe.txt`, `ambe.txt`, as the
/// trunk-recorder-sys `voice_capture` test writes them with
/// `VOICE_CORPUS`): each call's clear frames as op25 framed them.
pub struct Corpus {
    pub imbe: Vec<Vec<ImbeParams>>,
    pub ambe: Vec<Vec<AmbeParams>>,
}

impl Corpus {
    /// `None` without `VOCODER_CORPUS`. Phase 2's voice frames only.
    pub fn load() -> Option<Self> {
        Self::load_frames(false)
    }

    /// With `all_kinds`, phase 2's silence and tone frames too.
    pub fn load_frames(all_kinds: bool) -> Option<Self> {
        let dir = std::env::var("VOCODER_CORPUS").ok()?;
        let calls = |file: &str| -> Vec<Vec<Vec<u32>>> {
            std::fs::read_to_string(format!("{dir}/{file}"))
                .unwrap_or_default()
                .split("\n\n")
                .map(|call| {
                    call.lines()
                        .map(|l| l.split(' ').filter_map(|x| x.parse().ok()).collect())
                        .collect()
                })
                .collect()
        };
        let imbe = calls("imbe.txt")
            .into_iter()
            .map(|call| {
                call.iter()
                    .filter_map(|v| {
                        let u: [u32; 8] = v.as_slice().try_into().ok()?;
                        let p = ImbeParams::from_op25(u);
                        (imbe_b0(&p) <= 207).then_some(p)
                    })
                    .collect()
            })
            .collect();
        let ambe = calls("ambe.txt")
            .into_iter()
            .map(|call| {
                call.iter()
                    .filter_map(|v| {
                        let d: [u8; 36] = v
                            .iter()
                            .map(|&x| x as u8)
                            .collect::<Vec<u8>>()
                            .try_into()
                            .ok()?;
                        let (p, errors) = AmbeCodeword::from_dibits(&d).decode_op25();
                        let b0 = ambe_b0(p);
                        (errors == [0, 0] && (b0 < 120 || (all_kinds && b0 >= 124))).then_some(p)
                    })
                    .collect()
            })
            .collect();
        Some(Self { imbe, ambe })
    }

    /// Random frames in place of a corpus.
    pub fn random(rng: &mut Rng, sequences: usize, frames: usize) -> Self {
        Self {
            imbe: (0..sequences)
                .map(|_| (0..frames).map(|_| imbe_voice(rng)).collect())
                .collect(),
            ambe: (0..sequences)
                .map(|_| (0..frames).map(|_| ambe_voice(rng)).collect())
                .collect(),
        }
    }
}

const POINTS: usize = 32;
/// What a muted frame scores, everywhere: well below any voice.
pub const MUTE_FLOOR_DB: f64 = 40.0;

/// RMS difference (dB) between the two models' log envelopes, each
/// sampled at [`POINTS`] frequencies by its nearest harmonic; a muted frame
/// is [`MUTE_FLOOR_DB`] below the clean frame everywhere.
pub fn log_spectral_distance(played: Option<&Spectrum>, clean: &Spectrum) -> f64 {
    let level = |s: &Spectrum, f: f64| {
        let l = ((f / f64::from(s.w0)).round() as usize).clamp(1, s.amplitudes.len());
        20.0 * f64::from(s.amplitudes[l - 1].max(1e-6)).log10()
    };
    let mut sum = 0f64;
    for k in 0..POINTS {
        let f = std::f64::consts::PI * (k as f64 + 0.5) / POINTS as f64;
        let d = played.map_or(MUTE_FLOOR_DB, |p| {
            (level(p, f) - level(clean, f)).clamp(-MUTE_FLOOR_DB, MUTE_FLOOR_DB)
        });
        sum += d * d;
    }
    (sum / POINTS as f64).sqrt()
}
