//! P25's vocoders, IMBE (phase 1) and AMBE+2 (phase 2): a frame's voice
//! parameters to 20 ms of 8 kHz audio.
//!
//! A port of op25's software decoder (`software_imbe_decoder`, after
//! TIA-102.BABA), which synthesises both, and of mbelib's AMBE+2
//! dequantiser, with op25's rules for repeating and muting damaged frames.
//! The arithmetic is theirs operation for operation, in their precision
//! (built without fused multiply-adds, which C compilers may otherwise
//! contract). Where op25 departs from the standards, the standards were put
//! to a blind vote (2026-10-20, two listeners, `examples/vocoder_ab.rs`,
//! `trunk-discord ab`) and are followed where they won: phase 1 reads u7 as
//! sent (BABA §7.1), but keeps op25's repeat, mute and smoothing rules;
//! phase 2 predicts past silence frames from the last voice frame, knows a
//! tone by u0 alone and plays it at its log level (BABA-1 §4, §7).
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
    /// op25's rules on the error counts of a hard decoding of the frame:
    /// its erasure thresholds, its running error rate.
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
