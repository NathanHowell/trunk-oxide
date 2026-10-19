//! What the radio and the call state machine say to each other.
//!
//! These were the cxx bridge's shared structs; their names and fields are
//! kept so the state machine, the gRPC bridge and the replay tooling read
//! the same.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What a trunking message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TrMessageType {
    Grant = 0,
    Status = 1,
    Update = 2,
    ControlChannel = 3,
    Registration = 4,
    Deregistration = 5,
    Affiliation = 6,
    Sysid = 7,
    Acknowledge = 8,
    Location = 9,
    PatchAdd = 10,
    PatchDelete = 11,
    DataGrant = 12,
    UuAnsReq = 13,
    UuVGrant = 14,
    UuVUpdate = 15,
    InvalidCcMessage = 16,
    Tdulc = 17,
    Unknown = 99,
}

impl TrMessageType {
    pub const fn repr(self) -> u8 {
        self as u8
    }
}

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrPatchData {
    pub sg: u64,
    pub ga1: u64,
    pub ga2: u64,
    pub ga3: u64,
}

/// A parsed control-channel message.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TrTrunkMessage {
    pub message_type: TrMessageType,
    pub meta: String,
    pub freq: f64,
    pub talkgroup: i64,
    pub encrypted: bool,
    pub emergency: bool,
    pub duplex: bool,
    pub mode: bool,
    pub priority: i32,
    pub tdma_slot: i32,
    pub phase2_tdma: bool,
    pub source: i64,
    pub sys_num: i32,
    pub sys_id: u64,
    pub sys_rfss: i32,
    pub sys_site_id: i32,
    pub nac: u64,
    pub wacn: u64,
    pub patch_data: TrPatchData,
    pub opcode: u64,
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

/// Events from the DSP threads to the monitor loop.
pub enum RadioEvent {
    /// Parsed trunk messages from a control channel.
    TrunkMessages(Vec<TrTrunkMessage>),
    /// A recorder's squelch (carrier or tone gate) changed.
    Squelch {
        recorder_num: i32,
        muted: bool,
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
