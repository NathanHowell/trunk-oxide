#![allow(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cxx::bridge(namespace = "trunk_oxide")]
pub mod ffi {
    // ── Shared enums (mirrored in both C++ and Rust) ──

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TrSignalType {
        Normal = 0,
        Emergency = 1,
        EmergencyAck = 2,
        RadioCheck = 3,
        RadioCheckAck = 4,
        RadioStun = 5,
        RadioStunAck = 6,
        RadioRevive = 7,
        RadioReviveAck = 8,
        NormalPre = 9,
        EmergencyPre = 10,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TrMessageType {
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TrUnitEventType {
        Registration = 0,
        Deregistration = 1,
        Affiliation = 2,
        Acknowledge = 3,
        Location = 4,
        DataGrant = 5,
        AnswerRequest = 6,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TrCallDataStatus {
        Initial = 0,
        Success = 1,
        Retry = 2,
        Failed = 3,
    }

    // ── Shared structs (POD, copied across the boundary) ──

    #[derive(Debug, Clone)]
    struct TrPatchData {
        pub sg: u64,
        pub ga1: u64,
        pub ga2: u64,
        pub ga3: u64,
    }

    #[derive(Debug, Clone)]
    struct TrTrunkMessage {
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

    #[derive(Debug, Clone)]
    struct TrTransmission {
        pub source: i64,
        pub talkgroup: i64,
        pub slot: u32,
        pub color_code: u32,
        pub start_time: i64,
        pub stop_time: i64,
        pub sample_count: i64,
        pub spike_count: i64,
        pub error_count: i64,
        pub freq: f64,
    }

    #[derive(Debug, Clone)]
    struct TrCallSource {
        pub source: i64,
        pub position: f64,
        pub emergency: bool,
        pub signal_system: String,
        pub tag: String,
    }

    #[derive(Debug, Clone)]
    struct TrCallError {
        pub position: f64,
        pub total_len: f64,
        pub error_count: f64,
        pub spike_count: f64,
    }

    #[derive(Debug, Clone)]
    struct TrCallData {
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

    /// Lightweight live-call info for events (call_start, calls_active, signal).
    /// Built by Rust from CallEntry fields.
    #[derive(Debug, Clone)]
    struct TrCallInfo {
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
        // TODO(phase3): Move TrCallInfo out of CXX bridge and use CallState
        // directly instead of i32. This is i32 only because CXX shared structs
        // cannot hold Rust-native enums with data variants.
        pub call_state: i32,
    }

    #[derive(Debug, Clone)]
    struct TrStopRecordingResult {
        pub freq_error: i32,
        pub recorder_pwr: f64,
        pub has_recorder: bool,
        pub is_dmr: bool,
    }

    #[derive(Debug, Clone)]
    struct TrRecorderStatus {
        pub source_num: i32,
        pub source_center: f64,
        pub source_device: String,
        pub rec_num: i32,
        pub rec_type: String,
        pub rec_state: i32,
    }

    // ── Bulk config structs for source/system setup ──

    struct TrGainStage {
        pub name: String,
        pub value: f64,
    }

    struct TrSourceConfig {
        pub driver: String,
        pub device: String,
        pub center: f64,
        pub rate: f64,
        pub error: f64,
        pub gain: f64,
        pub agc: bool,
        pub ppm: f64,
        pub antenna: String,
        pub silence_frames: i32,
        pub signal_detector_threshold: f64,
        pub has_signal_detector_threshold: bool,
        pub gain_stages: Vec<TrGainStage>,
        pub digital_recorders: i32,
        pub analog_recorders: i32,
        pub sigmf_recorders: i32,
    }

    struct TrTalkgroupConfig {
        pub id: i64,
        pub mode: String,
        pub priority: i32,
        pub preferred_nac: u64,
    }

    struct TrChannelConfig {
        pub id: i64,
        pub freq: f64,
        pub tone: f64,
        pub squelch_db: f64,
        pub signal_detection: bool,
    }

    struct TrDetectedSignal {
        pub center_freq: f64,
        pub max_rssi: i32,
        pub threshold: i32,
    }

    struct TrUnitTagConfig {
        pub pattern: String,
        pub tag: String,
    }

    struct TrSystemConfig {
        pub short_name: String,
        pub system_type: String,
        pub qpsk_mod: bool,
        pub squelch_db: f64,
        pub analog_levels: f64,
        pub digital_levels: f64,
        pub tau: f64,
        pub max_dev: i32,
        pub filter_width: f64,
        pub decode_mdc: bool,
        pub decode_fsync: bool,
        pub decode_star: bool,
        pub decode_tps: bool,
        pub bandplan: String,
        pub bandplan_base: f64,
        pub bandplan_high: f64,
        pub bandplan_spacing: f64,
        pub bandplan_offset: i32,
        pub bandfreq: f64,
        pub sys_id: u64,
        pub wacn: u64,
        pub nac: u64,
        pub control_channels: Vec<f64>,
        pub channels: Vec<TrChannelConfig>,
        pub talkgroups: Vec<TrTalkgroupConfig>,
        pub unit_tags: Vec<TrUnitTagConfig>,
        pub unit_tags_mode: String,
    }

    /// Recorder start configuration, built by Rust and passed to C++.
    /// Maps 1:1 to the C++ RecorderConfig POD struct.
    #[derive(Debug, Clone)]
    struct TrRecorderConfig {
        pub talkgroup: i64,
        pub call_num: i64,
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

    // ── Opaque C++ types ──

    unsafe extern "C++" {
        include!("trunk-recorder-sys/cpp/bridge.h");

        // Opaque C++ types — these are pointers to trunk-recorder objects.
        // Accessor wrappers are free functions defined in bridge.h/bridge.cpp.
        type TrCall;
        type TrSystem;
        type TrRecorder;
        // ── Call accessors ──
        fn call_get_freq_error(call: &TrCall) -> i32;

        // ── Recorder accessors ──
        fn recorder_get_num(rec: &TrRecorder) -> i32;
        fn recorder_get_freq_error(rec: &TrRecorder) -> i32;
        fn recorder_get_wav_hz(rec: &TrRecorder) -> i64;

        // ── Opaque lifecycle type ──
        type TrContext;

        // ── Lifecycle functions ──
        fn tr_create_from_builder(handler: Box<EventHandlerBox>) -> Result<UniquePtr<TrContext>>;
        fn tr_finalize(ctx: Pin<&mut TrContext>) -> Result<()>;

        // ── Config builder functions ──
        fn tr_config_set_temp_dir(ctx: Pin<&mut TrContext>, dir: &str);
        fn tr_config_set_soft_vocoder(ctx: Pin<&mut TrContext>, enabled: bool);
        fn tr_config_set_debug_recorder(
            ctx: Pin<&mut TrContext>,
            enabled: bool,
            address: &str,
            port: i32,
        );

        // ── Bulk source/system builder functions ──
        fn tr_add_source_from_config(ctx: Pin<&mut TrContext>, cfg: &TrSourceConfig)
        -> Result<i32>;
        fn tr_add_system_from_config(ctx: Pin<&mut TrContext>, cfg: &TrSystemConfig)
        -> Result<i32>;

        // ── Phase A: Loop Control ──
        fn tr_start_gnuradio(ctx: Pin<&mut TrContext>);
        fn tr_stop_gnuradio(ctx: Pin<&mut TrContext>);

        // ── Phase B: Message Draining ──
        fn tr_context_check_sources(ctx: Pin<&mut TrContext>) -> Result<()>;
        fn tr_context_get_recorder_status(ctx: &TrContext) -> Vec<TrRecorderStatus>;
        fn tr_context_autotune_control_channels(ctx: Pin<&mut TrContext>);

        // ── Phase B: Message Routing (called from Rust handle_message) ──
        fn tr_system_add_control_channel_runtime(ctx: Pin<&mut TrContext>, sys_num: i32, freq: f64);
        fn tr_system_update_sysid(ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) -> bool;
        fn tr_system_update_status(ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) -> bool;

        // ── Phase B: Decode Rate Checking (called from Rust check_decode_rates) ──
        fn tr_system_get_current_control_channel(ctx: Pin<&mut TrContext>, sys_num: i32) -> f64;
        fn tr_system_get_control_channel_pwr(ctx: Pin<&mut TrContext>, sys_num: i32) -> f64;

        // ── Call — recorder status (null-guarded) ──
        fn call_has_recorder(call: &TrCall) -> bool;

        // ── Call — Rust call ID ──
        fn call_set_rust_call_id(call: Pin<&mut TrCall>, id: u64);

        // ── Call — lifecycle ──
        fn call_restart(call: Pin<&mut TrCall>);
        fn call_start_initial_recording(call: Pin<&mut TrCall>, cfg: &TrRecorderConfig);
        fn call_set_recorder_enabled(call: Pin<&mut TrCall>, enabled: bool);

        // ── Conventional call management — Rust selects source, C++ creates objects ──
        fn tr_create_conventional_call(
            ctx: Pin<&mut TrContext>,
            sys_num: i32,
            source_idx: i32,
            channel: &TrChannelConfig,
        ) -> UniquePtr<TrCall>;

        // ── Phase C: Call Management — lifecycle operations ──
        fn tr_context_create_call(
            ctx: Pin<&mut TrContext>,
            msg: &TrTrunkMessage,
        ) -> UniquePtr<TrCall>;

        // ── Phase C: Call Management — recording policy support ──
        fn tr_source_available_recorders(ctx: &TrContext, source_idx: i32, is_analog: bool) -> i32;
        fn tr_source_start_recording(
            ctx: Pin<&mut TrContext>,
            source_idx: i32,
            call: Pin<&mut TrCall>,
            is_analog: bool,
            cfg: &TrRecorderConfig,
        ) -> bool;

        // ── Phase C: Call Management — call-level recorder accessors ──
        fn call_recorder_since_last_write_secs(call: &TrCall) -> f64;
        fn call_process_recorder_queue(call: &TrCall);

        // ── Phase C: Call Management — conclude recording (recorder mechanics) ──
        fn call_stop_recording(call: Pin<&mut TrCall>) -> TrStopRecordingResult;
        fn call_get_transmission_list(call: &TrCall) -> Vec<TrTransmission>;
        fn call_get_dmr_transmission_list(call: &TrCall, slot: i32) -> Vec<TrTransmission>;
        fn call_get_recorder_num(call: &TrCall) -> i32;

        // ── Phase C: Call Management — update source tracking ──
        fn call_recorder_set_source(call: Pin<&mut TrCall>, src: i64);
        fn call_set_freq(call: Pin<&mut TrCall>, freq: f64);

        // ── Recorder setup notification (extracts recorder from call, fires through event_sink) ──
        fn call_notify_setup_recorder(ctx: Pin<&mut TrContext>, call: &TrCall);

        // ── Event channel wiring ──
        fn tr_context_set_event_notifier(ctx: Pin<&mut TrContext>, notifier: Box<EventNotifier>);
        fn tr_context_wire_event_notifiers(ctx: Pin<&mut TrContext>);
    }

    // ── Rust functions callable from C++ (event handler callbacks) ──

    extern "Rust" {
        type EventHandlerBox;

        // Forces cxx to generate the Box<EventHandlerBox> C++ template instantiation.
        fn drop_event_handler_box(handler: Box<EventHandlerBox>);

        fn event_handler_audio_stream(
            handler: &EventHandlerBox,
            rust_call_id: u64,
            recorder: &TrRecorder,
            samples: &[i16],
        );
        fn event_handler_setup_recorder(handler: &EventHandlerBox, recorder: &TrRecorder);
        fn event_handler_unit_alias_discovered(
            handler: &EventHandlerBox,
            system: &TrSystem,
            radio_id: i64,
            alias: &str,
            source: &str,
            talkgroup_id: i64,
        );
    }

    // ── Rust event notifier callable from C++ (GR thread → channel) ──

    extern "Rust" {
        type EventNotifier;

        #[allow(clippy::unnecessary_box_returns)]
        fn event_notifier_clone(notifier: &EventNotifier) -> Box<EventNotifier>;
        fn event_notifier_notify_trunk_messages(
            notifier: &EventNotifier,
            messages: Vec<TrTrunkMessage>,
        );
        fn event_notifier_notify_squelch(notifier: &EventNotifier, rec: i32, muted: bool, pwr: f64);
        fn event_notifier_notify_signals_detected(
            notifier: &EventNotifier,
            source_num: i32,
            source_center_freq: f64,
            signals: Vec<TrDetectedSignal>,
        );
    }
}

// ── GR event channel types ──

/// A signal detected by the signal detector FFT block.
#[derive(Debug, Clone)]
pub struct DetectedSignal {
    /// Center frequency relative to source center, in Hz.
    pub center_freq: f64,
    /// Maximum RSSI in dB.
    pub max_rssi: i32,
    /// Detection threshold in dB.
    pub threshold: i32,
}

/// Events sent from GNU Radio scheduler threads to the monitor loop via channel.
pub enum GrEvent {
    /// Parsed trunk messages from a system's `msg_queue` (already drained+parsed on GR thread).
    TrunkMessages(Vec<ffi::TrTrunkMessage>),
    /// Squelch state change from a recorder's squelch block.
    Squelch {
        recorder_num: i32,
        muted: bool,
        pwr_db: f64,
    },
    /// Signals detected on a conventional source by the signal detector.
    SignalsDetected {
        source_num: i32,
        source_center_freq: f64,
        signals: Vec<DetectedSignal>,
    },
}

/// Opaque notifier that wraps a cloned `tokio::sync::mpsc::Sender<GrEvent>`.
/// Each GR block owns one (cloned from a prototype via `event_notifier_clone`).
pub struct EventNotifier(tokio::sync::mpsc::Sender<GrEvent>, Arc<AtomicUsize>);

impl EventNotifier {
    pub const fn new(
        sender: tokio::sync::mpsc::Sender<GrEvent>,
        pending: Arc<AtomicUsize>,
    ) -> Self {
        Self(sender, pending)
    }
}

#[expect(clippy::unnecessary_box_returns, reason = "cxx requires Box<T> for opaque Rust types")]
fn event_notifier_clone(notifier: &EventNotifier) -> Box<EventNotifier> {
    Box::new(EventNotifier(notifier.0.clone(), Arc::clone(&notifier.1)))
}

fn event_notifier_notify_trunk_messages(
    notifier: &EventNotifier,
    messages: Vec<ffi::TrTrunkMessage>,
) {
    if !messages.is_empty()
        && notifier
            .0
            .try_send(GrEvent::TrunkMessages(messages))
            .is_ok()
    {
        notifier.1.fetch_add(1, Ordering::Relaxed);
    }
}

fn event_notifier_notify_squelch(notifier: &EventNotifier, rec: i32, muted: bool, pwr: f64) {
    if notifier
        .0
        .try_send(GrEvent::Squelch {
            recorder_num: rec,
            muted,
            pwr_db: pwr,
        })
        .is_ok()
    {
        notifier.1.fetch_add(1, Ordering::Relaxed);
    }
}

fn event_notifier_notify_signals_detected(
    notifier: &EventNotifier,
    source_num: i32,
    source_center_freq: f64,
    signals: Vec<ffi::TrDetectedSignal>,
) {
    let signals: Vec<DetectedSignal> = signals
        .into_iter()
        .map(|s| DetectedSignal {
            center_freq: s.center_freq,
            max_rssi: s.max_rssi,
            threshold: s.threshold,
        })
        .collect();
    if notifier
        .0
        .try_send(GrEvent::SignalsDetected {
            source_num,
            source_center_freq,
            signals,
        })
        .is_ok()
    {
        notifier.1.fetch_add(1, Ordering::Relaxed);
    }
}

/// Full system info built entirely from Rust-side caches (config + identity).
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

/// Trait for the 3 C++-initiated callbacks that come through cxx trampolines.
/// All methods take `&self` — no mutex needed.
pub trait CxxCallbacks: Send + Sync {
    fn audio_stream(&self, rust_call_id: u64, recorder: &ffi::TrRecorder, samples: &[i16]);
    fn setup_recorder(&self, recorder: &ffi::TrRecorder);
    fn unit_alias_discovered(
        &self,
        system: &ffi::TrSystem,
        radio_id: i64,
        alias: &str,
        source: &str,
        talkgroup_id: i64,
    );
}

/// Opaque type held by C++ via cxx for the 3 C++-initiated callbacks.
pub struct EventHandlerBox {
    inner: Arc<dyn CxxCallbacks>,
}

impl EventHandlerBox {
    pub fn new(inner: Arc<dyn CxxCallbacks>) -> Self {
        Self { inner }
    }
}

#[allow(clippy::boxed_local)]
fn drop_event_handler_box(_handler: Box<EventHandlerBox>) {
    // Box is dropped automatically
}

// ── Trampoline functions for C++-initiated callbacks ──
// Only 3 remain: audio_stream, setup_recorder, unit_alias_discovered.
// All other events are fired directly from Rust without FFI.

fn event_handler_audio_stream(
    handler: &EventHandlerBox,
    rust_call_id: u64,
    recorder: &ffi::TrRecorder,
    samples: &[i16],
) {
    handler.inner.audio_stream(rust_call_id, recorder, samples);
}

fn event_handler_setup_recorder(handler: &EventHandlerBox, recorder: &ffi::TrRecorder) {
    handler.inner.setup_recorder(recorder);
}

fn event_handler_unit_alias_discovered(
    handler: &EventHandlerBox,
    system: &ffi::TrSystem,
    radio_id: i64,
    alias: &str,
    source: &str,
    talkgroup_id: i64,
) {
    handler
        .inner
        .unit_alias_discovered(system, radio_id, alias, source, talkgroup_id);
}
