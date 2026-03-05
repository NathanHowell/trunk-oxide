use std::collections::HashMap;
use std::time::Instant;

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

/// Unified call state for both trunked and conventional calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallState {
    /// Actively recording audio.
    Recording,
    /// Between transmissions (squelch closed / no Grant refresh).
    Idle,
    /// Not recording for a policy reason.
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

// ── Effect enum ──

/// Commands returned by the state machine for the executor to process.
/// The state machine never performs side effects directly.
#[derive(Debug)]
pub(crate) enum Effect {
    // ── Recorder lifecycle ──
    CreateCall {
        call_id: CallId,
        talkgroup: Talkgroup,
        sys_num: SysNum,
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
        source_id: SourceId,
    },
    RestartRecorder {
        call_id: CallId,
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
        source_id: SourceId,
    },
    EmitUnitEvent {
        sys_num: SysNum,
        type_: UnitEventType,
        source_id: SourceId,
        talkgroup: Talkgroup,
    },
    EmitSetupSystem {
        sys_num: SysNum,
    },
    EmitSetupRecorder {
        call_id: CallId,
    },

    // ── FFI pass-through ──
    AddControlChannel {
        sys_num: SysNum,
        freq: Freq,
    },
    UpdateSystemSysid {
        sys_num: SysNum,
        rfss: i32,
        site_id: i32,
    },
    UpdateSystemStatus {
        sys_num: SysNum,
        sys_id: u64,
        wacn: u64,
        nac: u64,
    },

    // ── Conventional ──
    ConventionalRestart {
        call_id: CallId,
    },
    ConventionalStartInitial {
        call_id: CallId,
    },

    // ── Diagnostics ──
    Warn {
        message: String,
    },
}

/// Mirror of TrUnitEventType for use in pure Rust effects.
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
