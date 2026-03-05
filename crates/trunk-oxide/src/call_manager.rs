use std::collections::HashMap;
use std::pin::Pin;
use std::time::Instant;

use cxx::UniquePtr;
use trunk_recorder_sys::SystemInfo;
use trunk_recorder_sys::ffi::{
    self, TrCall, TrCallData, TrCallDataStatus, TrContext, TrMessageType, TrTrunkMessage,
    TrUnitEventType,
};

use std::sync::Arc;

use trunk_oxide_grpc::EventBridge;

use crate::call_state::{
    CallId, CallState, Effect, Freq, MessageType, RecorderNum, RecordingResult, RejectReason,
    Talkgroup, TrunkMessage, TrunkStateMachine, UnitEventType,
};

pub use crate::call_state::{CallManagerConfig, ConventionalChannel, SystemPolicy, TalkgroupPolicy};

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
}

impl CallManager {
    pub fn new(config: CallManagerConfig, bridge: Arc<EventBridge>) -> Self {
        Self {
            state: TrunkStateMachine::new(config),
            bridge,
            handles: HashMap::new(),
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
                    if let Some(handle) = self.handles.get_mut(&call_id) {
                        ffi::call_set_freq(handle.pin_mut(), freq.as_hz());
                    }
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
                        let info = self.build_call_info(entry);
                        self.bridge.call_start(&info);
                    }
                }
                Effect::EmitCallEnd { call_id } => {
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
                    let info = self.get_system_info(ctx.as_mut(), sys_num);
                    self.bridge
                        .unit_event(&info, ffi_type, source_id, talkgroup);
                }
                Effect::EmitSetupSystem { sys_num } => {
                    // Also fire FFI update (Sysid/Status pass-through)
                    let info = self.get_system_info(ctx.as_mut(), sys_num);
                    self.bridge.setup_system(&info);
                }
                Effect::EmitSetupRecorder { call_id } => {
                    if let Some(handle) = self.handles.get(&call_id) {
                        ffi::call_notify_setup_recorder(ctx.as_mut(), handle);
                    }
                }
                Effect::AddControlChannel { sys_num, freq } => {
                    ffi::tr_system_add_control_channel_runtime(ctx.as_mut(), sys_num, freq.as_hz());
                }
                Effect::ConventionalRestart { call_id } => {
                    if let Some(handle) = self.handles.get_mut(&call_id) {
                        ffi::call_restart(handle.pin_mut());
                        self.state.on_conventional_restarted(call_id);
                        // Restore recorder after restart (exec_conclude_call clears it)
                        if ffi::call_has_recorder(&**handle) {
                            let rec_num = RecorderNum(ffi::call_get_recorder_num(&**handle));
                            if let Some(entry) = self.state.get_call_mut(call_id) {
                                entry.recorder = Some(rec_num);
                            }
                        }
                    }
                }
                Effect::ConventionalStartInitial { call_id } => {
                    if let Some(handle) = self.handles.get_mut(&call_id) {
                        ffi::call_start_initial_recording(handle.pin_mut());
                    }
                }
                Effect::RemoveCall { call_id } => {
                    self.handles.remove(&call_id);
                    self.state.remove_call(call_id);
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
        let handle = ffi::tr_context_create_call(ctx.as_mut(), &ffi_msg);
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
        let freq_hz = freq.as_hz();
        let handle = match self.handles.get_mut(&call_id) {
            Some(h) => h,
            None => return,
        };

        let source_count = ffi::tr_context_source_count(&**ctx);
        let mut result = RecordingResult::NoSource;

        for source_idx in 0..source_count {
            if !ffi::tr_source_covers_freq(&**ctx, source_idx, freq_hz) {
                continue;
            }

            let available = ffi::tr_source_available_recorders(&**ctx, source_idx, is_analog);
            if priority > available {
                let entry = self.state.get_call(call_id);
                if let Some(entry) = entry {
                    tracing::error!(
                        talkgroup = entry.talkgroup,
                        sys_num = entry.sys_num,
                        priority,
                        available,
                        "not recording: priority exceeds available recorders"
                    );
                }
                result = RecordingResult::NoRecorder;
                break;
            }

            if ffi::tr_source_start_recording(ctx.as_mut(), source_idx, handle.pin_mut(), is_analog)
            {
                let rec_num = ffi::call_get_recorder_num(&**handle);
                result = RecordingResult::Started {
                    rec_num: RecorderNum(rec_num),
                };
            } else {
                result = RecordingResult::NoRecorder;
            }
            break;
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
                if let Some(entry) = self.state.get_call_mut(call_id) {
                    if metrics.recorder_idle {
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
                        if metrics.recorder_idle {
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

                let elapsed = entry.start_time.elapsed().map_or(0, |d| d.as_secs() as i64);
                tracing::info!(
                    short_name = %entry.short_name,
                    call_num = entry.call_num,
                    talkgroup = entry.talkgroup,
                    elapsed,
                    final_length = metrics.final_length,
                    segments = segments.len(),
                    "concluding recorded call"
                );

                let handle = self.handles.get(&call_id).unwrap();
                if metrics.is_dmr {
                    for slot in 0..2i32 {
                        let mut data = self.build_call_data(call_id);
                        data.tdma_slot = slot;
                        data.length = metrics.final_length;
                        data.freq_error = metrics.freq_error;
                        data.transmission_list =
                            ffi::call_get_dmr_transmission_list(&**handle, slot);
                        self.bridge.call_end(&data, &segments);
                    }
                } else {
                    let mut data = self.build_call_data(call_id);
                    data.length = metrics.final_length;
                    data.freq_error = metrics.freq_error;
                    data.transmission_list = ffi::call_get_transmission_list(&**handle);
                    self.bridge.call_end(&data, &segments);
                }

                self.state.on_recorder_stopped(call_id);
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

        // Conclude timed-out-by-write calls before the state machine tick
        for call_id in &timed_out_ids {
            self.exec_conclude_call(*call_id);
            if let Some(handle) = self.handles.get(&*call_id) {
                ffi::call_notify_setup_recorder(ctx.as_mut(), handle);
            }
        }
        // Remove them from state
        self.state
            .calls
            .retain(|e| !timed_out_ids.contains(&e.id));
        if !timed_out_ids.is_empty() {
            let infos: Vec<ffi::TrCallInfo> = self
                .state
                .calls
                .iter()
                .map(|e| self.build_call_info(e))
                .collect();
            self.bridge.calls_active(&infos);
        }

        // Now run the state machine tick (handles update timeouts + conventional)
        let effects = self.state.tick();
        self.process_effects(effects, &mut ctx);

        // Conventional tick: check FFI state and call back into state machine
        self.tick_conventional(ctx);
    }

    fn tick_conventional(&mut self, mut ctx: Pin<&mut TrContext>) {
        let timeout = self.state.config.call_timeout;
        let mut actions: Vec<(CallId, ConventionalAction)> = Vec::new();

        for entry in &self.state.calls {
            if !entry.is_conventional() || entry.recorder.is_none() {
                continue;
            }

            let handle = match self.handles.get(&entry.id) {
                Some(h) => h,
                None => continue,
            };

            let length = ffi::call_get_current_length(&**handle);
            if length > 0.0 {
                let idle_timed_out = entry
                    .idle_since
                    .map(|t| t.elapsed().as_secs_f64() > timeout)
                    .unwrap_or(false);
                if idle_timed_out || (entry.max_duration > 0.0 && length > entry.max_duration) {
                    actions.push((entry.id, ConventionalAction::IdleTimeout));
                }
            } else if !ffi::call_get_recorder_is_active(&**handle) {
                actions.push((entry.id, ConventionalAction::StartInitial));
            }
        }

        for (call_id, action) in actions {
            match action {
                ConventionalAction::IdleTimeout => {
                    let effects = self.state.on_conventional_idle_timeout(call_id);
                    self.process_effects(effects, &mut ctx);
                }
                ConventionalAction::StartInitial => {
                    let effects = self.state.on_conventional_start_initial(call_id);
                    self.process_effects(effects, &mut ctx);
                }
            }
        }
    }

    pub fn process_recorder_queues(&self) {
        for handle in self.handles.values() {
            ffi::call_process_recorder_queue(&**handle);
        }
    }

    pub fn add_call(
        &mut self,
        call: UniquePtr<TrCall>,
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

        let call_id = self
            .state
            .add_conventional_call(channel, sys_num, &policy, rec_num);
        self.handles.insert(call_id, call);
    }

    pub fn handle_squelch_event(&mut self, recorder_num: i32, muted: bool, pwr_db: f64) {
        self.state
            .on_squelch(RecorderNum(recorder_num), muted, pwr_db);
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
        self.handles.clear();
    }

    pub(crate) fn get_system_info(&self, ctx: Pin<&mut TrContext>, sys_num: i32) -> SystemInfo {
        let ffi_info = ffi::tr_context_get_system_info(ctx, sys_num);
        let mut info = SystemInfo::from_ffi(&ffi_info);
        if let Some(policy) = self.state.config.systems.get(&sys_num) {
            info.short_name = policy.short_name.clone();
            info.conversation_mode = policy.conversation_mode;
        }
        if let Some(id) = self.state.system_identities.get(&sys_num) {
            info.sys_id = id.sys_id;
            info.wacn = id.wacn;
            info.nac = id.nac;
            info.rfss = id.rfss;
            info.site_id = id.site_id;
        }
        info
    }

    pub fn print_status(&self, mut ctx: Pin<&mut TrContext>) {
        tracing::info!(count = self.state.calls.len(), "Active Calls");
        for entry in &self.state.calls {
            let elapsed = entry.start_time.elapsed().map_or(0, |d| d.as_secs() as i64);
            let state = entry.state.to_string();

            if entry.is_conventional() {
                let handle = self.handles.get(&entry.id);
                let enabled = handle.map_or(false, |h| ffi::call_get_recorder_is_enabled(&**h));
                tracing::info!(
                    system = %entry.short_name,
                    call_num = entry.call_num,
                    talkgroup = entry.talkgroup,
                    freq = %entry.freq,
                    elapsed,
                    state,
                    enabled,
                    "call status"
                );
            } else {
                tracing::info!(
                    system = %entry.short_name,
                    call_num = entry.call_num,
                    talkgroup = entry.talkgroup,
                    freq = %entry.freq,
                    elapsed,
                    state,
                    "call status"
                );
            }

            if let Some(rec_num) = entry.recorder {
                if let Some(handle) = self.handles.get(&entry.id) {
                    let rec_state =
                        format_recorder_state(ffi::call_get_recorder_state(&**handle));
                    tracing::info!(rec_num = rec_num.0, rec_state, "  recorder");
                }
            }
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
            let mut info = self.get_system_info(ctx.as_mut(), sys_num);
            if policy.system_type.is_trunked() {
                let count = self.state.message_counts.remove(&sys_num).unwrap_or(0);
                let decode_rate = (count as f32 / time_diff).floor() as i32;
                self.state.decode_rates.insert(sys_num, decode_rate);
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

            if decode_rate < 2 {
                tracing::warn!(
                    sys_num,
                    decode_rate,
                    "low decode rate (multi-decoder, no retune)"
                );
            }

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
            freq_error: handle.map_or(0, |h| ffi::call_get_freq_error(&**h)),
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
            current_length: handle.map_or(0.0, |h| ffi::call_get_current_length(&**h)),
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
            freq_error: handle.map_or(0, |h| ffi::call_get_freq_error(&**h)),
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

enum ConventionalAction {
    IdleTimeout,
    StartInitial,
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
