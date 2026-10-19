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

mod ambe;
mod imbe;
mod synth;
mod tables;

#[cfg(test)]
mod tests;

pub use ambe::AmbeVocoder;
pub use imbe::ImbeVocoder;
pub use synth::{Pcm, SAMPLES_PER_FRAME};
