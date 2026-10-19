//! Analog FM voice channel in Rust: what trunk-recorder's `analog_recorder`
//! did in GNU Radio, on the conditioned output of a channelizer slot.
//!
//! The audio chain ([`FmDemod`]) is the C++ one, block for block, so its
//! output can be compared with the recorder's sample for sample:
//! quadrature demodulation, 75 µs de-emphasis, decimation to 16 kHz, then a
//! 300 Hz high-pass and 3250 Hz low-pass for the recording. The decimated,
//! unfiltered audio is kept too: the CTCSS tones and the above-voice noise
//! the detectors look at are what the band-pass removes.

// The arithmetic mirrors GNU Radio's float/double mix and order of
// operations; fused multiply-adds or wider intermediates would break the
// sample-for-sample match the tests check.
#![allow(
    clippy::suboptimal_flops,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]

mod atan;
pub mod demod;
pub mod denoise;
pub mod design;
pub mod detect;
pub mod fir;
pub mod gate;
pub mod io;

use num_complex::Complex32;

pub use atan::fast_atan2;
pub use demod::{Deemphasis, QuadratureDemod};
pub use detect::{Detector, Measurement, Tone};
pub use fir::Fir;
pub use gate::{CloseReason, Event, Gate};

/// Peak deviation the demodulator is scaled to (`analog_recorder`'s
/// `d_max_dev`).
pub const MAX_DEV_HZ: f64 = 5_000.0;
/// Audio sample rate of the recording.
pub const AUDIO_RATE: f64 = 16_000.0;
/// US de-emphasis time constant.
pub const TAU_US: f64 = 75e-6;

/// One block's audio, at [`AUDIO_RATE`].
#[derive(Clone, Debug, Default)]
pub struct Audio {
    /// Decimated de-emphasised audio, before the band-pass: tones and the
    /// noise above the voice band are still in it.
    pub raw: Vec<f32>,
    /// The recording: `raw` through the 300–3250 Hz band-pass.
    pub voice: Vec<f32>,
}

impl Audio {
    pub fn clear(&mut self) {
        self.raw.clear();
        self.voice.clear();
    }
}

/// Which low-pass runs ahead of the decimation to [`AUDIO_RATE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decimation {
    /// The recorder's: 25 Kaiser taps with the cutoff at 0.45 cycles per
    /// *input* sample (43 kHz at 96 kS/s), so it rejects nothing above the
    /// output Nyquist and the FM noise above 8 kHz folds into the audio.
    /// For comparing with the C++ chain.
    Recorder,
    /// A real anti-alias filter: cutoff 7 kHz, 1.5 kHz transition, so the
    /// output holds only what was below its Nyquist.
    AntiAliased,
}

/// The audio chain from conditioned IQ to 16 kHz audio.
#[derive(Clone, Debug)]
pub struct FmDemod {
    demod: QuadratureDemod,
    deemph: Deemphasis,
    decim: Fir,
    high: Fir,
    low: Fir,
    scratch: Vec<f32>,
    scratch2: Vec<f32>,
}

impl FmDemod {
    /// For a slot producing `rate` Hz (an integer multiple of
    /// [`AUDIO_RATE`]), with de-emphasis `tau` seconds.
    ///
    /// # Panics
    /// If `rate` is not a multiple of [`AUDIO_RATE`].
    #[must_use]
    #[allow(clippy::cast_sign_loss)]
    pub fn new(rate: f64, tau: f64, decimation: Decimation) -> Self {
        let decim = rate / AUDIO_RATE;
        assert!(
            decim.fract() == 0.0 && decim >= 1.0,
            "channel rate {rate} is not a multiple of {AUDIO_RATE}"
        );
        let taps = match decimation {
            Decimation::Recorder => recorder_decimation_taps(),
            Decimation::AntiAliased => {
                design::low_pass(1.0, rate, 7_000.0, 1_500.0, design::Window::Kaiser(5.0))
            }
        };
        Self {
            demod: QuadratureDemod::new(rate, MAX_DEV_HZ),
            deemph: Deemphasis::new(rate, tau),
            decim: Fir::new(&taps, decim as usize),
            high: Fir::new(
                &design::high_pass(1.0, AUDIO_RATE, 300.0, 50.0, design::Window::Hann),
                1,
            ),
            low: Fir::new(
                &design::low_pass(1.0, AUDIO_RATE, 3_250.0, 500.0, design::Window::Hann),
                1,
            ),
            scratch: Vec::new(),
            scratch2: Vec::new(),
        }
    }

    /// Demodulate `iq`, appending the audio it yields to `audio`.
    pub fn process(&mut self, iq: &[Complex32], audio: &mut Audio) {
        self.scratch.clear();
        self.demod.process(iq, &mut self.scratch);
        self.deemph.process(&mut self.scratch);
        let raw_start = audio.raw.len();
        self.decim.process(&self.scratch, &mut audio.raw);
        self.scratch2.clear();
        self.high
            .process(&audio.raw[raw_start..], &mut self.scratch2);
        self.low.process(&self.scratch2, &mut audio.voice);
    }

    /// Forget all signal history, as for a newly tuned channel.
    pub fn reset(&mut self) {
        self.demod.reset();
        self.deemph.reset();
        self.decim.reset();
        self.high.reset();
        self.low.reset();
    }
}

/// `analog_recorder::design_filter(1, decim)`, whatever `decim`: see
/// [`Decimation::Recorder`].
fn recorder_decimation_taps() -> Vec<f32> {
    let trans_width = 0.5 - 0.4;
    let mid = 0.5 - trans_width / 2.0;
    design::low_pass(1.0, 1.0, mid, trans_width, design::Window::Kaiser(5.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn a_voice_band_tone_comes_through_at_16_khz() {
        let mut fm = FmDemod::new(96_000.0, TAU_US, Decimation::AntiAliased);
        // FM-modulate a 1 kHz tone at 3 kHz deviation.
        let mut phase = 0.0f32;
        let iq: Vec<Complex32> = (0..96_000)
            .map(|n| {
                let m = (TAU * 1_000.0 * n as f32 / 96_000.0).sin();
                phase += TAU * 3_000.0 * m / 96_000.0;
                Complex32::from_polar(1.0, phase)
            })
            .collect();
        let mut audio = Audio::default();
        fm.process(&iq, &mut audio);
        assert_eq!(audio.raw.len(), 16_000);
        assert_eq!(audio.voice.len(), 16_000);
        let peak = audio.voice[8_000..]
            .iter()
            .fold(0.0f32, |m, x| m.max(x.abs()));
        // 3 kHz of 5 kHz deviation is 0.6, de-emphasised at 1 kHz by ~0.9.
        assert!((0.4..0.7).contains(&peak), "peak {peak}");
    }

    #[test]
    fn decimation_taps_are_the_recorders() {
        assert_eq!(recorder_decimation_taps().len(), 25);
    }
}
