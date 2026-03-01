use std::collections::HashMap;
use std::pin::Pin;
use std::time::Instant;

use cxx::UniquePtr;
use trunk_config::{DefaultMode, SystemType, TalkgroupMode};
use trunk_recorder_sys::ffi::{
    self, TrCall, TrCallData, TrCallDataStatus, TrContext, TrMessageType, TrMonitoringState,
    TrState, TrTrunkMessage,
};

/// Recording policy for a single talkgroup.
#[derive(Clone)]
pub struct TalkgroupPolicy {
    pub mode: TalkgroupMode,
    pub priority: i32,
}

/// Recording policy for a single system.
#[derive(Clone)]
pub struct SystemPolicy {
    pub system_type: SystemType,
    pub record_unknown: bool,
    pub hide_unknown: bool,
    pub hide_encrypted: bool,
    pub monitor_encrypted: bool,
    pub talkgroups: HashMap<i64, TalkgroupPolicy>,
}

/// Configuration extracted from trunk-config for call management decisions.
#[derive(Clone)]
pub struct CallManagerConfig {
    pub call_timeout: f64,
    pub new_call_from_update: bool,
    pub record_uu_v_calls: bool,
    pub default_mode: DefaultMode,
    pub systems: HashMap<i32, SystemPolicy>,
}

/// A single active call tracked by the Rust CallManager.
struct CallEntry {
    call: UniquePtr<TrCall>,
    /// When the squelch last closed (idle start) for conventional calls.
    /// None means the call is currently active (squelch open).
    idle_since: Option<Instant>,
}

/// Manages all call lifecycle: creation, update, timeout, and conclusion.
///
/// Trunked calls are dynamic (from control channel messages) and get erased on
/// timeout. Conventional calls are static (created at startup), never erased,
/// and cycle between recording states via conclude+restart.
pub struct CallManager {
    calls: Vec<CallEntry>,
    config: CallManagerConfig,
}

struct MultiSiteResult {
    is_duplicate: bool,
    is_superseding: bool,
    /// Index into `calls` of the original call (valid when `is_duplicate` is true).
    original_idx: Option<usize>,
}

impl CallManager {
    pub fn new(config: CallManagerConfig) -> Self {
        Self {
            calls: Vec::new(),
            config,
        }
    }

    // ── Call data construction ──

    fn build_call_data(call: &TrCall) -> TrCallData {
        let is_analog = ffi::call_get_is_analog(call);
        let phase2_tdma = ffi::call_get_phase2_tdma(call);
        let audio_type = if is_analog {
            "analog"
        } else if phase2_tdma {
            "digital tdma"
        } else {
            "digital"
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);

        TrCallData {
            talkgroup: ffi::call_get_talkgroup(call),
            color_code: 0,
            patched_talkgroups: ffi::call_get_patched_talkgroups(call),
            call_num: ffi::call_get_call_num(call),
            freq: ffi::call_get_freq(call),
            freq_error: ffi::call_get_freq_error(call),
            source_num: 0,
            recorder_num: ffi::call_get_recorder_num(call),
            signal: ffi::call_get_signal(call),
            noise: ffi::call_get_noise(call),
            start_time: ffi::call_get_start_time(call),
            stop_time: now,
            error_count: 0,
            spike_count: 0,
            encrypted: ffi::call_get_encrypted(call),
            emergency: ffi::call_get_emergency(call),
            priority: ffi::call_get_priority(call),
            mode: ffi::call_get_mode(call),
            duplex: ffi::call_get_duplex(call),
            sys_num: ffi::call_get_sys_num(call),
            short_name: ffi::call_get_short_name(call),
            audio_type: audio_type.into(),
            tdma_slot: ffi::call_get_tdma_slot(call),
            length: 0.0, // filled by caller with metrics
            phase2_tdma,
            transmission_source_list: Vec::new(),
            transmission_error_list: Vec::new(),
            transmission_list: Vec::new(), // filled by caller
            status: TrCallDataStatus::Initial,
        }
    }

    // ── Call conclusion (replaces C++ Call_impl::conclude_call) ──

    fn conclude_call(ctx: &mut Pin<&mut TrContext>, call: &mut UniquePtr<TrCall>) {
        let state = ffi::call_get_state(&**call);
        let monitoring = ffi::call_get_monitoring_state(&**call);

        let should_stop_recording = state == TrState::Recording
            || (state == TrState::Monitoring && monitoring == TrMonitoringState::Superseded);

        if should_stop_recording {
            let metrics = ffi::call_stop_recording(call.pin_mut());
            if metrics.has_recorder {
                // For conventional calls, update signal/noise from recorder power
                if ffi::call_is_conventional(&**call) {
                    if metrics.recorder_idle {
                        ffi::call_set_noise(call.pin_mut(), metrics.recorder_pwr);
                    } else {
                        ffi::call_set_signal(call.pin_mut(), metrics.recorder_pwr);
                    }
                }

                let short_name = ffi::call_get_short_name(&**call);
                let call_num = ffi::call_get_call_num(&**call);
                let talkgroup = ffi::call_get_talkgroup(&**call);
                let elapsed = ffi::call_elapsed(&**call);
                tracing::info!(
                    %short_name,
                    call_num,
                    talkgroup,
                    elapsed,
                    final_length = metrics.final_length,
                    "concluding recorded call"
                );

                if metrics.is_dmr {
                    // DMR: fire call_end twice, once per TDMA slot
                    for slot in 0..2i32 {
                        let mut data = Self::build_call_data(&**call);
                        data.tdma_slot = slot;
                        data.length = metrics.final_length;
                        data.freq_error = metrics.freq_error;
                        data.transmission_list =
                            ffi::call_get_dmr_transmission_list(&**call, slot);
                        ffi::tr_context_fire_call_end(ctx.as_mut(), &data);
                    }
                } else {
                    let mut data = Self::build_call_data(&**call);
                    data.length = metrics.final_length;
                    data.freq_error = metrics.freq_error;
                    data.transmission_list = ffi::call_get_transmission_list(&**call);
                    ffi::tr_context_fire_call_end(ctx.as_mut(), &data);
                }
            }
        } else if state == TrState::Monitoring {
            // Monitored-only calls: still notify plugin that the call ended
            let data = Self::build_call_data(&**call);
            ffi::tr_context_fire_call_end(ctx.as_mut(), &data);
        }
    }

    // ── Source tracking update (replaces C++ Call_impl::update + add_source) ──

    fn update_call_source(
        ctx: &mut Pin<&mut TrContext>,
        call: &mut UniquePtr<TrCall>,
        msg: &TrTrunkMessage,
    ) -> bool {
        ffi::call_touch(call.pin_mut());

        let src = msg.source;
        if src == -1 {
            return false;
        }
        let curr = ffi::call_get_current_source_id(&**call);
        if src == curr {
            return false;
        }

        ffi::call_set_current_source_id(call.pin_mut(), src);
        ffi::call_recorder_set_source(call.pin_mut(), src);
        ffi::tr_context_fire_signal(ctx.as_mut(), src, &**call);
        true
    }

    // ── Message dispatch ──

    pub fn handle_message(&mut self, ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) {
        match msg.message_type {
            TrMessageType::Grant => self.handle_grant(ctx, msg, true),
            TrMessageType::Update => {
                if self.config.new_call_from_update {
                    self.handle_grant(ctx, msg, false);
                } else {
                    self.handle_update(ctx, msg);
                }
            }
            TrMessageType::UuVGrant => {
                if self.config.record_uu_v_calls {
                    self.handle_grant(ctx, msg, true);
                }
            }
            TrMessageType::UuVUpdate => {
                if self.config.record_uu_v_calls {
                    self.handle_update(ctx, msg);
                }
            }
            // All other message types (CONTROL_CHANNEL, REGISTRATION, PATCH_*, etc.)
            // are handled inside C++ by tr_context_drain_messages.
            _ => {}
        }
    }

    // ── Grant handling ──

    fn handle_grant(
        &mut self,
        mut ctx: Pin<&mut TrContext>,
        msg: &TrTrunkMessage,
        is_grant_message: bool,
    ) {
        // Check for exact match on existing calls
        let mut call_found = false;
        for entry in &mut self.calls {
            let call = &*entry.call;
            if ffi::call_get_talkgroup(call) == msg.talkgroup
                && ffi::call_get_sys_num(call) == msg.sys_num
                && ffi::call_get_freq(call) == msg.freq
                && ffi::call_get_tdma_slot(call) == msg.tdma_slot
                && ffi::call_get_phase2_tdma(call) == msg.phase2_tdma
            {
                call_found = true;
                let source_updated =
                    Self::update_call_source(&mut ctx, &mut entry.call, msg);
                if source_updated {
                    ffi::tr_context_fire_call_start(ctx.as_mut(), &*entry.call);
                }
            }
        }

        if call_found {
            return;
        }

        // New call
        let multisite = self.check_multisite(&*ctx, msg.talkgroup, msg.sys_num, msg);
        let mut new_call = ffi::tr_context_create_call(ctx.as_mut(), msg);

        if multisite.is_superseding {
            let recording_started =
                self.try_start_recording(&mut ctx, &mut new_call, msg);

            if recording_started {
                if let Some(orig_idx) = multisite.original_idx {
                    let orig = &mut self.calls[orig_idx];
                    ffi::tr_call_set_state(orig.call.pin_mut(), TrState::Monitoring);
                    ffi::tr_call_set_monitoring_state(
                        orig.call.pin_mut(),
                        TrMonitoringState::Superseded,
                    );
                    Self::conclude_call(&mut ctx, &mut orig.call);
                }
            }
        } else if multisite.is_duplicate {
            ffi::tr_call_set_state(new_call.pin_mut(), TrState::Monitoring);
            ffi::tr_call_set_monitoring_state(new_call.pin_mut(), TrMonitoringState::Duplicate);
        } else {
            let recording_started =
                self.try_start_recording(&mut ctx, &mut new_call, msg);
            if recording_started && !is_grant_message {
                tracing::info!(
                    talkgroup = msg.talkgroup,
                    sys_num = msg.sys_num,
                    "this was an UPDATE"
                );
            }
        }

        self.calls.push(CallEntry {
            call: new_call,
            idle_since: None,
        });

        let last = self.calls.last().unwrap();
        ffi::tr_context_fire_call_start(ctx.as_mut(), &*last.call);

        let ptrs: Vec<*const TrCall> = self.calls.iter().map(|e| &*e.call as *const TrCall).collect();
        ffi::tr_context_fire_calls_active(ctx, &ptrs);
    }

    // ── Update handling ──

    fn handle_update(&mut self, mut ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) {
        for entry in &mut self.calls {
            let call = &*entry.call;
            if ffi::call_get_talkgroup(call) == msg.talkgroup
                && ffi::call_get_sys_num(call) == msg.sys_num
                && ffi::call_get_freq(call) == msg.freq
                && ffi::call_get_tdma_slot(call) == msg.tdma_slot
                && ffi::call_get_phase2_tdma(call) == msg.phase2_tdma
            {
                if msg.encrypted {
                    ffi::tr_call_set_encrypted(entry.call.pin_mut(), true);
                }

                let source_updated =
                    Self::update_call_source(&mut ctx, &mut entry.call, msg);
                if source_updated {
                    ffi::tr_context_fire_call_start(ctx.as_mut(), &*entry.call);
                }
            }
        }
    }

    // ── Recording policy ──

    fn try_start_recording(
        &self,
        ctx: &mut Pin<&mut TrContext>,
        call: &mut UniquePtr<TrCall>,
        msg: &TrTrunkMessage,
    ) -> bool {
        let sys_num = msg.sys_num;
        let tg_id = msg.talkgroup;

        let sys_policy = match self.config.systems.get(&sys_num) {
            Some(sp) => sp,
            None => return false,
        };

        let tg_policy = sys_policy.talkgroups.get(&tg_id);

        // Query runtime patch data from C++
        let patch_tgs = ffi::tr_context_get_talkgroup_patch(ctx, sys_num, tg_id);

        // 1. Unknown TG check
        if tg_policy.is_none() {
            let patched_with_known = patch_tgs
                .iter()
                .any(|&ptg| sys_policy.talkgroups.contains_key(&ptg));

            if !sys_policy.record_unknown && !patched_with_known {
                ffi::tr_call_set_state(call.pin_mut(), TrState::Monitoring);
                ffi::tr_call_set_monitoring_state(
                    call.pin_mut(),
                    TrMonitoringState::UnknownTg,
                );
                if !sys_policy.hide_unknown {
                    tracing::info!(
                        talkgroup = tg_id,
                        sys_num,
                        "not recording: TG not in talkgroup file"
                    );
                }
                return false;
            }
        }

        // 2. Encryption check
        let is_encrypted =
            msg.encrypted || tg_policy.map_or(false, |tg| tg.mode.is_encrypted());
        if is_encrypted {
            ffi::tr_call_set_encrypted(call.pin_mut(), true);
            if !sys_policy.monitor_encrypted {
                ffi::tr_call_set_state(call.pin_mut(), TrState::Monitoring);
                ffi::tr_call_set_monitoring_state(
                    call.pin_mut(),
                    TrMonitoringState::Encrypted,
                );
                if !sys_policy.hide_encrypted {
                    tracing::info!(
                        talkgroup = tg_id,
                        sys_num,
                        "not recording: encrypted"
                    );
                }
                return false;
            }
        }

        // 3. Determine recorder type (analog vs digital)
        let is_analog = if let Some(tg) = tg_policy {
            tg.mode == TalkgroupMode::Analog
        } else {
            self.config.default_mode == DefaultMode::Analog
                && sys_policy.system_type == SystemType::Smartnet
        };
        if is_analog {
            ffi::call_set_is_analog(call.pin_mut(), true);
        }

        // 4. Calculate priority (best among patched TGs)
        let priority = if let Some(tg) = tg_policy {
            let mut p = tg.priority;
            for &ptg_id in &patch_tgs {
                if let Some(ptg) = sys_policy.talkgroups.get(&ptg_id) {
                    if ptg.priority >= 0 && ptg.priority < p {
                        p = ptg.priority;
                    }
                }
            }
            p
        } else {
            0 // default priority for unknown TGs
        };

        // 5. Priority = -1 means disabled
        if priority == -1 {
            ffi::tr_call_set_state(call.pin_mut(), TrState::Monitoring);
            ffi::tr_call_set_monitoring_state(
                call.pin_mut(),
                TrMonitoringState::IgnoredTg,
            );
            tracing::info!(talkgroup = tg_id, sys_num, "not recording: priority disabled (-1)");
            return false;
        }

        // 6. Acquire recorder (C++ handles source finding, allocation, start)
        let result =
            ffi::tr_context_acquire_recorder(ctx.as_mut(), call.pin_mut(), is_analog, priority);

        if result.available < 0 {
            ffi::tr_call_set_state(call.pin_mut(), TrState::Monitoring);
            ffi::tr_call_set_monitoring_state(
                call.pin_mut(),
                TrMonitoringState::NoSource,
            );
            tracing::error!(talkgroup = tg_id, sys_num, "not recording: no source covering freq");
            return false;
        }

        if !result.started {
            if priority > result.available {
                ffi::tr_call_set_state(call.pin_mut(), TrState::Monitoring);
                ffi::tr_call_set_monitoring_state(
                    call.pin_mut(),
                    TrMonitoringState::NoRecorder,
                );
                tracing::error!(
                    talkgroup = tg_id,
                    sys_num,
                    priority,
                    available = result.available,
                    "not recording: priority exceeds available recorders"
                );
            } else {
                ffi::tr_call_set_state(call.pin_mut(), TrState::Monitoring);
            }
            return false;
        }

        true
    }

    // ── Multisite duplicate detection ──

    fn check_multisite(&self, ctx: &TrContext, talkgroup: i64, sys_num: i32, msg: &TrTrunkMessage) -> MultiSiteResult {
        let mut result = MultiSiteResult {
            is_duplicate: false,
            is_superseding: false,
            original_idx: None,
        };

        for (idx, entry) in self.calls.iter().enumerate() {
            let call = &*entry.call;

            if ffi::call_get_talkgroup(call) != talkgroup {
                continue;
            }
            if ffi::call_get_sys_num(call) == sys_num {
                continue;
            }
            if !ffi::call_get_system_multi_site(call) {
                continue;
            }
            if ffi::call_get_state(call) != TrState::Recording {
                continue;
            }

            let call_wacn = ffi::call_get_system_wacn(call);
            if call_wacn == 0 {
                continue;
            }

            let call_multi_site_name = ffi::call_get_system_multi_site_name(call);

            // Default mode: match WACN, different RFSS/Site, no multiSiteSystemName
            if call_multi_site_name.is_empty() {
                if call_wacn != msg.wacn {
                    continue;
                }

                let call_rfss = ffi::call_get_system_rfss(call);
                let call_site_id = ffi::call_get_system_site_id(call);
                let call_rfss_site = call_rfss as u64 * 10000 + call_site_id as u64;
                let msg_rfss_site = msg.sys_rfss as u64 * 10000 + msg.sys_site_id as u64;

                if msg_rfss_site == call_rfss_site {
                    continue;
                }

                result.is_duplicate = true;
                result.original_idx = Some(idx);

                // Supersede check: the C++ code compares both the existing call's
                // and the incoming system's preferred NAC. We only have accessors
                // for the existing call's system, not the incoming system. The
                // message carries nac but not the system's preferred NAC for this
                // talkgroup. We can only check the RFSS/Site-based supersede path.
                let call_preferred_nac = ffi::call_system_get_preferred_nac(call, talkgroup);
                if call_preferred_nac != call_rfss_site && msg.nac == msg_rfss_site {
                    result.is_superseding = true;
                }
            }
            // Secondary mode: match multiSiteSystemName.
            else {
                let msg_multi_site_name = ffi::tr_context_get_system_multi_site_name(ctx, sys_num);
                if !msg_multi_site_name.is_empty() && call_multi_site_name == msg_multi_site_name {
                    result.is_duplicate = true;
                    result.original_idx = Some(idx);
                }
            }
        }

        result
    }

    // ── Periodic tick (call timeout management) ──

    pub fn tick(&mut self, mut ctx: Pin<&mut TrContext>) {
        let mut ended_call = false;
        let timeout = self.config.call_timeout;

        self.calls.retain_mut(|entry| {
            let call = &*entry.call;
            let state = ffi::call_get_state(call);
            let since_update = ffi::call_since_last_update_secs(call);

            match state {
                TrState::Monitoring => {
                    if since_update > timeout {
                        Self::conclude_call(&mut ctx, &mut entry.call);
                        ended_call = true;
                        return false; // remove
                    }
                }
                TrState::Recording => {
                    // Check both recorder idle time and control channel update time
                    let since_write = ffi::call_recorder_since_last_write_secs(call);
                    if since_write > timeout || since_update > timeout {
                        Self::conclude_call(&mut ctx, &mut entry.call);
                        ffi::tr_context_fire_setup_recorder(ctx.as_mut(), &*entry.call);
                        ended_call = true;
                        return false;
                    }
                }
                _ => {
                    if since_update > timeout {
                        Self::conclude_call(&mut ctx, &mut entry.call);
                        ended_call = true;
                        return false;
                    }
                }
            }

            true // keep
        });

        if ended_call {
            let ptrs: Vec<*const TrCall> =
                self.calls.iter().map(|e| &*e.call as *const TrCall).collect();
            ffi::tr_context_fire_calls_active(ctx.as_mut(), &ptrs);
        }

        self.tick_conventional(ctx);
    }

    // ── Process P25 recorder message queues ──

    pub fn process_recorder_queues(&self) {
        for entry in &self.calls {
            ffi::call_process_recorder_queue(&*entry.call);
        }
    }

    // ── Conventional call management ──

    /// Add a conventional call drained from C++ during startup.
    pub fn add_call(&mut self, call: UniquePtr<TrCall>) {
        self.calls.push(CallEntry { call, idle_since: None });
    }

    /// Collect raw pointers to all tracked calls (for passing to C++ print_status / calls_active).
    pub fn call_ptrs(&self) -> Vec<*const TrCall> {
        self.calls.iter().map(|e| &*e.call as *const TrCall).collect()
    }

    /// Manage conventional call idle detection via squelch events, conclude+restart, and initial recording.
    fn tick_conventional(&mut self, mut ctx: Pin<&mut TrContext>) {
        for entry in &mut self.calls {
            if !ffi::call_is_conventional(&*entry.call) {
                continue;
            }
            if !ffi::call_has_recorder(&*entry.call) {
                continue;
            }

            // Drain squelch events that accumulated since last tick
            let events = ffi::call_drain_squelch_events(&*entry.call);
            for ev in &events {
                if ev.muted {
                    // Squelch closed — signal went away
                    ffi::call_set_noise(entry.call.pin_mut(), ev.pwr_db);
                    if entry.idle_since.is_none() {
                        entry.idle_since = Some(Instant::now());
                    }
                } else {
                    // Squelch opened — signal detected
                    ffi::call_set_signal(entry.call.pin_mut(), ev.pwr_db);
                    entry.idle_since = None;
                }
            }

            let length = ffi::call_get_current_length(&*entry.call);
            if length > 0.0 {
                // Check idle timeout using Rust-side timestamps
                let idle_timed_out = entry
                    .idle_since
                    .map(|t| t.elapsed().as_secs_f64() > self.config.call_timeout)
                    .unwrap_or(false);
                let max_dur = ffi::call_get_system_max_duration(&*entry.call);
                if idle_timed_out || (max_dur > 0.0 && length > max_dur) {
                    // Conclude current recording, restart for next transmission
                    Self::conclude_call(&mut ctx, &mut entry.call);
                    ffi::call_restart(entry.call.pin_mut());
                    entry.idle_since = None;
                    ffi::tr_context_fire_setup_recorder(ctx.as_mut(), &*entry.call);
                    ffi::tr_context_fire_call_start(ctx.as_mut(), &*entry.call);
                }
            } else if !ffi::call_get_recorder_is_active(&*entry.call) {
                // No recording yet and recorder not active — start initial recording
                ffi::call_start_initial_recording(entry.call.pin_mut());
                ffi::tr_call_set_state(entry.call.pin_mut(), TrState::Recording);
                ffi::tr_context_fire_call_start(ctx.as_mut(), &*entry.call);
            }
        }
    }

    // ── Shutdown ──

    pub fn conclude_all(&mut self, mut ctx: Pin<&mut TrContext>) {
        for entry in &mut self.calls {
            if ffi::call_get_state(&*entry.call) != TrState::Monitoring {
                Self::conclude_call(&mut ctx, &mut entry.call);
            }
        }
        self.calls.clear();

        let ptrs: Vec<*const TrCall> = Vec::new();
        ffi::tr_context_fire_calls_active(ctx.as_mut(), &ptrs);
    }
}
