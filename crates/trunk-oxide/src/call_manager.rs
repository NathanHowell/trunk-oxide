use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::time::Instant;

use cxx::UniquePtr;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge};
use trunk_recorder_sys::SystemInfo;
use trunk_recorder_sys::ffi::{
    self, TrCall, TrCallData, TrCallDataStatus, TrContext, TrMessageType, TrRecorderConfig,
    TrTrunkMessage, TrUnitEventType,
};

use std::sync::Arc;

use trunk_config::SystemType;
use trunk_oxide_grpc::EventBridge;
use trunk_oxide_grpc::proto::SystemType as ProtoSystemType;

use crate::call_state::{
    CallId, CallState, Effect, Freq, MessageType, RecorderNum, RecordingResult, RejectReason,
    Talkgroup, TrunkMessage, TrunkStateMachine, UnitEventType,
};

pub use crate::call_state::{CallManagerConfig, ConventionalChannel, SystemPolicy, TalkgroupPolicy};

use trunk_oxide_grpc::AudioMeta;

const fn system_type_to_proto(st: SystemType) -> i32 {
    match st {
        SystemType::Conventional => ProtoSystemType::Conventional as i32,
        SystemType::ConventionalP25 => ProtoSystemType::ConventionalP25 as i32,
        SystemType::ConventionalDMR => ProtoSystemType::ConventionalDmr as i32,
        SystemType::ConventionalSigmf => ProtoSystemType::ConventionalSigmf as i32,
        SystemType::Smartnet => ProtoSystemType::Smartnet as i32,
        SystemType::P25 => ProtoSystemType::P25 as i32,
    }
}

fn format_recorder_state(state: i32) -> &'static str {
    match state {
        1 => "Recording",
        2 => "Inactive",
        3 => "Active",
        4 => "Idle",
        6 => "Stopped",
        7 => "Available",
        8 => "Ignored",
        _ => "Unknown",
    }
}

/// Executor: processes effects from the state machine against real FFI/bridge.
pub struct CallManager {
    state: TrunkStateMachine,
    bridge: Arc<EventBridge>,
    /// C++ call handles, keyed by CallId.
    handles: HashMap<CallId, UniquePtr<TrCall>>,
    /// Per-source frequency coverage, indexed by source position.
    source_ranges: Vec<RangeInclusive<f64>>,
    calls_started: Counter<u64>,
    calls_ended: Counter<u64>,
    recorder_denied: Counter<u64>,
    decode_rate_gauge: Gauge<i64>,
    active_calls_gauge: Gauge<u64>,
}

impl CallManager {
    pub fn new(
        config: CallManagerConfig,
        bridge: Arc<EventBridge>,
        source_ranges: Vec<RangeInclusive<f64>>,
    ) -> Self {
        let meter = opentelemetry::global::meter("trunk-oxide");
        Self {
            state: TrunkStateMachine::new(config),
            bridge,
            handles: HashMap::new(),
            source_ranges,
            calls_started: meter
                .u64_counter("trunk.oxide.calls_started")
                .with_description("Total calls started")
                .build(),
            calls_ended: meter
                .u64_counter("trunk.oxide.calls_ended")
                .with_description("Total calls ended")
                .build(),
            recorder_denied: meter
                .u64_counter("trunk.oxide.recorder_denied")
                .with_description("Calls denied a recorder")
                .build(),
            decode_rate_gauge: meter
                .i64_gauge("trunk.oxide.decode_rate")
                .with_description("Control channel decode rate (msg/sec)")
                .build(),
            active_calls_gauge: meter
                .u64_gauge("trunk.oxide.active_calls")
                .with_description("Number of active calls")
                .build(),
        }
    }

    /// Find the first source whose frequency range covers `freq_hz`.
    fn find_source(&self, freq_hz: f64) -> Option<i32> {
        self.source_ranges
            .iter()
            .position(|r| r.contains(&freq_hz))
            .map(|i| i as i32)
    }

    // ── Message conversion from CXX → pure Rust ──

    fn convert_message(msg: &TrTrunkMessage) -> TrunkMessage {
        let message_type = match msg.message_type {
            TrMessageType::Grant => MessageType::Grant,
            TrMessageType::Update => MessageType::Update,
            TrMessageType::UuVGrant => MessageType::UuVGrant,
            TrMessageType::UuVUpdate => MessageType::UuVUpdate,
            TrMessageType::Tdulc => MessageType::Tdulc,
            TrMessageType::PatchAdd => MessageType::PatchAdd,
            TrMessageType::PatchDelete => MessageType::PatchDelete,
            TrMessageType::ControlChannel => MessageType::ControlChannel,
            TrMessageType::Sysid => MessageType::Sysid,
            TrMessageType::Status => MessageType::Status,
            TrMessageType::InvalidCcMessage => MessageType::InvalidCcMessage,
            TrMessageType::Registration => MessageType::Registration,
            TrMessageType::Deregistration => MessageType::Deregistration,
            TrMessageType::Affiliation => MessageType::Affiliation,
            TrMessageType::Acknowledge => MessageType::Acknowledge,
            TrMessageType::Location => MessageType::Location,
            TrMessageType::DataGrant => MessageType::DataGrant,
            TrMessageType::UuAnsReq => MessageType::UuAnsReq,
            _ => MessageType::Unknown(msg.message_type.repr),
        };

        TrunkMessage {
            message_type,
            sys_num: msg.sys_num,
            talkgroup: msg.talkgroup,
            freq: Freq::from_hz(msg.freq),
            source: msg.source,
            tdma_slot: msg.tdma_slot,
            phase2_tdma: msg.phase2_tdma,
            mode: msg.mode,
            duplex: msg.duplex,
            emergency: msg.emergency,
            encrypted: msg.encrypted,
            priority: msg.priority,
            wacn: msg.wacn,
            nac: msg.nac,
            sys_id: msg.sys_id,
            sys_rfss: msg.sys_rfss,
            sys_site_id: msg.sys_site_id,
            patch_sg: msg.patch_data.sg,
            patch_ga1: msg.patch_data.ga1,
            patch_ga2: msg.patch_data.ga2,
            patch_ga3: msg.patch_data.ga3,
        }
    }

    // ── Audio metadata management ──

    fn register_audio_meta(&self, call_id: CallId) {
        if let Some(entry) = self.state.get_call(call_id) {
            self.bridge.register_audio_meta(
                call_id.as_u64(),
                AudioMeta {
                    talkgroup: entry.talkgroup,
                    sys_num: entry.sys_num,
                    freq: entry.freq.as_hz(),
                    sample_rate: if entry.is_analog { 16_000 } else { 8_000 },
                },
            );
        }
    }

    // ── Effect processing ──

    fn process_effects(&mut self, effects: Vec<Effect>, ctx: &mut Pin<&mut TrContext>) {
        for effect in effects {
            match effect {
                Effect::CreateCall { call_id } => {
                    self.exec_create_call(call_id, ctx);
                }
                Effect::RequestRecording {
                    call_id,
                    freq,
                    is_analog,
                    priority,
                } => {
                    self.exec_request_recording(call_id, freq, is_analog, priority, ctx);
                }
                Effect::StopRecording { call_id } => {
                    self.exec_stop_recorder(call_id);
                }
                Effect::SetFreq { call_id, freq } => {
                    self.bridge
                        .update_audio_meta_freq(call_id.as_u64(), freq.as_hz());
                }
                Effect::SetSource {
                    call_id,
                    source_id,
                } => {
                    if let Some(handle) = self.handles.get_mut(&call_id) {
                        ffi::call_recorder_set_source(handle.pin_mut(), source_id);
                    }
                }
                Effect::EmitCallStart { call_id } => {
                    if let Some(entry) = self.state.get_call(call_id) {
                        self.calls_started.add(1, &[KeyValue::new("sys_num", i64::from(entry.sys_num))]);
                        let info = self.build_call_info(entry);
                        self.bridge.call_start(&info);
                    }
                }
                Effect::EmitCallEnd { call_id } => {
                    if let Some(entry) = self.state.get_call(call_id) {
                        self.calls_ended.add(1, &[KeyValue::new("sys_num", i64::from(entry.sys_num))]);
                    }
                    self.exec_conclude_call(call_id);
                }
                Effect::EmitCallsActive => {
                    let infos: Vec<ffi::TrCallInfo> = self
                        .state
                        .calls
                        .iter()
                        .map(|e| self.build_call_info(e))
                        .collect();
                    self.bridge.calls_active(&infos);
                }
                Effect::EmitSignal {
                    call_id,
                    source_id,
                } => {
                    if let Some(entry) = self.state.get_call(call_id) {
                        let info = self.build_call_info(entry);
                        self.bridge.signal(source_id, &info);
                    }
                }
                Effect::EmitUnitEvent {
                    sys_num,
                    type_,
                    source_id,
                    talkgroup,
                } => {
                    let ffi_type = match type_ {
                        UnitEventType::Registration => TrUnitEventType::Registration,
                        UnitEventType::Deregistration => TrUnitEventType::Deregistration,
                        UnitEventType::Affiliation => TrUnitEventType::Affiliation,
                        UnitEventType::Acknowledge => TrUnitEventType::Acknowledge,
                        UnitEventType::Location => TrUnitEventType::Location,
                        UnitEventType::DataGrant => TrUnitEventType::DataGrant,
                        UnitEventType::AnswerRequest => TrUnitEventType::AnswerRequest,
                    };
                    let info = self.get_system_info(sys_num);
                    self.bridge
                        .unit_event(&info, ffi_type, source_id, talkgroup);
                }
                Effect::EmitSetupSystem { sys_num } => {
                    let info = self.get_system_info(sys_num);
                    self.bridge.setup_system(&info);
                }
                Effect::EmitSetupRecorder { call_id } => {
                    if let Some(entry) = self.state.get_call(call_id) {
                        if let Some(rec) = &entry.recorder {
                            self.bridge.setup_recorder(rec.0);
                        }
                    }
                }
                Effect::AddControlChannel { sys_num, freq } => {
                    ffi::tr_system_add_control_channel_runtime(ctx.as_mut(), sys_num, freq.as_hz());
                }
                Effect::ConventionalRestart { call_id } => {
                    if let Some(entry) = self.state.get_call(call_id) {
                        let cfg = self.build_recorder_config(entry);
                        if let Some(handle) = self.handles.get_mut(&call_id) {
                            ffi::call_restart(handle.pin_mut(), &cfg);
                            // Update recorder number — C++ may assign a different one
                            let rec_num = if ffi::call_has_recorder(&**handle) {
                                Some(RecorderNum(ffi::call_get_recorder_num(&**handle)))
                            } else {
                                None
                            };
                            self.state.on_conventional_restarted(call_id, rec_num);
                        }
                    }
                }
                Effect::EnableRecorder { call_id } => {
                    if let Some(entry) = self.state.get_call(call_id) {
                        tracing::info!(
                            freq = %entry.freq,
                            talkgroup = entry.talkgroup,
                            "enabling recorder (signal detected)"
                        );
                    }
                    if let Some(handle) = self.handles.get_mut(&call_id) {
                        ffi::call_set_recorder_enabled(handle.pin_mut(), true);
                    }
                }
                Effect::DisableRecorder { call_id } => {
                    if let Some(entry) = self.state.get_call(call_id) {
                        tracing::info!(
                            freq = %entry.freq,
                            talkgroup = entry.talkgroup,
                            "disabling recorder (signal lost)"
                        );
                    }
                    if let Some(handle) = self.handles.get_mut(&call_id) {
                        ffi::call_set_recorder_enabled(handle.pin_mut(), false);
                    }
                }
                Effect::RemoveCall { call_id } => {
                    self.handles.remove(&call_id);
                    self.state.remove_call(call_id);
                    // Unregister audio meta last: the write lock waits for any
                    // in-flight audio_stream() read lock, ensuring the final
                    // audio send completes before we remove the metadata.
                    self.bridge.unregister_audio_meta(call_id.as_u64());
                }
                Effect::Warn { message } => {
                    tracing::debug!(%message, "state machine warning");
                }
            }
        }
    }

    // ── Effect executors ──

    fn exec_create_call(&mut self, call_id: CallId, ctx: &mut Pin<&mut TrContext>) {
        let entry = match self.state.get_call(call_id) {
            Some(e) => e,
            None => return,
        };

        // Build a minimal TrTrunkMessage for C++ call creation (it only reads
        // freq, talkgroup, sys_num from it). Rust owns all call state.
        let ffi_msg = build_ffi_message(entry);
        let mut handle = ffi::tr_context_create_call(ctx.as_mut(), &ffi_msg);

        // Set rust_call_id on C++ Call for audio callback round-trip
        ffi::call_set_rust_call_id(handle.pin_mut(), call_id.as_u64());
        self.register_audio_meta(call_id);
        self.handles.insert(call_id, handle);
    }

    fn exec_request_recording(
        &mut self,
        call_id: CallId,
        freq: Freq,
        is_analog: bool,
        priority: i32,
        ctx: &mut Pin<&mut TrContext>,
    ) {
        let entry = match self.state.get_call(call_id) {
            Some(e) => e,
            None => {
                tracing::error!(?call_id, "exec_request_recording: CallEntry missing");
                return;
            }
        };

        // Build recorder config from Rust state (replaces C++ build_recorder_config)
        let cfg = self.build_recorder_config(entry);

        let freq_hz = freq.as_hz();
        let source_idx = self.find_source(freq_hz);

        let handle = match self.handles.get_mut(&call_id) {
            Some(h) => h,
            None => return,
        };

        let result = if let Some(source_idx) = source_idx {
            let available = ffi::tr_source_available_recorders(&**ctx, source_idx, is_analog);
            if priority > available {
                if let Some(entry) = self.state.get_call(call_id) {
                    tracing::error!(
                        talkgroup = entry.talkgroup,
                        sys_num = entry.sys_num,
                        priority,
                        available,
                        "not recording: priority exceeds available recorders"
                    );
                }
                RecordingResult::NoRecorder
            } else if ffi::tr_source_start_recording(
                ctx.as_mut(),
                source_idx,
                handle.pin_mut(),
                is_analog,
                &cfg,
            ) {
                let rec_num = ffi::call_get_recorder_num(&**handle);
                RecordingResult::Started {
                    rec_num: RecorderNum(rec_num),
                }
            } else {
                RecordingResult::NoRecorder
            }
        } else {
            RecordingResult::NoSource
        };

        if matches!(result, RecordingResult::NoRecorder | RecordingResult::NoSource)
            && let Some(entry) = self.state.get_call(call_id)
        {
            self.recorder_denied.add(1, &[KeyValue::new("sys_num", i64::from(entry.sys_num))]);
        }

        let followup = self.state.on_recording_result(call_id, result);
        self.process_effects(followup, ctx);
    }

    fn exec_stop_recorder(&mut self, call_id: CallId) {
        let handle = match self.handles.get_mut(&call_id) {
            Some(h) => h,
            None => return,
        };

        let entry = self.state.get_call(call_id);
        if entry.map_or(false, |e| e.recorder.is_some()) {
            let rec_num = entry.unwrap().recorder.unwrap();
            let metrics = ffi::call_stop_recording(handle.pin_mut());
            let entry = self.state.get_call(call_id).unwrap();
            tracing::info!(
                short_name = %entry.short_name,
                rec_num = rec_num.0,
                talkgroup = entry.talkgroup,
                freq = %entry.freq,
                tdma = entry.phase2_tdma,
                slot = entry.tdma_slot,
                freq_error = format_args!("{:+.0} Hz", metrics.freq_error),
                "stopping recorder"
            );

            // Update signal/noise from conventional recorder metrics
            if entry.is_conventional() {
                let was_idle = entry.state == CallState::Idle;
                if let Some(entry) = self.state.get_call_mut(call_id) {
                    if was_idle {
                        entry.noise = metrics.recorder_pwr;
                    } else {
                        entry.signal = metrics.recorder_pwr;
                    }
                }
            }

            self.state.on_recorder_stopped(call_id);
        }
    }

    fn exec_conclude_call(&mut self, call_id: CallId) {
        let entry = match self.state.get_call(call_id) {
            Some(e) => e,
            None => return,
        };

        let state = entry.state;
        let final_freq = entry.freq;
        let is_conventional = entry.is_conventional();

        let should_stop_recording = matches!(
            state,
            CallState::Recording | CallState::Idle | CallState::Rejected(RejectReason::Superseded)
        );

        if should_stop_recording {
            // Stop recorder and collect metrics
            let handle = match self.handles.get_mut(&call_id) {
                Some(h) => h,
                None => return,
            };

            let entry = self.state.get_call(call_id).unwrap();
            if let Some(rec_num) = entry.recorder {
                let metrics = ffi::call_stop_recording(handle.pin_mut());

                // Re-read entry after mutable borrow of handle
                if is_conventional {
                    if let Some(entry_mut) = self.state.get_call_mut(call_id) {
                        if state == CallState::Idle {
                            entry_mut.noise = metrics.recorder_pwr;
                        } else {
                            entry_mut.signal = metrics.recorder_pwr;
                        }
                    }
                }

                let entry = self.state.get_call(call_id).unwrap();
                tracing::info!(
                    short_name = %entry.short_name,
                    rec_num = rec_num.0,
                    talkgroup = entry.talkgroup,
                    freq = %entry.freq,
                    tdma = entry.phase2_tdma,
                    slot = entry.tdma_slot,
                    freq_error = format_args!("{:+.0} Hz", metrics.freq_error),
                    "stopping recorder"
                );

                let segments = self.build_proto_segments_final(call_id, final_freq, rec_num.0);
                let recording_length = entry.recording_duration().as_secs_f64();

                let elapsed = entry.start_time.elapsed().map_or(0, |d| d.as_secs() as i64);
                tracing::info!(
                    short_name = %entry.short_name,
                    call_num = entry.call_num,
                    talkgroup = entry.talkgroup,
                    elapsed,
                    recording_length,
                    segments = segments.len(),
                    "concluding recorded call"
                );

                let handle = self.handles.get(&call_id).unwrap();
                if metrics.is_dmr {
                    for slot in 0..2i32 {
                        let mut data = self.build_call_data(call_id);
                        data.tdma_slot = slot;
                        data.length = recording_length;
                        data.freq_error = metrics.freq_error;
                        data.transmission_list =
                            ffi::call_get_dmr_transmission_list(&**handle, slot);
                        self.bridge.call_end(&data, &segments);
                    }
                } else {
                    let mut data = self.build_call_data(call_id);
                    data.length = recording_length;
                    data.freq_error = metrics.freq_error;
                    data.transmission_list = ffi::call_get_transmission_list(&**handle);
                    self.bridge.call_end(&data, &segments);
                }
            }
        } else if matches!(state, CallState::Rejected(_)) {
            let segments = self.build_proto_segments_final(call_id, final_freq, -1);
            let data = self.build_call_data(call_id);
            self.bridge.call_end(&data, &segments);
        }
    }

    // ── Public API (delegates to state machine + processes effects) ──

    pub fn handle_message(&mut self, mut ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) {
        let rust_msg = Self::convert_message(msg);

        // Sysid/Status need FFI call before state machine processes them
        match rust_msg.message_type {
            MessageType::Sysid => {
                if !ffi::tr_system_update_sysid(ctx.as_mut(), msg) {
                    return;
                }
            }
            MessageType::Status => {
                if !ffi::tr_system_update_status(ctx.as_mut(), msg) {
                    return;
                }
            }
            _ => {}
        }

        let effects = self.state.on_message(&rust_msg);
        self.process_effects(effects, &mut ctx);
    }

    pub fn tick(&mut self, mut ctx: Pin<&mut TrContext>) {
        // First, check recorder write times for recording calls (needs FFI).
        // The state machine can't check this, so we do it here.
        let timeout = self.state.config.call_timeout;
        let mut timed_out_ids = Vec::new();

        for entry in &self.state.calls {
            if entry.is_conventional() || entry.state != CallState::Recording {
                continue;
            }
            if let Some(handle) = self.handles.get(&entry.id) {
                let since_write = ffi::call_recorder_since_last_write_secs(&**handle);
                if since_write > timeout {
                    timed_out_ids.push(entry.id);
                }
            }
        }

        // Expire timed-out-by-write calls through the effect system
        for call_id in timed_out_ids {
            let effects = self.state.expire_call(call_id);
            self.process_effects(effects, &mut ctx);
        }

        // Now run the state machine tick (handles update timeouts + conventional)
        let effects = self.state.tick();
        self.process_effects(effects, &mut ctx);

        self.active_calls_gauge.record(self.state.calls.len() as u64, &[]);
    }

    pub fn add_call(
        &mut self,
        mut call: UniquePtr<TrCall>,
        channel: &ConventionalChannel,
        sys_num: i32,
    ) {
        let policy = self
            .state
            .config
            .systems
            .get(&sys_num)
            .cloned()
            .unwrap_or_else(|| crate::call_state::DEFAULT_POLICY.clone());

        let rec_num = if ffi::call_has_recorder(&*call) {
            Some(RecorderNum(ffi::call_get_recorder_num(&*call)))
        } else {
            None
        };

        let (call_id, effects) = self
            .state
            .add_conventional_call(channel, sys_num, &policy, rec_num);

        // Set rust_call_id on the C++ handle for audio callback round-trip
        ffi::call_set_rust_call_id(call.pin_mut(), call_id.as_u64());
        self.register_audio_meta(call_id);
        self.handles.insert(call_id, call);

        // C++ start() no longer enables conventional recorders — Rust decides.
        for effect in effects {
            if let Effect::EnableRecorder { call_id } = effect {
                if let Some(handle) = self.handles.get_mut(&call_id) {
                    ffi::call_set_recorder_enabled(handle.pin_mut(), true);
                }
            }
        }
    }

    pub fn handle_squelch_event(&mut self, recorder_num: i32, muted: bool, pwr_db: f64) {
        self.state
            .on_squelch(RecorderNum(recorder_num), muted, pwr_db);
    }

    pub fn handle_signals_detected(
        &mut self,
        mut ctx: Pin<&mut TrContext>,
        source_center_freq: f64,
        signals: &[trunk_recorder_sys::DetectedSignal],
    ) {
        for signal in signals {
            let abs_freq = source_center_freq + signal.center_freq;
            tracing::info!(
                source_center_mhz = source_center_freq / 1_000_000.0,
                signal_offset_hz = signal.center_freq,
                abs_freq_mhz = abs_freq / 1_000_000.0,
                rssi = signal.max_rssi,
                threshold = signal.threshold,
                "signal detected"
            );
        }
        let effects = self
            .state
            .on_signals_detected(source_center_freq, signals);
        self.process_effects(effects, &mut ctx);
    }

    pub fn clear_stale_patches(&mut self) {
        self.state.clear_stale_patches();
    }

    pub fn print_active_patches(&self) {
        self.state.print_active_patches();
    }

    pub fn conclude_all(&mut self, ctx: Pin<&mut TrContext>) {
        let mut ctx = ctx;
        let effects = self.state.conclude_all();
        self.process_effects(effects, &mut ctx);
    }

    pub(crate) fn get_system_info(&self, sys_num: i32) -> SystemInfo {
        let policy = self
            .state
            .config
            .systems
            .get(&sys_num)
            .unwrap_or(&crate::call_state::DEFAULT_POLICY);
        let id = self.state.system_identities.get(&sys_num);

        SystemInfo {
            sys_num,
            system_type: system_type_to_proto(policy.system_type),
            short_name: policy.short_name.clone(),
            sys_id: id.map_or(0, |id| id.sys_id),
            wacn: id.map_or(0, |id| id.wacn),
            nac: id.map_or(0, |id| id.nac),
            rfss: id.map_or(0, |id| id.rfss),
            site_id: id.map_or(0, |id| id.site_id),
            bandplan: policy.bandplan.clone(),
            analog_levels: policy.analog_levels,
            digital_levels: policy.digital_levels,
            squelch_db: policy.squelch_db,
            conversation_mode: policy.conversation_mode,
            message_count: self.state.message_counts.get(&sys_num).copied().unwrap_or(0),
            decode_rate: self.state.decode_rates.get(&sys_num).copied().unwrap_or(0),
        }
    }

    /// Build a `TrRecorderConfig` from a Rust `CallEntry` and system metadata.
    /// This replaces the C++ `build_recorder_config(shared_ptr<Call>)`.
    pub(crate) fn build_recorder_config(
        &self,
        entry: &crate::call_state::CallEntry,
    ) -> TrRecorderConfig {
        let policy = self
            .state
            .config
            .systems
            .get(&entry.sys_num)
            .unwrap_or(&crate::call_state::DEFAULT_POLICY);

        let xor_mask = self
            .state
            .system_identities
            .get(&entry.sys_num)
            .map_or_else(Vec::new, |id| id.xor_mask.clone());

        TrRecorderConfig {
            talkgroup: entry.talkgroup,
            call_num: entry.call_num,
            freq: entry.freq.as_hz(),
            short_name: entry.short_name.clone(),
            temp_dir: self.state.config.temp_dir.clone(),
            phase2_tdma: entry.phase2_tdma,
            tdma_slot: entry.tdma_slot,
            xor_mask,
            squelch_db: entry.squelch_db,
            digital_levels: policy.digital_levels,
            rust_call_id: entry.id.as_u64(),
        }
    }

    pub fn print_status(&self, mut ctx: Pin<&mut TrContext>) {
        tracing::info!(count = self.state.calls.len(), "Active Calls");
        for entry in &self.state.calls {
            let elapsed = entry.start_time.elapsed().map_or(0, |d| d.as_secs() as i64);
            let state = entry.state.to_string();

            tracing::info!(
                system = %entry.short_name,
                call_num = entry.call_num,
                talkgroup = entry.talkgroup,
                freq = %entry.freq,
                elapsed,
                state,
                rec_num = entry.recorder.map(|r| r.0),
                "call status"
            );
        }

        tracing::info!("Control Channel Decode Rates");
        for (&sys_num, policy) in &self.state.config.systems {
            if policy.system_type != trunk_config::SystemType::Conventional
                && policy.system_type != trunk_config::SystemType::ConventionalP25
            {
                let cc_freq = ffi::tr_system_get_current_control_channel(ctx.as_mut(), sys_num);
                let decode_rate = self.state.decode_rates.get(&sys_num).copied().unwrap_or(0);
                tracing::info!(
                    system = %policy.short_name,
                    freq = %Freq::from_hz(cc_freq),
                    decode_rate,
                    "msg/sec"
                );
            }
        }
    }

    pub fn print_recorder_status(&self, ctx: &TrContext) {
        let recorders = ffi::tr_context_get_recorder_status(ctx);
        let mut current_source = -1i32;
        for r in &recorders {
            if r.source_num != current_source {
                current_source = r.source_num;
                tracing::info!(
                    source = r.source_num,
                    freq = %Freq::from_hz(r.source_center),
                    device = %r.source_device,
                    "source"
                );
            }
            tracing::info!(
                rec_num = r.rec_num,
                rec_type = %r.rec_type,
                rec_state = format_recorder_state(r.rec_state),
                "  recorder"
            );
        }
    }

    pub fn check_decode_rates(
        &mut self,
        mut ctx: Pin<&mut TrContext>,
        time_diff: f32,
    ) -> anyhow::Result<()> {
        let mut infos = Vec::new();
        for (&sys_num, policy) in &self.state.config.systems {
            let mut info = self.get_system_info(sys_num);
            if policy.system_type.is_trunked() {
                let count = self.state.message_counts.remove(&sys_num).unwrap_or(0);
                let decode_rate = (count as f32 / time_diff).floor() as i32;
                self.state.decode_rates.insert(sys_num, decode_rate);
                self.decode_rate_gauge.record(i64::from(decode_rate), &[KeyValue::new("sys_num", i64::from(sys_num))]);
                info.decode_rate = decode_rate;
                info.message_count = count;
            }
            infos.push(info);
        }

        self.bridge.setup_config(&infos);
        self.bridge.system_rates(&infos, time_diff);

        for info in &infos {
            let sys_num = info.sys_num;
            match self.state.config.systems.get(&sys_num) {
                Some(p) if p.system_type.is_trunked() => {}
                _ => continue,
            };
            let decode_rate = info.decode_rate;
            let count = info.message_count;

            if decode_rate < self.state.config.control_warn_rate {
                let freq = ffi::tr_system_get_current_control_channel(ctx.as_mut(), sys_num);
                let pwr_db = ffi::tr_system_get_control_channel_pwr(ctx.as_mut(), sys_num);
                tracing::warn!(
                    sys_num,
                    freq_mhz = freq / 1e6,
                    pwr_db,
                    decode_rate,
                    count,
                    "low decode rate",
                );
            } else if self.state.config.control_warn_rate == -1 {
                let freq = ffi::tr_system_get_current_control_channel(ctx.as_mut(), sys_num);
                let pwr_db = ffi::tr_system_get_control_channel_pwr(ctx.as_mut(), sys_num);
                tracing::info!(
                    sys_num,
                    freq_mhz = freq / 1e6,
                    pwr_db,
                    decode_rate,
                    count,
                    "decode rate",
                );
            }
        }
        Ok(())
    }

    // ── Helper: build TrCallInfo from pure CallEntry ──

    fn build_call_info(&self, entry: &crate::call_state::CallEntry) -> ffi::TrCallInfo {
        let elapsed = entry.start_time.elapsed().unwrap_or_default();
        let handle = self.handles.get(&entry.id);
        ffi::TrCallInfo {
            talkgroup: entry.talkgroup,
            call_num: entry.call_num,
            sys_num: entry.sys_num,
            freq: entry.freq.as_hz(),
            signal: entry.signal,
            noise: entry.noise,
            freq_error: 0,
            encrypted: entry.encrypted,
            emergency: entry.emergency,
            is_analog: entry.is_analog,
            is_conventional: entry.is_conventional(),
            phase2_tdma: entry.phase2_tdma,
            tdma_slot: entry.tdma_slot,
            priority: entry.priority,
            mode: entry.mode,
            duplex: entry.duplex,
            conversation_mode: entry
                .trunked_fields()
                .map_or(false, |t| t.conversation_mode),
            current_source_id: entry.current_source_id,
            start_time: crate::call_state::system_time_to_unix(entry.start_time),
            elapsed: elapsed.as_secs_f64(),
            short_name: entry.short_name.clone(),
            call_state: entry.state.to_proto(),
        }
    }

    fn build_call_data(&self, call_id: CallId) -> TrCallData {
        let entry = self.state.get_call(call_id).unwrap();
        let handle = self.handles.get(&call_id);
        let audio_type = if entry.is_analog {
            "analog"
        } else if entry.phase2_tdma {
            "digital tdma"
        } else {
            "digital"
        };

        let now = crate::call_state::system_time_to_unix(std::time::SystemTime::now());

        TrCallData {
            talkgroup: entry.talkgroup,
            color_code: 0,
            patched_talkgroups: self
                .state
                .patch_tables
                .get(&entry.sys_num)
                .map(|pt| {
                    pt.get_patch(Talkgroup(entry.talkgroup))
                        .into_iter()
                        .map(|t| t.0 as u64)
                        .collect()
                })
                .unwrap_or_default(),
            call_num: entry.call_num,
            freq: entry.freq.as_hz(),
            freq_error: 0,
            source_num: 0,
            recorder_num: entry.recorder.map_or(-1, |r| r.0),
            signal: entry.signal,
            noise: entry.noise,
            start_time: crate::call_state::system_time_to_unix(entry.start_time),
            stop_time: now,
            error_count: 0,
            spike_count: 0,
            encrypted: entry.encrypted,
            emergency: entry.emergency,
            priority: entry.priority,
            mode: entry.mode,
            duplex: entry.duplex,
            sys_num: entry.sys_num,
            short_name: entry.short_name.clone(),
            audio_type: audio_type.into(),
            tdma_slot: entry.tdma_slot,
            length: 0.0,
            phase2_tdma: entry.phase2_tdma,
            transmission_source_list: Vec::new(),
            transmission_error_list: Vec::new(),
            transmission_list: Vec::new(),
            status: TrCallDataStatus::Initial,
        }
    }

    fn build_proto_segments_final(
        &self,
        call_id: CallId,
        final_freq: Freq,
        final_rec_num: i32,
    ) -> Vec<trunk_oxide_grpc::proto::CallSegment> {
        let entry = match self.state.get_call(call_id) {
            Some(e) => e,
            None => return vec![],
        };

        let mut segments: Vec<_> = entry
            .segments
            .iter()
            .map(|s| Self::make_proto_segment(s.freq.as_hz(), s.start, s.rec_num.0))
            .collect();
        if entry.is_trunked() || !entry.segments.is_empty() {
            segments.push(Self::make_proto_segment(
                final_freq.as_hz(),
                entry.segment_start,
                final_rec_num,
            ));
        }
        segments
    }

    fn make_proto_segment(
        freq: f64,
        start: Instant,
        rec_num: i32,
    ) -> trunk_oxide_grpc::proto::CallSegment {
        use prost_types::{Duration as ProstDuration, Timestamp};
        let elapsed = start.elapsed();
        let wall_start = std::time::SystemTime::now() - elapsed;
        let since_epoch = wall_start
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        trunk_oxide_grpc::proto::CallSegment {
            freq,
            start_time: Some(Timestamp {
                seconds: since_epoch.as_secs() as i64,
                nanos: since_epoch.subsec_nanos() as i32,
            }),
            duration: Some(ProstDuration {
                seconds: elapsed.as_secs() as i64,
                nanos: elapsed.subsec_nanos() as i32,
            }),
            rec_num,
        }
    }
}

/// Build a minimal TrTrunkMessage from a CallEntry for C++ call creation.
fn build_ffi_message(entry: &crate::call_state::CallEntry) -> TrTrunkMessage {
    TrTrunkMessage {
        message_type: TrMessageType::Grant,
        sys_num: entry.sys_num,
        talkgroup: entry.talkgroup,
        freq: entry.freq.as_hz(),
        source: entry.current_source_id,
        tdma_slot: entry.tdma_slot,
        phase2_tdma: entry.phase2_tdma,
        mode: entry.mode,
        duplex: entry.duplex,
        emergency: entry.emergency,
        encrypted: entry.encrypted,
        priority: entry.priority,
        wacn: entry
            .trunked_fields()
            .map_or(0, |t| t.wacn),
        nac: 0,
        sys_id: 0,
        sys_rfss: entry
            .trunked_fields()
            .map_or(0, |t| t.rfss),
        sys_site_id: entry
            .trunked_fields()
            .map_or(0, |t| t.site_id),
        patch_data: ffi::TrPatchData {
            sg: 0,
            ga1: 0,
            ga2: 0,
            ga3: 0,
        },
        meta: String::new(),
        opcode: 0,
    }
}
