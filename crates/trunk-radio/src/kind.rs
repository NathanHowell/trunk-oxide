//! Kinds of channel, and how one is opened.
//!
//! A [`Kind`] says everything that differs between channels: which
//! channelizer profile it needs, how its slot is conditioned, what decodes
//! it and what it records. [`crate::Radio::open`] is the one way any
//! channel (a trunked call's voice, a conventional channel, a control
//! channel) comes to be; nothing else in the radio knows one kind from
//! another. Which kind a call gets is decided once, when the configuration
//! is read, as a [`Recipe`] behind an [`Opener`].

use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use trunk_channelizer::{ChannelDecoder, ConditioningSpec, ProfileId, SharedChannelizer};

use crate::radio::{Radio, SystemSetup};
use crate::recording::{Accounting, Tape};
use crate::types::{CallId, Channel, ChannelNum, Decoded};

/// The channelizer as the radio's channels use it.
pub type Chz = SharedChannelizer<Decoded, Cmd>;

/// What the control side tells an open channel's decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// The unit the state machine says is talking (P25: for Harris aliases).
    Source(i64),
}

/// A call (or a control channel's standing claim) as the radio opens a
/// channel for it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Call {
    pub id: CallId,
    pub sys_num: i32,
    /// 0 for a control channel.
    pub talkgroup: i64,
    /// Where to tune (any clock correction applied) and, on a phase 2
    /// channel, the TDMA slot.
    pub channel: Channel,
}

/// How a channel's slot is set up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    pub conditioning: ConditioningSpec,
    /// The channel filter's cutoff and transition (Hz); `None` for the
    /// profile's own.
    pub filter: Option<(f64, f64)>,
    /// The power squelch threshold (dB), when the conditioning squelches.
    pub squelch_db: Option<f64>,
}

/// Everything a kind opens a channel from.
pub struct Opening<'a, K: Kind> {
    pub call: &'a Call,
    pub params: &'a K::Params,
    pub system: &'a SystemSetup,
    /// The system's phase 2 scrambling mask (empty until known).
    pub xor_mask: &'a [u8],
    pub channelizer: &'a trunk_channelizer::Config,
    pub num: ChannelNum,
    /// The source and channelizer slot, for the metrics.
    pub source: i32,
    pub port: usize,
    /// Where baseline taps go, when they are taken.
    pub tap_dir: Option<&'a str>,
}

/// A kind of channel.
pub trait Kind: Sized + 'static {
    /// What a channel of this kind is opened with beyond its call, fixed
    /// when the configuration is read.
    type Params: fmt::Debug + Send + Sync + 'static;
    type Decoder: ChannelDecoder<Output = Decoded, Command = Cmd> + 'static;
    /// For logs, metrics and the wire.
    const NAME: &'static str;
    const PROFILE: ProfileId;
    /// The rate of the audio its decoder records; `None` if it records none.
    const AUDIO_RATE: Option<f64>;

    fn tuning(at: &Opening<'_, Self>) -> Tuning;

    /// The decoder, recording onto `tape`.
    ///
    /// # Errors
    /// When the call cannot be decoded as asked.
    fn decoder(at: &Opening<'_, Self>, tape: Tape) -> Result<Self::Decoder, OpenError>;

    /// A recording made on a channel of this kind is over.
    fn finished(_recording: &Accounting) {}
}

/// Why a channel could not be opened.
#[derive(Clone, Debug, PartialEq)]
pub enum OpenError {
    /// The call already has a channel.
    AlreadyOpen(CallId),
    NoSystem(i32),
    /// No source covers the frequency with the profile the kind needs.
    NoSource {
        freq_hz: f64,
        kind: &'static str,
    },
    /// A phase 2 channel is descrambled with the system's mask, and the
    /// system's identity (hence the mask) is not known yet.
    NoScramblingMask,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyOpen(id) => write!(f, "call {id} already has a channel"),
            Self::NoSystem(n) => write!(f, "no system {n}"),
            Self::NoSource { freq_hz, kind } => {
                write!(f, "no source carries a {kind} channel at {freq_hz} Hz")
            }
            Self::NoScramblingMask => {
                write!(
                    f,
                    "phase 2 needs the system's scrambling mask, not yet known"
                )
            }
        }
    }
}

impl std::error::Error for OpenError {}

/// Opens a channel of a kind fixed when it was made (see [`Recipe`]).
pub trait Opener: fmt::Debug + Send + Sync {
    /// Open a channel for `call`.
    ///
    /// # Errors
    /// See [`Radio::open`].
    fn open(&self, radio: &Radio, call: &Call) -> Result<ChannelNum, OpenError>;
    fn kind(&self) -> &'static str;
    /// The rate of the audio it records, if it records any.
    fn audio_rate(&self) -> Option<f64>;
}

/// A kind and its parameters: how one kind of call is to be opened.
pub struct Recipe<K: Kind> {
    pub params: K::Params,
    kind: PhantomData<fn() -> K>,
}

impl<K: Kind> Recipe<K> {
    /// The recipe behind the interface the state machine carries.
    #[must_use]
    pub fn opener(params: K::Params) -> Arc<dyn Opener> {
        Arc::new(Self {
            params,
            kind: PhantomData,
        })
    }
}

impl<K: Kind> fmt::Debug for Recipe<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple(K::NAME).field(&self.params).finish()
    }
}

impl<K: Kind> Opener for Recipe<K> {
    fn open(&self, radio: &Radio, call: &Call) -> Result<ChannelNum, OpenError> {
        radio.open::<K>(call, &self.params)
    }

    fn kind(&self) -> &'static str {
        K::NAME
    }

    fn audio_rate(&self) -> Option<f64> {
        K::AUDIO_RATE
    }
}
