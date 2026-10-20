//! What the radio and the call state machine say to each other.

use std::sync::Arc;

use std::sync::atomic::{AtomicUsize, Ordering};
pub use trunk_p25::voice::Encryption;

/// A unit's doing on the control channel, reported as an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitEventType {
    Registration,
    Deregistration,
    Affiliation,
    Acknowledge,
    Location,
    DataGrant,
    AnswerRequest,
}

/// A trunking message, as the parser reads it off a control channel.
#[derive(Debug, Clone, PartialEq)]
pub struct TrunkMessage {
    pub sys_num: i32,
    /// The NAC the packet came with (0 for a message without one).
    pub nac: u16,
    /// The packet's opcode, as trunk-recorder reported it (255: none).
    pub opcode: u64,
    pub kind: Trunking,
}

/// What a trunking message says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Trunking {
    /// A voice channel granted to a talkgroup (or a Motorola patch
    /// supergroup).
    Grant(Voice),
    /// A talkgroup's call going on, on its channel.
    Update(Voice),
    /// A voice channel granted to a unit-to-unit call (`talkgroup` is the
    /// called unit).
    UnitGrant(Voice),
    UnitUpdate(Voice),
    /// A unit asked to answer a unit-to-unit call (no channel yet).
    AnswerRequest(Voice),
    DataGrant {
        /// 0 if the channel is unknown.
        freq_hz: f64,
        unit: u32,
        options: Options,
    },
    Acknowledge {
        group: u16,
        unit: u32,
    },
    Affiliation {
        group: u16,
        unit: u32,
    },
    Location {
        group: u16,
        unit: u32,
    },
    Registration {
        unit: u32,
    },
    Deregistration {
        unit: u32,
    },
    /// Another control channel of the site.
    ControlChannel {
        freq_hz: f64,
    },
    /// RFSS status: which site this is.
    Site {
        sys_id: u16,
        rfss: u8,
        site: u8,
    },
    /// Network status: which system this is.
    Network {
        /// The control channel named; 0 if unknown.
        freq_hz: f64,
        wacn: u32,
        sys_id: u16,
    },
    PatchAdd(Patch),
    PatchDelete(Patch),
    /// A terminator with link control (a control channel's names no
    /// talkgroup; the voice channel's own terminators reach the state
    /// machine as `term_events`).
    Tdulc,
    /// A packet that could not be read.
    Invalid,
    /// A packet the state machine has no use for.
    Unknown,
}

/// A voice grant or update.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Voice {
    pub channel: Channel,
    /// The talkgroup, or the called unit of a unit-to-unit call.
    pub talkgroup: u32,
    /// The unit talking, when the message names it.
    pub source: Option<u32>,
    pub options: Options,
}

/// A traffic channel as a grant names it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Channel {
    /// 0 if no plan covers the channel.
    pub freq_hz: f64,
    /// The TDMA slot, on a phase 2 channel.
    pub slot: Option<u8>,
}

impl Channel {
    /// No channel, or one no plan covers.
    pub const NONE: Self = Self {
        freq_hz: 0.0,
        slot: None,
    };
}

/// A voice call's service options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Options {
    pub emergency: bool,
    pub encrypted: bool,
    pub duplex: bool,
    /// Packet (not circuit) mode.
    pub packet: bool,
    pub priority: u8,
}

/// Talkgroups patched into a supergroup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patch {
    pub supergroup: u16,
    pub groups: [u16; 3],
}

/// One recording's audio accounting.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transmission {
    pub talkgroup: u32,
    pub start_time: i64,
    pub stop_time: i64,
    pub sample_count: i64,
    pub spike_count: i64,
    pub error_count: i64,
    pub term_events: i64,
    /// `term_events` by what ended them, indexed by [`Ended::index`].
    ///
    /// [`Ended::index`]: trunk_p25::assemble::Ended::index
    pub ended_by: [i64; 4],
    pub after_term_samples: i64,
    pub freq: f64,
}

/// A call as the state machine numbers it: what audio and in-band events
/// are routed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CallId(pub u64);

impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A channel the radio opened, by number: a channelizer slot of one
/// source, numbered in the order the radio first used it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelNum(pub u32);

impl std::fmt::Display for ChannelNum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A channel slot as the status report shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelStatus {
    pub num: ChannelNum,
    pub source_num: i32,
    pub source_center: f64,
    pub source_device: String,
    /// The kind it was last opened as.
    pub kind: &'static str,
    /// Who holds it: `None` while it is free.
    pub call: Option<CallId>,
}

/// A P25 voice call's outcome, when it ends (metrics).
#[derive(Debug, Clone, PartialEq)]
pub struct CallReport {
    pub system: String,
    pub talkgroup: u32,
    pub freq: f64,
    pub phase2: bool,
    pub audio_seconds: f64,
    pub transmissions: i64,
    pub errors: i64,
    pub spikes: i64,
    pub sources: Vec<u32>,
    pub algid: u8,
    pub repeated: i64,
    pub muted: i64,
}

/// A call's voice decode errors and spikes, as counted by the decoder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallCounts {
    pub errors: u64,
    pub spikes: u64,
    /// The ALGID confirmed on the call's own slot (0x80: clear).
    pub algid: u8,
    pub repeated: u64,
    pub muted: u64,
}

/// Something said in band, at a sample of the decoded audio.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceTag {
    /// Into the audio the same `process` call appended.
    pub offset: u32,
    pub kind: VoiceTagKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VoiceTagKind {
    /// The unit talking.
    Source(u32),
    /// The talkgroup.
    Group(u16),
    /// The transmission ended.
    Terminate(trunk_p25::assemble::Ended),
    /// An over-the-air alias.
    Alias(trunk_p25::assemble::Alias),
}

/// A signal the wideband detector found.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedSignal {
    /// Relative to the source centre, Hz.
    pub center_freq: f64,
    pub max_rssi: i32,
    pub threshold: i32,
}

/// An alias a radio sent over the air.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitAlias {
    pub radio_id: i64,
    pub alias: String,
    /// Which decoder found it (`MotoP25_FDMA`, `HarrisP25_TDMA`, …).
    pub source: String,
    pub talkgroup_id: i64,
}

/// What a transmission detector decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Squelch {
    /// Someone keyed up.
    Open,
    /// The transmission is over.
    Close(CloseReason),
}

impl Squelch {
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Open)
    }
}

/// Why a transmission detector closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The power fell below the squelch threshold (digital channels).
    Power,
    /// The carrier went: the noise came back.
    Quieting,
    /// Carrier still there, the configured tone not.
    ToneLost,
    /// The audio went to zeros: something upstream muted the slot.
    Silence,
}

impl From<trunk_fm::gate::CloseReason> for CloseReason {
    fn from(r: trunk_fm::gate::CloseReason) -> Self {
        use trunk_fm::gate::CloseReason as G;
        match r {
            G::Quieting => Self::Quieting,
            G::ToneLost => Self::ToneLost,
            G::Silence => Self::Silence,
        }
    }
}

/// Events from the DSP threads to the monitor loop.
pub enum RadioEvent {
    /// Parsed trunk messages from a control channel.
    TrunkMessages(Vec<TrunkMessage>),
    /// A conventional channel's transmission detector (the power squelch
    /// of a digital channel, trunk-fm's gate on an analog one) opened or
    /// closed.
    Squelch {
        call_id: CallId,
        squelch: Squelch,
        /// The slot's power at the transition, if it has a reading.
        power_db: Option<f64>,
    },
    /// An in-band P25 source change on a voice channel: `unit` is talking.
    SourceChange { call_id: CallId, unit: u32 },
    /// In-band encryption parameters of a call we hold no key for.
    Crypto {
        call_id: CallId,
        encryption: Encryption,
    },
    /// Signals the wideband detector found on a source.
    SignalsDetected {
        source_num: i32,
        source_center_freq: f64,
        signals: Vec<DetectedSignal>,
    },
    /// A radio announced its alias.
    UnitAlias { sys_num: i32, alias: UnitAlias },
}

/// Sends [`RadioEvent`]s to the monitor loop; counts what is in flight.
#[derive(Clone)]
pub struct EventSender {
    tx: tokio::sync::mpsc::Sender<RadioEvent>,
    pending: Arc<AtomicUsize>,
}

impl EventSender {
    pub const fn new(tx: tokio::sync::mpsc::Sender<RadioEvent>, pending: Arc<AtomicUsize>) -> Self {
        Self { tx, pending }
    }

    /// Queue an event; dropped (and logged) if the loop has fallen 256 behind.
    pub fn send(&self, event: RadioEvent) {
        if self.tx.try_send(event).is_ok() {
            self.pending.fetch_add(1, Ordering::Relaxed);
        } else {
            tracing::warn!("radio event dropped: the monitor loop is not keeping up");
        }
    }
}

/// Where decoded audio goes: the gRPC bridge, keyed by the call.
pub trait AudioSink: Send + Sync {
    fn audio(&self, call_id: CallId, samples: &[i16]);
}

/// What one channelizer block's decoders produced, for the source thread to
/// route: audio to the [`AudioSink`], events to the [`EventSender`].
#[derive(Default)]
pub struct Decoded {
    /// Audio by call, in decode order.
    pub audio: Vec<(CallId, Vec<i16>)>,
    pub events: Vec<RadioEvent>,
}

impl Decoded {
    /// Append `samples` to the call's audio.
    pub fn audio(&mut self, call_id: CallId, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        match self.audio.last_mut() {
            Some((id, buf)) if *id == call_id => buf.extend_from_slice(samples),
            _ => self.audio.push((call_id, samples.to_vec())),
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.audio.is_empty() && self.events.is_empty()
    }

    pub fn clear(&mut self) {
        self.audio.clear();
        self.events.clear();
    }
}
