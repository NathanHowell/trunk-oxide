//! P25's vocoders, IMBE (phase 1) and AMBE+2 (phase 2): a frame's voice
//! parameters to 20 ms of 8 kHz audio, as op25 makes it.
//!
//! A port of op25's software decoder (`software_imbe_decoder`, after
//! TIA-102.BABA), which synthesises both, and of mbelib's AMBE+2
//! dequantiser, with op25's rules for repeating and muting damaged frames.
//! The arithmetic is theirs operation for operation, in their precision, so
//! the audio is op25's sample for sample (built without fused
//! multiply-adds, which C compilers may otherwise contract).
//!
//! One of each is kept per recorder across calls, as op25 keeps its own:
//! each carries pitch, phase, noise and error rate from frame to frame.
//!
//! Which frames play, repeat or fall silent is the [`Policy`]: op25's
//! rules on what op25 would have decoded, or the decoders' own soft
//! decisions. Only op25's is used: measured on tapped speech under injected
//! noise (`examples/vocoder_noise.rs`) and listened to blind
//! (`examples/vocoder_ab.rs`, `trunk-discord ab`), repeating a doubtful
//! frame sounded worse than playing it as decoded, every time on phase 1
//! and as often as not on phase 2.

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
/// Only [`Policy::Op25`] is used by the recorders (see the module doc).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    /// op25's: its error counts on its hard decoding of the frame, its
    /// erasure thresholds, its running error rate. Sample for sample what
    /// op25 plays.
    #[default]
    Op25,
    /// The decoders': a frame plays as decoded here when its weakest
    /// vector decision had a margin of [`PLAY_MARGIN`]; else the last frame
    /// repeats, up to three times, then silence.
    ///
    /// No running error rate, no gate on the unprotected bits (a repeat is
    /// spectrally further from the frame sent than one with a few
    /// unprotected bits wrong). Kept for the measurements, not used by the
    /// recorders.
    Soft,
    /// The standards' (TIA-102.BABA, BABA-1) where op25 departs from them.
    ///
    /// Phase 1: u7 as sent, not shifted a bit low (BABA §7.1, Fig. 22:
    /// b̂0's low bits, b̂2's LSB and the last scanned bits), the bits the
    /// decoder here corrected (op25, given a regenerated frame, counts none),
    /// a repeat only when both Eq. 97 and Eq. 98 hold, the local energy
    /// `S_E` and amplitude threshold `τ_M` carried from frame to frame
    /// (Eqs. 111, 115) and Eq. 112's 277.26.
    ///
    /// Phase 2: a silence frame is not the next frame's prediction (BABA-1
    /// Eqs. 26, 43), a tone is told by u0 alone (§7), and its amplitude is
    /// logarithmic (§7.2). Otherwise op25's, and its error counts.
    ///
    /// Being compared with op25's by ear (trunk-oxide-4hj, trunk-oxide-13y).
    Standard,
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
