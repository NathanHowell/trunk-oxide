use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Instant, SystemTime};

use trunk_config::{DefaultMode, SystemType, TalkgroupMode};

// ── Newtypes ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CallId(u64);

static NEXT_CALL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl CallId {
    pub(crate) fn next() -> Self {
        Self(NEXT_CALL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    pub(crate) fn as_u64(self) -> u64 {
        self.0
    }
}

/// Monotonic call number counter, replacing C++'s `Call::call_counter`.
static NEXT_CALL_NUM: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub(crate) fn next_call_num() -> i64 {
    NEXT_CALL_NUM.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn system_time_to_unix(t: SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Talkgroup(pub(crate) i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RecorderNum(pub(crate) i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Freq(pub(crate) u64);

impl Freq {
    pub fn from_hz(hz: f64) -> Self {
        Self(hz as u64)
    }

    pub(crate) fn as_hz(self) -> f64 {
        self.0 as f64
    }
}

impl std::fmt::Display for Freq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.4} MHz", self.0 as f64 / 1_000_000.0)
    }
}

// ── Call State ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallState {
    Recording,
    Idle,
    /// Recorder is disabled, waiting for signal detection to enable it.
    Disabled,
    Rejected(RejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectReason {
    UnknownTg,
    IgnoredTg,
    NoSource,
    NoRecorder,
    Encrypted,
    Duplicate,
    Superseded,
}

impl CallState {
    pub(crate) fn to_proto(self) -> i32 {
        use trunk_oxide_grpc::proto::CallState as ProtoCallState;
        match self {
            CallState::Recording => ProtoCallState::Recording as i32,
            CallState::Idle | CallState::Disabled => ProtoCallState::Idle as i32,
            CallState::Rejected(RejectReason::UnknownTg) => {
                ProtoCallState::RejectedUnknownTg as i32
            }
            CallState::Rejected(RejectReason::IgnoredTg) => {
                ProtoCallState::RejectedIgnoredTg as i32
            }
            CallState::Rejected(RejectReason::NoSource) => {
                ProtoCallState::RejectedNoSource as i32
            }
            CallState::Rejected(RejectReason::NoRecorder) => {
                ProtoCallState::RejectedNoRecorder as i32
            }
            CallState::Rejected(RejectReason::Encrypted) => {
                ProtoCallState::RejectedEncrypted as i32
            }
            CallState::Rejected(RejectReason::Duplicate) => {
                ProtoCallState::RejectedDuplicate as i32
            }
            CallState::Rejected(RejectReason::Superseded) => {
                ProtoCallState::RejectedSuperseded as i32
            }
        }
    }
}

impl std::fmt::Display for CallState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallState::Recording => write!(f, "Recording"),
            CallState::Idle => write!(f, "Idle"),
            CallState::Disabled => write!(f, "Disabled"),
            CallState::Rejected(r) => write!(f, "Rejected: {r}"),
        }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::UnknownTg => write!(f, "UNKNOWN TG"),
            RejectReason::IgnoredTg => write!(f, "IGNORED TG"),
            RejectReason::NoSource => write!(f, "NO SOURCE"),
            RejectReason::NoRecorder => write!(f, "NO RECORDER"),
            RejectReason::Encrypted => write!(f, "ENCRYPTED"),
            RejectReason::Duplicate => write!(f, "DUPLICATE"),
            RejectReason::Superseded => write!(f, "SUPERSEDED"),
        }
    }
}

// ── Segments ──

pub(crate) struct CallSegment {
    pub(crate) freq: Freq,
    pub(crate) start: Instant,
    pub(crate) rec_num: RecorderNum,
}

// ── Mode ──

pub(crate) struct TrunkedFields {
    pub(crate) multi_site: bool,
    pub(crate) wacn: u64,
    pub(crate) rfss: i32,
    pub(crate) site_id: i32,
    pub(crate) multi_site_name: String,
    pub(crate) conversation_mode: bool,
}

pub(crate) enum CallMode {
    Trunked(TrunkedFields),
    Conventional,
}

// ── Config types ──

#[derive(Clone)]
pub struct TalkgroupPolicy {
    pub mode: TalkgroupMode,
    pub priority: i32,
    pub preferred_nac: u64,
}

#[derive(Clone, Default)]
pub struct ConventionalChannel {
    pub talkgroup: Talkgroup,
    pub freq: Freq,
    pub is_analog: bool,
    pub priority: i32,
    pub signal_detection: bool,
    pub squelch_db: f64,
    pub tone: f64,
}

#[derive(Clone, Default)]
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
    pub control_channels: Vec<Freq>,
    pub talkgroups: HashMap<Talkgroup, TalkgroupPolicy>,
    pub channels: Vec<ConventionalChannel>,
    pub squelch_db: f64,
    pub analog_levels: f64,
    pub digital_levels: f64,
    pub bandplan: String,
    pub sys_id: u64,
    pub wacn: u64,
    pub nac: u64,
}

pub(crate) static DEFAULT_POLICY: LazyLock<SystemPolicy> =
    LazyLock::new(SystemPolicy::default);

#[derive(Clone)]
pub struct CallManagerConfig {
    pub call_timeout: f64,
    pub new_call_from_update: bool,
    pub record_uu_v_calls: bool,
    pub default_mode: DefaultMode,
    pub control_warn_rate: i32,
    pub temp_dir: String,
    pub systems: HashMap<i32, SystemPolicy>,
}

pub(crate) struct SystemIdentity {
    pub(crate) sys_id: u64,
    pub(crate) wacn: u64,
    pub(crate) nac: u64,
    pub(crate) rfss: i32,
    pub(crate) site_id: i32,
    /// P25 Phase 2 LFSR XOR mask, computed by C++ from (sys_id, wacn, nac).
    /// Each byte is a dibit (0–3).
    pub(crate) xor_mask: Vec<u8>,
}

// ── Patch Table ──

pub(crate) struct PatchTable {
    patches: HashMap<u64, HashMap<u64, Instant>>,
}

impl PatchTable {
    pub(crate) fn new() -> Self {
        Self {
            patches: HashMap::new(),
        }
    }

    pub(crate) fn update(&mut self, sg: u64, ga1: u64, ga2: u64, ga3: u64) {
        let now = Instant::now();
        let patch = self.patches.entry(sg).or_default();
        for &tg in &[sg, ga1, ga2, ga3] {
            if tg != 0 {
                patch.insert(tg, now);
            }
        }
    }

    pub(crate) fn delete(&mut self, sg: u64, ga1: u64, ga2: u64, ga3: u64) {
        if let Some(patch) = self.patches.get_mut(&sg) {
            for &tg in &[ga1, ga2, ga3] {
                if tg != 0 {
                    patch.remove(&tg);
                }
            }
        }
    }

    pub(crate) fn clear_stale(&mut self) {
        self.patches.retain(|_, patch| {
            patch.retain(|_, last_update| last_update.elapsed().as_secs() < 10);
            !patch.is_empty()
        });
    }

    pub(crate) fn log_patches(&self, short_name: &str) {
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

    pub(crate) fn get_patch(&self, talkgroup: Talkgroup) -> Vec<Talkgroup> {
        let tg = talkgroup.0 as u64;
        for patch in self.patches.values() {
            if patch.contains_key(&tg) {
                return patch.keys().map(|&k| Talkgroup(k as i64)).collect();
            }
        }
        Vec::new()
    }
}

// ── Call Entry (pure Rust, no C++ handle) ──

pub(crate) struct CallEntry {
    pub(crate) id: CallId,
    pub(crate) call_num: i64,
    pub(crate) talkgroup: i64,
    pub(crate) sys_num: i32,
    pub(crate) short_name: String,
    pub(crate) tdma_slot: i32,
    pub(crate) phase2_tdma: bool,
    pub(crate) mode: bool,
    pub(crate) duplex: bool,
    pub(crate) emergency: bool,
    pub(crate) start_time: SystemTime,
    pub(crate) freq: Freq,
    pub(crate) state: CallState,
    pub(crate) encrypted: bool,
    pub(crate) is_analog: bool,
    pub(crate) current_source_id: i64,
    pub(crate) signal: f64,
    pub(crate) noise: f64,
    pub(crate) priority: i32,
    pub(crate) recorder: Option<RecorderNum>,
    pub(crate) last_update: Instant,
    pub(crate) max_duration: f64,
    pub(crate) segments: Vec<CallSegment>,
    pub(crate) segment_start: Instant,
    pub(crate) idle_since: Option<Instant>,
    pub(crate) call_mode: CallMode,
    pub(crate) signal_detection: bool,
    pub(crate) squelch_db: f64,
}

impl Default for CallEntry {
    fn default() -> Self {
        Self {
            id: CallId::next(),
            call_num: next_call_num(),
            talkgroup: 0,
            sys_num: 0,
            short_name: String::new(),
            tdma_slot: 0,
            phase2_tdma: false,
            mode: false,
            duplex: false,
            emergency: false,
            start_time: SystemTime::now(),
            freq: Freq::from_hz(0.0),
            state: CallState::Idle,
            encrypted: false,
            is_analog: false,
            current_source_id: 0,
            signal: 0.0,
            noise: 0.0,
            priority: 0,
            recorder: None,
            last_update: Instant::now(),
            max_duration: 0.0,
            segments: Vec::new(),
            segment_start: Instant::now(),
            idle_since: None,
            call_mode: CallMode::Conventional,
            signal_detection: false,
            squelch_db: 0.0,
        }
    }
}

impl CallEntry {
    pub(crate) fn is_conventional(&self) -> bool {
        matches!(self.call_mode, CallMode::Conventional)
    }

    pub(crate) fn is_trunked(&self) -> bool {
        matches!(self.call_mode, CallMode::Trunked(_))
    }

    pub(crate) fn trunked_fields(&self) -> Option<&TrunkedFields> {
        match &self.call_mode {
            CallMode::Trunked(t) => Some(t),
            _ => None,
        }
    }
}

// ── Recording result (fed back from executor after FFI) ──

pub(crate) enum RecordingResult {
    Started { rec_num: RecorderNum },
    NoRecorder,
    NoSource,
}

// ── Multisite result ──

pub(crate) struct MultiSiteResult {
    pub(crate) is_duplicate: bool,
    pub(crate) is_superseding: bool,
    pub(crate) original_idx: Option<usize>,
}

// ── Effect enum ──

#[derive(Debug)]
pub(crate) enum Effect {
    // ── Recorder lifecycle ──
    CreateCall {
        call_id: CallId,
    },
    RequestRecording {
        call_id: CallId,
        freq: Freq,
        is_analog: bool,
        priority: i32,
    },
    StopRecording {
        call_id: CallId,
    },
    SetFreq {
        call_id: CallId,
        freq: Freq,
    },
    SetSource {
        call_id: CallId,
        source_id: i64,
    },
    // ── Event emission ──
    EmitCallStart {
        call_id: CallId,
    },
    EmitCallEnd {
        call_id: CallId,
    },
    EmitCallsActive,
    EmitSignal {
        call_id: CallId,
        source_id: i64,
    },
    EmitUnitEvent {
        sys_num: i32,
        type_: UnitEventType,
        source_id: i64,
        talkgroup: i64,
    },
    EmitSetupSystem {
        sys_num: i32,
    },
    EmitSetupRecorder {
        call_id: CallId,
    },

    // ── FFI pass-through ──
    AddControlChannel {
        sys_num: i32,
        freq: Freq,
    },

    // ── Conventional ──
    ConventionalRestart {
        call_id: CallId,
    },
    ConventionalStartInitial {
        call_id: CallId,
    },
    EnableRecorder {
        call_id: CallId,
    },
    // ── Lifecycle ──
    RemoveCall {
        call_id: CallId,
    },

    // ── Diagnostics ──
    Warn {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnitEventType {
    Registration,
    Deregistration,
    Affiliation,
    Acknowledge,
    Location,
    DataGrant,
    AnswerRequest,
}

// ── Message type (pure Rust mirror of TrMessageType) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageType {
    Grant,
    Update,
    UuVGrant,
    UuVUpdate,
    Tdulc,
    PatchAdd,
    PatchDelete,
    ControlChannel,
    Sysid,
    Status,
    InvalidCcMessage,
    Registration,
    Deregistration,
    Affiliation,
    Acknowledge,
    Location,
    DataGrant,
    UuAnsReq,
    Unknown(u8),
}

// ── Trunk message (pure Rust input to state machine) ──

pub(crate) struct TrunkMessage {
    pub(crate) message_type: MessageType,
    pub(crate) sys_num: i32,
    pub(crate) talkgroup: i64,
    pub(crate) freq: Freq,
    pub(crate) source: i64,
    pub(crate) tdma_slot: i32,
    pub(crate) phase2_tdma: bool,
    pub(crate) mode: bool,
    pub(crate) duplex: bool,
    pub(crate) emergency: bool,
    pub(crate) encrypted: bool,
    pub(crate) priority: i32,
    pub(crate) wacn: u64,
    pub(crate) nac: u64,
    pub(crate) sys_id: u64,
    pub(crate) sys_rfss: i32,
    pub(crate) sys_site_id: i32,
    pub(crate) patch_sg: u64,
    pub(crate) patch_ga1: u64,
    pub(crate) patch_ga2: u64,
    pub(crate) patch_ga3: u64,
}

// ══════════════════════════════════════════════════════════════════════
// State Machine
// ══════════════════════════════════════════════════════════════════════

pub(crate) struct TrunkStateMachine {
    pub(crate) calls: Vec<CallEntry>,
    pub(crate) config: CallManagerConfig,
    pub(crate) message_counts: HashMap<i32, i32>,
    pub(crate) decode_rates: HashMap<i32, i32>,
    pub(crate) patch_tables: HashMap<i32, PatchTable>,
    pub(crate) system_identities: HashMap<i32, SystemIdentity>,
}

impl TrunkStateMachine {
    pub(crate) fn new(config: CallManagerConfig) -> Self {
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
                        xor_mask: crate::p25_lfsr::compute_xor_mask(sp.nac, sp.sys_id, sp.wacn),
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
            message_counts: HashMap::new(),
            decode_rates: HashMap::new(),
            patch_tables,
            system_identities,
        }
    }

    // ── Top-level message dispatch ──

    pub(crate) fn on_message(&mut self, msg: &TrunkMessage) -> Vec<Effect> {
        *self.message_counts.entry(msg.sys_num).or_insert(0) += 1;

        match msg.message_type {
            MessageType::InvalidCcMessage => {
                if let Some(count) = self.message_counts.get_mut(&msg.sys_num) {
                    if *count > 1 {
                        *count -= 1;
                    }
                }
                vec![]
            }

            MessageType::ControlChannel => {
                vec![Effect::AddControlChannel {
                    sys_num: msg.sys_num,
                    freq: msg.freq,
                }]
            }

            MessageType::PatchAdd => {
                if let Some(pt) = self.patch_tables.get_mut(&msg.sys_num) {
                    pt.update(msg.patch_sg, msg.patch_ga1, msg.patch_ga2, msg.patch_ga3);
                }
                vec![]
            }
            MessageType::PatchDelete => {
                if let Some(pt) = self.patch_tables.get_mut(&msg.sys_num) {
                    pt.delete(msg.patch_sg, msg.patch_ga1, msg.patch_ga2, msg.patch_ga3);
                }
                vec![]
            }

            MessageType::Tdulc => self.on_tdulc(msg.sys_num, msg.talkgroup),

            MessageType::Sysid => {
                if let Some(id) = self.system_identities.get_mut(&msg.sys_num) {
                    id.rfss = msg.sys_rfss;
                    id.site_id = msg.sys_site_id;
                }
                vec![Effect::EmitSetupSystem {
                    sys_num: msg.sys_num,
                }]
            }
            MessageType::Status => {
                if let Some(id) = self.system_identities.get_mut(&msg.sys_num) {
                    id.sys_id = msg.sys_id;
                    id.wacn = msg.wacn;
                    id.nac = msg.nac;
                    id.xor_mask =
                        crate::p25_lfsr::compute_xor_mask(msg.nac, msg.sys_id, msg.wacn);
                }
                vec![Effect::EmitSetupSystem {
                    sys_num: msg.sys_num,
                }]
            }

            // Unit events (stateless)
            MessageType::Registration => vec![Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::Registration,
                source_id: msg.source,
                talkgroup: 0,
            }],
            MessageType::Deregistration => vec![Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::Deregistration,
                source_id: msg.source,
                talkgroup: 0,
            }],
            MessageType::Affiliation => vec![Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::Affiliation,
                source_id: msg.source,
                talkgroup: msg.talkgroup,
            }],
            MessageType::Acknowledge => vec![Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::Acknowledge,
                source_id: msg.source,
                talkgroup: 0,
            }],
            MessageType::Location => vec![Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::Location,
                source_id: msg.source,
                talkgroup: msg.talkgroup,
            }],
            MessageType::DataGrant => vec![Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::DataGrant,
                source_id: msg.source,
                talkgroup: 0,
            }],
            MessageType::UuAnsReq => vec![Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::AnswerRequest,
                source_id: msg.source,
                talkgroup: msg.talkgroup,
            }],

            // Call lifecycle
            MessageType::Grant => self.on_grant(msg, true),
            MessageType::Update => {
                if self.config.new_call_from_update {
                    self.on_grant(msg, false)
                } else {
                    self.on_update(msg)
                }
            }
            MessageType::UuVGrant => {
                if self.config.record_uu_v_calls {
                    self.on_grant(msg, true)
                } else {
                    vec![]
                }
            }
            MessageType::UuVUpdate => {
                if self.config.record_uu_v_calls {
                    self.on_update(msg)
                } else {
                    vec![]
                }
            }

            MessageType::Unknown(t) => vec![Effect::Warn {
                message: format!("unhandled message type: {t}"),
            }],
        }
    }

    // ── Grant handling ──

    fn on_grant(&mut self, msg: &TrunkMessage, is_grant_message: bool) -> Vec<Effect> {
        let mut effects = Vec::new();

        let existing_idx = self.calls.iter().position(|entry| {
            entry.talkgroup == msg.talkgroup
                && entry.sys_num == msg.sys_num
                && entry.tdma_slot == msg.tdma_slot
                && entry.phase2_tdma == msg.phase2_tdma
        });

        if let Some(idx) = existing_idx {
            let same_freq = self.calls[idx].freq == msg.freq;
            if same_freq {
                {
                    let entry = &mut self.calls[idx];

                    // TDULC may have moved this call to Idle — resume recording
                    if entry.state == CallState::Idle && entry.is_trunked() {
                        entry.state = CallState::Recording;
                        entry.idle_since = None;
                        entry.segment_start = Instant::now();
                        tracing::trace!(
                            short_name = %entry.short_name,
                            talkgroup = entry.talkgroup,
                            "Grant on Idle call: Idle → Recording"
                        );
                    }
                }

                if self.update_call_source(idx, msg.source, &mut effects) {
                    let call_id = self.calls[idx].id;
                    effects.push(Effect::EmitCallStart { call_id });
                }
            } else {
                // Retune: close current segment (if Recording), start new one
                let entry = &mut self.calls[idx];
                let old_freq = entry.freq;
                let rec_num = entry.recorder.unwrap_or(RecorderNum(-1));

                // Only push a segment if we're actively recording
                // (TDULC already closed the segment on PTT end)
                if entry.state == CallState::Recording {
                    entry.segments.push(CallSegment {
                        freq: old_freq,
                        start: entry.segment_start,
                        rec_num,
                    });
                }
                entry.segment_start = Instant::now();
                entry.idle_since = None;

                if entry.recorder.is_some() {
                    effects.push(Effect::StopRecording {
                        call_id: entry.id,
                    });
                    entry.recorder = None;
                }

                entry.freq = msg.freq;
                effects.push(Effect::SetFreq {
                    call_id: entry.id,
                    freq: msg.freq,
                });
                entry.last_update = Instant::now();

                // Evaluate recording policy for new freq
                let policy_effects = self.evaluate_recording_policy(idx, msg);
                effects.extend(policy_effects);
            }
            return effects;
        }

        // New call
        let multisite = self.check_multisite(msg.talkgroup, msg.sys_num, msg);
        let policy = self
            .config
            .systems
            .get(&msg.sys_num)
            .unwrap_or(&DEFAULT_POLICY);

        let new_entry = CallEntry {
            talkgroup: msg.talkgroup,
            sys_num: msg.sys_num,
            short_name: policy.short_name.clone(),
            tdma_slot: msg.tdma_slot,
            phase2_tdma: msg.phase2_tdma,
            mode: msg.mode,
            duplex: msg.duplex,
            emergency: msg.emergency,
            freq: msg.freq,
            encrypted: msg.encrypted,
            current_source_id: msg.source,
            priority: msg.priority,
            max_duration: policy.max_duration,
            squelch_db: policy.squelch_db,
            call_mode: CallMode::Trunked(TrunkedFields {
                multi_site: policy.multi_site,
                wacn: msg.wacn,
                rfss: msg.sys_rfss,
                site_id: msg.sys_site_id,
                multi_site_name: policy.multi_site_name.clone(),
                conversation_mode: policy.conversation_mode,
            }),
            ..Default::default()
        };
        let call_id = new_entry.id;

        self.calls.push(new_entry);
        let new_idx = self.calls.len() - 1;

        effects.push(Effect::CreateCall { call_id });

        if multisite.is_superseding {
            let policy_effects = self.evaluate_recording_policy(new_idx, msg);
            let recording_started = self.calls[new_idx].state == CallState::Recording
                || policy_effects
                    .iter()
                    .any(|e| matches!(e, Effect::RequestRecording { .. }));
            effects.extend(policy_effects);

            if recording_started {
                if let Some(orig_idx) = multisite.original_idx {
                    let orig = &mut self.calls[orig_idx];
                    orig.state = CallState::Rejected(RejectReason::Superseded);
                    let orig_id = orig.id;
                    effects.push(Effect::EmitCallEnd { call_id: orig_id });
                    effects.push(Effect::RemoveCall { call_id: orig_id });
                }
            }
        } else if multisite.is_duplicate {
            self.calls[new_idx].state = CallState::Rejected(RejectReason::Duplicate);
        } else {
            let policy_effects = self.evaluate_recording_policy(new_idx, msg);
            if !policy_effects.is_empty() && !is_grant_message {
                tracing::info!(
                    talkgroup = msg.talkgroup,
                    sys_num = msg.sys_num,
                    "this was an UPDATE"
                );
            }
            effects.extend(policy_effects);
        }

        effects.push(Effect::EmitCallStart { call_id });
        effects.push(Effect::EmitCallsActive);

        effects
    }

    // ── Update handling ──

    fn on_update(&mut self, msg: &TrunkMessage) -> Vec<Effect> {
        let mut effects = Vec::new();

        for entry in self.calls.iter_mut() {
            if entry.talkgroup == msg.talkgroup
                && entry.sys_num == msg.sys_num
                && entry.freq == msg.freq
                && entry.tdma_slot == msg.tdma_slot
                && entry.phase2_tdma == msg.phase2_tdma
            {
                if msg.encrypted {
                    entry.encrypted = true;
                }

                if Self::update_call_source_entry(entry, msg.source, &mut effects) {
                    effects.push(Effect::EmitCallStart {
                        call_id: entry.id,
                    });
                }
            }
        }

        effects
    }

    // ── TDULC handling (P25 Terminator Data Unit) ──

    fn on_tdulc(&mut self, sys_num: i32, talkgroup: i64) -> Vec<Effect> {
        for entry in &mut self.calls {
            if entry.sys_num != sys_num || entry.talkgroup != talkgroup {
                continue;
            }
            if !entry.is_trunked() {
                continue;
            }
            if entry.state != CallState::Recording {
                continue;
            }

            // Close current segment, transition to Idle
            let rec_num = entry.recorder.unwrap_or(RecorderNum(-1));
            entry.segments.push(CallSegment {
                freq: entry.freq,
                start: entry.segment_start,
                rec_num,
            });
            entry.segment_start = Instant::now();
            entry.state = CallState::Idle;
            entry.idle_since = Some(Instant::now());

            tracing::trace!(
                short_name = %entry.short_name,
                talkgroup = entry.talkgroup,
                sys_num,
                segments = entry.segments.len(),
                "TDULC: PTT end, Recording → Idle"
            );
            return vec![];
        }
        vec![]
    }

    // ── Source tracking ──

    fn update_call_source(&mut self, idx: usize, source: i64, effects: &mut Vec<Effect>) -> bool {
        let entry = &mut self.calls[idx];
        Self::update_call_source_entry(entry, source, effects)
    }

    fn update_call_source_entry(
        entry: &mut CallEntry,
        source: i64,
        effects: &mut Vec<Effect>,
    ) -> bool {
        entry.last_update = Instant::now();

        if source == -1 || source == entry.current_source_id {
            return false;
        }

        entry.current_source_id = source;
        effects.push(Effect::SetSource {
            call_id: entry.id,
            source_id: source,
        });
        effects.push(Effect::EmitSignal {
            call_id: entry.id,
            source_id: source,
        });
        true
    }

    // ── Recording policy (pure evaluation) ──

    fn evaluate_recording_policy(
        &mut self,
        entry_idx: usize,
        msg: &TrunkMessage,
    ) -> Vec<Effect> {
        let sys_num = msg.sys_num;
        let tg = Talkgroup(msg.talkgroup);

        let sys_policy = match self.config.systems.get(&sys_num) {
            Some(sp) => sp,
            None => return vec![],
        };

        let tg_policy = sys_policy.talkgroups.get(&tg);

        let patch_tgs = self
            .patch_tables
            .get(&sys_num)
            .map(|pt| pt.get_patch(tg))
            .unwrap_or_default();

        let entry = &mut self.calls[entry_idx];

        // 1. Unknown TG check
        if tg_policy.is_none() {
            let patched_with_known = patch_tgs
                .iter()
                .any(|ptg| sys_policy.talkgroups.contains_key(ptg));

            if !sys_policy.record_unknown && !patched_with_known {
                entry.state = CallState::Rejected(RejectReason::UnknownTg);
                if !sys_policy.hide_unknown {
                    tracing::info!(
                        talkgroup = tg.0,
                        sys_num,
                        "not recording: TG not in talkgroup file"
                    );
                }
                return vec![];
            }
        }

        // 2. Encryption check
        let is_encrypted = msg.encrypted || tg_policy.map_or(false, |tg| tg.mode.is_encrypted());
        if is_encrypted {
            entry.encrypted = true;
            if !sys_policy.monitor_encrypted {
                entry.state = CallState::Rejected(RejectReason::Encrypted);
                if !sys_policy.hide_encrypted {
                    tracing::info!(talkgroup = tg.0, sys_num, "not recording: encrypted");
                }
                return vec![];
            }
        }

        // 3. Determine recorder type
        let is_analog = if let Some(tgp) = tg_policy {
            tgp.mode == TalkgroupMode::Analog
        } else {
            self.config.default_mode == DefaultMode::Analog
                && sys_policy.system_type == SystemType::Smartnet
        };
        entry.is_analog = is_analog;

        // 4. Calculate priority
        let priority = if let Some(tgp) = tg_policy {
            let mut p = tgp.priority;
            for ptg in &patch_tgs {
                if let Some(ptgp) = sys_policy.talkgroups.get(ptg) {
                    if ptgp.priority >= 0 && ptgp.priority < p {
                        p = ptgp.priority;
                    }
                }
            }
            p
        } else {
            0
        };

        // 5. Priority = -1 means disabled
        if priority == -1 {
            entry.state = CallState::Rejected(RejectReason::IgnoredTg);
            tracing::info!(
                talkgroup = tg.0,
                sys_num,
                "not recording: priority disabled (-1)"
            );
            return vec![];
        }

        // 6. Emit RequestRecording — executor handles FFI source/recorder allocation
        vec![Effect::RequestRecording {
            call_id: entry.id,
            freq: entry.freq,
            is_analog,
            priority,
        }]
    }

    // ── Recording result callback (from executor after FFI) ──

    pub(crate) fn on_recording_result(
        &mut self,
        call_id: CallId,
        result: RecordingResult,
    ) -> Vec<Effect> {
        let entry = match self.calls.iter_mut().find(|e| e.id == call_id) {
            Some(e) => e,
            None => return vec![],
        };

        match result {
            RecordingResult::Started { rec_num } => {
                entry.state = CallState::Recording;
                entry.recorder = Some(rec_num);
                tracing::info!(
                    short_name = %entry.short_name,
                    rec_num = rec_num.0,
                    talkgroup = entry.talkgroup,
                    freq = %entry.freq,
                    tdma = entry.phase2_tdma,
                    slot = entry.tdma_slot,
                    analog = entry.is_analog,
                    "starting recorder"
                );
                vec![Effect::EmitSetupRecorder { call_id }]
            }
            RecordingResult::NoRecorder => {
                entry.state = CallState::Rejected(RejectReason::NoRecorder);
                vec![]
            }
            RecordingResult::NoSource => {
                entry.state = CallState::Rejected(RejectReason::NoSource);
                tracing::error!(
                    talkgroup = entry.talkgroup,
                    sys_num = entry.sys_num,
                    "not recording: no source covering freq"
                );
                vec![]
            }
        }
    }

    // ── Multisite duplicate detection (pure) ──

    fn check_multisite(
        &self,
        talkgroup: i64,
        sys_num: i32,
        msg: &TrunkMessage,
    ) -> MultiSiteResult {
        let mut result = MultiSiteResult {
            is_duplicate: false,
            is_superseding: false,
            original_idx: None,
        };

        for (idx, entry) in self.calls.iter().enumerate() {
            let Some(trunked) = entry.trunked_fields() else {
                continue;
            };
            if entry.talkgroup != talkgroup {
                continue;
            }
            if entry.sys_num == sys_num {
                continue;
            }
            if !trunked.multi_site {
                continue;
            }
            if entry.state != CallState::Recording {
                continue;
            }
            if trunked.wacn == 0 {
                continue;
            }

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

                let call_preferred_nac = self.get_preferred_nac(entry.sys_num, Talkgroup(talkgroup));
                if call_preferred_nac != call_rfss_site && msg.nac == msg_rfss_site {
                    result.is_superseding = true;
                }
            } else {
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

    fn get_preferred_nac(&self, sys_num: i32, talkgroup: Talkgroup) -> u64 {
        self.config
            .systems
            .get(&sys_num)
            .and_then(|p| p.talkgroups.get(&talkgroup))
            .map_or(0, |tg| tg.preferred_nac)
    }

    // ── Periodic tick ──

    pub(crate) fn tick(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let timeout = self.config.call_timeout;
        let mut ended_call = false;

        for entry in &self.calls {
            if entry.is_conventional() {
                continue;
            }

            let since_update = entry.last_update.elapsed().as_secs_f64();
            if since_update <= timeout {
                continue;
            }

            match entry.state {
                CallState::Recording => {
                    effects.push(Effect::EmitCallEnd { call_id: entry.id });
                    effects.push(Effect::EmitSetupRecorder { call_id: entry.id });
                    effects.push(Effect::RemoveCall { call_id: entry.id });
                    ended_call = true;
                }
                CallState::Idle | CallState::Disabled | CallState::Rejected(_) => {
                    effects.push(Effect::EmitCallEnd { call_id: entry.id });
                    effects.push(Effect::RemoveCall { call_id: entry.id });
                    ended_call = true;
                }
            }
        }

        if ended_call {
            effects.push(Effect::EmitCallsActive);
        }

        // Conventional tick
        effects.extend(self.tick_conventional());

        effects
    }

    fn tick_conventional(&mut self) -> Vec<Effect> {
        let timeout = self.config.call_timeout;
        let mut timed_out = Vec::new();

        for entry in &self.calls {
            if !entry.is_conventional() || entry.recorder.is_none() {
                continue;
            }
            if entry.state == CallState::Disabled {
                continue;
            }

            let idle_timed_out = entry
                .idle_since
                .is_some_and(|t| t.elapsed().as_secs_f64() > timeout);

            let elapsed = entry.start_time.elapsed().map_or(0.0, |d| d.as_secs_f64());
            let max_duration_exceeded =
                entry.max_duration > 0.0 && elapsed > entry.max_duration;

            if idle_timed_out || max_duration_exceeded {
                timed_out.push(entry.id);
            }
        }

        let mut effects = Vec::new();
        for call_id in timed_out {
            effects.extend(self.on_conventional_idle_timeout(call_id));
        }
        effects
    }

    // ── Conventional callbacks from executor ──

    pub(crate) fn on_conventional_idle_timeout(&mut self, call_id: CallId) -> Vec<Effect> {
        let entry = match self.calls.iter_mut().find(|e| e.id == call_id) {
            Some(e) => e,
            None => return vec![],
        };

        let mut effects = vec![
            Effect::EmitCallEnd { call_id },
            Effect::ConventionalRestart { call_id },
        ];

        // After restart, executor reads new call_num/start_time from C++ and
        // calls on_conventional_restarted.
        // C++ start() no longer enables conventional recorders. Rust emits
        // EnableRecorder for non-signal-detection, or leaves disabled for signal detection.
        entry.state = if entry.signal_detection {
            CallState::Disabled
        } else {
            CallState::Recording
        };
        entry.last_update = Instant::now();
        entry.idle_since = None;
        entry.segments.clear();
        entry.segment_start = Instant::now();

        effects.push(Effect::EmitSetupRecorder { call_id });
        effects.push(Effect::EmitCallStart { call_id });

        if !entry.signal_detection {
            effects.push(Effect::EnableRecorder { call_id });
        }

        effects
    }

    pub(crate) fn on_conventional_restarted(
        &mut self,
        call_id: CallId,
        recorder: Option<RecorderNum>,
    ) {
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            entry.call_num = next_call_num();
            entry.start_time = SystemTime::now();
            entry.recorder = recorder;
        }
    }

    pub(crate) fn on_conventional_start_initial(&mut self, call_id: CallId) -> Vec<Effect> {
        let entry = match self.calls.iter_mut().find(|e| e.id == call_id) {
            Some(e) => e,
            None => return vec![],
        };

        let mut effects = vec![
            Effect::ConventionalStartInitial { call_id },
            Effect::EmitCallStart { call_id },
        ];

        if entry.signal_detection {
            entry.state = CallState::Disabled;
        } else {
            entry.state = CallState::Recording;
            effects.push(Effect::EnableRecorder { call_id });
        }

        effects
    }

    // ── Squelch handling ──

    pub(crate) fn on_squelch(&mut self, recorder_num: RecorderNum, muted: bool, pwr_db: f64) {
        for entry in &mut self.calls {
            if !entry.is_conventional() {
                continue;
            }
            if entry.recorder != Some(recorder_num) {
                continue;
            }
            if muted {
                // Squelch closed — close current segment, transition to Idle
                entry.noise = pwr_db;
                if entry.state == CallState::Recording {
                    entry.segments.push(CallSegment {
                        freq: entry.freq,
                        start: entry.segment_start,
                        rec_num: recorder_num,
                    });
                    entry.state = CallState::Idle;
                    entry.idle_since = Some(Instant::now());
                } else if entry.idle_since.is_none() {
                    entry.idle_since = Some(Instant::now());
                }
            } else {
                // Squelch open — start new segment, transition to Recording
                entry.signal = pwr_db;
                entry.idle_since = None;
                if entry.state == CallState::Idle {
                    entry.state = CallState::Recording;
                    entry.segment_start = Instant::now();
                }
            }
            return;
        }
    }

    // ── Signal detection ──

    pub(crate) fn on_signals_detected(
        &mut self,
        source_center_freq: f64,
        signals: &[trunk_recorder_sys::DetectedSignal],
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        for signal in signals {
            let abs_freq = source_center_freq + signal.center_freq;
            for entry in &mut self.calls {
                if !entry.is_conventional() || !entry.signal_detection {
                    continue;
                }
                if entry.state != CallState::Disabled {
                    continue;
                }
                if (entry.freq.as_hz() - abs_freq).abs() < 30_000.0 {
                    entry.state = CallState::Idle;
                    entry.idle_since = Some(Instant::now());
                    effects.push(Effect::EnableRecorder { call_id: entry.id });
                }
            }
        }
        effects
    }

    // ── Call creation callback (executor feeds back C++ metadata) ──

    // ── Conventional call addition ──

    pub(crate) fn add_conventional_call(
        &mut self,
        channel: &ConventionalChannel,
        sys_num: i32,
        policy: &SystemPolicy,
        recorder: Option<RecorderNum>,
    ) -> (CallId, Vec<Effect>) {
        let state = if channel.signal_detection {
            CallState::Disabled
        } else {
            CallState::Idle
        };
        let entry = CallEntry {
            talkgroup: channel.talkgroup.0,
            sys_num,
            short_name: policy.short_name.clone(),
            freq: channel.freq,
            is_analog: channel.is_analog,
            priority: channel.priority,
            max_duration: policy.max_duration,
            recorder,
            state,
            signal_detection: channel.signal_detection,
            squelch_db: channel.squelch_db,
            ..Default::default()
        };
        let call_id = entry.id;
        let effects = if channel.signal_detection {
            vec![]
        } else {
            vec![Effect::EnableRecorder { call_id }]
        };
        self.calls.push(entry);
        (call_id, effects)
    }

    // ── Recorder stop callback ──

    pub(crate) fn on_recorder_stopped(&mut self, call_id: CallId) {
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            entry.recorder = None;
        }
    }

    // ── Patch maintenance ──

    pub(crate) fn clear_stale_patches(&mut self) {
        for pt in self.patch_tables.values_mut() {
            pt.clear_stale();
        }
    }

    pub(crate) fn print_active_patches(&self) {
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

    // ── Conclude all (shutdown) ──

    pub(crate) fn conclude_all(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        for entry in &self.calls {
            if matches!(entry.state, CallState::Recording | CallState::Idle | CallState::Disabled) {
                effects.push(Effect::EmitCallEnd { call_id: entry.id });
            }
            effects.push(Effect::RemoveCall { call_id: entry.id });
        }
        effects.push(Effect::EmitCallsActive);
        effects
    }

    // ── Accessors for executor ──

    pub(crate) fn get_call(&self, call_id: CallId) -> Option<&CallEntry> {
        self.calls.iter().find(|e| e.id == call_id)
    }

    pub(crate) fn get_call_mut(&mut self, call_id: CallId) -> Option<&mut CallEntry> {
        self.calls.iter_mut().find(|e| e.id == call_id)
    }

    /// Expire a trunked call due to recorder write timeout.
    /// Returns effects to conclude and remove the call.
    pub(crate) fn expire_call(&mut self, call_id: CallId) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(entry) = self.get_call(call_id) {
            if matches!(
                entry.state,
                CallState::Recording | CallState::Idle
            ) {
                effects.push(Effect::EmitCallEnd { call_id });
            }
        }
        effects.push(Effect::EmitSetupRecorder { call_id });
        effects.push(Effect::RemoveCall { call_id });
        effects.push(Effect::EmitCallsActive);
        effects
    }

    pub(crate) fn remove_call(&mut self, call_id: CallId) {
        self.calls.retain(|e| e.id != call_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CallManagerConfig {
        let mut systems = HashMap::new();
        systems.insert(1, SystemPolicy {
            short_name: "test".into(),
            record_unknown: true,
            ..Default::default()
        });
        CallManagerConfig {
            call_timeout: 1.0,
            new_call_from_update: false,
            record_uu_v_calls: false,
            default_mode: DefaultMode::Digital,
            control_warn_rate: 0,
            temp_dir: String::new(),
            systems,
        }
    }

    fn grant_msg(talkgroup: i64, freq_hz: f64) -> TrunkMessage {
        TrunkMessage {
            message_type: MessageType::Grant,
            sys_num: 1,
            talkgroup,
            freq: Freq::from_hz(freq_hz),
            source: 100,
            tdma_slot: 0,
            phase2_tdma: false,
            mode: false,
            duplex: false,
            emergency: false,
            encrypted: false,
            priority: 0,
            wacn: 0,
            nac: 0,
            sys_id: 0,
            sys_rfss: 0,
            sys_site_id: 0,
            patch_sg: 0,
            patch_ga1: 0,
            patch_ga2: 0,
            patch_ga3: 0,
        }
    }

    /// Verify effects contain the expected variant names in order.
    /// Only checks the variants listed — extra effects in between are allowed.
    fn assert_effects_contain_ordered(effects: &[Effect], expected: &[&str]) {
        let names: Vec<&str> = effects.iter().map(|e| match e {
            Effect::CreateCall { .. } => "CreateCall",
            Effect::RequestRecording { .. } => "RequestRecording",
            Effect::StopRecording { .. } => "StopRecording",
            Effect::SetFreq { .. } => "SetFreq",
            Effect::SetSource { .. } => "SetSource",
            Effect::EmitCallStart { .. } => "EmitCallStart",
            Effect::EmitCallEnd { .. } => "EmitCallEnd",
            Effect::EmitCallsActive => "EmitCallsActive",
            Effect::EmitSignal { .. } => "EmitSignal",
            Effect::EmitUnitEvent { .. } => "EmitUnitEvent",
            Effect::EmitSetupSystem { .. } => "EmitSetupSystem",
            Effect::EmitSetupRecorder { .. } => "EmitSetupRecorder",
            Effect::AddControlChannel { .. } => "AddControlChannel",
            Effect::ConventionalRestart { .. } => "ConventionalRestart",
            Effect::ConventionalStartInitial { .. } => "ConventionalStartInitial",
            Effect::EnableRecorder { .. } => "EnableRecorder",
            Effect::RemoveCall { .. } => "RemoveCall",
            Effect::Warn { .. } => "Warn",
        }).collect();

        let mut name_iter = names.iter();
        for &exp in expected {
            let found = name_iter.any(|&n| n == exp);
            assert!(found, "expected {exp} in effects, got: {names:?}");
        }
    }

    /// Return the CallId from a CreateCall effect.
    fn extract_create_call_id(effects: &[Effect]) -> CallId {
        effects.iter().find_map(|e| match e {
            Effect::CreateCall { call_id } => Some(*call_id),
            _ => None,
        }).expect("no CreateCall effect found")
    }

    // ── Grant creates a call ──

    #[test]
    fn grant_creates_call_with_recording_request() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);

        let effects = sm.on_message(&msg);

        assert_effects_contain_ordered(&effects, &[
            "CreateCall",
            "RequestRecording",
            "EmitCallStart",
        ]);
        assert_eq!(sm.calls.len(), 1);
        assert_eq!(sm.calls[0].talkgroup, 100);
    }

    // ── Grant on existing call with same freq updates source ──

    #[test]
    fn grant_same_freq_updates_source() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        sm.on_message(&msg);

        let mut msg2 = grant_msg(100, 851_000_000.0);
        msg2.source = 200;
        let effects = sm.on_message(&msg2);

        assert_effects_contain_ordered(&effects, &["SetSource", "EmitSignal"]);
        assert_eq!(sm.calls.len(), 1);
    }

    // ── Grant on existing call with different freq retunes ──

    #[test]
    fn grant_different_freq_retunes() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);

        // Simulate recording started
        sm.on_recording_result(call_id, RecordingResult::Started {
            rec_num: RecorderNum(0),
        });

        let msg2 = grant_msg(100, 852_000_000.0);
        let effects = sm.on_message(&msg2);

        assert_effects_contain_ordered(&effects, &[
            "StopRecording",
            "SetFreq",
            "RequestRecording",
        ]);
    }

    // ── Recording result updates state ──

    #[test]
    fn recording_result_success_sets_recording_state() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);

        let effects = sm.on_recording_result(call_id, RecordingResult::Started {
            rec_num: RecorderNum(5),
        });

        assert_eq!(sm.calls[0].state, CallState::Recording);
        assert_eq!(sm.calls[0].recorder, Some(RecorderNum(5)));
        assert_effects_contain_ordered(&effects, &["EmitSetupRecorder"]);
    }

    #[test]
    fn recording_result_no_source_rejects() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);

        sm.on_recording_result(call_id, RecordingResult::NoSource);

        assert_eq!(sm.calls[0].state, CallState::Rejected(RejectReason::NoSource));
    }

    // ── TDULC transitions Recording → Idle ──

    #[test]
    fn tdulc_transitions_recording_to_idle() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        sm.on_recording_result(call_id, RecordingResult::Started {
            rec_num: RecorderNum(0),
        });

        let tdulc = TrunkMessage {
            message_type: MessageType::Tdulc,
            talkgroup: 100,
            sys_num: 1,
            ..grant_msg(100, 0.0)
        };
        sm.on_message(&tdulc);

        assert_eq!(sm.calls[0].state, CallState::Idle);
        assert_eq!(sm.calls[0].segments.len(), 1);
    }

    // ── Grant on Idle call resumes recording ──

    #[test]
    fn grant_on_idle_resumes_recording() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        sm.on_recording_result(call_id, RecordingResult::Started {
            rec_num: RecorderNum(0),
        });

        // TDULC → Idle
        let tdulc = TrunkMessage {
            message_type: MessageType::Tdulc,
            talkgroup: 100,
            sys_num: 1,
            ..grant_msg(100, 0.0)
        };
        sm.on_message(&tdulc);
        assert_eq!(sm.calls[0].state, CallState::Idle);

        // Grant again → Recording
        sm.on_message(&msg);
        assert_eq!(sm.calls[0].state, CallState::Recording);
    }

    // ── Tick: timed-out call emits EmitCallEnd BEFORE RemoveCall ──

    #[test]
    fn tick_emits_call_end_before_remove() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        sm.on_recording_result(call_id, RecordingResult::Started {
            rec_num: RecorderNum(0),
        });

        // Force the call to be timed out
        sm.calls[0].last_update = Instant::now() - std::time::Duration::from_secs(10);

        let effects = sm.tick();

        // EmitCallEnd must come before RemoveCall
        let end_pos = effects.iter().position(|e| matches!(e, Effect::EmitCallEnd { .. }));
        let remove_pos = effects.iter().position(|e| matches!(e, Effect::RemoveCall { .. }));

        assert!(end_pos.is_some(), "expected EmitCallEnd in tick effects");
        assert!(remove_pos.is_some(), "expected RemoveCall in tick effects");
        assert!(
            end_pos.unwrap() < remove_pos.unwrap(),
            "EmitCallEnd must come before RemoveCall, got end={} remove={}",
            end_pos.unwrap(),
            remove_pos.unwrap()
        );

        // Call entry must still exist until RemoveCall is processed
        assert!(sm.get_call(call_id).is_some(), "entry must exist until RemoveCall is executed");
    }

    #[test]
    fn tick_emits_call_end_before_remove_for_rejected() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        sm.on_recording_result(call_id, RecordingResult::NoSource);

        sm.calls[0].last_update = Instant::now() - std::time::Duration::from_secs(10);

        let effects = sm.tick();

        let end_pos = effects.iter().position(|e| matches!(e, Effect::EmitCallEnd { .. }));
        let remove_pos = effects.iter().position(|e| matches!(e, Effect::RemoveCall { .. }));

        assert!(end_pos.is_some());
        assert!(remove_pos.is_some());
        assert!(end_pos.unwrap() < remove_pos.unwrap());
    }

    // ── Tick: call within timeout is NOT removed ──

    #[test]
    fn tick_does_not_remove_active_call() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        sm.on_recording_result(call_id, RecordingResult::Started {
            rec_num: RecorderNum(0),
        });

        let effects = sm.tick();

        assert!(!effects.iter().any(|e| matches!(e, Effect::RemoveCall { .. })));
        assert_eq!(sm.calls.len(), 1);
    }

    // ── conclude_all emits EmitCallEnd BEFORE RemoveCall ──

    #[test]
    fn conclude_all_emits_call_end_before_remove() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        sm.on_recording_result(call_id, RecordingResult::Started {
            rec_num: RecorderNum(0),
        });

        let effects = sm.conclude_all();

        let end_pos = effects.iter().position(|e| matches!(e, Effect::EmitCallEnd { .. }));
        let remove_pos = effects.iter().position(|e| matches!(e, Effect::RemoveCall { .. }));

        assert!(end_pos.is_some(), "expected EmitCallEnd");
        assert!(remove_pos.is_some(), "expected RemoveCall");
        assert!(
            end_pos.unwrap() < remove_pos.unwrap(),
            "EmitCallEnd must come before RemoveCall"
        );

        // Entries still exist until executor processes RemoveCall
        assert!(sm.get_call(call_id).is_some());
    }

    // ── remove_call actually removes ──

    #[test]
    fn remove_call_removes_entry() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);

        assert_eq!(sm.calls.len(), 1);
        sm.remove_call(call_id);
        assert_eq!(sm.calls.len(), 0);
    }

    // ── Conventional calls are NOT removed by tick ──

    #[test]
    fn tick_skips_conventional_calls() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(0)));

        sm.calls[0].last_update = Instant::now() - std::time::Duration::from_secs(10);

        let effects = sm.tick();

        assert!(!effects.iter().any(|e| matches!(e, Effect::RemoveCall { .. })));
        assert!(sm.get_call(call_id).is_some());
    }

    // ── Conventional idle timeout preserves recorder ──

    #[test]
    fn conventional_idle_timeout_preserves_recorder() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        sm.calls[0].state = CallState::Recording;

        let effects = sm.on_conventional_idle_timeout(call_id);

        // Must emit EmitCallEnd followed by ConventionalRestart
        assert!(effects.iter().any(|e| matches!(e, Effect::EmitCallEnd { .. })));
        assert!(effects.iter().any(|e| matches!(e, Effect::ConventionalRestart { .. })));

        // Recorder must still be set after timeout — the executor conclude path
        // must NOT clear it, or tick_conventional will skip this call forever.
        let entry = sm.get_call(call_id).unwrap();
        assert_eq!(entry.recorder, Some(RecorderNum(12)));
        assert_eq!(entry.state, CallState::Recording);
    }

    // ── Conventional restart updates recorder number ──

    #[test]
    fn conventional_restart_updates_recorder_for_squelch() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        sm.calls[0].state = CallState::Recording;

        // Simulate restart with a new recorder number (C++ may reassign)
        sm.on_conventional_restarted(call_id, Some(RecorderNum(13)));

        let entry = sm.get_call(call_id).unwrap();
        assert_eq!(entry.recorder, Some(RecorderNum(13)));

        // Squelch on the NEW recorder must match this call
        sm.on_squelch(RecorderNum(13), true, -30.0);
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);

        // Squelch on the OLD recorder must NOT match
        sm.on_squelch(RecorderNum(12), false, -20.0);
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
    }

    // ── Squelch handling ──

    #[test]
    fn squelch_muted_transitions_to_idle() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let _ = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(5)));
        sm.calls[0].state = CallState::Recording;

        sm.on_squelch(RecorderNum(5), true, -30.0);

        assert_eq!(sm.calls[0].state, CallState::Idle);
        assert_eq!(sm.calls[0].segments.len(), 1);
    }

    #[test]
    fn squelch_unmuted_transitions_to_recording() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let _ = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(5)));
        sm.calls[0].state = CallState::Idle;

        sm.on_squelch(RecorderNum(5), false, -20.0);

        assert_eq!(sm.calls[0].state, CallState::Recording);
    }

    // ── Unknown message type produces Warn ──

    #[test]
    fn unknown_message_type_produces_warn() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = TrunkMessage {
            message_type: MessageType::Unknown(99),
            ..grant_msg(0, 0.0)
        };
        let effects = sm.on_message(&msg);

        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], Effect::Warn { message } if message.contains("99")));
    }

    // ── Encrypted call is rejected ──

    #[test]
    fn encrypted_call_rejected_when_not_monitoring() {
        let mut sm = TrunkStateMachine::new(test_config());
        let mut msg = grant_msg(100, 851_000_000.0);
        msg.encrypted = true;

        let effects = sm.on_message(&msg);

        assert_effects_contain_ordered(&effects, &["CreateCall", "EmitCallStart"]);
        // Should NOT contain RequestRecording
        assert!(!effects.iter().any(|e| matches!(e, Effect::RequestRecording { .. })));
        assert_eq!(sm.calls[0].state, CallState::Rejected(RejectReason::Encrypted));
    }

    // ── Signal detection ──

    fn signal_detection_channel() -> ConventionalChannel {
        ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            signal_detection: true,
            ..Default::default()
        }
    }

    #[test]
    fn signal_detection_starts_disabled() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));

        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);
    }

    #[test]
    fn signal_detection_restart_transitions_to_disabled() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        sm.calls[0].state = CallState::Recording;

        let effects = sm.on_conventional_idle_timeout(call_id);

        assert!(effects.iter().any(|e| matches!(e, Effect::ConventionalRestart { .. })));
        assert!(!effects.iter().any(|e| matches!(e, Effect::EnableRecorder { .. })));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);
    }

    #[test]
    fn signals_detected_enables_matching_recorder() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);

        // Source center 487 MHz, signal at +1 MHz offset → 488 MHz absolute
        let signals = vec![trunk_recorder_sys::DetectedSignal {
            center_freq: 1_000_000.0,
            max_rssi: -20,
            threshold: -45,
        }];
        let effects = sm.on_signals_detected(487_000_000.0, &signals);

        assert!(effects.iter().any(|e| matches!(e, Effect::EnableRecorder { .. })));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
    }

    #[test]
    fn signals_detected_no_match_when_freq_too_far() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));

        // Signal at 490 MHz, channel at 488 MHz — 2 MHz apart, way beyond 12.5kHz tolerance
        let signals = vec![trunk_recorder_sys::DetectedSignal {
            center_freq: 3_000_000.0,
            max_rssi: -20,
            threshold: -45,
        }];
        let effects = sm.on_signals_detected(487_000_000.0, &signals);

        assert!(effects.is_empty());
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);
    }

    #[test]
    fn signals_detected_skips_already_recording() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        sm.calls[0].state = CallState::Recording;

        let signals = vec![trunk_recorder_sys::DetectedSignal {
            center_freq: 1_000_000.0,
            max_rssi: -20,
            threshold: -45,
        }];
        let effects = sm.on_signals_detected(487_000_000.0, &signals);

        assert!(effects.is_empty());
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Recording);
    }

    #[test]
    fn no_signal_detection_starts_idle() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));

        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
    }

    #[test]
    fn no_signal_detection_restart_stays_recording() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        sm.calls[0].state = CallState::Recording;

        let effects = sm.on_conventional_idle_timeout(call_id);

        assert!(effects.iter().any(|e| matches!(e, Effect::ConventionalRestart { .. })));
        assert!(effects.iter().any(|e| matches!(e, Effect::EnableRecorder { .. })));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Recording);
    }

    #[test]
    fn signals_detected_within_tolerance() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));

        // Signal at 488_010_000 Hz (10kHz offset from channel) — within 12.5kHz tolerance
        let signals = vec![trunk_recorder_sys::DetectedSignal {
            center_freq: 1_010_000.0,
            max_rssi: -20,
            threshold: -45,
        }];
        let effects = sm.on_signals_detected(487_000_000.0, &signals);

        assert!(effects.iter().any(|e| matches!(e, Effect::EnableRecorder { .. })));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
    }
}
