//! Noise suppression on the gated voice: WebRTC's suppressor (the pure-Rust
//! `sonora-ns` port), a speech-probability-weighted Wiener filter that
//! tracks the noise through the gaps between words.
//!
//! Chosen by ear on 2026-10-19 over a spectral gate, `cathar`'s spectral
//! subtraction and an MMSE-LSA estimator, on a weak Atherton PD call: at
//! 12 dB it took the hiss down ~11 dB with the voice intact, where the
//! others either left more hiss or warbled ("musical noise").
//!
//! The suppressor works on 10 ms frames and delays its output by
//! [`DELAY`] samples, so a transmission's tail must be flushed through it
//! when the transmission ends, or its last milliseconds would come out at
//! the start of the next one. Its noise estimate carries over from one
//! transmission to the next on the same channel.

use sonora_ns::config::{NS_FRAME_SIZE, NsConfig, OVERLAP_SIZE, SuppressionLevel};
use sonora_ns::noise_suppressor::NoiseSuppressor;

/// Samples the suppressor delays its output by.
pub const DELAY: usize = OVERLAP_SIZE;

/// The suppressor works on int16-scaled floats.
const SCALE: f32 = 32_768.0;

/// How hard to suppress (dB of noise reduction aimed at).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Db6,
    Db12,
    Db18,
    Db21,
}

impl Level {
    /// The level for `db`, if it is one the suppressor offers.
    #[must_use]
    pub const fn from_db(db: u8) -> Option<Self> {
        match db {
            6 => Some(Self::Db6),
            12 => Some(Self::Db12),
            18 => Some(Self::Db18),
            21 => Some(Self::Db21),
            _ => None,
        }
    }

    const fn target(self) -> SuppressionLevel {
        match self {
            Self::Db6 => SuppressionLevel::K6dB,
            Self::Db12 => SuppressionLevel::K12dB,
            Self::Db18 => SuppressionLevel::K18dB,
            Self::Db21 => SuppressionLevel::K21dB,
        }
    }
}

/// A channel's noise suppressor.
pub struct Denoiser {
    ns: NoiseSuppressor,
    /// Samples waiting for a whole frame.
    pending: Vec<f32>,
    /// Audio has gone in since the last flush.
    dirty: bool,
}

impl std::fmt::Debug for Denoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser")
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl Denoiser {
    #[must_use]
    pub fn new(level: Level) -> Self {
        Self {
            ns: NoiseSuppressor::new(NsConfig {
                target_level: level.target(),
            }),
            pending: Vec::with_capacity(NS_FRAME_SIZE),
            dirty: false,
        }
    }

    fn frame(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let mut f = [0f32; NS_FRAME_SIZE];
        for (d, s) in f.iter_mut().zip(input) {
            *d = s * SCALE;
        }
        self.ns.analyze(&f);
        self.ns.process(&mut f);
        out.extend(f.iter().map(|v| v / SCALE));
    }

    /// Suppress `audio`, appending what is ready to `out` ([`DELAY`] samples
    /// late; a partial frame waits for the next call or [`Self::flush`]).
    pub fn process(&mut self, audio: &[f32], out: &mut Vec<f32>) {
        if audio.is_empty() {
            return;
        }
        self.dirty = true;
        let mut rest = audio;
        if !self.pending.is_empty() {
            let take = (NS_FRAME_SIZE - self.pending.len()).min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() < NS_FRAME_SIZE {
                return;
            }
            let frame = std::mem::take(&mut self.pending);
            self.frame(&frame, out);
            self.pending = frame;
            self.pending.clear();
        }
        let (whole, tail) = rest.as_chunks::<NS_FRAME_SIZE>();
        for f in whole {
            self.frame(f, out);
        }
        self.pending.extend_from_slice(tail);
    }

    /// A transmission ended: push out the partial frame and the delay, so
    /// everything that went in has come out (padded with silence). A no-op
    /// when nothing went in since the last flush.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        if !self.dirty {
            return;
        }
        let owed = self.pending.len() + DELAY;
        let mut drained = Vec::with_capacity(2 * NS_FRAME_SIZE);
        let frame = std::mem::take(&mut self.pending);
        self.frame(&frame, &mut drained);
        while drained.len() < owed {
            self.frame(&[], &mut drained);
        }
        out.extend_from_slice(&drained[..owed]);
        self.pending = frame;
        self.pending.clear();
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise in [-amp, amp].
    fn noise(n: usize, amp: f32, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 11) as f32 / (1u64 << 53) as f32).mul_add(2.0, -1.0) * amp
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn everything_that_goes_in_comes_out_delayed() {
        let mut d = Denoiser::new(Level::Db12);
        let mut out = Vec::new();
        // ragged pieces, as the gate hands them over
        for n in [1600, 80, 1600, 33, 7] {
            d.process(&noise(n, 0.1, n as u64), &mut out);
        }
        d.flush(&mut out);
        assert_eq!(out.len(), 1600 + 80 + 1600 + 33 + 7 + DELAY);
        let before = out.len();
        d.flush(&mut out);
        assert_eq!(out.len(), before, "a second flush adds nothing");
    }

    #[test]
    fn steady_hiss_goes_down_and_bursts_in_it_stay() {
        // A steady tone would be suppressed too (stationary is what it
        // calls noise); speech comes in syllables, so: 1 s of hiss alone,
        // then 1 kHz bursts, 200 ms on and 200 ms off, through it.
        const SR: usize = 16_000;
        let n = 4 * SR;
        let hiss = noise(n, 0.05, 9);
        let on = |i: usize| i >= SR && (i / (SR / 5)).is_multiple_of(2);
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let tone = if on(i) {
                    0.2 * (std::f32::consts::TAU * 1_000.0 * i as f32 / SR as f32).sin()
                } else {
                    0.0
                };
                hiss[i] + tone
            })
            .collect();
        let mut d = Denoiser::new(Level::Db12);
        let mut out = Vec::new();
        d.process(&input, &mut out);
        d.flush(&mut out);
        let out = &out[DELAY..];
        let hiss_in = rms(&input[8_000..16_000]);
        let hiss_out = rms(&out[8_000..16_000]);
        assert!(hiss_out < hiss_in / 2.0, "hiss {hiss_in} -> {hiss_out}");
        // the middle of a late burst (3.2-3.4 s is on)
        let burst = 3 * SR + SR / 5 + SR / 40..3 * SR + 2 * SR / 5 - SR / 40;
        let tone_in = rms(&input[burst.clone()]);
        let tone_out = rms(&out[burst]);
        assert!(tone_out > tone_in * 0.7, "burst {tone_in} -> {tone_out}");
    }

    #[test]
    fn the_levels_the_suppressor_offers() {
        assert_eq!(Level::from_db(12), Some(Level::Db12));
        assert_eq!(Level::from_db(10), None);
    }
}
