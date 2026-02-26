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
        Inactive = 2,
        Active = 3,
        Idle = 4,
        Stopped = 6,
        Available = 7,
        Ignore = 8,
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
        pub filename: String,
    }

    #[derive(Debug, Clone)]
    struct TrCallSource {
        pub source: i64,
        pub time: i64,
        pub position: f64,
        pub emergency: bool,
        pub signal_system: String,
        pub tag: String,
    }

    #[derive(Debug, Clone)]
    struct TrCallError {
        pub time: i64,
        pub position: f64,
        pub total_len: f64,
        pub error_count: f64,
        pub spike_count: f64,
    }

    #[derive(Debug, Clone)]
    struct TrCallData {
        pub talkgroup: i64,
        pub color_code: i64,
        pub patched_talkgroups: Vec<u64>,
        pub talkgroup_tag: String,
        pub talkgroup_alpha_tag: String,
        pub talkgroup_description: String,
        pub talkgroup_display: String,
        pub talkgroup_group: String,
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
        pub audio_archive: bool,
        pub transmission_archive: bool,
        pub call_log: bool,
        pub compress_wav: bool,
        pub filename: String,
        pub status_filename: String,
        pub converted: String,
        pub sys_num: i32,
        pub short_name: String,
        pub upload_script: String,
        pub audio_type: String,
        pub tdma_slot: i32,
        pub length: f64,
        pub phase2_tdma: bool,
        pub transmission_source_list: Vec<TrCallSource>,
        pub transmission_error_list: Vec<TrCallError>,
        pub transmission_list: Vec<TrTransmission>,
        pub status: TrCallDataStatus,
        pub process_call_time: i64,
        pub retry_attempt: i32,
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
        fn call_get_talkgroup(call: &TrCall) -> i64;
        fn call_get_call_num(call: &TrCall) -> i64;
        fn call_get_sys_num(call: &TrCall) -> i32;
        fn call_get_freq(call: &TrCall) -> f64;
        fn call_get_signal(call: &TrCall) -> f64;
        fn call_get_noise(call: &TrCall) -> f64;
        fn call_get_freq_error(call: &TrCall) -> i32;
        fn call_get_encrypted(call: &TrCall) -> bool;
        fn call_get_emergency(call: &TrCall) -> bool;
        fn call_get_is_analog(call: &TrCall) -> bool;
        fn call_is_conventional(call: &TrCall) -> bool;
        fn call_get_phase2_tdma(call: &TrCall) -> bool;
        fn call_get_tdma_slot(call: &TrCall) -> i32;
        fn call_get_priority(call: &TrCall) -> i32;
        fn call_get_mode(call: &TrCall) -> bool;
        fn call_get_duplex(call: &TrCall) -> bool;
        fn call_get_conversation_mode(call: &TrCall) -> bool;
        fn call_get_current_source_id(call: &TrCall) -> i64;
        fn call_get_current_length(call: &TrCall) -> f64;
        fn call_get_start_time(call: &TrCall) -> i64;
        fn call_elapsed(call: &TrCall) -> i64;
        fn call_get_short_name(call: &TrCall) -> String;
        fn call_get_talkgroup_display(call: &TrCall) -> String;
        fn call_get_talkgroup_tag(call: &TrCall) -> String;
        fn call_get_state(call: &TrCall) -> TrState;
        fn call_get_monitoring_state(call: &TrCall) -> TrMonitoringState;

        // ── System accessors ──
        fn system_get_sys_num(sys: &TrSystem) -> i32;
        fn system_get_sys_id(sys: &TrSystem) -> u64;
        fn system_get_short_name(sys: &TrSystem) -> String;
        fn system_get_system_type(sys: &TrSystem) -> String;
        fn system_get_wacn(sys: &TrSystem) -> u64;
        fn system_get_nac(sys: &TrSystem) -> u64;
        fn system_get_sys_rfss(sys: &TrSystem) -> i32;
        fn system_get_sys_site_id(sys: &TrSystem) -> i32;
        fn system_get_bandplan(sys: &TrSystem) -> String;
        fn system_get_analog_levels(sys: &TrSystem) -> f64;
        fn system_get_digital_levels(sys: &TrSystem) -> f64;
        fn system_get_squelch_db(sys: &TrSystem) -> f64;
        fn system_get_compress_wav(sys: &TrSystem) -> bool;
        fn system_get_conversation_mode(sys: &TrSystem) -> bool;
        fn system_get_message_count(sys: &TrSystem) -> i32;
        fn system_get_decode_rate(sys: &TrSystem) -> i32;

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
        fn tr_create(config_json: &str) -> Result<UniquePtr<TrContext>>;
        fn tr_start(ctx: Pin<&mut TrContext>);
        fn tr_stop(ctx: Pin<&mut TrContext>);
        fn tr_set_plugin(ctx: Pin<&mut TrContext>, plugin: Box<RustPluginBox>);
    }

    // ── Rust functions callable from C++ (plugin callbacks) ──

    extern "Rust" {
        type RustPluginBox;

        // Takes ownership of a Box<RustPluginBox> to drop it. This also
        // forces cxx to generate the Box<RustPluginBox> C++ template
        // instantiation needed by bridge.cpp.
        fn drop_rust_plugin_box(plugin: Box<RustPluginBox>);

        fn rust_plugin_parse_config(plugin: &mut RustPluginBox, json_str: &str) -> i32;
        fn rust_plugin_start(plugin: &mut RustPluginBox) -> i32;
        fn rust_plugin_stop(plugin: &mut RustPluginBox) -> i32;
        fn rust_plugin_poll_one(plugin: &mut RustPluginBox) -> i32;
        fn rust_plugin_signal(
            plugin: &mut RustPluginBox,
            unit_id: i64,
            signaling_type: &str,
            sig_type: TrSignalType,
            call: &TrCall,
            system: &TrSystem,
            recorder: &TrRecorder,
        ) -> i32;
        fn rust_plugin_audio_stream(
            plugin: &mut RustPluginBox,
            call: &TrCall,
            recorder: &TrRecorder,
            samples: &[i16],
        ) -> i32;
        fn rust_plugin_call_start(plugin: &mut RustPluginBox, call: &TrCall) -> i32;
        fn rust_plugin_call_end(plugin: &mut RustPluginBox, call_data: &TrCallData) -> i32;
        fn rust_plugin_calls_active(plugin: &mut RustPluginBox, calls: &[*const TrCall]) -> i32;
        fn rust_plugin_setup_recorder(plugin: &mut RustPluginBox, recorder: &TrRecorder) -> i32;
        fn rust_plugin_setup_system(plugin: &mut RustPluginBox, system: &TrSystem) -> i32;
        fn rust_plugin_setup_systems(
            plugin: &mut RustPluginBox,
            systems: &[*const TrSystem],
        ) -> i32;
        fn rust_plugin_setup_sources(plugin: &mut RustPluginBox) -> i32;
        fn rust_plugin_setup_config(
            plugin: &mut RustPluginBox,
            systems: &[*const TrSystem],
        ) -> i32;
        fn rust_plugin_trunk_message(
            plugin: &mut RustPluginBox,
            messages: &[TrTrunkMessage],
            system: &TrSystem,
        ) -> i32;
        fn rust_plugin_system_rates(
            plugin: &mut RustPluginBox,
            systems: &[*const TrSystem],
            time_diff: f32,
        ) -> i32;
        fn rust_plugin_unit_registration(
            plugin: &mut RustPluginBox,
            system: &TrSystem,
            source_id: i64,
        ) -> i32;
        fn rust_plugin_unit_deregistration(
            plugin: &mut RustPluginBox,
            system: &TrSystem,
            source_id: i64,
        ) -> i32;
        fn rust_plugin_unit_acknowledge_response(
            plugin: &mut RustPluginBox,
            system: &TrSystem,
            source_id: i64,
        ) -> i32;
        fn rust_plugin_unit_group_affiliation(
            plugin: &mut RustPluginBox,
            system: &TrSystem,
            source_id: i64,
            talkgroup: i64,
        ) -> i32;
        fn rust_plugin_unit_data_grant(
            plugin: &mut RustPluginBox,
            system: &TrSystem,
            source_id: i64,
        ) -> i32;
        fn rust_plugin_unit_answer_request(
            plugin: &mut RustPluginBox,
            system: &TrSystem,
            source_id: i64,
            talkgroup: i64,
        ) -> i32;
        fn rust_plugin_unit_location(
            plugin: &mut RustPluginBox,
            system: &TrSystem,
            source_id: i64,
            talkgroup: i64,
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
/// This is the raw FFI-level trait; the safe `Plugin` trait in trunk-recorder-plugin
/// wraps this with ergonomic types.
pub trait RustPluginCallbacks: Send {
    fn parse_config(&mut self, json_str: &str) -> i32;
    fn start(&mut self) -> i32;
    fn stop(&mut self) -> i32;
    fn poll_one(&mut self) -> i32;
    fn signal(
        &mut self,
        unit_id: i64,
        signaling_type: &str,
        sig_type: ffi::TrSignalType,
        call: &ffi::TrCall,
        system: &ffi::TrSystem,
        recorder: &ffi::TrRecorder,
    ) -> i32;
    fn audio_stream(
        &mut self,
        call: &ffi::TrCall,
        recorder: &ffi::TrRecorder,
        samples: &[i16],
    ) -> i32;
    fn call_start(&mut self, call: &ffi::TrCall) -> i32;
    fn call_end(&mut self, call_data: &ffi::TrCallData) -> i32;
    fn calls_active(&mut self, calls: &[*const ffi::TrCall]) -> i32;
    fn setup_recorder(&mut self, recorder: &ffi::TrRecorder) -> i32;
    fn setup_system(&mut self, system: &ffi::TrSystem) -> i32;
    fn setup_systems(&mut self, systems: &[*const ffi::TrSystem]) -> i32;
    fn setup_sources(&mut self) -> i32;
    fn setup_config(&mut self, systems: &[*const ffi::TrSystem]) -> i32;
    fn trunk_message(&mut self, messages: &[ffi::TrTrunkMessage], system: &ffi::TrSystem) -> i32;
    fn system_rates(&mut self, systems: &[*const ffi::TrSystem], time_diff: f32) -> i32;
    fn unit_registration(&mut self, system: &ffi::TrSystem, source_id: i64) -> i32;
    fn unit_deregistration(&mut self, system: &ffi::TrSystem, source_id: i64) -> i32;
    fn unit_acknowledge_response(&mut self, system: &ffi::TrSystem, source_id: i64) -> i32;
    fn unit_group_affiliation(
        &mut self,
        system: &ffi::TrSystem,
        source_id: i64,
        talkgroup: i64,
    ) -> i32;
    fn unit_data_grant(&mut self, system: &ffi::TrSystem, source_id: i64) -> i32;
    fn unit_answer_request(
        &mut self,
        system: &ffi::TrSystem,
        source_id: i64,
        talkgroup: i64,
    ) -> i32;
    fn unit_location(&mut self, system: &ffi::TrSystem, source_id: i64, talkgroup: i64) -> i32;
}

// ── Trampoline functions from C++ → Rust via cxx ──

fn rust_plugin_parse_config(plugin: &mut RustPluginBox, json_str: &str) -> i32 {
    plugin.inner.parse_config(json_str)
}

fn rust_plugin_start(plugin: &mut RustPluginBox) -> i32 {
    plugin.inner.start()
}

fn rust_plugin_stop(plugin: &mut RustPluginBox) -> i32 {
    plugin.inner.stop()
}

fn rust_plugin_poll_one(plugin: &mut RustPluginBox) -> i32 {
    plugin.inner.poll_one()
}

fn rust_plugin_signal(
    plugin: &mut RustPluginBox,
    unit_id: i64,
    signaling_type: &str,
    sig_type: ffi::TrSignalType,
    call: &ffi::TrCall,
    system: &ffi::TrSystem,
    recorder: &ffi::TrRecorder,
) -> i32 {
    plugin
        .inner
        .signal(unit_id, signaling_type, sig_type, call, system, recorder)
}

fn rust_plugin_audio_stream(
    plugin: &mut RustPluginBox,
    call: &ffi::TrCall,
    recorder: &ffi::TrRecorder,
    samples: &[i16],
) -> i32 {
    plugin.inner.audio_stream(call, recorder, samples)
}

fn rust_plugin_call_start(plugin: &mut RustPluginBox, call: &ffi::TrCall) -> i32 {
    plugin.inner.call_start(call)
}

fn rust_plugin_call_end(plugin: &mut RustPluginBox, call_data: &ffi::TrCallData) -> i32 {
    plugin.inner.call_end(call_data)
}

fn rust_plugin_calls_active(plugin: &mut RustPluginBox, calls: &[*const ffi::TrCall]) -> i32 {
    plugin.inner.calls_active(calls)
}

fn rust_plugin_setup_recorder(plugin: &mut RustPluginBox, recorder: &ffi::TrRecorder) -> i32 {
    plugin.inner.setup_recorder(recorder)
}

fn rust_plugin_setup_system(plugin: &mut RustPluginBox, system: &ffi::TrSystem) -> i32 {
    plugin.inner.setup_system(system)
}

fn rust_plugin_setup_systems(plugin: &mut RustPluginBox, systems: &[*const ffi::TrSystem]) -> i32 {
    plugin.inner.setup_systems(systems)
}

fn rust_plugin_setup_sources(plugin: &mut RustPluginBox) -> i32 {
    plugin.inner.setup_sources()
}

fn rust_plugin_setup_config(
    plugin: &mut RustPluginBox,
    systems: &[*const ffi::TrSystem],
) -> i32 {
    plugin.inner.setup_config(systems)
}

fn rust_plugin_trunk_message(
    plugin: &mut RustPluginBox,
    messages: &[ffi::TrTrunkMessage],
    system: &ffi::TrSystem,
) -> i32 {
    plugin.inner.trunk_message(messages, system)
}

fn rust_plugin_system_rates(
    plugin: &mut RustPluginBox,
    systems: &[*const ffi::TrSystem],
    time_diff: f32,
) -> i32 {
    plugin.inner.system_rates(systems, time_diff)
}

fn rust_plugin_unit_registration(
    plugin: &mut RustPluginBox,
    system: &ffi::TrSystem,
    source_id: i64,
) -> i32 {
    plugin.inner.unit_registration(system, source_id)
}

fn rust_plugin_unit_deregistration(
    plugin: &mut RustPluginBox,
    system: &ffi::TrSystem,
    source_id: i64,
) -> i32 {
    plugin.inner.unit_deregistration(system, source_id)
}

fn rust_plugin_unit_acknowledge_response(
    plugin: &mut RustPluginBox,
    system: &ffi::TrSystem,
    source_id: i64,
) -> i32 {
    plugin.inner.unit_acknowledge_response(system, source_id)
}

fn rust_plugin_unit_group_affiliation(
    plugin: &mut RustPluginBox,
    system: &ffi::TrSystem,
    source_id: i64,
    talkgroup: i64,
) -> i32 {
    plugin
        .inner
        .unit_group_affiliation(system, source_id, talkgroup)
}

fn rust_plugin_unit_data_grant(
    plugin: &mut RustPluginBox,
    system: &ffi::TrSystem,
    source_id: i64,
) -> i32 {
    plugin.inner.unit_data_grant(system, source_id)
}

fn rust_plugin_unit_answer_request(
    plugin: &mut RustPluginBox,
    system: &ffi::TrSystem,
    source_id: i64,
    talkgroup: i64,
) -> i32 {
    plugin
        .inner
        .unit_answer_request(system, source_id, talkgroup)
}

fn rust_plugin_unit_location(
    plugin: &mut RustPluginBox,
    system: &ffi::TrSystem,
    source_id: i64,
    talkgroup: i64,
) -> i32 {
    plugin.inner.unit_location(system, source_id, talkgroup)
}
