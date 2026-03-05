use std::collections::HashMap;
use std::pin::Pin;
use std::time::Instant;

use cxx::UniquePtr;
use trunk_config::{DefaultMode, SystemType, TalkgroupMode};
use trunk_recorder_sys::SystemInfo;
use trunk_recorder_sys::ffi::{
    self, TrCall, TrCallData, TrCallDataStatus, TrContext, TrMessageType, TrMonitoringState,
    TrState, TrTrunkMessage, TrUnitEventType,
};

use std::sync::Arc;

use trunk_oxide_grpc::EventBridge;

/// Per-system patch table: maps supergroup → { talkgroup → last_update }.
///
/// Trunked P25 systems dynamically patch talkgroups together via control
/// channel messages.  Patches expire after 10 seconds without a refresh.
struct PatchTable {
    patches: HashMap<u64, HashMap<u64, Instant>>,
}

impl PatchTable {
    fn new() -> Self {
        Self {
            patches: HashMap::new(),
        }
    }

    /// Add/update talkgroups in a patch (from PatchAdd message).
    fn update(&mut self, pd: &ffi::TrPatchData) {
        let now = Instant::now();
        let patch = self.patches.entry(pd.sg).or_default();
        for &tg in &[pd.sg, pd.ga1, pd.ga2, pd.ga3] {
            if tg != 0 {
                patch.insert(tg, now);
            }
        }
    }

    /// Remove ga1/ga2/ga3 from a patch (from PatchDelete message).
    fn delete(&mut self, pd: &ffi::TrPatchData) {
        if let Some(patch) = self.patches.get_mut(&pd.sg) {
            for &tg in &[pd.ga1, pd.ga2, pd.ga3] {
                if tg != 0 {
                    patch.remove(&tg);
                }
            }
        }
    }

    /// Remove entries older than 10 seconds, prune empty patches.
    fn clear_stale(&mut self) {
        self.patches.retain(|_, patch| {
            patch.retain(|_, last_update| last_update.elapsed().as_secs() < 10);
            !patch.is_empty()
        });
    }

    /// Log all active patches for a given system short name.
    fn log_patches(&self, short_name: &str) {
        tracing::info!(
            system = short_name,
            count = self.patches.len(),
            "active talkgroup patches"
        );
        for (sg, members) in &self.patches {
            let tgids: Vec<u64> = members.keys().copied().collect();
            tracing::info!(supergroup = sg, ?tgids, "active patch");
        }
    }

    /// Get all talkgroups in the same patch as the given talkgroup.
    fn get_patch(&self, talkgroup: i64) -> Vec<i64> {
        let tg = talkgroup as u64;
        for patch in self.patches.values() {
            if patch.contains_key(&tg) {
                return patch.keys().map(|&k| k as i64).collect();
            }
        }
        Vec::new()
    }
}

fn format_freq(f: f64) -> String {
    format!("{:.4} MHz", f / 1_000_000.0)
}

fn format_call_state(state: TrState, monitoring: TrMonitoringState) -> &'static str {
    match state {
        TrState::Monitoring => match monitoring {
            TrMonitoringState::UnknownTg => "Monitoring: UNKNOWN TG",
            TrMonitoringState::IgnoredTg => "Monitoring: IGNORED TG",
            TrMonitoringState::NoSource => "Monitoring: NO SOURCE",
            TrMonitoringState::NoRecorder => "Monitoring: NO RECORDER",
            TrMonitoringState::Encrypted => "Monitoring: ENCRYPTED",
            TrMonitoringState::Duplicate => "Monitoring: DUPLICATE",
            TrMonitoringState::Superseded => "Monitoring: SUPERSEDED",
            _ => "Monitoring",
        },
        TrState::Recording => "Recording",
        _ => "Unknown",
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

/// Recording policy for a single talkgroup.
#[derive(Clone)]
pub struct TalkgroupPolicy {
    pub mode: TalkgroupMode,
    pub priority: i32,
    pub preferred_nac: u64,
}

/// Recording policy for a single system.
#[derive(Clone)]
pub struct SystemPolicy {
    pub system_type: SystemType,
    pub short_name: String,
    pub record_unknown: bool,
    pub hide_unknown: bool,
    pub hide_encrypted: bool,
    pub monitor_encrypted: bool,
    pub multi_site: bool,
    pub multi_site_name: String,
    pub conversation_mode: bool,
    pub max_duration: f64,
    pub control_channels: Vec<f64>,
    pub talkgroups: HashMap<i64, TalkgroupPolicy>,
    /// Initial P25 identity from config (for LFSR / XOR mask).
    pub sys_id: u64,
    pub wacn: u64,
    pub nac: u64,
}

static DEFAULT_POLICY: std::sync::LazyLock<SystemPolicy> =
    std::sync::LazyLock::new(|| SystemPolicy {
        system_type: SystemType::P25,
        short_name: String::new(),
        record_unknown: false,
        hide_unknown: false,
        hide_encrypted: false,
        monitor_encrypted: false,
        multi_site: false,
        multi_site_name: String::new(),
        conversation_mode: false,
        max_duration: 0.0,
        control_channels: Vec::new(),
        talkgroups: HashMap::new(),
        sys_id: 0,
        wacn: 0,
        nac: 0,
    });

/// Mutable P25 identity fields, updated once at runtime from trunk messages.
struct SystemIdentity {
    sys_id: u64,
    wacn: u64,
    nac: u64,
    rfss: i32,
    site_id: i32,
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

/// A recorder currently assigned to a call. Consuming `stop()` ensures the
/// recorder number cannot be used after the C++ recorder is released.
struct Recorder {
    rec_num: i32,
}

impl Recorder {
    /// Stop recording on the associated call, consuming this handle.
    fn stop(self, call: Pin<&mut TrCall>) -> ffi::TrStopRecordingResult {
        ffi::call_stop_recording(call)
    }
}

/// Fields shared by all call types (trunked and conventional).
struct CallCommon {
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
    /// The recorder assigned to this call, if any.
    recorder: Option<Recorder>,

    // Rust-only timing
    last_update: Instant,

    // System-derived (cached at creation)
    max_duration: f64,
}

impl From<UniquePtr<TrCall>> for CallCommon {
    fn from(call: UniquePtr<TrCall>) -> Self {
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
            start_time: m.start_time,
            state: m.state,
            monitoring_state: m.monitoring_state,
            encrypted: m.encrypted,
            is_analog: m.is_analog,
            current_source_id: m.current_source_id,
            signal: m.signal,
            noise: m.noise,
            priority: m.priority,
            recorder: None,
            last_update: Instant::now(),
            max_duration: 0.0,
        }
    }
}

/// P25 multisite fields — only meaningful for trunked calls.
struct TrunkedFields {
    multi_site: bool,
    wacn: u64,
    rfss: i32,
    site_id: i32,
    multi_site_name: String,
    conversation_mode: bool,
}

/// Squelch-based idle tracking — only meaningful for conventional calls.
struct ConventionalFields {
    /// When the squelch last closed (idle start). None = squelch open / active.
    idle_since: Option<Instant>,
}

/// A call entry that forces callers to handle trunked vs conventional via match.
enum CallEntry {
    Trunked {
        common: CallCommon,
        trunked: TrunkedFields,
    },
    Conventional {
        common: CallCommon,
        conv: ConventionalFields,
    },
}

impl CallEntry {
    fn common(&self) -> &CallCommon {
        match self {
            CallEntry::Trunked { common, .. } | CallEntry::Conventional { common, .. } => common,
        }
    }

    fn common_mut(&mut self) -> &mut CallCommon {
        match self {
            CallEntry::Trunked { common, .. } | CallEntry::Conventional { common, .. } => common,
        }
    }

    /// Create from a trunked grant message.
    fn new_trunked(call: UniquePtr<TrCall>, msg: &TrTrunkMessage, policy: &SystemPolicy) -> Self {
        let mut common = CallCommon::from(call);
        // Override fields that come from the message rather than C++ metadata
        common.talkgroup = msg.talkgroup;
        common.freq = msg.freq;
        common.tdma_slot = msg.tdma_slot;
        common.phase2_tdma = msg.phase2_tdma;
        common.mode = msg.mode;
        common.duplex = msg.duplex;
        common.emergency = msg.emergency;
        common.encrypted = msg.encrypted;
        common.is_analog = false;
        common.current_source_id = msg.source;
        common.priority = msg.priority;
        common.max_duration = policy.max_duration;
        CallEntry::Trunked {
            common,
            trunked: TrunkedFields {
                multi_site: policy.multi_site,
                wacn: msg.wacn,
                rfss: msg.sys_rfss,
                site_id: msg.sys_site_id,
                multi_site_name: policy.multi_site_name.clone(),
                conversation_mode: policy.conversation_mode,
            },
        }
    }

    /// Create from an existing C++ call (conventional calls at startup).
    fn new_conventional(call: UniquePtr<TrCall>, policy: &SystemPolicy) -> Self {
        let has_recorder = ffi::call_has_recorder(&*call);
        let rec_num = if has_recorder {
            Some(ffi::call_get_recorder_num(&*call))
        } else {
            None
        };
        let mut common = CallCommon::from(call);
        common.max_duration = policy.max_duration;
        common.recorder = rec_num.map(|rec_num| Recorder { rec_num });
        CallEntry::Conventional {
            common,
            conv: ConventionalFields { idle_since: None },
        }
    }

    /// Build a lightweight `TrCallInfo` for event dispatch.
    fn to_call_info(&self) -> ffi::TrCallInfo {
        let c = self.common();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        ffi::TrCallInfo {
            talkgroup: c.talkgroup,
            call_num: c.call_num,
            sys_num: c.sys_num,
            freq: c.freq,
            signal: c.signal,
            noise: c.noise,
            freq_error: ffi::call_get_freq_error(&*c.handle),
            encrypted: c.encrypted,
            emergency: c.emergency,
            is_analog: c.is_analog,
            is_conventional: matches!(self, CallEntry::Conventional { .. }),
            phase2_tdma: c.phase2_tdma,
            tdma_slot: c.tdma_slot,
            priority: c.priority,
            mode: c.mode,
            duplex: c.duplex,
            conversation_mode: match self {
                CallEntry::Trunked { trunked, .. } => trunked.conversation_mode,
                CallEntry::Conventional { .. } => false,
            },
            current_source_id: c.current_source_id,
            current_length: ffi::call_get_current_length(&*c.handle),
            start_time: c.start_time,
            elapsed: (now_secs - c.start_time) as f64,
            short_name: c.short_name.clone(),
            state: c.state,
            monitoring_state: c.monitoring_state,
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
    bridge: Arc<EventBridge>,
    /// Per-system raw gr::message count (for decode rate calculation).
    message_counts: HashMap<i32, i32>,
    /// Per-system consecutive retune attempt counter.
    retune_attempts: HashMap<i32, i32>,
    /// Per-system computed decode rate (messages/sec, for event dispatch).
    decode_rates: HashMap<i32, i32>,
    /// Per-system patch tables (P25 systems only).
    patch_tables: HashMap<i32, PatchTable>,
    /// Per-system P25 identity (seeded from config, updated once from trunk messages).
    system_identities: HashMap<i32, SystemIdentity>,
}

struct MultiSiteResult {
    is_duplicate: bool,
    is_superseding: bool,
    /// Index into `calls` of the original call (valid when `is_duplicate` is true).
    original_idx: Option<usize>,
}

impl CallManager {
    pub fn new(config: CallManagerConfig, bridge: Arc<EventBridge>) -> Self {
        let patch_tables = config
            .systems
            .iter()
            .filter(|(_, sp)| sp.system_type == SystemType::P25)
            .map(|(&sys_num, _)| (sys_num, PatchTable::new()))
            .collect();
        let system_identities = config
            .systems
            .iter()
            .map(|(&sys_num, sp)| {
                (
                    sys_num,
                    SystemIdentity {
                        sys_id: sp.sys_id,
                        wacn: sp.wacn,
                        nac: sp.nac,
                        rfss: 0,
                        site_id: 0,
                    },
                )
            })
            .collect();
        Self {
            calls: Vec::new(),
            config,
            bridge,
            message_counts: HashMap::new(),
            retune_attempts: HashMap::new(),
            decode_rates: HashMap::new(),
            patch_tables,
            system_identities,
        }
    }

    /// Build full SystemInfo from C++ DSP fields + Rust-owned metadata.
    pub(crate) fn get_system_info(&self, ctx: Pin<&mut TrContext>, sys_num: i32) -> SystemInfo {
        let ffi_info = ffi::tr_context_get_system_info(ctx, sys_num);
        let mut info = SystemInfo::from_ffi(&ffi_info);
        if let Some(policy) = self.config.systems.get(&sys_num) {
            info.short_name = policy.short_name.clone();
            info.conversation_mode = policy.conversation_mode;
        }
        if let Some(id) = self.system_identities.get(&sys_num) {
            info.sys_id = id.sys_id;
            info.wacn = id.wacn;
            info.nac = id.nac;
            info.rfss = id.rfss;
            info.site_id = id.site_id;
        }
        info
    }

    // ── Call data construction ──

    fn build_call_data(c: &CallCommon, patches: &HashMap<i32, PatchTable>) -> TrCallData {
        let audio_type = if c.is_analog {
            "analog"
        } else if c.phase2_tdma {
            "digital tdma"
        } else {
            "digital"
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);

        TrCallData {
            talkgroup: c.talkgroup,
            color_code: 0,
            patched_talkgroups: patches
                .get(&c.sys_num)
                .map(|pt| {
                    pt.get_patch(c.talkgroup)
                        .into_iter()
                        .map(|t| t as u64)
                        .collect()
                })
                .unwrap_or_default(),
            call_num: c.call_num,
            freq: c.freq,
            freq_error: ffi::call_get_freq_error(&*c.handle),
            source_num: 0,
            recorder_num: c.recorder.as_ref().map_or(-1, |r| r.rec_num),
            signal: c.signal,
            noise: c.noise,
            start_time: c.start_time,
            stop_time: now,
            error_count: 0,
            spike_count: 0,
            encrypted: c.encrypted,
            emergency: c.emergency,
            priority: c.priority,
            mode: c.mode,
            duplex: c.duplex,
            sys_num: c.sys_num,
            short_name: c.short_name.clone(),
            audio_type: audio_type.into(),
            tdma_slot: c.tdma_slot,
            length: 0.0, // filled by caller with metrics
            phase2_tdma: c.phase2_tdma,
            transmission_source_list: Vec::new(),
            transmission_error_list: Vec::new(),
            transmission_list: Vec::new(), // filled by caller
            status: TrCallDataStatus::Initial,
        }
    }

    // ── Call conclusion (replaces C++ Call_impl::conclude_call) ──

    fn conclude_call(
        entry: &mut CallEntry,
        patches: &HashMap<i32, PatchTable>,
        bridge: &EventBridge,
    ) {
        let is_conventional = matches!(entry, CallEntry::Conventional { .. });
        let c = entry.common_mut();

        let should_stop_recording = c.state == TrState::Recording
            || (c.state == TrState::Monitoring
                && c.monitoring_state == TrMonitoringState::Superseded);

        if should_stop_recording {
            if let Some(recorder) = c.recorder.take() {
                let rec_num = recorder.rec_num;
                let metrics = recorder.stop(c.handle.pin_mut());
                tracing::info!(
                    short_name = %c.short_name,
                    rec_num,
                    talkgroup = c.talkgroup,
                    freq = %format_freq(c.freq),
                    tdma = c.phase2_tdma,
                    slot = c.tdma_slot,
                    freq_error = format_args!("{:+.0} Hz", metrics.freq_error),
                    "stopping recorder"
                );
                if is_conventional {
                    if metrics.recorder_idle {
                        c.noise = metrics.recorder_pwr;
                    } else {
                        c.signal = metrics.recorder_pwr;
                    }
                }

                let elapsed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64)
                    - c.start_time;
                tracing::info!(
                    short_name = %c.short_name,
                    call_num = c.call_num,
                    talkgroup = c.talkgroup,
                    elapsed,
                    final_length = metrics.final_length,
                    "concluding recorded call"
                );

                if metrics.is_dmr {
                    for slot in 0..2i32 {
                        let mut data = Self::build_call_data(c, patches);
                        data.tdma_slot = slot;
                        data.length = metrics.final_length;
                        data.freq_error = metrics.freq_error;
                        data.transmission_list =
                            ffi::call_get_dmr_transmission_list(&*c.handle, slot);
                        bridge.call_end(&data);
                    }
                } else {
                    let mut data = Self::build_call_data(c, patches);
                    data.length = metrics.final_length;
                    data.freq_error = metrics.freq_error;
                    data.transmission_list = ffi::call_get_transmission_list(&*c.handle);
                    bridge.call_end(&data);
                }
            }
        } else if c.state == TrState::Monitoring {
            let data = Self::build_call_data(c, patches);
            bridge.call_end(&data);
        }
    }

    // ── Source tracking update (replaces C++ Call_impl::update + add_source) ──

    fn update_call_source(
        entry: &mut CallEntry,
        msg: &TrTrunkMessage,
        bridge: &EventBridge,
    ) -> bool {
        let c = entry.common_mut();
        c.last_update = Instant::now();

        let src = msg.source;
        if src == -1 {
            return false;
        }
        if src == c.current_source_id {
            return false;
        }

        c.current_source_id = src;
        ffi::call_recorder_set_source(c.handle.pin_mut(), src);
        let info = entry.to_call_info();
        bridge.signal(src, &info);
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
                if let Some(pt) = self.patch_tables.get_mut(&msg.sys_num) {
                    pt.update(&msg.patch_data);
                }
            }
            TrMessageType::PatchDelete => {
                if let Some(pt) = self.patch_tables.get_mut(&msg.sys_num) {
                    pt.delete(&msg.patch_data);
                }
            }
            TrMessageType::Tdulc => {
                ffi::tr_system_retune(ctx, msg.sys_num);
            }

            // ── System identity (once-only, fire setup_system if changed) ──
            TrMessageType::Sysid => {
                if ffi::tr_system_update_sysid(ctx.as_mut(), msg) {
                    if let Some(id) = self.system_identities.get_mut(&msg.sys_num) {
                        id.rfss = msg.sys_rfss;
                        id.site_id = msg.sys_site_id;
                    }
                    let info = self.get_system_info(ctx.as_mut(), msg.sys_num);
                    self.bridge.setup_system(&info);
                }
            }
            TrMessageType::Status => {
                if ffi::tr_system_update_status(ctx.as_mut(), msg) {
                    if let Some(id) = self.system_identities.get_mut(&msg.sys_num) {
                        id.sys_id = msg.sys_id;
                        id.wacn = msg.wacn;
                        id.nac = msg.nac;
                    }
                    let info = self.get_system_info(ctx.as_mut(), msg.sys_num);
                    self.bridge.setup_system(&info);
                }
            }

            // ── Unit-level events ──
            TrMessageType::Registration => {
                let info = self.get_system_info(ctx.as_mut(), msg.sys_num);
                self.bridge
                    .unit_event(&info, TrUnitEventType::Registration, msg.source, 0);
            }
            TrMessageType::Deregistration => {
                let info = self.get_system_info(ctx.as_mut(), msg.sys_num);
                self.bridge
                    .unit_event(&info, TrUnitEventType::Deregistration, msg.source, 0);
            }
            TrMessageType::Affiliation => {
                let info = self.get_system_info(ctx.as_mut(), msg.sys_num);
                self.bridge.unit_event(
                    &info,
                    TrUnitEventType::Affiliation,
                    msg.source,
                    msg.talkgroup,
                );
            }
            TrMessageType::Acknowledge => {
                let info = self.get_system_info(ctx.as_mut(), msg.sys_num);
                self.bridge
                    .unit_event(&info, TrUnitEventType::Acknowledge, msg.source, 0);
            }
            TrMessageType::Location => {
                let info = self.get_system_info(ctx.as_mut(), msg.sys_num);
                self.bridge
                    .unit_event(&info, TrUnitEventType::Location, msg.source, msg.talkgroup);
            }
            TrMessageType::DataGrant => {
                let info = self.get_system_info(ctx.as_mut(), msg.sys_num);
                self.bridge
                    .unit_event(&info, TrUnitEventType::DataGrant, msg.source, 0);
            }
            TrMessageType::UuAnsReq => {
                let info = self.get_system_info(ctx.as_mut(), msg.sys_num);
                self.bridge.unit_event(
                    &info,
                    TrUnitEventType::AnswerRequest,
                    msg.source,
                    msg.talkgroup,
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
            let c = entry.common();
            if c.talkgroup == msg.talkgroup
                && c.sys_num == msg.sys_num
                && c.freq == msg.freq
                && c.tdma_slot == msg.tdma_slot
                && c.phase2_tdma == msg.phase2_tdma
            {
                call_found = true;
                let source_updated = Self::update_call_source(entry, msg, &self.bridge);
                if source_updated {
                    self.bridge.call_start(&entry.to_call_info());
                }
            }
        }

        if call_found {
            return;
        }

        // New call
        let multisite = self.check_multisite(msg.talkgroup, msg.sys_num, msg);
        let new_call = ffi::tr_context_create_call(ctx.as_mut(), msg);
        let policy = self
            .config
            .systems
            .get(&msg.sys_num)
            .unwrap_or(&DEFAULT_POLICY);
        let mut new_entry = CallEntry::new_trunked(new_call, msg, policy);

        if multisite.is_superseding {
            let recording_started = self.try_start_recording(&mut ctx, &mut new_entry, msg);

            if recording_started {
                if let Some(orig_idx) = multisite.original_idx {
                    let orig = &mut self.calls[orig_idx];
                    let oc = orig.common_mut();
                    oc.state = TrState::Monitoring;
                    oc.monitoring_state = TrMonitoringState::Superseded;
                    Self::conclude_call(orig, &self.patch_tables, &self.bridge);
                }
            }
        } else if multisite.is_duplicate {
            let c = new_entry.common_mut();
            c.state = TrState::Monitoring;
            c.monitoring_state = TrMonitoringState::Duplicate;
        } else {
            let recording_started = self.try_start_recording(&mut ctx, &mut new_entry, msg);
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
        self.bridge.call_start(&last.to_call_info());

        let infos: Vec<ffi::TrCallInfo> = self.calls.iter().map(|e| e.to_call_info()).collect();
        self.bridge.calls_active(&infos);
    }

    // ── Update handling ──

    fn handle_update(&mut self, ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) {
        let _ = ctx;
        for entry in &mut self.calls {
            let c = entry.common();
            if c.talkgroup == msg.talkgroup
                && c.sys_num == msg.sys_num
                && c.freq == msg.freq
                && c.tdma_slot == msg.tdma_slot
                && c.phase2_tdma == msg.phase2_tdma
            {
                if msg.encrypted {
                    entry.common_mut().encrypted = true;
                }

                let source_updated = Self::update_call_source(entry, msg, &self.bridge);
                if source_updated {
                    self.bridge.call_start(&entry.to_call_info());
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

        // Query runtime patch data from Rust-owned patch table
        let patch_tgs = self
            .patch_tables
            .get(&sys_num)
            .map(|pt| pt.get_patch(tg_id))
            .unwrap_or_default();

        let c = entry.common_mut();

        // 1. Unknown TG check
        if tg_policy.is_none() {
            let patched_with_known = patch_tgs
                .iter()
                .any(|&ptg| sys_policy.talkgroups.contains_key(&ptg));

            if !sys_policy.record_unknown && !patched_with_known {
                c.state = TrState::Monitoring;
                c.monitoring_state = TrMonitoringState::UnknownTg;
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
        let is_encrypted = msg.encrypted || tg_policy.map_or(false, |tg| tg.mode.is_encrypted());
        if is_encrypted {
            c.encrypted = true;
            if !sys_policy.monitor_encrypted {
                c.state = TrState::Monitoring;
                c.monitoring_state = TrMonitoringState::Encrypted;
                if !sys_policy.hide_encrypted {
                    tracing::info!(talkgroup = tg_id, sys_num, "not recording: encrypted");
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
        c.is_analog = is_analog;

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
            c.state = TrState::Monitoring;
            c.monitoring_state = TrMonitoringState::IgnoredTg;
            tracing::info!(
                talkgroup = tg_id,
                sys_num,
                "not recording: priority disabled (-1)"
            );
            return false;
        }

        // 6. Find source and acquire recorder
        let source_count = ffi::tr_context_source_count(&*ctx);
        let freq = c.freq;
        for source_idx in 0..source_count {
            if !ffi::tr_source_covers_freq(&*ctx, source_idx, freq) {
                continue;
            }

            let available = ffi::tr_source_available_recorders(&*ctx, source_idx, is_analog);
            if priority > available {
                c.state = TrState::Monitoring;
                c.monitoring_state = TrMonitoringState::NoRecorder;
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
                c.handle.pin_mut(),
                is_analog,
            ) {
                c.state = TrState::Recording;
                let rec_num = ffi::call_get_recorder_num(&*c.handle);
                c.recorder = Some(Recorder { rec_num });
                tracing::info!(
                    short_name = %c.short_name,
                    rec_num,
                    talkgroup = tg_id,
                    freq = %format_freq(c.freq),
                    tdma = c.phase2_tdma,
                    slot = c.tdma_slot,
                    analog = is_analog,
                    "starting recorder"
                );
                return true;
            }

            // Recorder allocation or start failed
            c.state = TrState::Monitoring;
            return false;
        }

        // No source covered the frequency
        c.state = TrState::Monitoring;
        c.monitoring_state = TrMonitoringState::NoSource;
        tracing::error!(
            talkgroup = tg_id,
            sys_num,
            "not recording: no source covering freq"
        );
        false
    }

    // ── Multisite duplicate detection ──

    fn check_multisite(
        &self,
        talkgroup: i64,
        sys_num: i32,
        msg: &TrTrunkMessage,
    ) -> MultiSiteResult {
        let mut result = MultiSiteResult {
            is_duplicate: false,
            is_superseding: false,
            original_idx: None,
        };

        for (idx, entry) in self.calls.iter().enumerate() {
            let CallEntry::Trunked { common: c, trunked } = entry else {
                continue;
            };
            if c.talkgroup != talkgroup {
                continue;
            }
            if c.sys_num == sys_num {
                continue;
            }
            if !trunked.multi_site {
                continue;
            }
            if c.state != TrState::Recording {
                continue;
            }

            if trunked.wacn == 0 {
                continue;
            }

            // Default mode: match WACN, different RFSS/Site, no multiSiteSystemName
            if trunked.multi_site_name.is_empty() {
                if trunked.wacn != msg.wacn {
                    continue;
                }

                let call_rfss_site = trunked.rfss as u64 * 10000 + trunked.site_id as u64;
                let msg_rfss_site = msg.sys_rfss as u64 * 10000 + msg.sys_site_id as u64;

                if msg_rfss_site == call_rfss_site {
                    continue;
                }

                result.is_duplicate = true;
                result.original_idx = Some(idx);

                // Supersede check
                let call_preferred_nac = self.get_preferred_nac(c.sys_num, talkgroup);
                if call_preferred_nac != call_rfss_site && msg.nac == msg_rfss_site {
                    result.is_superseding = true;
                }
            }
            // Secondary mode: match multiSiteSystemName.
            else {
                let msg_multi_site_name = self
                    .config
                    .systems
                    .get(&sys_num)
                    .map(|p| p.multi_site_name.as_str())
                    .unwrap_or("");
                if !msg_multi_site_name.is_empty()
                    && trunked.multi_site_name == msg_multi_site_name
                {
                    result.is_duplicate = true;
                    result.original_idx = Some(idx);
                }
            }
        }

        result
    }

    /// Look up preferred NAC for a talkgroup from Rust-owned config.
    fn get_preferred_nac(&self, sys_num: i32, talkgroup: i64) -> u64 {
        self.config
            .systems
            .get(&sys_num)
            .and_then(|p| p.talkgroups.get(&talkgroup))
            .map_or(0, |tg| tg.preferred_nac)
    }

    // ── Periodic tick (call timeout management) ──

    pub fn tick(&mut self, mut ctx: Pin<&mut TrContext>) {
        let mut ended_call = false;
        let timeout = self.config.call_timeout;
        let bridge = &self.bridge;

        self.calls.retain_mut(|entry| {
            match entry {
                CallEntry::Conventional { .. } => true, // never removed by tick
                CallEntry::Trunked { common: c, .. } => {
                    let since_update = c.last_update.elapsed().as_secs_f64();

                    match c.state {
                        TrState::Monitoring => {
                            if since_update > timeout {
                                Self::conclude_call(entry, &self.patch_tables, bridge);
                                ended_call = true;
                                return false;
                            }
                        }
                        TrState::Recording => {
                            let since_write =
                                ffi::call_recorder_since_last_write_secs(&*c.handle);
                            if since_write > timeout || since_update > timeout {
                                Self::conclude_call(entry, &self.patch_tables, bridge);
                                ffi::call_notify_setup_recorder(ctx.as_mut(), &*c.handle);
                                ended_call = true;
                                return false;
                            }
                        }
                        _ => {
                            if since_update > timeout {
                                Self::conclude_call(entry, &self.patch_tables, bridge);
                                ended_call = true;
                                return false;
                            }
                        }
                    }

                    true
                }
            }
        });

        if ended_call {
            let infos: Vec<ffi::TrCallInfo> = self.calls.iter().map(|e| e.to_call_info()).collect();
            self.bridge.calls_active(&infos);
        }

        self.tick_conventional(ctx);
    }

    // ── Process P25 recorder message queues ──

    pub fn process_recorder_queues(&self) {
        for entry in &self.calls {
            ffi::call_process_recorder_queue(&*entry.common().handle);
        }
    }

    // ── Conventional call management ──

    /// Add a conventional call drained from C++ during startup.
    pub fn add_call(&mut self, call: UniquePtr<TrCall>) {
        let sys_num = ffi::call_get_metadata(&*call).sys_num;
        let policy = self.config.systems.get(&sys_num).unwrap_or(&DEFAULT_POLICY);
        self.calls.push(CallEntry::new_conventional(call, policy));
    }

    /// Handle a squelch event pushed from a GR block via the event channel.
    /// Finds the conventional call entry whose recorder matches `recorder_num`
    /// and updates its idle tracking state.
    pub fn handle_squelch_event(&mut self, recorder_num: i32, muted: bool, pwr_db: f64) {
        for entry in &mut self.calls {
            let CallEntry::Conventional { common: c, conv } = entry else {
                continue;
            };
            if c.recorder
                .as_ref()
                .map_or(true, |r| r.rec_num != recorder_num)
            {
                continue;
            }
            if muted {
                c.noise = pwr_db;
                if conv.idle_since.is_none() {
                    conv.idle_since = Some(Instant::now());
                }
            } else {
                c.signal = pwr_db;
                conv.idle_since = None;
            }
            return;
        }
    }

    /// Manage conventional call idle detection via squelch events, conclude+restart, and initial recording.
    fn tick_conventional(&mut self, mut ctx: Pin<&mut TrContext>) {
        for entry in &mut self.calls {
            let CallEntry::Conventional { common: c, conv } = entry else {
                continue;
            };
            if c.recorder.is_none() {
                continue;
            }

            let length = ffi::call_get_current_length(&*c.handle);
            if length > 0.0 {
                let idle_timed_out = conv
                    .idle_since
                    .map(|t| t.elapsed().as_secs_f64() > self.config.call_timeout)
                    .unwrap_or(false);
                if idle_timed_out || (c.max_duration > 0.0 && length > c.max_duration) {
                    Self::conclude_call(entry, &self.patch_tables, &self.bridge);
                    // Re-destructure after conclude_call borrows entry
                    let CallEntry::Conventional { common: c, conv } = entry else {
                        unreachable!()
                    };
                    ffi::call_restart(c.handle.pin_mut());
                    let m = ffi::call_get_metadata(&*c.handle);
                    c.call_num = m.call_num;
                    c.start_time = m.start_time;
                    c.state = m.state;
                    c.monitoring_state = m.monitoring_state;
                    c.last_update = Instant::now();
                    conv.idle_since = None;
                    ffi::call_notify_setup_recorder(ctx.as_mut(), &*c.handle);
                    self.bridge.call_start(&entry.to_call_info());
                }
            } else if !ffi::call_get_recorder_is_active(&*c.handle) {
                ffi::call_start_initial_recording(c.handle.pin_mut());
                c.state = TrState::Recording;
                self.bridge.call_start(&entry.to_call_info());
            }
        }
    }

    // ── Patch maintenance ──

    /// Remove stale patches (>10s without refresh) for all P25 systems.
    pub fn clear_stale_patches(&mut self) {
        for pt in self.patch_tables.values_mut() {
            pt.clear_stale();
        }
    }

    /// Log all active patches for all systems.
    pub fn print_active_patches(&self) {
        tracing::info!("Active Patches:");
        for (&sys_num, pt) in &self.patch_tables {
            let short_name = self
                .config
                .systems
                .get(&sys_num)
                .map(|sp| sp.short_name.as_str())
                .unwrap_or("unknown");
            pt.log_patches(short_name);
        }
    }

    /// Log status for all active calls and control channel decode rates.
    pub fn print_status(&self, mut ctx: Pin<&mut TrContext>) {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);

        tracing::info!(count = self.calls.len(), "Active Calls");
        for entry in &self.calls {
            let c = entry.common();
            let elapsed = now_secs - c.start_time;
            let state = format_call_state(c.state, c.monitoring_state);

            match entry {
                CallEntry::Conventional { .. } => {
                    let enabled = ffi::call_get_recorder_is_enabled(&*c.handle);
                    tracing::info!(
                        system = %c.short_name,
                        call_num = c.call_num,
                        talkgroup = c.talkgroup,
                        freq = format_freq(c.freq),
                        elapsed,
                        state,
                        enabled,
                        "call status"
                    );
                }
                CallEntry::Trunked { .. } => {
                    tracing::info!(
                        system = %c.short_name,
                        call_num = c.call_num,
                        talkgroup = c.talkgroup,
                        freq = format_freq(c.freq),
                        elapsed,
                        state,
                        "call status"
                    );
                }
            }

            if let Some(recorder) = &c.recorder {
                let rec_state = format_recorder_state(ffi::call_get_recorder_state(&*c.handle));
                tracing::info!(rec_num = recorder.rec_num, rec_state, "  recorder");
            }
        }

        tracing::info!("Control Channel Decode Rates");
        for (&sys_num, policy) in &self.config.systems {
            if policy.system_type != SystemType::Conventional
                && policy.system_type != SystemType::ConventionalP25
            {
                let cc_freq = ffi::tr_system_get_current_control_channel(ctx.as_mut(), sys_num);
                let decode_rate = self.decode_rates.get(&sys_num).copied().unwrap_or(0);
                tracing::info!(
                    system = %policy.short_name,
                    freq = format_freq(cc_freq),
                    decode_rate,
                    "msg/sec"
                );
            }
        }
    }

    /// Log recorder status for all sources.
    pub fn print_recorder_status(&self, ctx: &TrContext) {
        let recorders = ffi::tr_context_get_recorder_status(ctx);
        let mut current_source = -1i32;
        for r in &recorders {
            if r.source_num != current_source {
                current_source = r.source_num;
                tracing::info!(
                    source = r.source_num,
                    freq = %format_freq(r.source_center),
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

    // ── Shutdown ──

    /// Check decode rates for all trunked systems and retune if needed.
    /// Called every ~3 seconds from `run_monitor`.
    pub fn check_decode_rates(
        &mut self,
        mut ctx: Pin<&mut TrContext>,
        time_diff: f32,
    ) -> anyhow::Result<()> {
        let mut infos = Vec::new();
        for (&sys_num, policy) in &self.config.systems {
            let mut info = self.get_system_info(ctx.as_mut(), sys_num);
            if policy.system_type.is_trunked() {
                let count = self.message_counts.remove(&sys_num).unwrap_or(0);
                let decode_rate = (count as f32 / time_diff).floor() as i32;
                self.decode_rates.insert(sys_num, decode_rate);
                info.decode_rate = decode_rate;
                info.message_count = count;
            }
            infos.push(info);
        }

        self.bridge.setup_config(&infos);
        self.bridge.system_rates(&infos, time_diff);

        for info in &infos {
            let sys_num = info.sys_num;
            let policy = match self.config.systems.get(&sys_num) {
                Some(p) if p.system_type.is_trunked() => p,
                _ => continue,
            };
            let decode_rate = info.decode_rate;
            let count = info.message_count;

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
                if policy.control_channels.len() > 1 {
                    ffi::tr_system_retune(ctx.as_mut(), sys_num);
                } else {
                    tracing::error!(sys_num, "only one control channel defined");
                }
            } else {
                self.retune_attempts.insert(sys_num, 0);
            }

            if decode_rate < self.config.control_warn_rate {
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
            } else if self.config.control_warn_rate == -1 {
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

    pub fn conclude_all(&mut self, ctx: Pin<&mut TrContext>) {
        let _ = ctx;
        for entry in &mut self.calls {
            if entry.common().state != TrState::Monitoring {
                Self::conclude_call(entry, &self.patch_tables, &self.bridge);
            }
        }
        self.calls.clear();

        let empty: Vec<ffi::TrCallInfo> = Vec::new();
        self.bridge.calls_active(&empty);
    }
}
