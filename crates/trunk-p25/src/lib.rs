//! P25 receivers: control channels, phase 1 and phase 2 voice.
//!
//! Replaces op25's control-channel chain (Gardner clock, differential
//! decoder, Costas loop, slicer, framer and TSBK decoder) with one that keeps
//! soft symbols all the way to the error-correcting decoders:
//!
//! - [`demod`]: conditioned 24 kS/s IQ to soft symbols. Gardner timing
//!   recovery, decision-feedback differential detection, phase-bias tracking.
//! - [`framer`]: frame sync, NID decode (hard BCH until the NAC is learned,
//!   then maximum-likelihood over the valid DUIDs for that NAC), status-symbol
//!   removal.
//! - [`trellis`]: the rate-1/2 trellis code with a soft-decision Viterbi
//!   decoder, in place of op25's greedy hard-decision walk.
//! - [`control`]: frames to the same messages op25 emits (TSBK, MBT, TDU,
//!   TDULC, timeout).
//! - [`trunking`]: what those messages say (typed OSPs) and the channel
//!   plans, shared across a system's control channels, that place grants.
//!
//! Voice channels:
//! - [`voice`]: phase 1 (HDU, LDU1/2, terminators, IMBE) and its
//!   regenerator, which re-sends the channel corrected for op25's frame
//!   assembler.
//! - [`tdma`]: phase 2 (ISCH, DUID, AMBE+2, FACCH/SACCH, encryption sync)
//!   and its regenerator.
//! - [`assemble`]: calls from either: the frames to vocode, who is talking,
//!   encryption, talker aliases.
//! - [`vocoder`]: IMBE and AMBE+2 frames to audio.
//!
//! [`testgen`] builds modulated test signals for the unit tests and benches.

pub mod assemble;
pub mod bch;
pub mod control;
pub mod crc;
pub mod demod;
pub mod fec;
pub mod framer;
pub mod gf64;
pub mod symbols;
pub mod tdma;
pub mod testgen;
pub mod trellis;
pub mod trunking;
pub mod types;
pub mod vocoder;
pub mod voice;

pub use control::{ControlChannelDecoder, DecoderConfig, DecoderStats, Message, Received};
pub use demod::{DemodConfig, Demodulator, Modulation};
pub use framer::{Duid, Frame, Framer};
pub use types::{Nac, Recovery, Tsbk};
