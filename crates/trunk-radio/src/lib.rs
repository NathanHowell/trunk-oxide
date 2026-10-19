//! The radio: sources streaming into shared channelizers, decoders on their
//! slots, and the recorders and call handles the call state machine drives.
//!
//! Every DSP runs on a source's own thread (see [`source`]); everything the
//! monitor loop needs comes out as a [`RadioEvent`] or through the
//! [`AudioSink`], and everything it decides goes in through [`Radio`].

pub mod alias;
pub mod fm;
pub mod p25;
pub mod p25_calls;
pub mod p25_parse;
pub mod p25_voice;
pub mod radio;
pub mod recorder;
pub mod source;
pub mod types;

pub use radio::{Radio, RadioConfig, SourceSetup, SystemSetup};
pub use types::*;

pub use trunk_channelizer::Counters as ChannelizerCounters;
pub use trunk_channelizer::{ConditionerParams, ConditioningSettings, ResetPolicy};

/// One source's shared-channelizer counters (see [`source::channelizer_stats`]).
#[derive(Clone, Copy, Debug)]
pub struct ChannelizerStats {
    pub source: i32,
    pub counters: ChannelizerCounters,
    /// Real-time duration of one block; a block slower than this is an overrun.
    pub block_budget_ns: u64,
}

pub use source::channelizer_stats;
