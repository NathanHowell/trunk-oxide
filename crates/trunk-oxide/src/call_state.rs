use std::collections::HashMap;
use std::time::Instant;

use trunk_config::{DefaultMode, SystemType, TalkgroupMode};

// ── Newtypes ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CallId(u64);

static NEXT_CALL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl CallId {
    pub(crate) fn next() -> Self {
        Self(NEXT_CALL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SysNum(pub(crate) i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Talkgroup(pub(crate) i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SourceId(pub(crate) i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RecorderNum(pub(crate) i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Freq(pub(crate) u64);

impl Freq {
    pub(crate) fn from_hz(hz: f64) -> Self {
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
            CallState::Idle => ProtoCallState::Idle as i32,
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
    pub talkgroups: HashMap<Talkgroup, TalkgroupPolicy>,
    pub sys_id: u64,
    pub wacn: u64,
    pub nac: u64,
}

pub(crate) static DEFAULT_POLICY: std::sync::LazyLock<SystemPolicy> =
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

#[derive(Clone)]
pub struct CallManagerConfig {
    pub call_timeout: f64,
    pub new_call_from_update: bool,
    pub record_uu_v_calls: bool,
    pub default_mode: DefaultMode,
    pub control_warn_rate: i32,
    pub systems: HashMap<i32, SystemPolicy>,
}

pub(crate) struct SystemIdentity {
    pub(crate) sys_id: u64,
    pub(crate) wacn: u64,
    pub(crate) nac: u64,
    pub(crate) rfss: i32,
    pub(crate) site_id: i32,
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
    pub(crate) start_time: i64,
    pub(crate) freq: f64,
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

// ── Grant input (pure Rust view of a trunk message for call dispatch) ──

pub(crate) struct GrantInput {
    pub(crate) sys_num: i32,
    pub(crate) talkgroup: i64,
    pub(crate) freq: f64,
    pub(crate) tdma_slot: i32,
    pub(crate) phase2_tdma: bool,
    pub(crate) mode: bool,
    pub(crate) duplex: bool,
    pub(crate) emergency: bool,
    pub(crate) encrypted: bool,
    pub(crate) source: i64,
    pub(crate) priority: i32,
    pub(crate) wacn: u64,
    pub(crate) nac: u64,
    pub(crate) sys_rfss: i32,
    pub(crate) sys_site_id: i32,
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
        grant: CreateCallData,
    },
    RequestRecording {
        call_id: CallId,
        freq: f64,
        is_analog: bool,
        priority: i32,
    },
    StopRecording {
        call_id: CallId,
    },
    SetFreq {
        call_id: CallId,
        freq: f64,
    },
    SetSource {
        call_id: CallId,
        source_id: i64,
    },
    DestroyCall {
        call_id: CallId,
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
        freq: f64,
    },

    // ── Conventional ──
    ConventionalRestart {
        call_id: CallId,
    },
    ConventionalStartInitial {
        call_id: CallId,
    },
}

#[derive(Debug)]
pub(crate) struct CreateCallData {
    pub(crate) sys_num: i32,
    pub(crate) talkgroup: i64,
    pub(crate) freq: f64,
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
    pub(crate) freq: f64,
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

                if self.update_call_source(idx, msg.source, &mut effects) {
                    effects.push(Effect::EmitCallStart {
                        call_id: entry.id,
                    });
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
                        freq: Freq::from_hz(old_freq),
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

        let call_id = CallId::next();
        let new_entry = CallEntry {
            id: call_id,
            call_num: 0, // set by executor after C++ call creation
            talkgroup: msg.talkgroup,
            sys_num: msg.sys_num,
            short_name: policy.short_name.clone(),
            tdma_slot: msg.tdma_slot,
            phase2_tdma: msg.phase2_tdma,
            mode: msg.mode,
            duplex: msg.duplex,
            emergency: msg.emergency,
            start_time: 0, // set by executor after C++ call creation
            freq: msg.freq,
            state: CallState::Idle,
            encrypted: msg.encrypted,
            is_analog: false,
            current_source_id: msg.source,
            signal: 0.0,
            noise: 0.0,
            priority: msg.priority,
            recorder: None,
            last_update: Instant::now(),
            max_duration: policy.max_duration,
            segments: Vec::new(),
            segment_start: Instant::now(),
            idle_since: None,
            call_mode: CallMode::Trunked(TrunkedFields {
                multi_site: policy.multi_site,
                wacn: msg.wacn,
                rfss: msg.sys_rfss,
                site_id: msg.sys_site_id,
                multi_site_name: policy.multi_site_name.clone(),
                conversation_mode: policy.conversation_mode,
            }),
        };

        self.calls.push(new_entry);
        let new_idx = self.calls.len() - 1;

        effects.push(Effect::CreateCall {
            call_id,
            grant: CreateCallData {
                sys_num: msg.sys_num,
                talkgroup: msg.talkgroup,
                freq: msg.freq,
            },
        });

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
                    effects.push(Effect::EmitCallEnd {
                        call_id: orig.id,
                    });
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

        for (idx, entry) in self.calls.iter_mut().enumerate() {
            if entry.talkgroup == msg.talkgroup
                && entry.sys_num == msg.sys_num
                && entry.freq == msg.freq
                && entry.tdma_slot == msg.tdma_slot
                && entry.phase2_tdma == msg.phase2_tdma
            {
                if msg.encrypted {
                    entry.encrypted = true;
                }

                if self.update_call_source_entry(entry, msg.source, &mut effects) {
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
                freq: Freq::from_hz(entry.freq),
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
                    freq = %Freq::from_hz(entry.freq),
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

        self.calls.retain_mut(|entry| {
            if entry.is_conventional() {
                return true;
            }

            let since_update = entry.last_update.elapsed().as_secs_f64();

            match entry.state {
                CallState::Recording => {
                    // Executor must check recorder write time via FFI and call
                    // on_recorder_timeout if needed. Here we only check update timeout.
                    if since_update > timeout {
                        effects.push(Effect::EmitCallEnd { call_id: entry.id });
                        effects.push(Effect::EmitSetupRecorder { call_id: entry.id });
                        ended_call = true;
                        return false;
                    }
                }
                CallState::Idle | CallState::Rejected(_) => {
                    if since_update > timeout {
                        effects.push(Effect::EmitCallEnd { call_id: entry.id });
                        ended_call = true;
                        return false;
                    }
                }
            }

            true
        });

        if ended_call {
            effects.push(Effect::EmitCallsActive);
        }

        // Conventional tick
        effects.extend(self.tick_conventional());

        effects
    }

    fn tick_conventional(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();

        for entry in &mut self.calls {
            if !entry.is_conventional() {
                continue;
            }
            if entry.recorder.is_none() {
                continue;
            }

            // The executor checks call_get_current_length and call_get_recorder_is_active
            // via FFI, then calls on_conventional_idle_timeout or on_conventional_start_initial.
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
        entry.state = CallState::Recording;
        entry.last_update = Instant::now();
        entry.idle_since = None;
        entry.segments.clear();
        entry.segment_start = Instant::now();

        effects.push(Effect::EmitSetupRecorder { call_id });
        effects.push(Effect::EmitCallStart { call_id });

        effects
    }

    pub(crate) fn on_conventional_restarted(
        &mut self,
        call_id: CallId,
        call_num: i64,
        start_time: i64,
    ) {
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            entry.call_num = call_num;
            entry.start_time = start_time;
        }
    }

    pub(crate) fn on_conventional_start_initial(&mut self, call_id: CallId) -> Vec<Effect> {
        let entry = match self.calls.iter_mut().find(|e| e.id == call_id) {
            Some(e) => e,
            None => return vec![],
        };

        entry.state = CallState::Recording;
        vec![
            Effect::ConventionalStartInitial { call_id },
            Effect::EmitCallStart { call_id },
        ]
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
                        freq: Freq::from_hz(entry.freq),
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

    // ── Call creation callback (executor feeds back C++ metadata) ──

    pub(crate) fn on_call_created(&mut self, call_id: CallId, call_num: i64, start_time: i64) {
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            entry.call_num = call_num;
            entry.start_time = start_time;
        }
    }

    // ── Conventional call addition ──

    pub(crate) fn add_conventional_call(
        &mut self,
        call_id: CallId,
        call_num: i64,
        talkgroup: i64,
        freq: f64,
        sys_num: i32,
        start_time: i64,
        short_name: String,
        tdma_slot: i32,
        phase2_tdma: bool,
        mode: bool,
        duplex: bool,
        emergency: bool,
        encrypted: bool,
        is_analog: bool,
        current_source_id: i64,
        signal: f64,
        noise: f64,
        priority: i32,
        max_duration: f64,
        recorder: Option<RecorderNum>,
    ) {
        self.calls.push(CallEntry {
            id: call_id,
            call_num,
            talkgroup,
            sys_num,
            short_name,
            tdma_slot,
            phase2_tdma,
            mode,
            duplex,
            emergency,
            start_time,
            freq,
            state: CallState::Idle,
            encrypted,
            is_analog,
            current_source_id,
            signal,
            noise,
            priority,
            recorder,
            last_update: Instant::now(),
            max_duration,
            segments: Vec::new(),
            segment_start: Instant::now(),
            idle_since: None,
            call_mode: CallMode::Conventional,
        });
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
            if entry.state == CallState::Recording || entry.state == CallState::Idle {
                effects.push(Effect::EmitCallEnd { call_id: entry.id });
            }
        }
        self.calls.clear();
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
}
