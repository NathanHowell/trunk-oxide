//! P25's vocoders, IMBE (phase 1) and AMBE+2 (phase 2): a frame's voice
//! parameters to 20 ms of 8 kHz audio.
//!
//! A port of op25's software decoder (`software_imbe_decoder`, after
//! TIA-102.BABA), which synthesises both, and of mbelib's AMBE+2
//! dequantiser, with op25's rules for repeating and muting damaged frames.
//! The arithmetic is theirs operation for operation, in their precision
//! (built without fused multiply-adds, which C compilers may otherwise
//! contract). Where op25 departs from the standards, the standards'
//! way was put to two blind votes (2026-10-20, `examples/vocoder_ab.rs`,
//! `trunk-discord ab`), heard as close or not at all, and is followed:
//! phase 1 reads u7 as sent (BABA §7.1) and repeats, mutes and smooths by
//! BABA §7.7–7.8 and §9 on the bits the decoder here corrected; phase 2
//! predicts past silence frames from the last voice frame, knows a tone by
//! u0 alone and plays it at its log level (BABA-1 §4, §7). Since then the
//! rest of the standards' error handling has been taken without a vote,
//! measured by `examples/vocoder_noise.rs`: phase 2's error counts are the
//! bits corrected and drive its smoothing (BABA-1 §5.5), any frame may be
//! repeated (§5.6, §7.3), a mute keeps the last model (§5.7, BABA §7.8),
//! phase 1 repeats without limit (BABA §7.7), and a muted frame is heard as
//! [`ComfortNoise`].
//!
//! One of each is kept per recorder across calls, as op25 keeps its own:
//! each carries pitch, phase, noise and error rate from frame to frame.
//!
//! Which frames play, repeat or fall silent is the [`Policy`]: the error
//! counts of a hard decoding, or the decoders' own soft decisions. Only
//! the first is used: measured on tapped speech under injected noise
//! (`examples/vocoder_noise.rs`) and listened to blind, repeating a
//! doubtful frame sounded worse than playing it as decoded, every time on
//! phase 1 and as often as not on phase 2.

mod ambe;
mod imbe;
mod synth;
mod tables;

#[cfg(test)]
mod tests;

pub use ambe::AmbeVocoder;
pub use imbe::ImbeVocoder;
pub use synth::{Pcm, SAMPLES_PER_FRAME};

/// Which frames play, repeat or fall silent.
///
/// Only [`Policy::Hard`] is used by the recorders (see the module doc).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    /// The error counts of a hard decoding of the frame, by the standards'
    /// thresholds and running error rate.
    #[default]
    Hard,
    /// The decoders': a frame plays as decoded here when its weakest
    /// vector decision had a margin of [`PLAY_MARGIN`]; else the last frame
    /// repeats, up to three times, then silence.
    ///
    /// No running error rate, no gate on the unprotected bits (a repeat is
    /// spectrally further from the frame sent than one with a few
    /// unprotected bits wrong). Kept for the measurements, not used by the
    /// recorders.
    Soft,
}

/// `Policy::Soft`: the least margin (total |LLR|) of a frame's weakest
/// vector decision for it to play.
///
/// At 2, one in ten of the frames op25 would have played wrongly at
/// σ = 0.6 still plays; at 4 or 6, right frames are repeated wholesale.
pub const PLAY_MARGIN: f32 = 2.0;

/// What a muted frame is heard as: noise uniform on [−5, 5].
///
/// The standards' recommended mute (BABA §7.8, BABA-1 §5.7), a little
/// "comfort noise" in place of dead air. Seeded alike every time, so a
/// replay's audio is the same run to run.
#[derive(Clone, Debug)]
pub struct ComfortNoise(u32);

impl Default for ComfortNoise {
    fn default() -> Self {
        Self::new()
    }
}

impl ComfortNoise {
    pub const fn new() -> Self {
        Self(1)
    }

    /// One frame of it.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    pub fn frame(&mut self) -> Pcm {
        std::array::from_fn(|_| {
            // Numerical Recipes' LCG; its top bits are the well mixed ones.
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (((u64::from(self.0) * 11) >> 32) as i16) - 5
        })
    }
}

/// The harmonic model a frame was played from: fundamental (radians a
/// sample) and the spectral amplitudes of its L harmonics as decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct Spectrum {
    pub w0: f32,
    pub amplitudes: Vec<f32>,
}

/// What a vocoder did with its last frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Decision {
    #[default]
    Played,
    /// The frame before it again.
    Repeated,
    Muted,
}
