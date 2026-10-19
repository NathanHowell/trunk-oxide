#![allow(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

pub mod p25;
pub mod p25_parse;
pub mod p25_voice;
pub mod p25_calls;

// cxx derives `Clone` field by field for shared structs.
#[allow(clippy::clone_on_copy)]
#[cxx::bridge(namespace = "trunk_oxide")]
pub mod ffi {
    // ── Shared enums (mirrored in both C++ and Rust) ──

    /// Decoder a shared-channelizer slot runs on its conditioned output.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TrControlDecoder {
        None = 0,
        P25Cqpsk = 1,
        P25C4fm = 2,
    }

    /// A control-channel decoder for a shared-channelizer port.
    struct TrControlDecoderSpec {
        decoder: TrControlDecoder,
        /// The port's conditioned output rate (S/s).
        rate: f64,
        /// System short name and control channel frequency, for metrics.
        system: String,
        freq_hz: f64,
        /// The system its trunking messages are for.
        sys_num: i32,
        /// Non-empty: write every message to `<tap_base>.msgs.jsonl`.
        tap_base: String,
    }

    /// What a voice tag says (`TrVoiceTag::kind`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TrVoiceTagKind {
        /// The unit talking (`value`).
        Source = 0,
        /// The talkgroup (`value`).
        Group = 1,
        /// The transmission ended.
        Terminate = 2,
        /// An op25-style JSON message (`json`): a talker alias.
        Json = 3,
    }

    /// Something said in band, at a sample of the decoded audio.
    struct TrVoiceTag {
        /// Into the audio this call to `process` appended.
        offset: u32,
        kind: TrVoiceTagKind,
        value: i64,
        json: String,
    }

    /// A call's voice decode errors (bits corrected) and op25-style spikes.
    struct TrCallCounts {
        errors: u64,
        spikes: u64,
        /// The ALGID confirmed on the call's own slot (0x80: clear).
        algid: u8,
    }

    /// A P25 voice call's outcome, when it ends.
    struct TrCallReport {
        system: String,
        recorder: i32,
        talkgroup: i64,
        freq: f64,
        phase2: bool,
        audio_seconds: f64,
        transmissions: i64,
        errors: i64,
        spikes: i64,
        /// Source units named in band, in order of first appearance.
        sources: Vec<i64>,
        /// The ALGID confirmed on the call (0x80: clear).
        algid: u8,
    }

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

    #[derive(Debug, Clone, PartialEq)]
    struct TrPatchData {
        pub sg: u64,
        pub ga1: u64,
        pub ga2: u64,
        pub ga3: u64,
    }

    #[derive(Debug, Clone, PartialEq)]
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
        pub term_events: i64,
        pub after_term_samples: i64,
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

    // ── GR block performance metrics ──

    #[derive(Debug, Clone)]
    struct TrGrBlockMetrics {
        pub block_name: String,
        pub block_alias: String,
        pub items_read: u64,
        pub items_written: u64,
        pub work_time_total: f32,
        pub input_buf_full: f32,
        pub output_buf_full: f32,
    }

    #[derive(Debug, Clone)]
    struct TrRecorderMetrics {
        pub recorder_id: i32,
        pub recorder_type: String,
        pub source_id: i32,
        pub blocks: Vec<TrGrBlockMetrics>,
    }

    #[derive(Debug, Clone)]
    struct TrSourceMetrics {
        pub source_id: i32,
        pub center_freq: f64,
        pub sample_rate: f64,
        pub blocks: Vec<TrGrBlockMetrics>,
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
        /// `iqfile` driver only: file format, "cf32" or "cu8".
        pub iq_format: String,
        /// `iqfile` driver only: loop the file (false = play once then stop).
        pub iq_repeat: bool,
    }

    /// Minimal device parameters needed to open a source standalone for
    /// pre-flight gain calibration (a subset of `TrSourceConfig`).
    struct TrProbeConfig {
        pub driver: String,
        pub device: String,
        pub center: f64,
        pub rate: f64,
        pub error: f64,
        pub ppm: f64,
        pub antenna: String,
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
        pub analog_levels: f64,
        pub tau: f64,
        pub max_dev: i32,
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
        fn tr_system_short_name(sys: &TrSystem) -> String;

        // ── Opaque lifecycle type ──
        type TrContext;

        // ── Pre-flight gain calibration probe (standalone device access) ──
        type TrProbe;
        fn tr_probe_open(cfg: &TrProbeConfig) -> Result<UniquePtr<TrProbe>>;
        fn tr_probe_gain_steps(probe: &TrProbe) -> Vec<f64>;
        fn tr_probe_capture(
            probe: Pin<&mut TrProbe>,
            gain_db: f64,
            n_samples: usize,
            settle_samples: usize,
        ) -> Result<Vec<f32>>;

        // ── Lifecycle functions ──
        fn tr_create_from_builder(handler: Box<EventHandlerBox>) -> Result<UniquePtr<TrContext>>;
        fn tr_finalize(ctx: Pin<&mut TrContext>) -> Result<()>;

        // ── Config builder functions ──
        fn tr_config_set_temp_dir(ctx: Pin<&mut TrContext>, dir: &str);
        fn tr_config_set_tap_dir(ctx: Pin<&mut TrContext>, dir: &str);
        fn tr_config_set_soft_vocoder(ctx: Pin<&mut TrContext>, enabled: bool);

        /// op25's P25 vocoders, one 20 ms frame (160 samples) at a time, as
        /// the reference trunk-p25's are checked against.
        type Op25VocoderReference;
        fn tr_op25_vocoder_reference_new() -> UniquePtr<Op25VocoderReference>;
        /// One IMBE frame: op25's `u[0..8]` and the bits corrected in c0 and
        /// in all.
        fn tr_op25_vocoder_reference_phase1(
            vocoder: Pin<&mut Op25VocoderReference>,
            u: &[u32],
            e0: u32,
            et: u32,
            out: &mut [i16],
        );
        /// One AMBE+2 frame: its 36 descrambled dibits. False if muted.
        fn tr_op25_vocoder_reference_phase2(
            vocoder: Pin<&mut Op25VocoderReference>,
            dibits: &[u8],
            encrypted: bool,
            out: &mut [i16],
        ) -> bool;

        /// op25's voice frame assembler driven with dibits, as the reference
        /// trunk-p25's call assembler is checked against.
        type Op25VoiceReference;
        fn tr_op25_voice_reference_new(
            phase2: bool,
            slot: i32,
            xor_mask: &[u8],
        ) -> UniquePtr<Op25VoiceReference>;
        fn tr_op25_voice_reference_push(reference: Pin<&mut Op25VoiceReference>, dibits: &[u8]);
        fn tr_op25_voice_reference_pcm(reference: &Op25VoiceReference) -> Vec<i16>;
        fn tr_op25_voice_reference_sources(reference: &Op25VoiceReference) -> Vec<i64>;
        fn tr_op25_voice_reference_terminators(reference: &Op25VoiceReference) -> Vec<i64>;
        fn tr_op25_voice_reference_json(reference: &Op25VoiceReference) -> Vec<String>;
        /// Every frame's vocoder input as records: phase 1 {u0..u7, E0,
        /// ET, encrypted}, phase 2 {36 dibits, encrypted}.
        fn tr_op25_voice_reference_inputs(reference: &Op25VoiceReference) -> Vec<u32>;
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
        fn call_restart(call: Pin<&mut TrCall>, cfg: &TrRecorderConfig);
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
        /// Like `since_last_write`, but only counts blocks with non-silent audio.
        fn call_recorder_since_last_voice_secs(call: &TrCall) -> f64;

        // ── Phase C: Call Management — conclude recording (recorder mechanics) ──
        fn call_stop_recording(call: Pin<&mut TrCall>) -> TrStopRecordingResult;
        fn call_get_transmission_list(call: &TrCall) -> Vec<TrTransmission>;
        fn call_get_dmr_transmission_list(call: &TrCall, slot: i32) -> Vec<TrTransmission>;
        fn call_get_recorder_num(call: &TrCall) -> i32;

        // ── Phase C: Call Management — update source tracking ──
        fn call_recorder_set_source(call: Pin<&mut TrCall>, src: i64);

        // ── Adaptive squelch ──
        fn call_set_squelch_db(call: Pin<&mut TrCall>, db: f64);
        fn call_get_pwr_db(call: &TrCall) -> f64;

        // ── Event channel wiring ──
        fn tr_context_set_event_notifier(ctx: Pin<&mut TrContext>, notifier: Box<EventNotifier>);
        fn tr_context_wire_event_notifiers(ctx: Pin<&mut TrContext>);

        // ── GR block performance metrics ──
        fn tr_collect_recorder_metrics(ctx: &TrContext) -> Vec<TrRecorderMetrics>;
        fn tr_collect_source_metrics(ctx: &TrContext) -> Vec<TrSourceMetrics>;
    }

    // ── Rust functions callable from C++ (event handler callbacks) ──

    extern "Rust" {
        type EventHandlerBox;

        // Forces cxx to generate the Box<EventHandlerBox> C++ template instantiation.
        fn drop_event_handler_box(handler: Box<EventHandlerBox>);

        fn event_handler_audio_stream(
            handler: &EventHandlerBox,
            rust_call_id: u64,
            samples: &[i16],
        );
        fn event_handler_unit_alias_discovered(
            handler: &EventHandlerBox,
            system: &TrSystem,
            radio_id: i64,
            alias: &str,
            source: &str,
            talkgroup_id: i64,
        );
    }

    // ── Shared FFT channelizer engine (called from the GR thread / control threads) ──

    // ── P25 voice observer (every implementation, for metrics and shadow mode) ──

    extern "Rust" {
        /// A P25 voice call ended.
        fn p25_observe_call(report: &TrCallReport);
    }

    // ── trunk-p25 voice decoder, phase 1 and 2, with op25's vocoders ──

    extern "Rust" {
        type P25VoiceHandle;

        fn p25_voice_new(rate: f64, recorder: i32) -> Box<P25VoiceHandle>;
        fn start_call(
            self: &mut P25VoiceHandle,
            qpsk: bool,
            tdma: bool,
            xor_mask: &[u8],
            system: &str,
            slot: i32,
        );
        /// `iq`: interleaved IQ. Appends the decoded audio (8 kHz) and the
        /// tags at offsets into what was appended.
        fn process(
            self: &mut P25VoiceHandle,
            iq: &[f32],
            audio: &mut Vec<i16>,
            tags: &mut Vec<TrVoiceTag>,
        );
        /// Encryption confirmed on the call's own slot since the last call
        /// (0: none; else `1 << 24 | algid << 16 | keyid`).
        fn take_crypto(self: &mut P25VoiceHandle) -> u32;
        fn take_call_counts(self: &mut P25VoiceHandle) -> TrCallCounts;
    }

    extern "Rust" {
        type SharedChannelizerHandle;

        fn shared_channelizer_new(
            sample_rate: f64,
            source: i32,
        ) -> Result<Box<SharedChannelizerHandle>>;
        fn hop(self: &SharedChannelizerHandle) -> usize;
        /// Number of channel profiles (0 = narrow digital, 1 = wide analog;
        /// the wide one is absent when the source rate is too low).
        fn profile_count(self: &SharedChannelizerHandle) -> u32;
        fn out_per_block(self: &SharedChannelizerHandle, profile: u32) -> usize;
        fn max_out_per_block(self: &SharedChannelizerHandle) -> usize;
        fn out_rate(self: &SharedChannelizerHandle, profile: u32) -> f64;
        /// Enable `port` as a `profile` channel at `offset_hz`, or retune it.
        fn set_channel(self: &SharedChannelizerHandle, port: u32, profile: u32, offset_hz: f64);
        /// Give `port` its own filter; persists while the port is disabled.
        fn set_filter(
            self: &SharedChannelizerHandle,
            port: u32,
            cutoff_hz: f64,
            transition_hz: f64,
        );
        fn clear_channel(self: &SharedChannelizerHandle, port: u32);
        /// Condition the port's output: resample `in_rate` -> `out_rate` if
        /// lower, squelch (`squelch_mode` 0 none, 1 analog, 2 digital), AGC,
        /// FLL. Calling again changes `sps` and the squelch mode.
        fn set_conditioning(
            self: &SharedChannelizerHandle,
            port: u32,
            sps: u32,
            excess_bw: f64,
            in_rate: f64,
            out_rate: f64,
            squelch_mode: u32,
        );
        fn set_squelch_db(self: &SharedChannelizerHandle, port: u32, db: f64);
        fn squelch_open(self: &SharedChannelizerHandle, port: u32) -> bool;
        /// Pop the oldest squelch transition: 0 none, 1 opened, 2 closed
        /// (`pwr_db` = squelch power at the transition).
        fn next_squelch_event(self: &SharedChannelizerHandle, port: u32, pwr_db: &mut f64) -> i32;
        fn freq_error_hz(self: &SharedChannelizerHandle, port: u32) -> i32;
        /// NaN until the port has carried signal.
        fn power_db(self: &SharedChannelizerHandle, port: u32) -> f64;
        /// Run (or with `None`, stop) a control-channel decoder on the port's
        /// conditioned output; it parses what it decodes too.
        fn set_control_decoder(
            self: &SharedChannelizerHandle,
            port: u32,
            spec: &TrControlDecoderSpec,
        );
        /// Where the control decoders' trunking messages go. Until it is
        /// set they wait in the decoders' queues.
        fn set_event_notifier(self: &SharedChannelizerHandle, notifier: Box<EventNotifier>);
        /// `input`: `hop` interleaved IQ samples. `output`: port-major,
        /// `max_out_per_block` IQ samples of stride per port. `produced[p]` =
        /// samples written for port p (0 = port inactive).
        fn process(
            self: &SharedChannelizerHandle,
            input: &[f32],
            output: &mut [f32],
            produced: &mut [u32],
        );
    }

    // ── Rust logging bridge (Boost.Log → tracing) ──

    extern "Rust" {
        fn rust_tracing_event(level: u8, message: &str);
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
        fn event_notifier_notify_source_change(
            notifier: &EventNotifier,
            rust_call_id: u64,
            src_id: i64,
        );
        fn event_notifier_notify_crypto(
            notifier: &EventNotifier,
            rust_call_id: u64,
            alg_id: u32,
            key_id: u32,
        );
        fn event_notifier_notify_signals_detected(
            notifier: &EventNotifier,
            source_num: i32,
            source_center_freq: f64,
            signals: Vec<TrDetectedSignal>,
        );
    }
}

// ── Signal/noise sentinel encoding ──

/// dB value used on the FFI wire to mean "no measurement".
///
/// cxx shared structs (`TrCallData`/`TrCallInfo`) carry `signal`/`noise` as
/// plain `f64`, so absence is encoded as a sentinel rather than `Option`. This
/// is the threshold: any value at or above it decodes to `None`. It also
/// catches trunk-recorder's C++ `DB_UNSET` (999) returned by `get_pwr()` when
/// the power probe has no level.
pub const NO_SIGNAL_DB: f64 = 998.0;

/// Encode an optional dB reading for the FFI wire (`None` → [`NO_SIGNAL_DB`]).
#[must_use]
pub fn signal_to_ffi(value: Option<f64>) -> f64 {
    value.unwrap_or(NO_SIGNAL_DB)
}

/// Decode a dB reading off the FFI wire, mapping the sentinel (and C++
/// `DB_UNSET`) back to `None`.
#[must_use]
pub fn signal_from_ffi(value: f64) -> Option<f64> {
    (value < NO_SIGNAL_DB).then_some(value)
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
    /// In-band P25 source (unit) change decoded by op25 on the voice channel,
    /// keyed by the recorder's `rust_call_id`. Drives per-speaker transmission
    /// boundaries in the state machine.
    SourceChange { rust_call_id: u64, src_id: i64 },
    /// In-band P25 encryption parameters (ESS `ALGID` / `KeyID`) decoded by
    /// op25 for an encrypted call we hold no key for, keyed by `rust_call_id`.
    Crypto {
        rust_call_id: u64,
        alg_id: u32,
        key_id: u32,
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

#[expect(
    clippy::unnecessary_box_returns,
    reason = "cxx requires Box<T> for opaque Rust types"
)]
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

fn event_notifier_notify_source_change(notifier: &EventNotifier, rust_call_id: u64, src_id: i64) {
    if notifier
        .0
        .try_send(GrEvent::SourceChange {
            rust_call_id,
            src_id,
        })
        .is_ok()
    {
        notifier.1.fetch_add(1, Ordering::Relaxed);
    }
}

fn event_notifier_notify_crypto(
    notifier: &EventNotifier,
    rust_call_id: u64,
    alg_id: u32,
    key_id: u32,
) {
    if notifier
        .0
        .try_send(GrEvent::Crypto {
            rust_call_id,
            alg_id,
            key_id,
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

/// Trait for C++-initiated callbacks that come through cxx trampolines.
/// All methods take `&self` — no mutex needed.
pub trait CxxCallbacks: Send + Sync {
    fn audio_stream(&self, rust_call_id: u64, samples: &[i16]);
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

// ── Shared FFT channelizer ──

use p25_voice::{P25VoiceHandle, p25_voice_new};

fn p25_observe_call(report: &ffi::TrCallReport) {
    p25_calls::observe_call(report);
}

/// Rust half of the C++ `channelizer_backend` (see bridge.cpp).
pub struct SharedChannelizerHandle {
    engine: Arc<trunk_channelizer::SharedChannelizer>,
    source: i32,
    /// Trunking side of each port's control-channel decoder.
    decoders: Mutex<HashMap<u32, p25::ControlPort>>,
    notifier: Mutex<Option<Box<EventNotifier>>>,
}

/// Live channelizers by source index, so the Rust side can read their
/// counters without a trip through C++. Weak: the GR block owns the engine.
static CHANNELIZERS: Mutex<Vec<(i32, Weak<trunk_channelizer::SharedChannelizer>)>> =
    Mutex::new(Vec::new());

fn shared_channelizer_new(
    sample_rate: f64,
    source: i32,
) -> Result<Box<SharedChannelizerHandle>, String> {
    let cfg = trunk_channelizer::Config::standard(sample_rate).map_err(|e| e.to_string())?;
    let settings = *CONDITIONING.lock().unwrap_or_else(PoisonError::into_inner);
    let inner = Arc::new(
        trunk_channelizer::SharedChannelizer::with_conditioning(cfg, settings)
            .map_err(|e| e.to_string())?,
    );
    let mut reg = CHANNELIZERS.lock().unwrap_or_else(PoisonError::into_inner);
    reg.retain(|(s, w)| *s != source && w.strong_count() > 0);
    reg.push((source, Arc::downgrade(&inner)));
    Ok(Box::new(SharedChannelizerHandle {
        engine: inner,
        source,
        decoders: Mutex::new(HashMap::new()),
        notifier: Mutex::new(None),
    }))
}

static CONDITIONING: Mutex<trunk_channelizer::ConditioningSettings> =
    Mutex::new(trunk_channelizer::ConditioningSettings {
        params: trunk_channelizer::ConditionerParams::DEFAULT,
        reset: trunk_channelizer::ResetPolicy::Never,
    });

/// Conditioner settings for shared channelizers created from now on (call
/// before the sources are built).
pub fn set_conditioning_settings(settings: trunk_channelizer::ConditioningSettings) {
    *CONDITIONING.lock().unwrap_or_else(PoisonError::into_inner) = settings;
}

pub use trunk_channelizer::Counters as ChannelizerCounters;
pub use trunk_channelizer::{ConditionerParams, ConditioningSettings, ResetPolicy};

/// One source's shared-channelizer counters (see [`channelizer_stats`]).
#[derive(Clone, Copy, Debug)]
pub struct ChannelizerStats {
    pub source: i32,
    pub counters: ChannelizerCounters,
    /// Real-time duration of one block; a block slower than this is an overrun.
    pub block_budget_ns: u64,
}

/// Counters for every live shared channelizer. `max_ns` is the longest block
/// since the previous call (it is reset by reading).
pub fn channelizer_stats() -> Vec<ChannelizerStats> {
    let reg = CHANNELIZERS.lock().unwrap_or_else(PoisonError::into_inner);
    reg.iter()
        .filter_map(|(source, w)| {
            let c = w.upgrade()?;
            Some(ChannelizerStats {
                source: *source,
                counters: c.take_counters(),
                block_budget_ns: c.block_budget_ns(),
            })
        })
        .collect()
}

impl SharedChannelizerHandle {
    fn hop(&self) -> usize {
        self.engine.config().hop()
    }

    fn profile_count(&self) -> u32 {
        u32::try_from(self.engine.config().profiles.len()).unwrap_or(u32::MAX)
    }

    fn out_per_block(&self, profile: u32) -> usize {
        self.engine.config().out_per_block(profile as usize)
    }

    fn max_out_per_block(&self) -> usize {
        self.engine.config().max_out_per_block()
    }

    fn out_rate(&self, profile: u32) -> f64 {
        self.engine.config().out_rate(profile as usize)
    }

    fn set_channel(&self, port: u32, profile: u32, offset_hz: f64) {
        self.engine
            .set_slot(port as usize, profile as usize, offset_hz);
    }

    fn set_filter(&self, port: u32, cutoff_hz: f64, transition_hz: f64) {
        self.engine
            .set_slot_filter(port as usize, cutoff_hz, transition_hz);
    }

    fn clear_channel(&self, port: u32) {
        self.engine.clear_slot(port as usize);
    }

    #[allow(clippy::cast_possible_truncation)]
    fn set_conditioning(
        &self,
        port: u32,
        sps: u32,
        excess_bw: f64,
        in_rate: f64,
        out_rate: f64,
        squelch_mode: u32,
    ) {
        let squelch = match squelch_mode {
            1 => Some(trunk_channelizer::SquelchMode::ANALOG),
            2 => Some(trunk_channelizer::SquelchMode::DIGITAL),
            _ => None,
        };
        self.engine.set_slot_conditioning(
            port as usize,
            trunk_channelizer::ConditioningSpec {
                sps,
                excess_bw: excess_bw as f32,
                in_rate,
                out_rate,
                squelch,
            },
        );
    }

    fn set_squelch_db(&self, port: u32, db: f64) {
        self.engine.set_slot_squelch_db(port as usize, db);
    }

    fn squelch_open(&self, port: u32) -> bool {
        self.engine.slot_squelch_open(port as usize)
    }

    fn next_squelch_event(&self, port: u32, pwr_db: &mut f64) -> i32 {
        match self.engine.next_squelch_event(port as usize) {
            Some(e) => {
                *pwr_db = e.pwr_db;
                if e.muted { 2 } else { 1 }
            }
            None => 0,
        }
    }

    fn freq_error_hz(&self, port: u32) -> i32 {
        self.engine.slot_freq_error_hz(port as usize)
    }

    fn power_db(&self, port: u32) -> f64 {
        self.engine.slot_power_db(port as usize)
    }

    fn process(&self, input: &[f32], output: &mut [f32], produced: &mut [u32]) {
        self.engine
            .process(as_complex(input), as_complex_mut(output), produced);
        // The decoders ran on this block; parse and send what they decoded.
        let notifier = self.notifier.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = notifier.as_deref() {
            let mut decoders = self.decoders.lock().unwrap_or_else(PoisonError::into_inner);
            for port in decoders.values_mut() {
                port.drain(&mut |m| event_notifier_notify_trunk_messages(n, m));
            }
        }
    }

    fn set_event_notifier(&self, notifier: Box<EventNotifier>) {
        *self.notifier.lock().unwrap_or_else(PoisonError::into_inner) = Some(notifier);
    }

    fn set_control_decoder(&self, port: u32, spec: &ffi::TrControlDecoderSpec) {
        let mut decoders = self.decoders.lock().unwrap_or_else(PoisonError::into_inner);
        let installed = trunk_p25::Modulation::try_from(spec.decoder)
            .ok()
            .map(|modulation| {
                let channel = p25::ControlChannel {
                    system: spec.system.clone(),
                    freq_hz: spec.freq_hz,
                    source: self.source,
                    port,
                };
                let (dsp, shared) = p25::attach(channel, modulation, spec.rate);
                decoders.insert(
                    port,
                    p25::ControlPort::new(shared, spec.sys_num, &spec.tap_base),
                );
                dsp
            });
        if installed.is_none() {
            decoders.remove(&port);
        }
        self.engine.set_slot_decoder(port as usize, installed);
    }
}

/// Reinterpret interleaved `[re, im, re, im, ..]` floats as complex samples.
fn as_complex(s: &[f32]) -> &[num_complex::Complex32] {
    assert!(
        s.len().is_multiple_of(2),
        "interleaved IQ must have even length"
    );
    // SAFETY: Complex<f32> is #[repr(C)] { re: f32, im: f32 }: size 8, align 4,
    // identical layout to two consecutive f32s, and the length is halved.
    unsafe { std::slice::from_raw_parts(s.as_ptr().cast(), s.len() / 2) }
}

fn as_complex_mut(s: &mut [f32]) -> &mut [num_complex::Complex32] {
    assert!(
        s.len().is_multiple_of(2),
        "interleaved IQ must have even length"
    );
    // SAFETY: as above; exclusive borrow carries over.
    unsafe { std::slice::from_raw_parts_mut(s.as_mut_ptr().cast(), s.len() / 2) }
}

// ── Boost.Log → tracing bridge ──
// Called from the custom Boost.Log sink backend in bridge.cpp.
// Boost trivial severity: 0=trace, 1=debug, 2=info, 3=warning, 4=error, 5=fatal

fn rust_tracing_event(level: u8, message: &str) {
    match level {
        0 => tracing::trace!(target: "trunk_recorder", "{message}"),
        1 => tracing::debug!(target: "trunk_recorder", "{message}"),
        2 => tracing::info!(target: "trunk_recorder", "{message}"),
        3 => tracing::warn!(target: "trunk_recorder", "{message}"),
        _ => tracing::error!(target: "trunk_recorder", "{message}"),
    }
}

// ── Trampoline functions for C++-initiated callbacks ──
// Only 2 remain: audio_stream, unit_alias_discovered.
// All other events are fired directly from Rust without FFI.

fn event_handler_audio_stream(handler: &EventHandlerBox, rust_call_id: u64, samples: &[i16]) {
    handler.inner.audio_stream(rust_call_id, samples);
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
