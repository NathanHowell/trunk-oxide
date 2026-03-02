#![allow(unsafe_code)]

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
    enum TrState {
        Monitoring = 0,
        Recording = 1,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TrMonitoringState {
        Unspecified = 0,
        UnknownTg = 1,
        IgnoredTg = 2,
        NoSource = 3,
        NoRecorder = 4,
        Encrypted = 5,
        Duplicate = 6,
        Superseded = 7,
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
        pub length: f64,
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
    /// Built by Rust from CallEntry fields, passed through C++ fire functions.
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
        pub current_length: f64,
        pub start_time: i64,
        pub elapsed: f64,
        pub short_name: String,
        pub state: TrState,
        pub monitoring_state: TrMonitoringState,
    }

    #[derive(Debug, Clone)]
    struct TrCallMetadata {
        pub call_num: i64,
        pub talkgroup: i64,
        pub freq: f64,
        pub sys_num: i32,
        pub short_name: String,
        pub tdma_slot: i32,
        pub phase2_tdma: bool,
        pub mode: bool,
        pub duplex: bool,
        pub emergency: bool,
        pub is_conventional: bool,
        pub start_time: i64,
        pub state: TrState,
        pub monitoring_state: TrMonitoringState,
        pub encrypted: bool,
        pub is_analog: bool,
        pub current_source_id: i64,
        pub signal: f64,
        pub noise: f64,
        pub priority: i32,
        pub multi_site: bool,
        pub wacn: u64,
        pub rfss: i32,
        pub site_id: i32,
        pub multi_site_name: String,
        pub conversation_mode: bool,
        pub max_duration: f64,
    }

    #[derive(Debug, Clone)]
    struct TrSystemInfo {
        pub sys_num: i32,
        pub sys_id: u64,
        pub short_name: String,
        pub system_type: i32,
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

    #[derive(Debug, Clone)]
    struct TrSquelchEvent {
        pub call_num: i64,
        pub muted: bool,   // true = squelch closed (idle), false = squelch open (active)
        pub pwr_db: f64,   // power at transition time
    }

    #[derive(Debug, Clone)]
    struct TrStopRecordingResult {
        pub final_length: f64,
        pub freq_error: i32,
        pub recorder_pwr: f64,
        pub recorder_idle: bool,
        pub has_recorder: bool,
        pub is_dmr: bool,
    }

    // ── Opaque C++ types ──

    unsafe extern "C++" {
        include!("trunk-recorder-sys/cpp/bridge.h");

        // Opaque C++ types — these are pointers to trunk-recorder objects.
        // Accessor wrappers are free functions defined in bridge.h/bridge.cpp.
        type TrCall;
        type TrSystem;
        type TrRecorder;
        // ── Call accessors (most removed — Rust owns call state via CallEntry) ──
        fn call_get_freq_error(call: &TrCall) -> i32;
        fn call_get_current_length(call: &TrCall) -> f64;

        // ── System info bulk read (replaces 15 individual system_get_* functions) ──
        fn tr_context_get_system_info(ctx: Pin<&mut TrContext>, sys_num: i32) -> TrSystemInfo;

        // ── Recorder accessors ──
        fn recorder_get_num(rec: &TrRecorder) -> i32;
        fn recorder_get_freq(rec: &TrRecorder) -> f64;
        fn recorder_get_freq_error(rec: &TrRecorder) -> i32;
        fn recorder_get_talkgroup(rec: &TrRecorder) -> i64;
        fn recorder_get_wav_hz(rec: &TrRecorder) -> i64;
        fn recorder_is_analog(rec: &TrRecorder) -> bool;
        fn recorder_is_conventional(rec: &TrRecorder) -> bool;
        fn recorder_is_active(rec: &TrRecorder) -> bool;
        fn recorder_is_idle(rec: &TrRecorder) -> bool;
        fn recorder_get_current_length(rec: &TrRecorder) -> f64;

        // ── Opaque lifecycle type ──
        type TrContext;

        // ── Lifecycle functions ──
        fn tr_create_from_builder(plugin: Box<RustPluginBox>) -> Result<UniquePtr<TrContext>>;
        fn tr_finalize(ctx: Pin<&mut TrContext>) -> Result<()>;

        // ── Config builder functions ──
        fn tr_config_set_temp_dir(ctx: Pin<&mut TrContext>, dir: &str);
        fn tr_config_set_call_timeout(ctx: Pin<&mut TrContext>, seconds: f64);
        fn tr_config_set_control_warn_rate(ctx: Pin<&mut TrContext>, rate: i32);
        fn tr_config_set_control_retune_limit(ctx: Pin<&mut TrContext>, limit: i32);
        fn tr_config_set_broadcast_signals(ctx: Pin<&mut TrContext>, enabled: bool);
        fn tr_config_set_soft_vocoder(ctx: Pin<&mut TrContext>, enabled: bool);
        fn tr_config_set_record_uu_v_calls(ctx: Pin<&mut TrContext>, enabled: bool);
        fn tr_config_set_new_call_from_update(ctx: Pin<&mut TrContext>, enabled: bool);
        fn tr_config_set_frequency_format(ctx: Pin<&mut TrContext>, format: i32);
        fn tr_config_set_default_mode(ctx: Pin<&mut TrContext>, mode: &str);
        fn tr_config_set_debug_recorder(ctx: Pin<&mut TrContext>, enabled: bool, address: &str, port: i32);

        // ── Source builder functions ──
        fn tr_add_source(ctx: Pin<&mut TrContext>, driver: &str, device: &str,
                         center: f64, rate: f64, error: f64) -> Result<i32>;
        fn tr_source_set_gain(ctx: Pin<&mut TrContext>, idx: i32, gain: f64);
        fn tr_source_set_gain_by_name(ctx: Pin<&mut TrContext>, idx: i32, name: &str, gain: f64);
        fn tr_source_set_antenna(ctx: Pin<&mut TrContext>, idx: i32, antenna: &str);
        fn tr_source_set_freq_corr(ctx: Pin<&mut TrContext>, idx: i32, ppm: f64);
        fn tr_source_set_gain_mode(ctx: Pin<&mut TrContext>, idx: i32, agc: bool);
        fn tr_source_set_silence_frames(ctx: Pin<&mut TrContext>, idx: i32, frames: i32);
        fn tr_source_set_signal_detector_threshold(ctx: Pin<&mut TrContext>, idx: i32, threshold: f32);
        fn tr_source_create_digital_recorders(ctx: Pin<&mut TrContext>, idx: i32, count: i32);
        fn tr_source_create_analog_recorders(ctx: Pin<&mut TrContext>, idx: i32, count: i32);
        fn tr_source_create_sigmf_recorders(ctx: Pin<&mut TrContext>, idx: i32, count: i32);

        // ── System builder functions ──
        fn tr_add_system(ctx: Pin<&mut TrContext>, short_name: &str, system_type: &str) -> Result<i32>;
        fn tr_system_add_control_channel(ctx: Pin<&mut TrContext>, sys_idx: i32, freq: f64);
        fn tr_system_add_channel(ctx: Pin<&mut TrContext>, sys_idx: i32, freq: f64);
        fn tr_system_set_qpsk_mod(ctx: Pin<&mut TrContext>, sys_idx: i32, qpsk: bool);
        fn tr_system_set_squelch_db(ctx: Pin<&mut TrContext>, sys_idx: i32, db: f64);
        fn tr_system_set_analog_levels(ctx: Pin<&mut TrContext>, sys_idx: i32, level: f64);
        fn tr_system_set_digital_levels(ctx: Pin<&mut TrContext>, sys_idx: i32, level: f64);
        fn tr_system_set_tau(ctx: Pin<&mut TrContext>, sys_idx: i32, tau: f32);
        fn tr_system_set_max_dev(ctx: Pin<&mut TrContext>, sys_idx: i32, max_dev: i32);
        fn tr_system_set_filter_width(ctx: Pin<&mut TrContext>, sys_idx: i32, width: f64);
        fn tr_system_set_conversation_mode(ctx: Pin<&mut TrContext>, sys_idx: i32, mode: bool);
        fn tr_system_set_record_unknown(ctx: Pin<&mut TrContext>, sys_idx: i32, record: bool);
        fn tr_system_set_min_duration(ctx: Pin<&mut TrContext>, sys_idx: i32, duration: f64);
        fn tr_system_set_max_duration(ctx: Pin<&mut TrContext>, sys_idx: i32, duration: f64);
        fn tr_system_set_min_tx_duration(ctx: Pin<&mut TrContext>, sys_idx: i32, duration: f64);
        fn tr_system_set_hide_encrypted(ctx: Pin<&mut TrContext>, sys_idx: i32, hide: bool);
        fn tr_system_set_monitor_encrypted(ctx: Pin<&mut TrContext>, sys_idx: i32, monitor: bool);
        fn tr_system_set_hide_unknown(ctx: Pin<&mut TrContext>, sys_idx: i32, hide: bool);
        fn tr_system_set_mdc_enabled(ctx: Pin<&mut TrContext>, sys_idx: i32, enabled: bool);
        fn tr_system_set_fsync_enabled(ctx: Pin<&mut TrContext>, sys_idx: i32, enabled: bool);
        fn tr_system_set_star_enabled(ctx: Pin<&mut TrContext>, sys_idx: i32, enabled: bool);
        fn tr_system_set_tps_enabled(ctx: Pin<&mut TrContext>, sys_idx: i32, enabled: bool);
        fn tr_system_set_bandplan(ctx: Pin<&mut TrContext>, sys_idx: i32, bandplan: &str);
        fn tr_system_set_bandfreq(ctx: Pin<&mut TrContext>, sys_idx: i32, freq: i32);
        fn tr_system_set_bandplan_base(ctx: Pin<&mut TrContext>, sys_idx: i32, base: f64);
        fn tr_system_set_bandplan_high(ctx: Pin<&mut TrContext>, sys_idx: i32, high: f64);
        fn tr_system_set_bandplan_spacing(ctx: Pin<&mut TrContext>, sys_idx: i32, spacing: f64);
        fn tr_system_set_bandplan_offset(ctx: Pin<&mut TrContext>, sys_idx: i32, offset: i32);
        fn tr_system_set_xor_mask(ctx: Pin<&mut TrContext>, sys_idx: i32,
                                  sys_id: u64, wacn: u64, nac: u64);
        fn tr_system_set_multi_site(ctx: Pin<&mut TrContext>, sys_idx: i32, multi_site: bool);
        fn tr_system_set_multi_site_name(ctx: Pin<&mut TrContext>, sys_idx: i32, name: &str);
        fn tr_system_set_multi_site_number(ctx: Pin<&mut TrContext>, sys_idx: i32, number: u64);

        // ── Talkgroup/Channel builder functions ──
        fn tr_system_add_talkgroup(ctx: Pin<&mut TrContext>, sys_idx: i32,
                                   id: i64, mode: &str, priority: i32, preferred_nac: u64);
        fn tr_system_add_conventional_channel(ctx: Pin<&mut TrContext>, sys_idx: i32,
                                              tg_number: i64, freq: f64, tone: f64,
                                              squelch_db: f64, signal_detection: bool);

        // ── Unit tag builder functions ──
        fn tr_system_add_unit_tag(ctx: Pin<&mut TrContext>, sys_idx: i32,
                                  pattern: &str, tag: &str);
        fn tr_system_set_unit_tags_mode(ctx: Pin<&mut TrContext>, sys_idx: i32, mode: &str);

        // ── Phase A: Loop Control ──
        fn tr_start_gnuradio(ctx: Pin<&mut TrContext>);
        fn tr_stop_gnuradio(ctx: Pin<&mut TrContext>);

        // ── Phase B: Message Draining ──
        fn tr_context_drain_messages(ctx: Pin<&mut TrContext>) -> Vec<TrTrunkMessage>;
        fn tr_context_process_conventional_queues(ctx: Pin<&mut TrContext>);
        fn tr_context_check_conventional_detection(ctx: Pin<&mut TrContext>);
        fn tr_context_check_sources(ctx: Pin<&mut TrContext>) -> Result<()>;
        fn tr_context_print_status(ctx: Pin<&mut TrContext>, calls: &[*const TrCall]);

        // ── Phase B: Message Routing (called from Rust handle_message) ──
        fn tr_system_add_control_channel_runtime(ctx: Pin<&mut TrContext>, sys_num: i32, freq: f64);
        fn tr_system_retune(ctx: Pin<&mut TrContext>, sys_num: i32);
        fn tr_system_update_sysid(ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) -> bool;
        fn tr_system_update_status(ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) -> bool;
        fn tr_context_fire_unit_event(ctx: Pin<&mut TrContext>, info: &TrSystemInfo,
            event_type: TrUnitEventType, source_id: i64, talkgroup: i64);
        fn tr_context_fire_setup_system(ctx: Pin<&mut TrContext>, info: &TrSystemInfo);

        // ── Phase B: Decode Rate Checking (called from Rust check_decode_rates) ──
        fn tr_system_control_channel_count(ctx: Pin<&mut TrContext>, sys_num: i32) -> i32;
        fn tr_system_get_current_control_channel(ctx: Pin<&mut TrContext>, sys_num: i32) -> f64;
        fn tr_context_fire_trunk_message(ctx: Pin<&mut TrContext>, messages: &[TrTrunkMessage], info: &TrSystemInfo);
        fn tr_context_fire_setup_config(ctx: Pin<&mut TrContext>, infos: &[TrSystemInfo]);
        fn tr_context_fire_system_rates(ctx: Pin<&mut TrContext>, infos: &[TrSystemInfo], time_diff: f32);

        // ── Conventional call management — recorder status (null-guarded) ──
        fn call_has_recorder(call: &TrCall) -> bool;
        fn call_get_recorder_is_active(call: &TrCall) -> bool;

        // ── Conventional call management — signal/noise/system info removed (Rust owns) ──

        // ── Conventional call management — lifecycle ──
        fn call_restart(call: Pin<&mut TrCall>);
        fn call_start_initial_recording(call: Pin<&mut TrCall>);

        // ── Conventional call management — squelch event drain ──
        fn call_drain_squelch_events(call: &TrCall) -> Vec<TrSquelchEvent>;

        // ── Conventional call management — context-level ──
        fn tr_context_take_next_conventional_call(ctx: Pin<&mut TrContext>) -> UniquePtr<TrCall>;

        // ── Phase C: Call Management — additional accessors (most removed — Rust owns via CallEntry) ──
        fn call_system_get_preferred_nac(call: &TrCall, talkgroup: i64) -> u64;

        // ── Phase C: Call Management — additional recorder accessors ──
        fn recorder_since_last_write_secs(rec: &TrRecorder) -> f64;
        fn recorder_get_type(rec: &TrRecorder) -> i32;

        // ── Phase C: Call Management — lifecycle operations ──
        fn tr_context_create_call(ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) -> UniquePtr<TrCall>;
        fn call_get_metadata(call: &TrCall) -> TrCallMetadata;

        // ── Phase C: Call Management — recording policy support ──
        fn tr_context_source_count(ctx: &TrContext) -> i32;
        fn tr_source_covers_freq(ctx: &TrContext, source_idx: i32, freq: f64) -> bool;
        fn tr_source_available_recorders(ctx: &TrContext, source_idx: i32, is_analog: bool) -> i32;
        fn tr_source_start_recording(ctx: Pin<&mut TrContext>, source_idx: i32, call: Pin<&mut TrCall>, is_analog: bool) -> bool;

        // ── Phase C: Call Management — call-level recorder accessors ──
        fn call_recorder_since_last_write_secs(call: &TrCall) -> f64;
        fn call_process_recorder_queue(call: &TrCall);

        // ── Phase C: Call Management — system lookup by sys_num ──
        fn tr_context_get_system_multi_site_name(ctx: &TrContext, sys_num: i32) -> String;

        // ── Phase C: Call Management — conclude recording (recorder mechanics) ──
        fn call_stop_recording(call: Pin<&mut TrCall>) -> TrStopRecordingResult;
        fn call_get_transmission_list(call: &TrCall) -> Vec<TrTransmission>;
        fn call_get_dmr_transmission_list(call: &TrCall, slot: i32) -> Vec<TrTransmission>;
        fn call_get_recorder_num(call: &TrCall) -> i32;

        // ── Phase C: Call Management — update source tracking ──
        fn call_recorder_set_source(call: Pin<&mut TrCall>, src: i64);

        // ── Phase C: Call Management — event firing ──
        fn tr_context_fire_call_start(ctx: Pin<&mut TrContext>, info: &TrCallInfo);
        fn tr_context_fire_calls_active(ctx: Pin<&mut TrContext>, infos: &[TrCallInfo]);
        fn tr_context_fire_setup_recorder(ctx: Pin<&mut TrContext>, call: &TrCall);
        fn tr_context_fire_signal(ctx: Pin<&mut TrContext>, src: i64, info: &TrCallInfo);
        fn tr_context_fire_call_end(ctx: Pin<&mut TrContext>, data: &TrCallData);
        fn tr_recorder_process_queue(rec: &TrRecorder);
    }

    // ── Rust functions callable from C++ (plugin callbacks) ──

    extern "Rust" {
        type RustPluginBox;

        // Takes ownership of a Box<RustPluginBox> to drop it. This also
        // forces cxx to generate the Box<RustPluginBox> C++ template
        // instantiation needed by bridge.cpp.
        fn drop_rust_plugin_box(plugin: Box<RustPluginBox>);

        fn rust_plugin_start(plugin: &mut RustPluginBox) -> i32;
        fn rust_plugin_stop(plugin: &mut RustPluginBox) -> i32;
        fn rust_plugin_poll_one(plugin: &mut RustPluginBox) -> i32;
        fn rust_plugin_signal(
            plugin: &mut RustPluginBox,
            unit_id: i64,
            info: &TrCallInfo,
        ) -> i32;
        unsafe fn rust_plugin_audio_stream(
            plugin: &mut RustPluginBox,
            call: *const TrCall,
            recorder: &TrRecorder,
            samples: &[i16],
        ) -> i32;
        fn rust_plugin_call_start(plugin: &mut RustPluginBox, info: &TrCallInfo) -> i32;
        fn rust_plugin_call_end(plugin: &mut RustPluginBox, call_data: &TrCallData) -> i32;
        fn rust_plugin_calls_active(plugin: &mut RustPluginBox, infos: &[TrCallInfo]) -> i32;
        fn rust_plugin_setup_recorder(plugin: &mut RustPluginBox, recorder: &TrRecorder) -> i32;
        fn rust_plugin_setup_system(plugin: &mut RustPluginBox, info: &TrSystemInfo) -> i32;
        fn rust_plugin_setup_sources(plugin: &mut RustPluginBox) -> i32;
        fn rust_plugin_setup_config(
            plugin: &mut RustPluginBox,
            infos: &[TrSystemInfo],
        ) -> i32;
        fn rust_plugin_trunk_message(
            plugin: &mut RustPluginBox,
            messages: &[TrTrunkMessage],
            info: &TrSystemInfo,
        ) -> i32;
        fn rust_plugin_system_rates(
            plugin: &mut RustPluginBox,
            infos: &[TrSystemInfo],
            time_diff: f32,
        ) -> i32;
        fn rust_plugin_unit_event(
            plugin: &mut RustPluginBox,
            info: &TrSystemInfo,
            event_type: TrUnitEventType,
            source_id: i64,
            talkgroup: i64,
        ) -> i32;
        fn rust_plugin_unit_alias_discovered(
            plugin: &mut RustPluginBox,
            system: &TrSystem,
            radio_id: i64,
            alias: &str,
            source: &str,
            talkgroup_id: i64,
        ) -> i32;
    }
}

/// Opaque type that holds the Rust plugin implementation.
/// The C++ bridge holds a `Box<RustPluginBox>` via cxx.
pub struct RustPluginBox {
    inner: Box<dyn RustPluginCallbacks>,
}

impl RustPluginBox {
    pub fn new(plugin: Box<dyn RustPluginCallbacks>) -> Self {
        Self { inner: plugin }
    }
}

fn drop_rust_plugin_box(_plugin: Box<RustPluginBox>) {
    // Box is dropped automatically
}

/// Trait that downstream crates implement to receive plugin callbacks.
pub trait RustPluginCallbacks: Send {
    fn start(&mut self) -> i32;
    fn stop(&mut self) -> i32;
    fn poll_one(&mut self) -> i32;
    fn signal(&mut self, unit_id: i64, info: &ffi::TrCallInfo) -> i32;
    fn audio_stream(
        &mut self,
        call: Option<&ffi::TrCall>,
        recorder: &ffi::TrRecorder,
        samples: &[i16],
    ) -> i32;
    fn call_start(&mut self, info: &ffi::TrCallInfo) -> i32;
    fn call_end(&mut self, call_data: &ffi::TrCallData) -> i32;
    fn calls_active(&mut self, infos: &[ffi::TrCallInfo]) -> i32;
    fn setup_recorder(&mut self, recorder: &ffi::TrRecorder) -> i32;
    fn setup_system(&mut self, info: &ffi::TrSystemInfo) -> i32;
    fn setup_sources(&mut self) -> i32;
    fn setup_config(&mut self, infos: &[ffi::TrSystemInfo]) -> i32;
    fn trunk_message(&mut self, messages: &[ffi::TrTrunkMessage], info: &ffi::TrSystemInfo) -> i32;
    fn system_rates(&mut self, infos: &[ffi::TrSystemInfo], time_diff: f32) -> i32;
    fn unit_event(
        &mut self,
        info: &ffi::TrSystemInfo,
        event_type: ffi::TrUnitEventType,
        source_id: i64,
        talkgroup: i64,
    ) -> i32;
    fn unit_alias_discovered(
        &mut self,
        system: &ffi::TrSystem,
        radio_id: i64,
        alias: &str,
        source: &str,
        talkgroup_id: i64,
    ) -> i32;
}

// ── Trampoline functions from C++ → Rust via cxx ──

fn rust_plugin_start(plugin: &mut RustPluginBox) -> i32 {
    plugin.inner.start()
}

fn rust_plugin_stop(plugin: &mut RustPluginBox) -> i32 {
    plugin.inner.stop()
}

fn rust_plugin_poll_one(plugin: &mut RustPluginBox) -> i32 {
    plugin.inner.poll_one()
}

/// Convert a nullable opaque pointer to `Option<&T>`.
///
/// # Safety
/// The pointer must be valid for the lifetime of the returned reference,
/// or null.
unsafe fn nullable<T>(ptr: *const T) -> Option<&'static T> {
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &*ptr })
    }
}

fn rust_plugin_signal(
    plugin: &mut RustPluginBox,
    unit_id: i64,
    info: &ffi::TrCallInfo,
) -> i32 {
    plugin.inner.signal(unit_id, info)
}

unsafe fn rust_plugin_audio_stream(
    plugin: &mut RustPluginBox,
    call: *const ffi::TrCall,
    recorder: &ffi::TrRecorder,
    samples: &[i16],
) -> i32 {
    let call = unsafe { nullable(call) };
    plugin.inner.audio_stream(call, recorder, samples)
}

fn rust_plugin_call_start(plugin: &mut RustPluginBox, info: &ffi::TrCallInfo) -> i32 {
    plugin.inner.call_start(info)
}

fn rust_plugin_call_end(plugin: &mut RustPluginBox, call_data: &ffi::TrCallData) -> i32 {
    plugin.inner.call_end(call_data)
}

fn rust_plugin_calls_active(plugin: &mut RustPluginBox, infos: &[ffi::TrCallInfo]) -> i32 {
    plugin.inner.calls_active(infos)
}

fn rust_plugin_setup_recorder(plugin: &mut RustPluginBox, recorder: &ffi::TrRecorder) -> i32 {
    plugin.inner.setup_recorder(recorder)
}

fn rust_plugin_setup_system(plugin: &mut RustPluginBox, info: &ffi::TrSystemInfo) -> i32 {
    plugin.inner.setup_system(info)
}

fn rust_plugin_setup_sources(plugin: &mut RustPluginBox) -> i32 {
    plugin.inner.setup_sources()
}

fn rust_plugin_setup_config(
    plugin: &mut RustPluginBox,
    infos: &[ffi::TrSystemInfo],
) -> i32 {
    plugin.inner.setup_config(infos)
}

fn rust_plugin_trunk_message(
    plugin: &mut RustPluginBox,
    messages: &[ffi::TrTrunkMessage],
    info: &ffi::TrSystemInfo,
) -> i32 {
    plugin.inner.trunk_message(messages, info)
}

fn rust_plugin_system_rates(
    plugin: &mut RustPluginBox,
    infos: &[ffi::TrSystemInfo],
    time_diff: f32,
) -> i32 {
    plugin.inner.system_rates(infos, time_diff)
}

fn rust_plugin_unit_event(
    plugin: &mut RustPluginBox,
    info: &ffi::TrSystemInfo,
    event_type: ffi::TrUnitEventType,
    source_id: i64,
    talkgroup: i64,
) -> i32 {
    plugin.inner.unit_event(info, event_type, source_id, talkgroup)
}

fn rust_plugin_unit_alias_discovered(
    plugin: &mut RustPluginBox,
    system: &ffi::TrSystem,
    radio_id: i64,
    alias: &str,
    source: &str,
    talkgroup_id: i64,
) -> i32 {
    plugin
        .inner
        .unit_alias_discovered(system, radio_id, alias, source, talkgroup_id)
}
