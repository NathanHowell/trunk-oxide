//! The radio: sources streaming into shared channelizers, and the channels
//! the call state machine opens on them.
//!
//! Every DSP runs on a source's own thread (see [`source`]); everything the
//! monitor loop needs comes out as a [`RadioEvent`] or through the
//! [`AudioSink`], and everything it decides goes in through [`Radio`]. A
//! channel's [`Kind`] (P25 voice, analog FM, P25 control) is chosen once,
//! when the configuration is read, as a [`Recipe`].

pub mod alias;
pub mod analog;
pub mod audible;
pub mod carrier;
pub mod correction;
pub mod fm;
pub mod kind;
pub mod p25;
pub mod p25_calls;
pub mod p25_parse;
pub mod p25_voice;
pub mod pace;
pub mod radio;
pub mod recording;
pub mod registry;
pub mod shape;
pub mod snapshot;
pub mod source;
pub mod tap;
pub mod types;

pub use analog::{AnalogFm, AnalogParams};
pub use kind::{Call, Kind, OpenError, Opener, Recipe};
pub use p25::P25Control;
pub use p25_voice::{Keying, P25Voice};
pub use pace::Pacer;
pub use radio::{ChannelReport, Radio, RadioConfig, SourceSetup, SystemSetup};
pub use recording::Accounting;
pub use registry::Entry;
pub use shape::{AudioShape, ShapeMeter};
pub use trunk_channelizer::Config as ChannelizerConfig;
pub use trunk_channelizer::passband;
pub use trunk_fm::denoise::Level as NoiseSuppression;
pub use trunk_fm::signalling;
pub use trunk_fm::{Alert, Heard, MdcOp, Signal};
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
