use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cxx::UniquePtr;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge};
use trunk_recorder_sys::SystemInfo;
use trunk_recorder_sys::ffi::{
    self, TrCall, TrCallData, TrCallDataStatus, TrContext, TrMessageType, TrRecorderConfig,
    TrTrunkMessage, TrUnitEventType,
};

use trunk_config::SystemType;
use trunk_oxide_grpc::EventBridge;
use trunk_oxide_grpc::proto::SystemType as ProtoSystemType;

use crate::ConventionalMetricsSnapshot;

use crate::call_state::{
    CallId, CallState, CallView, Effect, Effects, Freq, MessageType, RecorderNum, RecordingResult,
    RejectReason, Talkgroup, TrunkMessage, TrunkStateMachine, UnitEventType,
};

pub use crate::call_state::{
    CallManagerConfig, ConventionalChannel, SystemPolicy, TalkgroupPolicy,
};

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
    /// Per-source configured ppm correction, indexed by source position.
    source_ppms: Vec<f64>,
    calls_started: Counter<u64>,
    calls_ended: Counter<u64>,
    recorder_denied: Counter<u64>,
    control_channel_messages: Counter<u64>,
    active_calls_gauge: Gauge<u64>,
    conv_metrics: Arc<Mutex<ConventionalMetricsSnapshot>>,
}

impl CallManager {
    pub fn new(
        config: CallManagerConfig,
        bridge: Arc<EventBridge>,
        source_ranges: Vec<RangeInclusive<f64>>,
        source_ppms: Vec<f64>,
        conv_metrics: Arc<Mutex<ConventionalMetricsSnapshot>>,
    ) -> Self {
        // Pre-populate conventional channel metrics entries.
        if let Ok(mut snap) = conv_metrics.lock() {
            for policy in config.systems.values() {
                for ch in &policy.channels {
                    snap.channels.push(crate::ConventionalChannelMetrics {
                        system: policy.short_name.clone(),
                        talkgroup: ch.talkgroup.0,
                        freq_hz: ch.freq.as_hz(),
                        state: "Disabled",
                        signal_power_db: 0.0,
                        noise_floor_db: 0.0,
                        detection_rssi_db: 0.0,
                        squelch_open_count: 0,
                        squelch_close_count: 0,
                        enable_count: 0,
                        disable_count: 0,
                        idle_timeout_count: 0,
                        recordings_completed: 0,
                        squelch_db: ch.squelch_db,
                        adaptive_squelch_db: None,
                        last_power_reading_db: None,
                    });
                }
            }
        }

        let meter = opentelemetry::global::meter("trunk-oxide");
        Self {
            state: TrunkStateMachine::new(config),
            bridge,
            handles: HashMap::new(),
            source_ranges,
            source_ppms,
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
            control_channel_messages: meter
                .u64_counter("trunk.oxide.control_channel_messages")
                .with_description("Total control channel messages decoded")
                .build(),
            active_calls_gauge: meter
                .u64_gauge("trunk.oxide.active_calls")
                .with_description("Number of active calls")
                .build(),
            conv_metrics,
        }
    }

    /// Find the first source whose frequency range covers `freq_hz`.
    fn find_source(&self, freq_hz: f64) -> Option<i32> {
        self.source_ranges
            .iter()
            .position(|r| r.contains(&freq_hz))
            .map(|i| i as i32)
    }

    /// Suggested per-source ppm correction that would pull the FLL frequency
    /// error toward zero. `freq_error_hz > 0` means the signal sat above the
    /// tuned center; empirically (confirmed against live readings on these
    /// rtlsdr dongles) raising the ppm correction lowers a positive error, so
    /// the suggestion adds the error-in-ppm to the configured value. Hardware
    /// sensitivity is sub-unity and the offset drifts with temperature, so
    /// converging on a dongle may take a step or two — treat this as a
    /// first-order estimate, not an exact target. Returns the configured ppm
    /// unchanged when there's no error, so a logged value drifting away from
    /// the config is the calibration signal.
    fn preferred_ppm(&self, freq_hz: f64, freq_error_hz: i32) -> Option<f64> {
        let idx = usize::try_from(self.find_source(freq_hz)?).ok()?;
        let current = *self.source_ppms.get(idx)?;
        Some(current + f64::from(freq_error_hz) / freq_hz * 1e6)
    }

    /// Update a conventional channel's metrics entry by talkgroup + freq.
    fn update_conv_metric(
        &self,
        talkgroup: i64,
        freq: Freq,
        f: impl FnOnce(&mut crate::ConventionalChannelMetrics),
    ) {
        if let Ok(mut snap) = self.conv_metrics.lock()
            && let Some(ch) = snap
                .channels
                .iter_mut()
                .find(|c| c.talkgroup == talkgroup && (c.freq_hz - freq.as_hz()).abs() < 1.0)
        {
            f(ch);
        }
    }

    /// Sync conventional channel states from call entries into the metrics snapshot.
    fn sync_conv_states(&self) {
        if let Ok(mut snap) = self.conv_metrics.lock() {
            for view in self.state.iter_calls() {
                if !view.is_conventional() {
                    continue;
                }
                if let Some(ch) = snap.channels.iter_mut().find(|c| {
                    c.talkgroup == view.talkgroup() && (c.freq_hz - view.freq().as_hz()).abs() < 1.0
                }) {
                    ch.state = view.state().as_str();
                }
            }
        }
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
        if let Some(view) = self.state.get_call_view(call_id) {
            self.bridge.register_audio_meta(
                call_id.as_u64(),
                AudioMeta {
                    talkgroup: view.talkgroup(),
                    sys_num: view.sys_num(),
                    freq: view.freq().as_hz(),
                    sample_rate: if view.is_analog() { 16_000 } else { 8_000 },
                },
            );
        }
    }

    // ── Effect processing ──

    fn process_effects(&mut self, effects: Effects, ctx: &mut Pin<&mut TrContext>) {
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
                Effect::SetSource { call_id, source_id } => {
                    if let Some(handle) = self.handles.get_mut(&call_id) {
                        ffi::call_recorder_set_source(handle.pin_mut(), source_id);
                    }
                }
                Effect::EmitCallStart { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        self.calls_started
                            .add(1, &[KeyValue::new("sys_num", i64::from(view.sys_num()))]);
                        let info = self.build_call_info_from_view(&view);
                        self.bridge.call_start(&info);
                    }
                }
                Effect::EmitCallEnd { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        self.calls_ended
                            .add(1, &[KeyValue::new("sys_num", i64::from(view.sys_num()))]);
                    }
                    self.exec_conclude_call(call_id);
                }
                Effect::EmitCallsActive => {
                    let infos: Vec<ffi::TrCallInfo> = self
                        .state
                        .iter_calls()
                        .map(|v| self.build_call_info_from_view(&v))
                        .collect();
                    self.bridge.calls_active(&infos);
                }
                Effect::EmitSignal { call_id, source_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        let info = self.build_call_info_from_view(&view);
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
                    if let Some(view) = self.state.get_call_view(call_id)
                        && let Some(rec) = view.recorder()
                    {
                        self.bridge.setup_recorder(rec.0);
                    }
                }
                Effect::AddControlChannel { sys_num, freq } => {
                    ffi::tr_system_add_control_channel_runtime(ctx.as_mut(), sys_num, freq.as_hz());
                }
                Effect::ConventionalRestart { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                            m.idle_timeout_count += 1;
                        });
                        let cfg = self.build_recorder_config_from_view(&view);
                        if let Some(handle) = self.handles.get_mut(&call_id) {
                            ffi::call_restart(handle.pin_mut(), &cfg);
                            // Update recorder number — C++ may assign a different one
                            let rec_num = if ffi::call_has_recorder(handle) {
                                Some(RecorderNum(ffi::call_get_recorder_num(handle)))
                            } else {
                                None
                            };
                            self.state.on_conventional_restarted(call_id, rec_num);
                        }
                    }
                }
                Effect::InitRecorder { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        let cfg = self.build_recorder_config_from_view(&view);
                        if let Some(handle) = self.handles.get_mut(&call_id) {
                            ffi::call_start_initial_recording(handle.pin_mut(), &cfg);
                        }
                    }
                }
                Effect::EnableRecorder { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        tracing::info!(
                            freq = %view.freq(),
                            talkgroup = view.talkgroup(),
                            "enabling recorder (signal detected)"
                        );
                        self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                            m.enable_count += 1;
                        });
                    }
                    if let Some(handle) = self.handles.get_mut(&call_id) {
                        ffi::call_set_recorder_enabled(handle.pin_mut(), true);
                    }
                }
                Effect::DisableRecorder { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        tracing::info!(
                            freq = %view.freq(),
                            talkgroup = view.talkgroup(),
                            "disabling recorder (signal lost)"
                        );
                        self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                            m.disable_count += 1;
                        });
                    }
                    if let Some(handle) = self.handles.get_mut(&call_id) {
                        ffi::call_set_recorder_enabled(handle.pin_mut(), false);
                    }
                }
                Effect::UpdateSquelch {
                    call_id,
                    threshold_db,
                } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        tracing::info!(
                            freq = %view.freq(),
                            talkgroup = view.talkgroup(),
                            threshold_db,
                            "adaptive squelch update"
                        );
                        self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                            m.adaptive_squelch_db = Some(threshold_db);
                        });
                    }
                    if let Some(handle) = self.handles.get_mut(&call_id) {
                        ffi::call_set_squelch_db(handle.pin_mut(), threshold_db);
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
        let view = match self.state.get_call_view(call_id) {
            Some(v) => v,
            None => return,
        };

        // Build a minimal TrTrunkMessage for C++ call creation (it only reads
        // freq, talkgroup, sys_num from it). Rust owns all call state.
        let ffi_msg = build_ffi_message_from_view(&view);
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
        let view = match self.state.get_call_view(call_id) {
            Some(v) => v,
            None => {
                tracing::error!(?call_id, "exec_request_recording: CallEntry missing");
                return;
            }
        };

        // Build recorder config from Rust state (replaces C++ build_recorder_config)
        let cfg = self.build_recorder_config_from_view(&view);
        // Capture values we need after the view borrow ends
        let talkgroup = view.talkgroup();
        let sys_num = view.sys_num();

        let freq_hz = freq.as_hz();
        let source_idx = self.find_source(freq_hz);

        let handle = match self.handles.get_mut(&call_id) {
            Some(h) => h,
            None => return,
        };

        let result = if let Some(source_idx) = source_idx {
            let available = ffi::tr_source_available_recorders(ctx, source_idx, is_analog);
            if priority > available {
                tracing::error!(
                    talkgroup,
                    sys_num,
                    priority,
                    available,
                    "not recording: priority exceeds available recorders"
                );
                RecordingResult::NoRecorder
            } else if ffi::tr_source_start_recording(
                ctx.as_mut(),
                source_idx,
                handle.pin_mut(),
                is_analog,
                &cfg,
            ) {
                let rec_num = ffi::call_get_recorder_num(handle);
                RecordingResult::Started {
                    rec_num: RecorderNum(rec_num),
                }
            } else {
                RecordingResult::NoRecorder
            }
        } else {
            RecordingResult::NoSource
        };

        if matches!(
            result,
            RecordingResult::NoRecorder | RecordingResult::NoSource
        ) {
            self.recorder_denied
                .add(1, &[KeyValue::new("sys_num", i64::from(sys_num))]);
        }

        let followup = self.state.on_recording_result(call_id, result);
        self.process_effects(followup, ctx);
    }

    fn exec_stop_recorder(&mut self, call_id: CallId) {
        let handle = match self.handles.get_mut(&call_id) {
            Some(h) => h,
            None => return,
        };

        // Consult the C++ Call — not the state machine — to decide whether a
        // recorder is attached. On retune, on_grant clears entry.recorder
        // *before* this StopRecording effect is processed, so a
        // `view.recorder().is_some()` guard here would always be false and the
        // old digital recorder would never be stopped, leaking the slot until
        // the source's recorder pool is exhausted.
        if !ffi::call_has_recorder(handle) {
            return;
        }
        let rec_num = ffi::call_get_recorder_num(handle);

        let metrics = ffi::call_stop_recording(handle.pin_mut());

        let mut was_idle = false;
        if let Some(view) = self.state.get_call_view(call_id) {
            was_idle = view.state() == CallState::Idle;
            let pref_ppm = self
                .preferred_ppm(view.freq().as_hz(), metrics.freq_error)
                .unwrap_or(f64::NAN);
            tracing::info!(
                short_name = %view.short_name(),
                rec_num,
                talkgroup = view.talkgroup(),
                freq = %view.freq(),
                tdma = view.phase2_tdma(),
                slot = view.tdma_slot(),
                freq_error = format_args!("{:+.0} Hz", metrics.freq_error),
                preferred_ppm = format_args!("{pref_ppm:.2}"),
                "stopping recorder"
            );
        }

        // Update signal/noise from the recorder's power probe. Works for every
        // recorder type (analog/P25/DMR all read xlat_channelizer::get_pwr);
        // a DB_UNSET reading is mapped to None inside feed_recorder_power.
        self.state
            .feed_recorder_power(call_id, metrics.recorder_pwr, was_idle);

        self.state.on_recorder_stopped(call_id);
    }

    fn exec_conclude_call(&mut self, call_id: CallId) {
        let view = match self.state.get_call_view(call_id) {
            Some(v) => v,
            None => return,
        };

        let state = view.state();
        let final_freq = view.freq();
        let is_conventional = view.is_conventional();
        let rec_num = view.recorder();

        // Signal-detection conventional channels transition to Disabled
        // inside on_conventional_idle_timeout() *before* effects are
        // processed, so we must also accept Disabled for conventional calls.
        let should_stop_recording = matches!(
            state,
            CallState::Recording | CallState::Idle | CallState::Rejected(RejectReason::Superseded)
        ) || (is_conventional && state == CallState::Disabled);

        if should_stop_recording {
            // Stop recorder and collect metrics
            let handle = match self.handles.get_mut(&call_id) {
                Some(h) => h,
                None => return,
            };

            if let Some(rec_num) = rec_num {
                let metrics = ffi::call_stop_recording(handle.pin_mut());

                // Feed recorder power back into state machine (all recorder
                // types; DB_UNSET maps to None inside feed_recorder_power).
                let was_idle = state == CallState::Idle;
                self.state
                    .feed_recorder_power(call_id, metrics.recorder_pwr, was_idle);

                let pref_ppm = self
                    .preferred_ppm(final_freq.as_hz(), metrics.freq_error)
                    .unwrap_or(f64::NAN);
                if let Some(view) = self.state.get_call_view(call_id) {
                    tracing::info!(
                        short_name = %view.short_name(),
                        rec_num = rec_num.0,
                        talkgroup = view.talkgroup(),
                        freq = %view.freq(),
                        tdma = view.phase2_tdma(),
                        slot = view.tdma_slot(),
                        freq_error = format_args!("{:+.0} Hz", metrics.freq_error),
                        preferred_ppm = format_args!("{pref_ppm:.2}"),
                        "stopping recorder"
                    );

                    let segments = self.build_proto_segments_final(call_id, final_freq, rec_num.0);
                    let recording_length = view.recording_duration().as_secs_f64();

                    let elapsed = view
                        .start_time()
                        .elapsed()
                        .map_or(0, |d| d.as_secs() as i64);
                    tracing::info!(
                        short_name = %view.short_name(),
                        call_num = view.call_num(),
                        talkgroup = view.talkgroup(),
                        elapsed,
                        recording_length,
                        segments = segments.len(),
                        "concluding recorded call"
                    );

                    if is_conventional {
                        self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                            m.recordings_completed += 1;
                        });
                    }

                    let handle = self.handles.get(&call_id).unwrap();
                    if metrics.is_dmr {
                        for slot in 0..2i32 {
                            let mut data = self.build_call_data(call_id);
                            data.tdma_slot = slot;
                            data.length = recording_length;
                            data.freq_error = metrics.freq_error;
                            data.transmission_list =
                                ffi::call_get_dmr_transmission_list(handle, slot);
                            self.bridge.call_end(&data, &segments, is_conventional);
                        }
                    } else {
                        let mut data = self.build_call_data(call_id);
                        data.length = recording_length;
                        data.freq_error = metrics.freq_error;
                        data.transmission_list = ffi::call_get_transmission_list(handle);
                        self.bridge.call_end(&data, &segments, is_conventional);
                    }
                }
            } else {
                // A call in a should-conclude state but with no recorder tracked
                // by the state machine (e.g. the recorder was dropped on a
                // retune that never re-acquired one). Audio for this call has
                // already streamed live to subscribers, so we MUST still emit
                // call_end or the call never finalizes downstream — it lingers
                // showing only a partial transcription. Stop any recorder the
                // C++ side still holds first, so we don't re-leak the slot.
                if ffi::call_has_recorder(handle) {
                    let _ = ffi::call_stop_recording(handle.pin_mut());
                }
                let talkgroup = self
                    .state
                    .get_call_view(call_id)
                    .map_or(0, |v| v.talkgroup());
                tracing::warn!(talkgroup, ?state, "concluding call with no recorder");
                let segments = self.build_proto_segments_final(call_id, final_freq, -1);
                let data = self.build_call_data(call_id);
                self.bridge.call_end(&data, &segments, is_conventional);
            }
        } else if matches!(state, CallState::Rejected(_)) {
            let segments = self.build_proto_segments_final(call_id, final_freq, -1);
            let data = self.build_call_data(call_id);
            self.bridge.call_end(&data, &segments, is_conventional);
        }
    }

    // ── Public API (delegates to state machine + processes effects) ──

    pub fn handle_message(&mut self, mut ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) {
        let rust_msg = Self::convert_message(msg);

        if !matches!(rust_msg.message_type, MessageType::InvalidCcMessage) {
            self.control_channel_messages
                .add(1, &[KeyValue::new("sys_num", i64::from(msg.sys_num))]);
        }

        // Sysid/Status need FFI call before state machine processes them
        let ffi_ok = match rust_msg.message_type {
            MessageType::Sysid => ffi::tr_system_update_sysid(ctx.as_mut(), msg),
            MessageType::Status => ffi::tr_system_update_status(ctx.as_mut(), msg),
            _ => true,
        };
        if !ffi_ok {
            return;
        }

        let effects = self.state.on_message(&rust_msg);
        self.process_effects(effects, &mut ctx);
    }

    pub fn tick(&mut self, mut ctx: Pin<&mut TrContext>) {
        // First, check recorder write times for recording calls (needs FFI).
        // The state machine can't check this, so we do it here.
        let timeout = self.state.config.call_timeout;
        let mut timed_out_ids = Vec::new();

        for view in self.state.iter_calls() {
            if view.is_conventional() || view.state() != CallState::Recording {
                continue;
            }
            if let Some(handle) = self.handles.get(&view.id()) {
                let since_write = ffi::call_recorder_since_last_write_secs(handle);
                if since_write > timeout {
                    timed_out_ids.push(view.id());
                }
            }
        }

        // Expire timed-out-by-write calls through the effect system
        for call_id in timed_out_ids {
            let effects = self.state.expire_call(call_id);
            self.process_effects(effects, &mut ctx);
        }

        // Poll instantaneous power from the squelch block for each conventional
        // analog call and feed it to the adaptive squelch tracker.
        let conv_analog: Vec<_> = self
            .state
            .iter_calls()
            .filter(|v| v.is_conventional() && v.is_analog())
            .map(|v| (v.id(), v.talkgroup(), v.freq()))
            .collect();

        for (call_id, tg, freq) in conv_analog {
            if let Some(handle) = self.handles.get(&call_id) {
                let pwr_db = ffi::call_get_pwr_db(handle);
                self.update_conv_metric(tg, freq, |m| {
                    m.last_power_reading_db = Some(pwr_db);
                });
                #[allow(clippy::cast_possible_truncation)]
                let effects = self.state.feed_power_sample(call_id, pwr_db as f32);
                self.process_effects(effects, &mut ctx);
            }
        }

        // Now run the state machine tick (handles update timeouts + conventional)
        let effects = self.state.tick();
        self.process_effects(effects, &mut ctx);

        self.active_calls_gauge
            .record(self.state.call_count() as u64, &[]);
        self.sync_conv_states();
    }

    pub fn add_call(
        &mut self,
        mut ctx: Pin<&mut TrContext>,
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

        let rec_num = if ffi::call_has_recorder(&call) {
            Some(RecorderNum(ffi::call_get_recorder_num(&call)))
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

        self.process_effects(effects, &mut ctx);
    }

    pub fn handle_squelch_event(
        &mut self,
        mut ctx: Pin<&mut TrContext>,
        recorder_num: i32,
        muted: bool,
        pwr_db: f64,
    ) {
        // Record signal quality metrics before mutating state
        if let Some(view) = self
            .state
            .iter_calls()
            .find(|v| v.is_conventional() && v.recorder() == Some(RecorderNum(recorder_num)))
        {
            let tg = view.talkgroup();
            let freq = view.freq();
            self.update_conv_metric(tg, freq, |m| {
                m.last_power_reading_db = Some(pwr_db);
            });
            if muted {
                self.update_conv_metric(tg, freq, |m| {
                    m.noise_floor_db = pwr_db;
                    m.squelch_close_count += 1;
                });
            } else {
                self.update_conv_metric(tg, freq, |m| {
                    m.signal_power_db = pwr_db;
                    m.squelch_open_count += 1;
                });
            }
        }

        let effects = self
            .state
            .on_squelch(RecorderNum(recorder_num), muted, pwr_db);
        self.process_effects(effects, &mut ctx);
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

            for view in self.state.iter_calls() {
                if view.is_conventional()
                    && view.signal_detection()
                    && (view.freq().as_hz() - abs_freq).abs() < 30_000.0
                {
                    self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                        m.detection_rssi_db = f64::from(signal.max_rssi);
                    });
                }
            }
        }
        let effects = self.state.on_signals_detected(source_center_freq, signals);
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
            message_count: 0,
            decode_rate: 0,
        }
    }

    /// Build a `TrRecorderConfig` from a [`CallView`] and system metadata.
    fn build_recorder_config_from_view(&self, view: &CallView<'_>) -> TrRecorderConfig {
        let policy = self
            .state
            .config
            .systems
            .get(&view.sys_num())
            .unwrap_or(&crate::call_state::DEFAULT_POLICY);

        let xor_mask = self
            .state
            .system_identities
            .get(&view.sys_num())
            .map_or_else(Vec::new, |id| id.xor_mask.clone());

        TrRecorderConfig {
            talkgroup: view.talkgroup(),
            call_num: view.call_num(),
            freq: view.freq().as_hz(),
            short_name: view.short_name().to_owned(),
            temp_dir: self.state.config.temp_dir.clone(),
            phase2_tdma: view.phase2_tdma(),
            tdma_slot: view.tdma_slot(),
            xor_mask,
            squelch_db: view.squelch_db(),
            digital_levels: policy.digital_levels,
            rust_call_id: view.id().as_u64(),
        }
    }

    pub fn print_status(&self, _ctx: Pin<&mut TrContext>) {
        tracing::info!(count = self.state.call_count(), "Active Calls");
        for view in self.state.iter_calls() {
            let elapsed = view
                .start_time()
                .elapsed()
                .map_or(0, |d| d.as_secs() as i64);
            let state = view.state().to_string();

            tracing::info!(
                system = %view.short_name(),
                call_num = view.call_num(),
                talkgroup = view.talkgroup(),
                freq = %view.freq(),
                elapsed,
                state,
                rec_num = view.recorder().map(|r| r.0),
                "call status"
            );
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

    // ── Helper: build TrCallInfo from CallView ──

    fn build_call_info_from_view(&self, view: &CallView<'_>) -> ffi::TrCallInfo {
        let elapsed = view.start_time().elapsed().unwrap_or_default();
        ffi::TrCallInfo {
            talkgroup: view.talkgroup(),
            call_num: view.call_num(),
            sys_num: view.sys_num(),
            freq: view.freq().as_hz(),
            signal: trunk_recorder_sys::signal_to_ffi(view.signal()),
            noise: trunk_recorder_sys::signal_to_ffi(view.noise()),
            freq_error: 0,
            encrypted: view.encrypted(),
            emergency: view.emergency(),
            is_analog: view.is_analog(),
            is_conventional: view.is_conventional(),
            phase2_tdma: view.phase2_tdma(),
            tdma_slot: view.tdma_slot(),
            priority: view.priority(),
            mode: view.mode(),
            duplex: view.duplex(),
            conversation_mode: view.trunked_fields().is_some_and(|t| t.conversation_mode),
            current_source_id: view.current_source_id(),
            start_time: crate::call_state::system_time_to_unix(view.start_time()),
            elapsed: elapsed.as_secs_f64(),
            short_name: view.short_name().to_owned(),
            call_state: view.state().to_proto(),
        }
    }

    fn build_call_data(&self, call_id: CallId) -> TrCallData {
        let view = self.state.get_call_view(call_id).unwrap();
        let audio_type = if view.is_analog() {
            "analog"
        } else if view.phase2_tdma() {
            "digital tdma"
        } else {
            "digital"
        };

        let now = crate::call_state::system_time_to_unix(std::time::SystemTime::now());

        TrCallData {
            talkgroup: view.talkgroup(),
            color_code: 0,
            patched_talkgroups: self
                .state
                .patch_tables
                .get(&view.sys_num())
                .map(|pt| {
                    pt.get_patch(Talkgroup(view.talkgroup()))
                        .into_iter()
                        .map(|t| t.0 as u64)
                        .collect()
                })
                .unwrap_or_default(),
            call_num: view.call_num(),
            freq: view.freq().as_hz(),
            freq_error: 0,
            source_num: 0,
            recorder_num: view.recorder().map_or(-1, |r| r.0),
            signal: trunk_recorder_sys::signal_to_ffi(view.signal()),
            noise: trunk_recorder_sys::signal_to_ffi(view.noise()),
            start_time: crate::call_state::system_time_to_unix(view.start_time()),
            stop_time: now,
            error_count: 0,
            spike_count: 0,
            encrypted: view.encrypted(),
            emergency: view.emergency(),
            priority: view.priority(),
            mode: view.mode(),
            duplex: view.duplex(),
            sys_num: view.sys_num(),
            short_name: view.short_name().to_owned(),
            audio_type: audio_type.into(),
            tdma_slot: view.tdma_slot(),
            length: 0.0,
            phase2_tdma: view.phase2_tdma(),
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
        let view = match self.state.get_call_view(call_id) {
            Some(v) => v,
            None => return vec![],
        };

        let mut segments: Vec<_> = view
            .segments()
            .iter()
            .map(|s| Self::make_proto_segment(s.freq.as_hz(), s.start, s.rec_num.0))
            .collect();
        // Append the still-open segment when concluding mid-recording. For
        // always-on conventional channels a transmission is concluded while
        // still in Recording with no prior pushed segment, so this is the one
        // segment that represents the keyed transmission.
        if view.is_trunked()
            || !view.segments().is_empty()
            || view.state() == CallState::Recording
        {
            segments.push(Self::make_proto_segment(
                final_freq.as_hz(),
                view.segment_start(),
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

/// Build a minimal `TrTrunkMessage` from a [`CallView`] for C++ call creation.
fn build_ffi_message_from_view(view: &CallView<'_>) -> TrTrunkMessage {
    TrTrunkMessage {
        message_type: TrMessageType::Grant,
        sys_num: view.sys_num(),
        talkgroup: view.talkgroup(),
        freq: view.freq().as_hz(),
        source: view.current_source_id(),
        tdma_slot: view.tdma_slot(),
        phase2_tdma: view.phase2_tdma(),
        mode: view.mode(),
        duplex: view.duplex(),
        emergency: view.emergency(),
        encrypted: view.encrypted(),
        priority: view.priority(),
        wacn: view.trunked_fields().map_or(0, |t| t.wacn),
        nac: 0,
        sys_id: 0,
        sys_rfss: view.trunked_fields().map_or(0, |t| t.rfss),
        sys_site_id: view.trunked_fields().map_or(0, |t| t.site_id),
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
