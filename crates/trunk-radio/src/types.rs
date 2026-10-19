//! What the radio and the call state machine say to each other.
//!
//! These were the cxx bridge's shared structs; their names and fields are
//! kept so the state machine, the gRPC bridge and the replay tooling read
//! the same.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrUnitEventType {
    Registration = 0,
    Deregistration = 1,
    Affiliation = 2,
    Acknowledge = 3,
    Location = 4,
    DataGrant = 5,
    AnswerRequest = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrCallDataStatus {
    Initial = 0,
    Success = 1,
    Retry = 2,
    Failed = 3,
}

impl TrCallDataStatus {
    pub const fn repr(self) -> u8 {
        self as u8
    }
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
    /// A terminator with link control, naming the talkgroup when it does
    /// (a control channel's never does).
    Tdulc {
        talkgroup: Option<u32>,
    },
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
pub struct TrTransmission {
    pub source: i64,
    pub talkgroup: i64,
    pub slot: u32,
    pub color_code: u32,
    pub start_time: i64,
    pub stop_time: i64,
    pub sample_count: i64,
    pub spike_count: i64,
    pub error_count: i64,
    pub term_events: i64,
    pub after_term_samples: i64,
    pub freq: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrCallSource {
    pub source: i64,
    pub position: f64,
    pub emergency: bool,
    pub signal_system: String,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrCallError {
    pub position: f64,
    pub total_len: f64,
    pub error_count: f64,
    pub spike_count: f64,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TrCallData {
    pub talkgroup: i64,
    pub color_code: u32,
    pub patched_talkgroups: Vec<u64>,
    pub call_num: i64,
    pub freq: f64,
    pub freq_error: i32,
    pub source_num: i32,
    pub recorder_num: i32,
    pub signal: f64,
    pub noise: f64,
    pub start_time: i64,
    pub stop_time: i64,
    pub error_count: i64,
    pub spike_count: i64,
    pub encrypted: bool,
    pub emergency: bool,
    pub priority: i32,
    pub mode: bool,
    pub duplex: bool,
    pub sys_num: i32,
    pub short_name: String,
    pub audio_type: String,
    pub tdma_slot: i32,
    pub length: f64,
    pub phase2_tdma: bool,
    pub transmission_source_list: Vec<TrCallSource>,
    pub transmission_error_list: Vec<TrCallError>,
    pub transmission_list: Vec<TrTransmission>,
    pub status: TrCallDataStatus,
}

/// Live-call info for events (call start, calls active, signal).
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TrCallInfo {
    pub talkgroup: i64,
    pub call_num: i64,
    pub sys_num: i32,
    pub freq: f64,
    pub signal: f64,
    pub noise: f64,
    pub freq_error: i32,
    pub encrypted: bool,
    pub emergency: bool,
    pub is_analog: bool,
    pub is_conventional: bool,
    pub phase2_tdma: bool,
    pub tdma_slot: i32,
    pub priority: i32,
    pub mode: bool,
    pub duplex: bool,
    pub conversation_mode: bool,
    pub current_source_id: i64,
    pub start_time: i64,
    pub elapsed: f64,
    pub short_name: String,
    pub call_state: i32,
}

/// What a recorder needs to know to start a call.
#[derive(Debug, Clone, PartialEq)]
pub struct TrRecorderConfig {
    pub talkgroup: i64,
    pub call_num: i64,
    /// Where to tune (software clock correction already applied).
    pub freq: f64,
    pub short_name: String,
    pub temp_dir: String,
    pub phase2_tdma: bool,
    pub tdma_slot: i32,
    pub xor_mask: Vec<u8>,
    pub squelch_db: f64,
    pub digital_levels: f64,
    pub rust_call_id: u64,
}

/// What stopping a recorder reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrStopRecordingResult {
    pub freq_error: i32,
    pub recorder_pwr: f64,
    pub has_recorder: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrRecorderStatus {
    pub source_num: i32,
    pub source_center: f64,
    pub source_device: String,
    pub rec_num: i32,
    pub rec_type: String,
    pub rec_state: i32,
}

/// trunk-recorder's recorder states, as the status log prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum RecorderState {
    Recording = 1,
    Inactive = 2,
    Active = 3,
    Idle = 4,
    Stopped = 6,
    Available = 7,
}

/// A conventional channel, for a dedicated recorder.
#[derive(Debug, Clone, PartialEq)]
pub struct TrChannelConfig {
    pub id: i64,
    pub freq: f64,
    pub tone: f64,
    pub squelch_db: f64,
    pub signal_detection: bool,
}

/// A P25 voice call's outcome, when it ends (metrics).
#[derive(Debug, Clone, PartialEq)]
pub struct TrCallReport {
    pub system: String,
    pub recorder: i32,
    pub talkgroup: i64,
    pub freq: f64,
    pub phase2: bool,
    pub audio_seconds: f64,
    pub transmissions: i64,
    pub errors: i64,
    pub spikes: i64,
    pub sources: Vec<i64>,
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
    pub value: i64,
    pub alias: Option<trunk_p25::assemble::Alias>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceTagKind {
    /// The unit talking (`value`).
    Source,
    /// The talkgroup (`value`).
    Group,
    /// The transmission ended.
    Terminate,
    /// An over-the-air alias (`alias`).
    Alias,
}

/// A signal the wideband detector found.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedSignal {
    /// Relative to the source centre, Hz.
    pub center_freq: f64,
    pub max_rssi: i32,
    pub threshold: i32,
}

/// Full system info built from config and identity.
#[derive(Debug, Clone)]
pub struct SystemInfo {
    pub sys_num: i32,
    pub system_type: i32,
    pub short_name: String,
    pub sys_id: u64,
    pub wacn: u64,
    pub nac: u64,
    pub rfss: i32,
    pub site_id: i32,
    pub bandplan: String,
    pub analog_levels: f64,
    pub digital_levels: f64,
    pub squelch_db: f64,
    pub conversation_mode: bool,
    pub message_count: i32,
    pub decode_rate: i32,
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

/// dB value meaning "no measurement" where a struct carries a plain `f64`.
pub const NO_SIGNAL_DB: f64 = 998.0;

#[must_use]
pub fn signal_to_ffi(value: Option<f64>) -> f64 {
    value.unwrap_or(NO_SIGNAL_DB)
}

#[must_use]
pub fn signal_from_ffi(value: f64) -> Option<f64> {
    (value < NO_SIGNAL_DB).then_some(value)
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
    /// A recorder's transmission detector (the power squelch of a digital
    /// channel, trunk-fm's gate on an analog one) opened or closed.
    Squelch {
        recorder_num: i32,
        squelch: Squelch,
        /// The slot's power at the transition ([`crate::recorder::DB_UNSET`]
        /// without a reading).
        pwr_db: f64,
    },
    /// An in-band P25 source change on a voice channel.
    SourceChange { rust_call_id: u64, src_id: i64 },
    /// In-band encryption parameters of a call we hold no key for.
    Crypto {
        rust_call_id: u64,
        alg_id: u32,
        key_id: u32,
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
    fn audio(&self, rust_call_id: u64, samples: &[i16]);
}

/// What one channelizer block's decoders produced, for the source thread to
/// route: audio to the [`AudioSink`], events to the [`EventSender`].
#[derive(Default)]
pub struct Decoded {
    /// Audio by call id, in decode order.
    pub audio: Vec<(u64, Vec<i16>)>,
    pub events: Vec<RadioEvent>,
}

impl Decoded {
    /// Append `samples` to the call's audio.
    pub fn audio(&mut self, rust_call_id: u64, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        match self.audio.last_mut() {
            Some((id, buf)) if *id == rust_call_id => buf.extend_from_slice(samples),
            _ => self.audio.push((rust_call_id, samples.to_vec())),
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
