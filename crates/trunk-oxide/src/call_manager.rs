use std::collections::HashMap;
use std::pin::Pin;
use std::time::Instant;

use cxx::UniquePtr;
use trunk_config::{DefaultMode, SystemType, TalkgroupMode};
use trunk_recorder_sys::ffi::{
    self, TrCall, TrCallData, TrCallDataStatus, TrContext, TrMessageType, TrMonitoringState,
    TrState, TrTrunkMessage, TrUnitEventType,
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
    pub control_retune_limit: i32,
    pub control_warn_rate: i32,
    pub systems: HashMap<i32, SystemPolicy>,
}

/// A single active call tracked by the Rust CallManager.
///
/// Rust owns all call state. The C++ `TrCall` handle is retained only for
/// recorder operations (`start`, `stop`, `restart`, etc.).
struct CallEntry {
    /// C++ Call — recorder handle only.
    handle: UniquePtr<TrCall>,

    // Immutable (set at creation)
    call_num: i64,
    talkgroup: i64,
    freq: f64,
    sys_num: i32,
    short_name: String,
    tdma_slot: i32,
    phase2_tdma: bool,
    mode: bool,
    duplex: bool,
    emergency: bool,
    is_conventional: bool,
    start_time: i64,

    // Mutable (Rust-owned, no C++ sync)
    state: TrState,
    monitoring_state: TrMonitoringState,
    encrypted: bool,
    is_analog: bool,
    current_source_id: i64,
    signal: f64,
    noise: f64,
    priority: i32,

    // Rust-only timing
    last_update: Instant,
    /// When the squelch last closed (idle start) for conventional calls.
    /// None means the call is currently active (squelch open).
    idle_since: Option<Instant>,

    // System-derived (cached at creation)
    multi_site: bool,
    wacn: u64,
    rfss: i32,
    site_id: i32,
    multi_site_name: String,
    conversation_mode: bool,
    max_duration: f64,
}

impl CallEntry {
    /// Create from a trunked grant message. Immutable fields come from the
    /// message; remaining fields from C++ metadata.
    fn from_message(call: UniquePtr<TrCall>, msg: &TrTrunkMessage) -> Self {
        let m = ffi::call_get_metadata(&*call);
        Self {
            handle: call,
            call_num: m.call_num,
            talkgroup: msg.talkgroup,
            freq: msg.freq,
            sys_num: msg.sys_num,
            short_name: m.short_name,
            tdma_slot: msg.tdma_slot,
            phase2_tdma: msg.phase2_tdma,
            mode: msg.mode,
            duplex: msg.duplex,
            emergency: msg.emergency,
            is_conventional: m.is_conventional,
            start_time: m.start_time,
            state: m.state,
            monitoring_state: m.monitoring_state,
            encrypted: msg.encrypted,
            is_analog: false,
            current_source_id: msg.source,
            signal: m.signal,
            noise: m.noise,
            priority: msg.priority,
            last_update: Instant::now(),
            idle_since: None,
            multi_site: m.multi_site,
            wacn: m.wacn,
            rfss: m.rfss,
            site_id: m.site_id,
            multi_site_name: m.multi_site_name,
            conversation_mode: m.conversation_mode,
            max_duration: m.max_duration,
        }
    }

    /// Create from an existing C++ call (conventional calls at startup).
    fn from_existing(call: UniquePtr<TrCall>) -> Self {
        let m = ffi::call_get_metadata(&*call);
        Self {
            handle: call,
            call_num: m.call_num,
            talkgroup: m.talkgroup,
            freq: m.freq,
            sys_num: m.sys_num,
            short_name: m.short_name,
            tdma_slot: m.tdma_slot,
            phase2_tdma: m.phase2_tdma,
            mode: m.mode,
            duplex: m.duplex,
            emergency: m.emergency,
            is_conventional: m.is_conventional,
            start_time: m.start_time,
            state: m.state,
            monitoring_state: m.monitoring_state,
            encrypted: m.encrypted,
            is_analog: m.is_analog,
            current_source_id: m.current_source_id,
            signal: m.signal,
            noise: m.noise,
            priority: m.priority,
            last_update: Instant::now(),
            idle_since: None,
            multi_site: m.multi_site,
            wacn: m.wacn,
            rfss: m.rfss,
            site_id: m.site_id,
            multi_site_name: m.multi_site_name,
            conversation_mode: m.conversation_mode,
            max_duration: m.max_duration,
        }
    }

    /// Refresh mutable fields after `call_restart()` (conventional calls).
    fn refresh_after_restart(&mut self) {
        let m = ffi::call_get_metadata(&*self.handle);
        self.call_num = m.call_num;
        self.start_time = m.start_time;
        self.state = m.state;
        self.monitoring_state = m.monitoring_state;
        self.last_update = Instant::now();
    }

    /// Build a lightweight `TrCallInfo` for event dispatch.
    fn to_call_info(&self) -> ffi::TrCallInfo {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        ffi::TrCallInfo {
            talkgroup: self.talkgroup,
            call_num: self.call_num,
            sys_num: self.sys_num,
            freq: self.freq,
            signal: self.signal,
            noise: self.noise,
            freq_error: ffi::call_get_freq_error(&*self.handle),
            encrypted: self.encrypted,
            emergency: self.emergency,
            is_analog: self.is_analog,
            is_conventional: self.is_conventional,
            phase2_tdma: self.phase2_tdma,
            tdma_slot: self.tdma_slot,
            priority: self.priority,
            mode: self.mode,
            duplex: self.duplex,
            conversation_mode: self.conversation_mode,
            current_source_id: self.current_source_id,
            current_length: ffi::call_get_current_length(&*self.handle),
            start_time: self.start_time,
            elapsed: (now_secs - self.start_time) as f64,
            short_name: self.short_name.clone(),
            state: self.state,
            monitoring_state: self.monitoring_state,
        }
    }
}

/// Manages all call lifecycle: creation, update, timeout, and conclusion.
///
/// Trunked calls are dynamic (from control channel messages) and get erased on
/// timeout. Conventional calls are static (created at startup), never erased,
/// and cycle between recording states via conclude+restart.
pub struct CallManager {
    calls: Vec<CallEntry>,
    config: CallManagerConfig,
    /// Per-system raw gr::message count (for decode rate calculation).
    message_counts: HashMap<i32, i32>,
    /// Per-system consecutive retune attempt counter.
    retune_attempts: HashMap<i32, i32>,
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
            message_counts: HashMap::new(),
            retune_attempts: HashMap::new(),
        }
    }

    // ── Call data construction ──

    fn build_call_data(entry: &CallEntry) -> TrCallData {
        let audio_type = if entry.is_analog {
            "analog"
        } else if entry.phase2_tdma {
            "digital tdma"
        } else {
            "digital"
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);

        TrCallData {
            talkgroup: entry.talkgroup,
            color_code: 0,
            patched_talkgroups: ffi::call_get_patched_talkgroups(&*entry.handle),
            call_num: entry.call_num,
            freq: entry.freq,
            freq_error: ffi::call_get_freq_error(&*entry.handle),
            source_num: 0,
            recorder_num: ffi::call_get_recorder_num(&*entry.handle),
            signal: entry.signal,
            noise: entry.noise,
            start_time: entry.start_time,
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
            length: 0.0, // filled by caller with metrics
            phase2_tdma: entry.phase2_tdma,
            transmission_source_list: Vec::new(),
            transmission_error_list: Vec::new(),
            transmission_list: Vec::new(), // filled by caller
            status: TrCallDataStatus::Initial,
        }
    }

    // ── Call conclusion (replaces C++ Call_impl::conclude_call) ──

    fn conclude_call(ctx: &mut Pin<&mut TrContext>, entry: &mut CallEntry) {
        let should_stop_recording = entry.state == TrState::Recording
            || (entry.state == TrState::Monitoring
                && entry.monitoring_state == TrMonitoringState::Superseded);

        if should_stop_recording {
            let metrics = ffi::call_stop_recording(entry.handle.pin_mut());
            if metrics.has_recorder {
                // For conventional calls, update signal/noise from recorder power
                if entry.is_conventional {
                    if metrics.recorder_idle {
                        entry.noise = metrics.recorder_pwr;
                    } else {
                        entry.signal = metrics.recorder_pwr;
                    }
                }

                let elapsed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64)
                    - entry.start_time;
                tracing::info!(
                    short_name = %entry.short_name,
                    call_num = entry.call_num,
                    talkgroup = entry.talkgroup,
                    elapsed,
                    final_length = metrics.final_length,
                    "concluding recorded call"
                );

                if metrics.is_dmr {
                    for slot in 0..2i32 {
                        let mut data = Self::build_call_data(entry);
                        data.tdma_slot = slot;
                        data.length = metrics.final_length;
                        data.freq_error = metrics.freq_error;
                        data.transmission_list =
                            ffi::call_get_dmr_transmission_list(&*entry.handle, slot);
                        ffi::tr_context_fire_call_end(ctx.as_mut(), &data);
                    }
                } else {
                    let mut data = Self::build_call_data(entry);
                    data.length = metrics.final_length;
                    data.freq_error = metrics.freq_error;
                    data.transmission_list = ffi::call_get_transmission_list(&*entry.handle);
                    ffi::tr_context_fire_call_end(ctx.as_mut(), &data);
                }
            }
        } else if entry.state == TrState::Monitoring {
            let data = Self::build_call_data(entry);
            ffi::tr_context_fire_call_end(ctx.as_mut(), &data);
        }
    }

    // ── Source tracking update (replaces C++ Call_impl::update + add_source) ──

    fn update_call_source(
        ctx: &mut Pin<&mut TrContext>,
        entry: &mut CallEntry,
        msg: &TrTrunkMessage,
    ) -> bool {
        entry.last_update = Instant::now();

        let src = msg.source;
        if src == -1 {
            return false;
        }
        if src == entry.current_source_id {
            return false;
        }

        entry.current_source_id = src;
        ffi::call_recorder_set_source(entry.handle.pin_mut(), src);
        ffi::tr_context_fire_signal(ctx.as_mut(), src, &entry.to_call_info());
        true
    }

    // ── Message dispatch ──

    pub fn handle_message(&mut self, mut ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) {
        // Count every raw message for decode rate calculation.
        *self.message_counts.entry(msg.sys_num).or_insert(0) += 1;

        match msg.message_type {
            // ── Counter adjustment ──
            TrMessageType::InvalidCcMessage => {
                if let Some(count) = self.message_counts.get_mut(&msg.sys_num) {
                    if *count > 1 {
                        *count -= 1;
                    }
                }
            }

            // ── System state mutations ──
            TrMessageType::ControlChannel => {
                ffi::tr_system_add_control_channel_runtime(ctx, msg.sys_num, msg.freq);
            }
            TrMessageType::PatchAdd => {
                ffi::tr_system_update_patches(ctx, msg.sys_num, &msg.patch_data);
            }
            TrMessageType::PatchDelete => {
                ffi::tr_system_delete_patch(ctx, msg.sys_num, &msg.patch_data);
            }
            TrMessageType::Tdulc => {
                ffi::tr_system_retune(ctx, msg.sys_num);
            }

            // ── System identity (once-only, fire setup_system if changed) ──
            TrMessageType::Sysid => {
                if ffi::tr_system_update_sysid(ctx.as_mut(), msg) {
                    ffi::tr_context_fire_setup_system(ctx, msg.sys_num);
                }
            }
            TrMessageType::Status => {
                if ffi::tr_system_update_status(ctx.as_mut(), msg) {
                    ffi::tr_context_fire_setup_system(ctx, msg.sys_num);
                }
            }

            // ── Unit-level plugin events ──
            TrMessageType::Registration => {
                ffi::tr_context_fire_unit_event(
                    ctx, msg.sys_num, TrUnitEventType::Registration, msg.source, 0,
                );
            }
            TrMessageType::Deregistration => {
                ffi::tr_context_fire_unit_event(
                    ctx, msg.sys_num, TrUnitEventType::Deregistration, msg.source, 0,
                );
            }
            TrMessageType::Affiliation => {
                ffi::tr_context_fire_unit_event(
                    ctx, msg.sys_num, TrUnitEventType::Affiliation, msg.source, msg.talkgroup,
                );
            }
            TrMessageType::Acknowledge => {
                ffi::tr_context_fire_unit_event(
                    ctx, msg.sys_num, TrUnitEventType::Acknowledge, msg.source, 0,
                );
            }
            TrMessageType::Location => {
                ffi::tr_context_fire_unit_event(
                    ctx, msg.sys_num, TrUnitEventType::Location, msg.source, msg.talkgroup,
                );
            }
            TrMessageType::DataGrant => {
                ffi::tr_context_fire_unit_event(
                    ctx, msg.sys_num, TrUnitEventType::DataGrant, msg.source, 0,
                );
            }
            TrMessageType::UuAnsReq => {
                ffi::tr_context_fire_unit_event(
                    ctx, msg.sys_num, TrUnitEventType::AnswerRequest, msg.source, msg.talkgroup,
                );
            }

            // ── Call-related messages (existing logic) ──
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
            if entry.talkgroup == msg.talkgroup
                && entry.sys_num == msg.sys_num
                && entry.freq == msg.freq
                && entry.tdma_slot == msg.tdma_slot
                && entry.phase2_tdma == msg.phase2_tdma
            {
                call_found = true;
                let source_updated =
                    Self::update_call_source(&mut ctx, entry, msg);
                if source_updated {
                    ffi::tr_context_fire_call_start(ctx.as_mut(), &entry.to_call_info());
                }
            }
        }

        if call_found {
            return;
        }

        // New call
        let multisite = self.check_multisite(&*ctx, msg.talkgroup, msg.sys_num, msg);
        let new_call = ffi::tr_context_create_call(ctx.as_mut(), msg);
        let mut new_entry = CallEntry::from_message(new_call, msg);

        if multisite.is_superseding {
            let recording_started =
                self.try_start_recording(&mut ctx, &mut new_entry, msg);

            if recording_started {
                if let Some(orig_idx) = multisite.original_idx {
                    let orig = &mut self.calls[orig_idx];
                    orig.state = TrState::Monitoring;
                    orig.monitoring_state = TrMonitoringState::Superseded;
                    Self::conclude_call(&mut ctx, orig);
                }
            }
        } else if multisite.is_duplicate {
            new_entry.state = TrState::Monitoring;
            new_entry.monitoring_state = TrMonitoringState::Duplicate;
        } else {
            let recording_started =
                self.try_start_recording(&mut ctx, &mut new_entry, msg);
            if recording_started && !is_grant_message {
                tracing::info!(
                    talkgroup = msg.talkgroup,
                    sys_num = msg.sys_num,
                    "this was an UPDATE"
                );
            }
        }

        self.calls.push(new_entry);

        let last = self.calls.last().unwrap();
        ffi::tr_context_fire_call_start(ctx.as_mut(), &last.to_call_info());

        let infos: Vec<ffi::TrCallInfo> = self.calls.iter().map(|e| e.to_call_info()).collect();
        ffi::tr_context_fire_calls_active(ctx, &infos);
    }

    // ── Update handling ──

    fn handle_update(&mut self, mut ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) {
        for entry in &mut self.calls {
            if entry.talkgroup == msg.talkgroup
                && entry.sys_num == msg.sys_num
                && entry.freq == msg.freq
                && entry.tdma_slot == msg.tdma_slot
                && entry.phase2_tdma == msg.phase2_tdma
            {
                if msg.encrypted {
                    entry.encrypted = true;
                }

                let source_updated =
                    Self::update_call_source(&mut ctx, entry, msg);
                if source_updated {
                    ffi::tr_context_fire_call_start(ctx.as_mut(), &entry.to_call_info());
                }
            }
        }
    }

    // ── Recording policy ──

    fn try_start_recording(
        &self,
        ctx: &mut Pin<&mut TrContext>,
        entry: &mut CallEntry,
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
                entry.state = TrState::Monitoring;
                entry.monitoring_state = TrMonitoringState::UnknownTg;
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
            entry.encrypted = true;
            if !sys_policy.monitor_encrypted {
                entry.state = TrState::Monitoring;
                entry.monitoring_state = TrMonitoringState::Encrypted;
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
        entry.is_analog = is_analog;

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
            entry.state = TrState::Monitoring;
            entry.monitoring_state = TrMonitoringState::IgnoredTg;
            tracing::info!(talkgroup = tg_id, sys_num, "not recording: priority disabled (-1)");
            return false;
        }

        // 6. Find source and acquire recorder
        let source_count = ffi::tr_context_source_count(&*ctx);
        let freq = entry.freq;
        let mut found_source = false;

        for source_idx in 0..source_count {
            if !ffi::tr_source_covers_freq(&*ctx, source_idx, freq) {
                continue;
            }
            found_source = true;

            let available = ffi::tr_source_available_recorders(&*ctx, source_idx, is_analog);
            if priority > available {
                entry.state = TrState::Monitoring;
                entry.monitoring_state = TrMonitoringState::NoRecorder;
                tracing::error!(
                    talkgroup = tg_id,
                    sys_num,
                    priority,
                    available,
                    "not recording: priority exceeds available recorders"
                );
                return false;
            }

            if ffi::tr_source_start_recording(
                ctx.as_mut(),
                source_idx,
                entry.handle.pin_mut(),
                is_analog,
            ) {
                entry.state = TrState::Recording;
                return true;
            }

            // Recorder allocation or start failed
            entry.state = TrState::Monitoring;
            return false;
        }

        if !found_source {
            entry.state = TrState::Monitoring;
            entry.monitoring_state = TrMonitoringState::NoSource;
            tracing::error!(talkgroup = tg_id, sys_num, "not recording: no source covering freq");
        }
        false
    }

    // ── Multisite duplicate detection ──

    fn check_multisite(&self, ctx: &TrContext, talkgroup: i64, sys_num: i32, msg: &TrTrunkMessage) -> MultiSiteResult {
        let mut result = MultiSiteResult {
            is_duplicate: false,
            is_superseding: false,
            original_idx: None,
        };

        for (idx, entry) in self.calls.iter().enumerate() {
            if entry.talkgroup != talkgroup {
                continue;
            }
            if entry.sys_num == sys_num {
                continue;
            }
            if !entry.multi_site {
                continue;
            }
            if entry.state != TrState::Recording {
                continue;
            }

            if entry.wacn == 0 {
                continue;
            }

            // Default mode: match WACN, different RFSS/Site, no multiSiteSystemName
            if entry.multi_site_name.is_empty() {
                if entry.wacn != msg.wacn {
                    continue;
                }

                let call_rfss_site = entry.rfss as u64 * 10000 + entry.site_id as u64;
                let msg_rfss_site = msg.sys_rfss as u64 * 10000 + msg.sys_site_id as u64;

                if msg_rfss_site == call_rfss_site {
                    continue;
                }

                result.is_duplicate = true;
                result.original_idx = Some(idx);

                // Supersede check
                let call_preferred_nac = ffi::call_system_get_preferred_nac(&*entry.handle, talkgroup);
                if call_preferred_nac != call_rfss_site && msg.nac == msg_rfss_site {
                    result.is_superseding = true;
                }
            }
            // Secondary mode: match multiSiteSystemName.
            else {
                let msg_multi_site_name = ffi::tr_context_get_system_multi_site_name(ctx, sys_num);
                if !msg_multi_site_name.is_empty() && entry.multi_site_name == msg_multi_site_name {
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
            let since_update = entry.last_update.elapsed().as_secs_f64();

            match entry.state {
                TrState::Monitoring => {
                    if since_update > timeout {
                        Self::conclude_call(&mut ctx, entry);
                        ended_call = true;
                        return false; // remove
                    }
                }
                TrState::Recording => {
                    let since_write = ffi::call_recorder_since_last_write_secs(&*entry.handle);
                    if since_write > timeout || since_update > timeout {
                        Self::conclude_call(&mut ctx, entry);
                        ffi::tr_context_fire_setup_recorder(ctx.as_mut(), &*entry.handle);
                        ended_call = true;
                        return false;
                    }
                }
                _ => {
                    if since_update > timeout {
                        Self::conclude_call(&mut ctx, entry);
                        ended_call = true;
                        return false;
                    }
                }
            }

            true // keep
        });

        if ended_call {
            let infos: Vec<ffi::TrCallInfo> =
                self.calls.iter().map(|e| e.to_call_info()).collect();
            ffi::tr_context_fire_calls_active(ctx.as_mut(), &infos);
        }

        self.tick_conventional(ctx);
    }

    // ── Process P25 recorder message queues ──

    pub fn process_recorder_queues(&self) {
        for entry in &self.calls {
            ffi::call_process_recorder_queue(&*entry.handle);
        }
    }

    // ── Conventional call management ──

    /// Add a conventional call drained from C++ during startup.
    pub fn add_call(&mut self, call: UniquePtr<TrCall>) {
        self.calls.push(CallEntry::from_existing(call));
    }

    /// Collect raw pointers to all tracked calls (for passing to C++ print_status / calls_active).
    pub fn call_ptrs(&self) -> Vec<*const TrCall> {
        self.calls.iter().map(|e| &*e.handle as *const TrCall).collect()
    }

    /// Manage conventional call idle detection via squelch events, conclude+restart, and initial recording.
    fn tick_conventional(&mut self, mut ctx: Pin<&mut TrContext>) {
        for entry in &mut self.calls {
            if !entry.is_conventional {
                continue;
            }
            if !ffi::call_has_recorder(&*entry.handle) {
                continue;
            }

            // Drain squelch events that accumulated since last tick
            let events = ffi::call_drain_squelch_events(&*entry.handle);
            for ev in &events {
                if ev.muted {
                    // Squelch closed — signal went away
                    entry.noise = ev.pwr_db;
                    if entry.idle_since.is_none() {
                        entry.idle_since = Some(Instant::now());
                    }
                } else {
                    // Squelch opened — signal detected
                    entry.signal = ev.pwr_db;
                    entry.idle_since = None;
                }
            }

            let length = ffi::call_get_current_length(&*entry.handle);
            if length > 0.0 {
                // Check idle timeout using Rust-side timestamps
                let idle_timed_out = entry
                    .idle_since
                    .map(|t| t.elapsed().as_secs_f64() > self.config.call_timeout)
                    .unwrap_or(false);
                if idle_timed_out || (entry.max_duration > 0.0 && length > entry.max_duration) {
                    // Conclude current recording, restart for next transmission
                    Self::conclude_call(&mut ctx, entry);
                    ffi::call_restart(entry.handle.pin_mut());
                    entry.refresh_after_restart();
                    entry.idle_since = None;
                    ffi::tr_context_fire_setup_recorder(ctx.as_mut(), &*entry.handle);
                    ffi::tr_context_fire_call_start(ctx.as_mut(), &entry.to_call_info());
                }
            } else if !ffi::call_get_recorder_is_active(&*entry.handle) {
                // No recording yet and recorder not active — start initial recording
                ffi::call_start_initial_recording(entry.handle.pin_mut());
                entry.state = TrState::Recording;
                ffi::tr_context_fire_call_start(ctx.as_mut(), &entry.to_call_info());
            }
        }
    }

    // ── Shutdown ──

    /// Check decode rates for all trunked systems and retune if needed.
    /// Called every ~3 seconds from `run_monitor`.
    pub fn check_decode_rates(
        &mut self,
        mut ctx: Pin<&mut TrContext>,
        time_diff: f32,
    ) -> anyhow::Result<()> {
        ffi::tr_context_fire_setup_config(ctx.as_mut());
        ffi::tr_context_fire_system_rates(ctx.as_mut(), time_diff);

        for (&sys_num, policy) in &self.config.systems {
            if !policy.system_type.is_trunked() {
                continue;
            }

            let count = self.message_counts.remove(&sys_num).unwrap_or(0);
            let decode_rate = (count as f32 / time_diff).floor() as i32;
            ffi::tr_system_set_decode_rate(ctx.as_mut(), sys_num, decode_rate);

            if decode_rate < 2 {
                let attempts = self.retune_attempts.entry(sys_num).or_insert(0);
                *attempts += 1;
                if self.config.control_retune_limit > 0
                    && *attempts > self.config.control_retune_limit
                {
                    anyhow::bail!(
                        "Control channel retune limit exceeded after {} tries for sys {}",
                        attempts,
                        sys_num
                    );
                }
                if ffi::tr_system_control_channel_count(ctx.as_mut(), sys_num) > 1 {
                    ffi::tr_system_retune(ctx.as_mut(), sys_num);
                } else {
                    tracing::error!(sys_num, "only one control channel defined");
                }
            } else {
                self.retune_attempts.insert(sys_num, 0);
            }

            if decode_rate < self.config.control_warn_rate {
                let freq = ffi::tr_system_get_current_control_channel(ctx.as_mut(), sys_num);
                tracing::warn!(
                    sys_num,
                    freq_mhz = freq / 1e6,
                    decode_rate,
                    count,
                    "low decode rate",
                );
            } else if self.config.control_warn_rate == -1 {
                let freq = ffi::tr_system_get_current_control_channel(ctx.as_mut(), sys_num);
                tracing::info!(
                    sys_num,
                    freq_mhz = freq / 1e6,
                    decode_rate,
                    count,
                    "decode rate",
                );
            }
        }
        Ok(())
    }

    pub fn conclude_all(&mut self, mut ctx: Pin<&mut TrContext>) {
        for entry in &mut self.calls {
            if entry.state != TrState::Monitoring {
                Self::conclude_call(&mut ctx, entry);
            }
        }
        self.calls.clear();

        let empty: Vec<ffi::TrCallInfo> = Vec::new();
        ffi::tr_context_fire_calls_active(ctx.as_mut(), &empty);
    }
}
