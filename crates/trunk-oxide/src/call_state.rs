use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime};

use crate::clock::Clock;

use trunk_config::{DefaultMode, SystemType, TalkgroupMode};

use crate::adaptive_squelch::AdaptiveSquelch;

// ── Read-only view ──

/// Read-only view of a [`CallEntry`], exposed to the executor during effect
/// processing.  All field access goes through getters — no direct mutation
/// is possible.
pub(crate) struct CallView<'a>(&'a CallEntry);

impl<'a> CallView<'a> {
    pub(crate) const fn id(&self) -> CallId {
        self.0.id
    }

    /// Control channel grants and updates that refreshed the call.
    pub(crate) const fn refreshes(&self) -> u32 {
        self.0.refreshes
    }

    /// Since the control channel last mentioned the call.
    pub(crate) fn since_update(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.0.last_update)
    }
    pub(crate) const fn call_num(&self) -> i64 {
        self.0.call_num
    }
    pub(crate) const fn talkgroup(&self) -> i64 {
        self.0.talkgroup
    }
    pub(crate) const fn sys_num(&self) -> i32 {
        self.0.sys_num
    }
    pub(crate) fn short_name(&self) -> &str {
        &self.0.short_name
    }
    pub(crate) const fn tdma_slot(&self) -> i32 {
        self.0.tdma_slot
    }
    pub(crate) const fn phase2_tdma(&self) -> bool {
        self.0.phase2_tdma
    }
    pub(crate) const fn mode(&self) -> bool {
        self.0.mode
    }
    pub(crate) const fn duplex(&self) -> bool {
        self.0.duplex
    }
    pub(crate) const fn emergency(&self) -> bool {
        self.0.emergency
    }
    pub(crate) const fn start_time(&self) -> Instant {
        self.0.start_time
    }
    pub(crate) const fn freq(&self) -> Freq {
        self.0.freq
    }
    pub(crate) const fn state(&self) -> CallState {
        self.0.state
    }
    pub(crate) const fn encrypted(&self) -> bool {
        self.0.encrypted
    }
    pub(crate) const fn reject_reason(&self) -> Option<RejectReason> {
        match self.0.state {
            CallState::Rejected(r) => Some(r),
            _ => None,
        }
    }
    pub(crate) const fn is_analog(&self) -> bool {
        self.0.is_analog
    }
    pub(crate) const fn current_source_id(&self) -> i64 {
        self.0.current_source_id
    }
    pub(crate) const fn signal(&self) -> Option<f64> {
        self.0.signal
    }
    pub(crate) const fn noise(&self) -> Option<f64> {
        self.0.noise
    }
    pub(crate) const fn priority(&self) -> i32 {
        self.0.priority
    }
    pub(crate) const fn recorder(&self) -> Option<RecorderNum> {
        self.0.recorder
    }
    pub(crate) const fn squelch_db(&self) -> f64 {
        self.0.squelch_db
    }
    pub(crate) const fn signal_detection(&self) -> bool {
        self.0.signal_detection
    }
    pub(crate) const fn is_conventional(&self) -> bool {
        self.0.is_conventional()
    }
    pub(crate) const fn is_trunked(&self) -> bool {
        self.0.is_trunked()
    }
    pub(crate) const fn trunked_fields(&self) -> Option<&TrunkedFields> {
        self.0.trunked_fields()
    }
    pub(crate) fn recording_duration(&self, now: Instant) -> Duration {
        self.0.recording_duration(now)
    }
    pub(crate) fn segments(&self) -> &[CallSegment] {
        &self.0.segments
    }
    pub(crate) const fn segment_start(&self) -> Instant {
        self.0.segment_start
    }
    #[allow(dead_code)]
    pub(crate) fn adaptive_squelch_threshold(&self) -> Option<f64> {
        self.0
            .adaptive_squelch
            .as_ref()
            .map(AdaptiveSquelch::current_threshold)
    }
}

/// Minimum time squelch must stay closed before we transition from Recording
/// to Idle. Filters out rapid open/close chatter near the noise floor without
/// losing any audio (the C++ recorder captures independently).
const SQUELCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Maximum time a conventional channel can record continuously without any
/// squelch close event before forcing an idle timeout. Catches stuck
/// recordings where the noise floor sits at or above the squelch threshold,
/// preventing indefinite garbage recordings.
const STUCK_RECORDING_BACKSTOP: Duration = Duration::from_secs(30);

/// Minimum length of a trunked transmission before an in-band source change is
/// allowed to cut a new per-speaker boundary. Rapid P25 dispatch hand-offs
/// (A→B→A within a second) would otherwise fragment one exchange into many
/// sub-second transmissions; below this floor the source change only re-routes
/// (SetSource + EmitSignal) without ending the transmission.
const MIN_TRANSMISSION: Duration = Duration::from_millis(700);

// ── Newtypes ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CallId(u64);

static NEXT_CALL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl CallId {
    pub(crate) fn next() -> Self {
        Self(NEXT_CALL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    pub(crate) const fn as_u64(self) -> u64 {
        self.0
    }

    /// Reconstruct a `CallId` from the `rust_call_id` round-tripped through C++
    /// (set via [`Self::as_u64`] at recorder creation, returned on audio/source
    /// callbacks).
    pub(crate) const fn from_u64(id: u64) -> Self {
        Self(id)
    }
}

/// Monotonic call number counter, replacing C++'s `Call::call_counter`.
static NEXT_CALL_NUM: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub(crate) fn next_call_num() -> i64 {
    NEXT_CALL_NUM.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn system_time_to_unix(t: SystemTime) -> i64 {
    crate::clock::unix_seconds(t)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Talkgroup(pub(crate) i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RecorderNum(pub(crate) i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Freq(pub(crate) u64);

impl Freq {
    pub const fn from_hz(hz: f64) -> Self {
        Self(hz as u64)
    }

    pub(crate) const fn as_hz(self) -> f64 {
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
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            CallState::Recording => "Recording",
            CallState::Idle => "Idle",
            CallState::Disabled => "Disabled",
            CallState::Rejected(_) => "Rejected",
        }
    }

    pub(crate) const fn to_proto(self) -> i32 {
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
            CallState::Rejected(RejectReason::NoSource) => ProtoCallState::RejectedNoSource as i32,
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

impl RejectReason {
    /// Low-cardinality label for metrics.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownTg => "unknown_tg",
            Self::IgnoredTg => "ignored_tg",
            Self::NoSource => "no_source",
            Self::NoRecorder => "no_recorder",
            Self::Encrypted => "encrypted",
            Self::Duplicate => "duplicate",
            Self::Superseded => "superseded",
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
    /// When it closed. (It used to be measured to "now", so a call's earlier
    /// segments kept growing until it ended.)
    pub(crate) end: Instant,
    pub(crate) rec_num: RecorderNum,
}

impl CallSegment {
    pub(crate) fn duration(&self) -> Duration {
        self.end.saturating_duration_since(self.start)
    }
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

pub(crate) static DEFAULT_POLICY: LazyLock<SystemPolicy> = LazyLock::new(SystemPolicy::default);

#[derive(Clone)]
pub struct CallManagerConfig {
    pub call_timeout: f64,
    pub new_call_from_update: bool,
    pub record_uu_v_calls: bool,
    pub default_mode: DefaultMode,
    pub temp_dir: String,
    pub systems: HashMap<i32, SystemPolicy>,
    /// Enable the per-source adaptive frequency-correction loop (tracks TCXO
    /// drift by nudging the software tune offset toward zero FLL freq_error).
    pub adaptive_freq_correction: bool,
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

    pub(crate) fn update(&mut self, sg: u64, ga1: u64, ga2: u64, ga3: u64, now: Instant) {
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

    pub(crate) fn clear_stale(&mut self, now: Instant) {
        self.patches.retain(|_, patch| {
            patch.retain(|_, last_update| {
                now.saturating_duration_since(*last_update).as_secs() < 10
            });
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
    pub(crate) start_time: Instant,
    pub(crate) freq: Freq,
    pub(crate) state: CallState,
    pub(crate) encrypted: bool,
    /// P25 encryption algorithm ID (ESS `ALGID`, e.g. `0x84` AES-256) as decoded
    /// from the voice channel. `None` until the decoder reports one (only
    /// encrypted calls we hold no key for).
    pub(crate) alg_id: Option<u8>,
    /// P25 encryption key ID (ESS `KeyID`), reported together with `alg_id`.
    pub(crate) key_id: Option<u16>,
    pub(crate) is_analog: bool,
    pub(crate) current_source_id: i64,
    pub(crate) signal: Option<f64>,
    pub(crate) noise: Option<f64>,
    pub(crate) priority: i32,
    pub(crate) recorder: Option<RecorderNum>,
    pub(crate) last_update: Instant,
    /// Control channel grants and updates that refreshed `last_update`.
    pub(crate) refreshes: u32,
    /// The voice channel ended a transmission (terminator) since this one
    /// began: the next grant is a new key-up, even from the same handset.
    pub(crate) ptt_ended: bool,
    pub(crate) max_duration: f64,
    pub(crate) segments: Vec<CallSegment>,
    pub(crate) segment_start: Instant,
    pub(crate) idle_since: Option<Instant>,
    pub(crate) call_mode: CallMode,
    pub(crate) signal_detection: bool,
    pub(crate) squelch_db: f64,
    /// When the squelch last closed (pending debounce). `None` means squelch
    /// is open or the close has been finalized.
    pub(crate) squelch_closed_at: Option<Instant>,
    /// Per-channel adaptive squelch tracker. Present only for analog
    /// conventional channels.
    pub(crate) adaptive_squelch: Option<AdaptiveSquelch>,
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
            start_time: Instant::now(),
            freq: Freq::from_hz(0.0),
            state: CallState::Idle,
            encrypted: false,
            alg_id: None,
            key_id: None,
            is_analog: false,
            current_source_id: 0,
            signal: None,
            noise: None,
            priority: 0,
            recorder: None,
            last_update: Instant::now(),
            refreshes: 0,
            ptt_ended: false,
            max_duration: 0.0,
            segments: Vec::new(),
            segment_start: Instant::now(),
            idle_since: None,
            call_mode: CallMode::Conventional,
            signal_detection: false,
            squelch_db: 0.0,
            squelch_closed_at: None,
            adaptive_squelch: None,
        }
    }
}

impl CallEntry {
    pub(crate) const fn is_conventional(&self) -> bool {
        matches!(self.call_mode, CallMode::Conventional)
    }

    pub(crate) const fn is_trunked(&self) -> bool {
        matches!(self.call_mode, CallMode::Trunked(_))
    }

    pub(crate) const fn trunked_fields(&self) -> Option<&TrunkedFields> {
        match &self.call_mode {
            CallMode::Trunked(t) => Some(t),
            _ => None,
        }
    }

    /// The grant names exactly this call's channel and slot.
    fn on_channel(&self, msg: &TrunkMessage) -> bool {
        self.freq == msg.freq
            && self.phase2_tdma == msg.phase2_tdma
            && self.tdma_slot == msg.tdma_slot
    }

    /// The grant needs this call's channel: the same frequency, and the same
    /// slot when both are TDMA (a phase 1 grant takes both slots).
    fn shares_channel(&self, msg: &TrunkMessage) -> bool {
        self.freq == msg.freq
            && (!(self.phase2_tdma && msg.phase2_tdma) || self.tdma_slot == msg.tdma_slot)
    }

    /// End the current keyed transmission *without* emitting a boundary: push
    /// the in-progress segment, reset `segment_start`, and transition Recording
    /// → Idle. Shared by the conventional squelch-debounce finalize and the
    /// trunked TDULC PTT-end — both close the transmission in place and leave a
    /// later trigger (squelch open / grant) to resume or conclude the call.
    fn close_segment_and_idle(&mut self, now: Instant) {
        let rec_num = self.recorder.unwrap_or(RecorderNum(-1));
        self.segments.push(CallSegment {
            freq: self.freq,
            start: self.segment_start,
            end: now,
            rec_num,
        });
        self.segment_start = now;
        self.state = CallState::Idle;
        self.idle_since = Some(now);
    }

    /// Rotation bookkeeping shared by a trunked soft boundary
    /// ([`Effect::RotateTransmission`]) and a conventional recorder recycle
    /// ([`Effect::ConventionalRestart`]): start a fresh logical `call_num` /
    /// `start_time` and clear the segment list. Recorder ownership and state
    /// transition are the caller's job (they differ by [`RecorderPolicy`]).
    fn rotate_bookkeeping(&mut self, now: Instant) {
        self.call_num = next_call_num();
        self.start_time = now;
        self.segments.clear();
        self.segment_start = now;
        self.ptt_ended = false;
        self.refreshes = 0;
    }

    /// If a squelch close is pending debounce, finalize it now: push the
    /// in-progress segment and transition to Idle.
    pub(crate) fn finalize_squelch_debounce(&mut self, now: Instant) {
        if self.squelch_closed_at.take().is_some()
            && self.state == CallState::Recording
            && self.recorder.is_some()
        {
            self.close_segment_and_idle(now);
        }
    }

    /// Total time spent in the Recording state: completed segments + current segment.
    pub(crate) fn recording_duration(&self, now: Instant) -> Duration {
        let completed: Duration = self.segments.iter().map(CallSegment::duration).sum();
        if self.state == CallState::Recording {
            completed + now.saturating_duration_since(self.segment_start)
        } else {
            completed
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

/// Why a call ended — carried on [`Effect::EmitCallEnd`] so the executor can
/// label the call span and the `calls_ended` metric. The trunked-cut-short
/// suspect is [`CallEndReason::GrantTimeout`] (an active recording concluded
/// only because grant-updates went stale for `call_timeout`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallEndReason {
    /// Recording-state call ended because no grant-update arrived within
    /// `call_timeout` (the likely cause of fragmented/cut-short trunked calls).
    GrantTimeout,
    /// An already-idle call aged out of the table after `call_timeout`.
    IdleTimeout,
    /// Superseded by an overlapping grant (e.g. multisite re-grant).
    Superseded,
    /// Recorder write/keepalive timeout expired the call.
    WriteTimeout,
    /// Conventional per-transmission boundary (squelch close / retune).
    Conventional,
    /// Trunked per-speaker boundary: the in-band source (unit) changed mid-call.
    /// A *logical* boundary — the recorder keeps running and audio stays
    /// continuous; only the CallEnd/CallStart event pair is emitted. Distinct
    /// from [`Self::Conventional`] (which stops/restarts the recorder).
    SourceChange,
    /// Trunked per-PTT boundary: a control channel grant for a call already
    /// recording, i.e. a new key-up (another handset, or the same one after
    /// the voice channel ended its transmission). Logical, like
    /// [`Self::SourceChange`].
    KeyUp,
    /// The control channel granted the call's channel (and TDMA slot) to
    /// another talkgroup, so the call is over: left running, its recorder
    /// would record the new talkgroup until a timeout.
    ChannelReassigned,
    /// Process shutdown drained all calls.
    Shutdown,
}

impl CallEndReason {
    /// Ends that stop the recorder whether or not anyone is talking: with
    /// voice still arriving, the transmission was cut short.
    pub(crate) const fn can_cut_speech(self) -> bool {
        matches!(
            self,
            Self::GrantTimeout
                | Self::IdleTimeout
                | Self::WriteTimeout
                | Self::Superseded
                | Self::ChannelReassigned
        )
    }

    /// Low-cardinality label for spans and metrics.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::GrantTimeout => "grant_timeout",
            Self::IdleTimeout => "idle_timeout",
            Self::Superseded => "superseded",
            Self::WriteTimeout => "write_timeout",
            Self::Conventional => "conventional",
            Self::SourceChange => "source_change",
            Self::KeyUp => "key_up",
            Self::ChannelReassigned => "channel_reassigned",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Recorder-ownership policy for a transmission boundary (`cut_transmission`).
///
/// The unified call model: a `Call` owns one recorder lifecycle (and one
/// downstream/Discord embed); inside it lives an ordered list of
/// `Transmission`s (one continuously-keyed speaker each). `cut_transmission`
/// always ends the current transmission on the wire (`EmitCallEnd`) and rotates
/// the per-transmission bookkeeping; the policy is the *only* thing that differs
/// between trunked and conventional — whether the recorder spans the whole call
/// or is recycled per transmission. This is what the old `ConventionalRestart`
/// effect really encoded (recorder recycle), NOT a WAV rotation — this headless
/// build has no WAV; `call_restart` resets the sink accumulators.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RecorderPolicy {
    /// Recorder spans the entire call (trunked per-speaker boundary). The cut is
    /// a *logical* boundary: the recorder keeps running, audio is continuous,
    /// the source is re-routed, and the next transmission opens eagerly on the
    /// same recorder (`rust_call_id` unchanged → audio routing + Discord
    /// coalescing carry across).
    Span { source_id: i64 },
    /// As [`Self::Span`], cut at a new key-up the control channel granted.
    KeyUp { source_id: i64 },
    /// Recorder recycles per transmission (conventional keyed transmission). The
    /// cut stops the recorder, reads its metrics, and restarts it with a fresh
    /// `rec_num`; no eager `EmitCallStart` — the next squelch open begins the
    /// next transmission as its own `CallStart`.
    Recycle,
}

impl RecorderPolicy {
    /// The `EmitCallEnd` reason carried by a cut under this policy.
    const fn end_reason(self) -> CallEndReason {
        match self {
            Self::Span { .. } => CallEndReason::SourceChange,
            Self::KeyUp { .. } => CallEndReason::KeyUp,
            Self::Recycle => CallEndReason::Conventional,
        }
    }
}

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
        reason: CallEndReason,
    },
    /// Rotate to the next logical transmission on the SAME recorder (no
    /// stop/restart): assign a fresh `call_num`, clear segments, reset
    /// `segment_start`. The trunked soft-boundary analog of
    /// [`Self::ConventionalRestart`] minus the recorder restart. Ordered after
    /// the closing `EmitCallEnd` (which reads the old `call_num`/segments) and
    /// before the opening `EmitCallStart` (which reads the new `call_num`).
    RotateTransmission {
        call_id: CallId,
        source_id: i64,
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
    /// Call `rec->start(cfg)` to initialize the recorder (open WAV, set
    /// squelch threshold).  Must be processed before `EnableRecorder`.
    InitRecorder {
        call_id: CallId,
    },
    EnableRecorder {
        call_id: CallId,
    },
    DisableRecorder {
        call_id: CallId,
    },
    /// Push a new squelch threshold to the C++ recorder at runtime.
    UpdateSquelch {
        call_id: CallId,
        threshold_db: f64,
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

/// Newtype wrapper for a list of effects returned by state machine methods.
///
/// The `#[must_use]` attribute ensures callers cannot silently discard effects —
/// doing so is a compile error under the workspace's `unused_must_use = "deny"`.
#[must_use = "effects must be processed via process_effects()"]
#[derive(Debug)]
pub(crate) struct Effects(Vec<Effect>);

impl Effects {
    pub(crate) const fn new() -> Self {
        Self(Vec::new())
    }

    pub(crate) fn push(&mut self, effect: Effect) {
        self.0.push(effect);
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Effect> for Effects {
    fn from(effect: Effect) -> Self {
        Self(vec![effect])
    }
}

impl Extend<Effect> for Effects {
    fn extend<T: IntoIterator<Item = Effect>>(&mut self, iter: T) {
        self.0.extend(iter);
    }
}

impl IntoIterator for Effects {
    type Item = Effect;
    type IntoIter = std::vec::IntoIter<Effect>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Effects {
    type Item = &'a Effect;
    type IntoIter = std::slice::Iter<'a, Effect>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl std::ops::Deref for Effects {
    type Target = [Effect];

    fn deref(&self) -> &[Effect] {
        &self.0
    }
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
    calls: Vec<CallEntry>,
    pub(crate) config: CallManagerConfig,
    pub(crate) patch_tables: HashMap<i32, PatchTable>,
    pub(crate) system_identities: HashMap<i32, SystemIdentity>,
    /// Last ESS `ALGID` seen per `(sys_num, talkgroup)`. Encrypted talkgroups use
    /// a fixed algorithm, so a call we never recorded (rejected from the grant)
    /// can still be labelled with what an earlier recorded call on the same
    /// talkgroup revealed.
    known_alg_ids: HashMap<(i32, i64), u8>,
    /// Every time the state machine reads (see [`crate::clock`]).
    clock: Clock,
}

impl TrunkStateMachine {
    pub(crate) fn new(config: CallManagerConfig) -> Self {
        Self::with_clock(config, Clock::system())
    }

    pub(crate) const fn clock(&self) -> &Clock {
        &self.clock
    }

    pub(crate) fn with_clock(config: CallManagerConfig, clock: Clock) -> Self {
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
            clock,
            calls: Vec::new(),
            config,
            patch_tables,
            system_identities,
            known_alg_ids: HashMap::new(),
        }
    }

    // ── Top-level message dispatch ──

    pub(crate) fn on_message(&mut self, msg: &TrunkMessage) -> Effects {
        let now = self.clock.now();
        match msg.message_type {
            MessageType::InvalidCcMessage => Effects::new(),

            MessageType::ControlChannel => Effect::AddControlChannel {
                sys_num: msg.sys_num,
                freq: msg.freq,
            }
            .into(),

            MessageType::PatchAdd => {
                if let Some(pt) = self.patch_tables.get_mut(&msg.sys_num) {
                    pt.update(
                        msg.patch_sg,
                        msg.patch_ga1,
                        msg.patch_ga2,
                        msg.patch_ga3,
                        now,
                    );
                }
                Effects::new()
            }
            MessageType::PatchDelete => {
                if let Some(pt) = self.patch_tables.get_mut(&msg.sys_num) {
                    pt.delete(msg.patch_sg, msg.patch_ga1, msg.patch_ga2, msg.patch_ga3);
                }
                Effects::new()
            }

            MessageType::Tdulc => self.on_tdulc(msg.sys_num, msg.talkgroup),

            MessageType::Sysid => {
                if let Some(id) = self.system_identities.get_mut(&msg.sys_num) {
                    id.rfss = msg.sys_rfss;
                    id.site_id = msg.sys_site_id;
                }
                Effect::EmitSetupSystem {
                    sys_num: msg.sys_num,
                }
                .into()
            }
            MessageType::Status => {
                if let Some(id) = self.system_identities.get_mut(&msg.sys_num) {
                    id.sys_id = msg.sys_id;
                    id.wacn = msg.wacn;
                    id.nac = msg.nac;
                    id.xor_mask = crate::p25_lfsr::compute_xor_mask(msg.nac, msg.sys_id, msg.wacn);
                }
                Effect::EmitSetupSystem {
                    sys_num: msg.sys_num,
                }
                .into()
            }

            // Unit events (stateless)
            MessageType::Registration => Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::Registration,
                source_id: msg.source,
                talkgroup: 0,
            }
            .into(),
            MessageType::Deregistration => Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::Deregistration,
                source_id: msg.source,
                talkgroup: 0,
            }
            .into(),
            MessageType::Affiliation => Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::Affiliation,
                source_id: msg.source,
                talkgroup: msg.talkgroup,
            }
            .into(),
            MessageType::Acknowledge => Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::Acknowledge,
                source_id: msg.source,
                talkgroup: 0,
            }
            .into(),
            MessageType::Location => Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::Location,
                source_id: msg.source,
                talkgroup: msg.talkgroup,
            }
            .into(),
            MessageType::DataGrant => Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::DataGrant,
                source_id: msg.source,
                talkgroup: 0,
            }
            .into(),
            MessageType::UuAnsReq => Effect::EmitUnitEvent {
                sys_num: msg.sys_num,
                type_: UnitEventType::AnswerRequest,
                source_id: msg.source,
                talkgroup: msg.talkgroup,
            }
            .into(),

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
                    Effects::new()
                }
            }
            MessageType::UuVUpdate => {
                if self.config.record_uu_v_calls {
                    self.on_update(msg)
                } else {
                    Effects::new()
                }
            }

            MessageType::Unknown(t) => Effect::Warn {
                message: format!("unhandled message type: {t}"),
            }
            .into(),
        }
    }

    // ── Grant handling ──

    /// Ends the calls whose channel a grant gives to another talkgroup. The
    /// site only reassigns a channel once the call on it has ended, so these
    /// calls are over; left alone, their recorders would record the new
    /// talkgroup until a timeout. Talkgroups patched together share a channel
    /// legitimately and are left alone.
    fn end_reassigned_calls(&self, msg: &TrunkMessage) -> Effects {
        let now = self.clock.now();
        let mut effects = Effects::new();
        let patched = self
            .patch_tables
            .get(&msg.sys_num)
            .map(|pt| pt.get_patch(Talkgroup(msg.talkgroup)))
            .unwrap_or_default();
        for entry in &self.calls {
            if !entry.is_trunked()
                || entry.sys_num != msg.sys_num
                || entry.talkgroup == msg.talkgroup
                || !entry.shares_channel(msg)
                || patched.contains(&Talkgroup(entry.talkgroup))
            {
                continue;
            }
            tracing::info!(
                short_name = %entry.short_name,
                talkgroup = entry.talkgroup,
                to_talkgroup = msg.talkgroup,
                freq = %entry.freq,
                slot = entry.tdma_slot,
                since_update_s = now.saturating_duration_since(entry.last_update).as_secs_f64(),
                "channel granted to another talkgroup: ending call"
            );
            effects.push(Effect::EmitCallEnd {
                call_id: entry.id,
                reason: CallEndReason::ChannelReassigned,
            });
            if entry.state == CallState::Recording {
                effects.push(Effect::EmitSetupRecorder { call_id: entry.id });
            }
            effects.push(Effect::RemoveCall { call_id: entry.id });
        }
        if !effects.is_empty() {
            effects.push(Effect::EmitCallsActive);
        }
        effects
    }

    fn on_grant(&mut self, msg: &TrunkMessage, is_grant_message: bool) -> Effects {
        let now = self.clock.now();
        let mut effects = if is_grant_message {
            self.end_reassigned_calls(msg)
        } else {
            Effects::new()
        };

        // A talkgroup has one call per system. A grant on another channel or
        // TDMA slot moves it there (the retune below), rather than starting a
        // second call and leaving the first running on the old channel.
        let existing_idx = self
            .calls
            .iter()
            .position(|entry| entry.talkgroup == msg.talkgroup && entry.sys_num == msg.sys_num);

        if let Some(idx) = existing_idx {
            let same_channel = self.calls[idx].on_channel(msg);
            if same_channel {
                let entry = &mut self.calls[idx];
                let resuming_idle = entry.state == CallState::Idle && entry.is_trunked();
                let source_changed = msg.source > 0 && msg.source != entry.current_source_id;

                // A TDULC parked this call Idle while keeping the recorder warm.
                // If a *different* unit now keys up, the prior speaker's
                // transmission must be cut as its own per-speaker boundary, not
                // silently merged into the next: the control-channel regrant
                // otherwise wins the race against the in-band `src_id` detector
                // (which would then dedup on the already-moved source). The
                // recorder spans the gap (`RecorderPolicy::Span`);
                // `on_transmission_rotated` completes the Idle → Recording
                // transition between the closing `EmitCallEnd` (which still sees
                // the prior Idle state + segments) and the opening
                // `EmitCallStart`. Same speaker (or unknown source) re-keying
                // falls through to a plain in-place resume so we don't fragment
                // one speaker's PTT cycle.
                if resuming_idle && source_changed {
                    let call_id = entry.id;
                    entry.current_source_id = msg.source;
                    tracing::info!(
                        short_name = %entry.short_name,
                        talkgroup = entry.talkgroup,
                        source = msg.source,
                        "regrant with new speaker on parked call: cutting per-speaker boundary"
                    );
                    return self.cut_transmission(
                        call_id,
                        RecorderPolicy::Span {
                            source_id: msg.source,
                        },
                    );
                }

                // A grant for a call already recording is a new key-up: from
                // another handset (the grant names it), or from the same one
                // after the voice channel ended its transmission. Each is its
                // own transmission. The grant usually beats the in-band source
                // change, which then matches and is ignored, so cut here.
                let other_handset = msg.source > 0 && msg.source != entry.current_source_id;
                if is_grant_message
                    && entry.is_trunked()
                    && entry.state == CallState::Recording
                    && (other_handset || entry.ptt_ended)
                    && now.saturating_duration_since(entry.segment_start) >= MIN_TRANSMISSION
                {
                    let call_id = entry.id;
                    let source_id = if msg.source > 0 {
                        msg.source
                    } else {
                        entry.current_source_id
                    };
                    entry.current_source_id = source_id;
                    tracing::info!(
                        short_name = %entry.short_name,
                        talkgroup = entry.talkgroup,
                        source = source_id,
                        other_handset,
                        "new key-up on a recording call: cutting per-PTT transmission boundary"
                    );
                    return self.cut_transmission(call_id, RecorderPolicy::KeyUp { source_id });
                }

                // TDULC may have moved this call to Idle — resume recording.
                if resuming_idle {
                    entry.state = CallState::Recording;
                    entry.idle_since = None;
                    entry.segment_start = now;
                    tracing::trace!(
                        short_name = %entry.short_name,
                        talkgroup = entry.talkgroup,
                        "Grant on Idle call: Idle → Recording"
                    );
                }

                // Source changed: re-route to the new source (SetSource +
                // EmitSignal inside update_call_source). Do NOT emit
                // EmitCallStart — this is the same logical call already in
                // progress, and a duplicate call_start orphans/duplicates the
                // downstream embed (it is keyed by talkgroup, not call id).
                self.update_call_source(idx, msg.source, &mut effects);
            } else {
                // Retune to the new channel or slot: close the current segment
                // (if Recording), start a new one
                let entry = &mut self.calls[idx];
                let old_freq = entry.freq;
                let rec_num = entry.recorder.unwrap_or(RecorderNum(-1));

                // Only push a segment if we're actively recording
                // (TDULC already closed the segment on PTT end)
                if entry.state == CallState::Recording {
                    entry.segments.push(CallSegment {
                        end: now,
                        freq: old_freq,
                        start: entry.segment_start,
                        rec_num,
                    });
                }
                entry.segment_start = now;
                entry.idle_since = None;

                if entry.recorder.is_some() {
                    effects.push(Effect::StopRecording { call_id: entry.id });
                    entry.recorder = None;
                }

                entry.freq = msg.freq;
                entry.tdma_slot = msg.tdma_slot;
                entry.phase2_tdma = msg.phase2_tdma;
                // Refresh the tracked source for the new freq so the next grant
                // on it isn't misread as a source change (which would emit a
                // spurious SetSource/EmitSignal).
                entry.current_source_id = msg.source;
                effects.push(Effect::SetFreq {
                    call_id: entry.id,
                    freq: msg.freq,
                });
                entry.last_update = now;

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
            start_time: now,
            last_update: now,
            segment_start: now,
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

            if recording_started && let Some(orig_idx) = multisite.original_idx {
                let orig = &mut self.calls[orig_idx];
                orig.state = CallState::Rejected(RejectReason::Superseded);
                let orig_id = orig.id;
                effects.push(Effect::EmitCallEnd {
                    call_id: orig_id,
                    reason: CallEndReason::Superseded,
                });
                effects.push(Effect::RemoveCall { call_id: orig_id });
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

    fn on_update(&mut self, msg: &TrunkMessage) -> Effects {
        let now = self.clock.now();
        let mut effects = Effects::new();

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

                // Source changed: re-route only (SetSource + EmitSignal). No
                // EmitCallStart for an already-started call — see on_grant.
                Self::update_call_source_entry(entry, msg.source, &mut effects, now);
            }
        }

        effects
    }

    // ── TDULC handling (P25 Terminator Data Unit) ──

    fn on_tdulc(&mut self, sys_num: i32, talkgroup: i64) -> Effects {
        let now = self.clock.now();
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

            // Close current segment, transition to Idle. No boundary is emitted
            // (cf. `cut_transmission`): a later grant resumes the same call,
            // merging across the PTT gap into one transmission. Routing TDULC
            // through `cut_transmission` would change that semantics, so it
            // stays a plain in-place close (see per-speaker-boundaries note).
            entry.close_segment_and_idle(now);

            tracing::trace!(
                short_name = %entry.short_name,
                talkgroup = entry.talkgroup,
                sys_num,
                segments = entry.segments.len(),
                "TDULC: PTT end, Recording → Idle"
            );
            return Effects::new();
        }
        Effects::new()
    }

    // ── Source tracking ──

    fn update_call_source(&mut self, idx: usize, source: i64, effects: &mut Effects) -> bool {
        let now = self.clock.now();
        let entry = &mut self.calls[idx];
        Self::update_call_source_entry(entry, source, effects, now)
    }

    fn update_call_source_entry(
        entry: &mut CallEntry,
        source: i64,
        effects: &mut Effects,
        now: Instant,
    ) -> bool {
        entry.last_update = now;
        entry.refreshes += 1;

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

    /// Record the P25 encryption parameters (ESS `ALGID` / `KeyID`) decoded by
    /// op25 on the voice channel (carried as `rust_call_id` → [`CallId`]). The
    /// ESS is authoritative, so this also marks the call encrypted. Unknown
    /// calls are ignored.
    pub(crate) fn on_crypto(&mut self, call_id: CallId, alg_id: u8, key_id: u16) {
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            entry.encrypted = true;
            entry.alg_id = Some(alg_id);
            entry.key_id = Some(key_id);
            self.known_alg_ids
                .insert((entry.sys_num, entry.talkgroup), alg_id);
        }
    }

    /// The call's own decoded `ALGID`, else the last one seen on its talkgroup.
    pub(crate) fn alg_id_for(&self, call_id: CallId) -> Option<u8> {
        let entry = self.calls.iter().find(|e| e.id == call_id)?;
        entry.alg_id.or_else(|| {
            self.known_alg_ids
                .get(&(entry.sys_num, entry.talkgroup))
                .copied()
        })
    }

    /// Handle an in-band source (unit) change decoded by op25 on the voice
    /// channel (carried as `rust_call_id` → [`CallId`]). Unlike the
    /// control-channel source updates in [`Self::update_call_source_entry`],
    /// this cuts a real per-speaker **transmission boundary** so each unit's
    /// audio becomes its own CallStart/CallEnd pair — the same model as a
    /// conventional keyed transmission, but on a continuously-running recorder
    /// (no stop/retune; `rust_call_id` is unchanged so audio routing and Discord
    /// coalescing carry across the boundary).
    /// The voice channel ended a transmission on `call_id` (a terminator):
    /// the next grant for it is a new key-up.
    pub(crate) fn on_ptt_ended(&mut self, call_id: CallId) {
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id)
            && entry.is_trunked()
            && entry.state == CallState::Recording
        {
            entry.ptt_ended = true;
        }
    }

    pub(crate) fn on_source_change(&mut self, call_id: CallId, source: i64) -> Effects {
        let now = self.clock.now();
        let mut effects = Effects::new();
        let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) else {
            return effects;
        };

        // Only trunked, only while actively recording. Ignore unknown sources
        // (encrypted / no-LC frames decode to -1) and no-op repeats: the tag
        // re-fires every superframe, and the control channel may have already
        // moved `current_source_id` — dedup against it so we never emit a
        // double boundary.
        if !entry.is_trunked()
            || entry.state != CallState::Recording
            || source <= 0
            || source == entry.current_source_id
        {
            return effects;
        }

        // Below the minimum transmission length, treat it as a plain re-route
        // (SetSource + EmitSignal) rather than fragmenting rapid A↔B↔A hand-offs
        // into sub-second transmissions.
        if now.saturating_duration_since(entry.segment_start) < MIN_TRANSMISSION {
            Self::update_call_source_entry(entry, source, &mut effects, now);
            return effects;
        }

        // Real per-speaker boundary: close the current transmission and open
        // the next on the same recorder. Set `current_source_id` now so a
        // following control-channel update for the same unit dedups.
        entry.current_source_id = source;
        tracing::info!(
            short_name = %entry.short_name,
            talkgroup = entry.talkgroup,
            source,
            "in-band source change: cutting per-speaker transmission boundary"
        );
        self.cut_transmission(call_id, RecorderPolicy::Span { source_id: source })
    }

    /// Cut a transmission boundary: end the current transmission on the wire and
    /// open the next, the single verb shared by trunked per-speaker boundaries
    /// ([`RecorderPolicy::Span`]) and conventional keyed transmissions
    /// ([`RecorderPolicy::Recycle`]). The two source types differ ONLY by the
    /// policy threaded here; everything else is identical.
    ///
    /// Effect ordering is the load-bearing coalescing contract — `EmitCallEnd`
    /// MUST precede the rotate effect (the executor reads the old
    /// `call_num`/segments while concluding, before the rotate advances
    /// bookkeeping) and any `EmitCallStart`. Reordering corrupts the downstream
    /// embed with no compile error.
    fn cut_transmission(&mut self, call_id: CallId, policy: RecorderPolicy) -> Effects {
        let mut effects = Effects::new();
        effects.push(Effect::EmitCallEnd {
            call_id,
            reason: policy.end_reason(),
        });
        match policy {
            RecorderPolicy::Span { source_id } | RecorderPolicy::KeyUp { source_id } => {
                // Recorder keeps running: rotate bookkeeping only, re-route the
                // source, and open the next transmission eagerly.
                effects.push(Effect::RotateTransmission { call_id, source_id });
                effects.push(Effect::SetSource { call_id, source_id });
                effects.push(Effect::EmitSignal { call_id, source_id });
                effects.push(Effect::EmitCallStart { call_id });
            }
            RecorderPolicy::Recycle => {
                // Recorder recycles: stop → read metrics → restart, reconfigure,
                // re-enable. No eager EmitCallStart — the next squelch open
                // begins the next transmission as its own CallStart.
                effects.push(Effect::ConventionalRestart { call_id });
                effects.push(Effect::EmitSetupRecorder { call_id });
                effects.push(Effect::EnableRecorder { call_id });
            }
        }
        effects
    }

    /// Apply a [`Effect::RotateTransmission`]: advance to the next logical
    /// transmission on the same recorder. Mirrors the non-recorder half of
    /// [`Self::on_conventional_restarted`] — fresh `call_num`, cleared segments,
    /// reset `segment_start` — but stays `Recording` and leaves the recorder
    /// untouched.
    pub(crate) fn on_transmission_rotated(&mut self, call_id: CallId, source_id: i64) {
        let now = self.clock.now();
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            entry.rotate_bookkeeping(now);
            // Recorder spans the call (RecorderPolicy::Span): recorder untouched.
            // Force Recording so a Span cut originating from a TDULC-parked
            // (Idle) call completes the Idle → Recording transition *here* —
            // after the closing EmitCallEnd saw the prior Idle state/segments,
            // before the opening EmitCallStart. For a mid-keyup source change
            // the call is already Recording, so this is a no-op.
            entry.state = CallState::Recording;
            entry.idle_since = None;
            entry.current_source_id = source_id;
        }
    }

    // ── Recording policy (pure evaluation) ──

    fn evaluate_recording_policy(&mut self, entry_idx: usize, msg: &TrunkMessage) -> Effects {
        let sys_num = msg.sys_num;
        let tg = Talkgroup(msg.talkgroup);

        let sys_policy = match self.config.systems.get(&sys_num) {
            Some(sp) => sp,
            None => return Effects::new(),
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
                return Effects::new();
            }
        }

        // 2. Encryption check
        let is_encrypted = msg.encrypted || tg_policy.is_some_and(|tg| tg.mode.is_encrypted());
        if is_encrypted {
            entry.encrypted = true;
            if !sys_policy.monitor_encrypted {
                entry.state = CallState::Rejected(RejectReason::Encrypted);
                if !sys_policy.hide_encrypted {
                    tracing::info!(
                        talkgroup = tg.0,
                        sys_num,
                        freq = %msg.freq,
                        slot = msg.tdma_slot,
                        "not recording: encrypted"
                    );
                }
                return Effects::new();
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
                if let Some(ptgp) = sys_policy.talkgroups.get(ptg)
                    && ptgp.priority >= 0
                    && ptgp.priority < p
                {
                    p = ptgp.priority;
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
            return Effects::new();
        }

        // 6. Emit RequestRecording — executor handles FFI source/recorder allocation
        Effect::RequestRecording {
            call_id: entry.id,
            freq: entry.freq,
            is_analog,
            priority,
        }
        .into()
    }

    // ── Recording result callback (from executor after FFI) ──

    pub(crate) fn on_recording_result(
        &mut self,
        call_id: CallId,
        result: RecordingResult,
    ) -> Effects {
        let entry = match self.calls.iter_mut().find(|e| e.id == call_id) {
            Some(e) => e,
            None => return Effects::new(),
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
                Effect::EmitSetupRecorder { call_id }.into()
            }
            RecordingResult::NoRecorder => {
                entry.state = CallState::Rejected(RejectReason::NoRecorder);
                Effects::new()
            }
            RecordingResult::NoSource => {
                entry.state = CallState::Rejected(RejectReason::NoSource);
                tracing::error!(
                    talkgroup = entry.talkgroup,
                    sys_num = entry.sys_num,
                    "not recording: no source covering freq"
                );
                Effects::new()
            }
        }
    }

    // ── Multisite duplicate detection (pure) ──

    fn check_multisite(&self, talkgroup: i64, sys_num: i32, msg: &TrunkMessage) -> MultiSiteResult {
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

                let call_preferred_nac =
                    self.get_preferred_nac(entry.sys_num, Talkgroup(talkgroup));
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
                if !msg_multi_site_name.is_empty() && trunked.multi_site_name == msg_multi_site_name
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

    pub(crate) fn tick(&mut self) -> Effects {
        let now = self.clock.now();
        let mut effects = Effects::new();
        let timeout = self.config.call_timeout;
        let mut ended_call = false;

        for entry in &self.calls {
            if entry.is_conventional() {
                continue;
            }

            let since_update = now
                .saturating_duration_since(entry.last_update)
                .as_secs_f64();
            if since_update <= timeout {
                continue;
            }

            match entry.state {
                CallState::Recording => {
                    effects.push(Effect::EmitCallEnd {
                        call_id: entry.id,
                        reason: CallEndReason::GrantTimeout,
                    });
                    effects.push(Effect::EmitSetupRecorder { call_id: entry.id });
                    effects.push(Effect::RemoveCall { call_id: entry.id });
                    ended_call = true;
                }
                CallState::Idle | CallState::Disabled | CallState::Rejected(_) => {
                    effects.push(Effect::EmitCallEnd {
                        call_id: entry.id,
                        reason: CallEndReason::IdleTimeout,
                    });
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

    fn tick_conventional(&mut self) -> Effects {
        let now = self.clock.now();
        let timeout = self.config.call_timeout;
        let mut debounce_always_on = Vec::new();
        let mut debounce_sig_det = Vec::new();
        let mut timed_out = Vec::new();

        for entry in &self.calls {
            if !entry.is_conventional() || entry.recorder.is_none() {
                continue;
            }

            // Collect entries whose squelch close debounce has expired. On an
            // always-on channel each squelch open/close is a discrete keyed
            // transmission, so a debounced close concludes the current call
            // (one CallStart/CallEnd per transmission, like a trunked grant);
            // the channel then re-arms to Idle for the next open. Signal-
            // detection channels keep the old behaviour: push a segment and
            // wait for the idle timeout to conclude the detection cycle.
            if entry
                .squelch_closed_at
                .is_some_and(|t| now.saturating_duration_since(t) >= SQUELCH_DEBOUNCE)
            {
                if entry.signal_detection {
                    debounce_sig_det.push(entry.id);
                } else {
                    debounce_always_on.push(entry.id);
                }
            }

            if entry.state == CallState::Disabled {
                continue;
            }

            let idle_timed_out = entry
                .idle_since
                .is_some_and(|t| now.saturating_duration_since(t).as_secs_f64() > timeout);

            let elapsed = now
                .saturating_duration_since(entry.start_time)
                .as_secs_f64();
            let max_duration_exceeded = entry.max_duration > 0.0 && elapsed > entry.max_duration;

            // Safety backstop: if recording continuously without any squelch
            // close event, the noise floor is likely above the threshold.
            let stuck_recording = entry.state == CallState::Recording
                && entry.segments.is_empty()
                && entry.squelch_closed_at.is_none()
                && now.saturating_duration_since(entry.segment_start) > STUCK_RECORDING_BACKSTOP;

            if idle_timed_out || max_duration_exceeded || stuck_recording {
                timed_out.push(entry.id);
            }
        }

        let mut effects = Effects::new();

        // Signal-detection: finalize the segment, transition to Idle, and let
        // the idle timeout conclude the detection cycle.
        for call_id in debounce_sig_det {
            if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
                entry.finalize_squelch_debounce(now);
            }
        }

        // Always-on: conclude each keyed transmission as its own call.
        let mut concluded = Vec::new();
        for call_id in debounce_always_on {
            concluded.push(call_id);
            effects.extend(self.conclude_conventional_transmission(call_id));
        }

        // Idle/stuck/max-duration backstops — skip any channel we just
        // concluded above so a single tick never finalizes a call twice.
        for call_id in timed_out {
            if concluded.contains(&call_id) {
                continue;
            }
            effects.extend(self.on_conventional_idle_timeout(call_id));
        }
        effects
    }

    /// Conclude the current keyed transmission on an always-on conventional
    /// channel and re-arm it for the next squelch open.
    ///
    /// The entry is left in `Recording` so [`Self::exec_conclude_call`] (via
    /// the `EmitCallEnd` effect) captures the open segment (`segment_start..now`)
    /// before the `ConventionalRestart` effect rotates the WAV, assigns a fresh
    /// `call_num`, clears the segments, and re-arms the channel to `Idle`. No
    /// `EmitCallStart` is emitted here — the next squelch open does that, so
    /// each transmission becomes one CallStart/CallEnd pair on the wire,
    /// identical to a trunked grant.
    fn conclude_conventional_transmission(&mut self, call_id: CallId) -> Effects {
        let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) else {
            return Effects::new();
        };
        // Clear the debounce marker; the conclude reads the open segment
        // directly while the entry is still Recording.
        entry.squelch_closed_at = None;

        self.cut_transmission(call_id, RecorderPolicy::Recycle)
    }

    // ── Conventional callbacks from executor ──

    pub(crate) fn on_conventional_idle_timeout(&mut self, call_id: CallId) -> Effects {
        let now = self.clock.now();
        let signal_detection = match self.calls.iter_mut().find(|e| e.id == call_id) {
            Some(entry) => {
                entry.finalize_squelch_debounce(now);
                entry.signal_detection
            }
            None => return Effects::new(),
        };

        // Don't clear segments or change state here — EmitCallEnd is processed
        // later as an effect and needs the current segments/state intact.
        // State reset happens in on_conventional_restarted() during the
        // ConventionalRestart effect, which runs after EmitCallEnd.

        if signal_detection {
            let Some(entry) = self.calls.iter().find(|e| e.id == call_id) else {
                return Effects::new();
            };
            let mut effects = Effects::new();

            // Conclude the current call if any audio was captured.
            if !entry.segments.is_empty() || entry.state == CallState::Recording {
                effects.push(Effect::EmitCallEnd {
                    call_id,
                    reason: CallEndReason::Conventional,
                });
            }

            // Restart opens a fresh WAV file for the next detection cycle.
            effects.push(Effect::ConventionalRestart { call_id });

            // Disable after restart — recorder is configured but not flowing data.
            effects.push(Effect::DisableRecorder { call_id });

            return effects;
        }

        // Always-on (non-signal-detection): this is only reached via the
        // stuck-recording / max-duration backstops — a transmission that never
        // produced a clean squelch close. Conclude it like any other and
        // re-arm; the next squelch open starts the next call.
        self.conclude_conventional_transmission(call_id)
    }

    pub(crate) fn on_conventional_restarted(
        &mut self,
        call_id: CallId,
        recorder: Option<RecorderNum>,
    ) {
        let now = self.clock.now();
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            entry.rotate_bookkeeping(now);
            // Recorder recycles (RecorderPolicy::Recycle): adopt the freshly
            // assigned recorder and re-arm to await the next keyed transmission.
            entry.recorder = recorder;
            entry.last_update = now;
            entry.idle_since = None;
            entry.squelch_closed_at = None;
            // Re-arm to Idle (not Recording): an always-on channel waits for the
            // next squelch open to start the next keyed transmission as its own
            // call. (Previously this eagerly returned to Recording as part of a
            // free-running restart loop, which is what made conventional calls
            // un-coalescable on the Discord side.)
            entry.state = if entry.signal_detection {
                CallState::Disabled
            } else {
                CallState::Idle
            };
        }
    }

    // ── Squelch handling ──

    pub(crate) fn on_squelch(
        &mut self,
        recorder_num: RecorderNum,
        muted: bool,
        pwr_db: f64,
    ) -> Effects {
        let now = self.clock.now();
        for entry in &mut self.calls {
            if !entry.is_conventional() {
                continue;
            }
            if entry.recorder != Some(recorder_num) {
                continue;
            }

            let mut effects = Effects::new();

            // Feed every squelch power reading to the adaptive squelch
            // tracker. The squelch block reports power in its own units,
            // which is exactly the scale we need for threshold updates.
            #[allow(clippy::cast_possible_truncation)]
            if let Some(ref mut tracker) = entry.adaptive_squelch
                && let Some(new_threshold) = tracker.feed(pwr_db as f32)
            {
                effects.push(Effect::UpdateSquelch {
                    call_id: entry.id,
                    threshold_db: new_threshold,
                });
            }

            if muted {
                // Squelch closed — debounce: don't transition immediately.
                // tick_conventional() finalizes if squelch stays closed past
                // SQUELCH_DEBOUNCE.
                entry.noise = trunk_recorder_sys::signal_from_ffi(pwr_db);
                if entry.state == CallState::Recording {
                    entry.squelch_closed_at.get_or_insert_with(Instant::now);
                } else if entry.idle_since.is_none() {
                    entry.idle_since = Some(now);
                }
            } else {
                // Squelch open
                entry.signal = trunk_recorder_sys::signal_from_ffi(pwr_db);
                if entry.squelch_closed_at.take().is_some() {
                    // Reopened before debounce expired — cancel pending close,
                    // stay in Recording. No audio lost.
                } else {
                    // Normal open from Idle (debounce already finalized)
                    entry.idle_since = None;
                    if entry.state == CallState::Idle {
                        let first_open = entry.segments.is_empty();
                        entry.state = CallState::Recording;
                        entry.segment_start = now;
                        if first_open {
                            effects.push(Effect::EmitCallStart { call_id: entry.id });
                            effects.push(Effect::EmitCallsActive);
                        }
                    }
                }
            }
            return effects;
        }
        Effects::new()
    }

    // ── Adaptive squelch ──

    /// Feed a polled power sample to the adaptive squelch tracker for a
    /// conventional call. Unlike `on_squelch`, this does not change call state.
    pub(crate) fn feed_power_sample(&mut self, call_id: CallId, power_db: f32) -> Effects {
        let mut effects = Effects::new();
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            #[allow(clippy::cast_possible_truncation)]
            if let Some(ref mut tracker) = entry.adaptive_squelch
                && let Some(new_threshold) = tracker.feed(power_db)
            {
                effects.push(Effect::UpdateSquelch {
                    call_id,
                    threshold_db: new_threshold,
                });
            }
        }
        effects
    }

    // ── Signal detection ──

    pub(crate) fn on_signals_detected(
        &mut self,
        source_center_freq: f64,
        signals: &[trunk_recorder_sys::DetectedSignal],
    ) -> Effects {
        let mut effects = Effects::new();
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
                    entry.idle_since = None;
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
    ) -> (CallId, Effects) {
        let now = self.clock.now();
        let state = if channel.signal_detection {
            CallState::Disabled
        } else {
            CallState::Idle
        };
        let adaptive_squelch = if channel.is_analog {
            Some(AdaptiveSquelch::new(channel.squelch_db))
        } else {
            None
        };
        let entry = CallEntry {
            start_time: now,
            last_update: now,
            segment_start: now,
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
            adaptive_squelch,
            ..Default::default()
        };
        let call_id = entry.id;
        let mut effects: Effects = Effect::InitRecorder { call_id }.into();
        if !channel.signal_detection {
            effects.push(Effect::EnableRecorder { call_id });
        }
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
        let now = self.clock.now();
        for pt in self.patch_tables.values_mut() {
            pt.clear_stale(now);
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

    pub(crate) fn conclude_all(&self) -> Effects {
        let mut effects = Effects::new();
        for entry in &self.calls {
            if matches!(
                entry.state,
                CallState::Recording | CallState::Idle | CallState::Disabled
            ) {
                effects.push(Effect::EmitCallEnd {
                    call_id: entry.id,
                    reason: CallEndReason::Shutdown,
                });
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

    pub(crate) fn get_call_view(&self, call_id: CallId) -> Option<CallView<'_>> {
        self.calls.iter().find(|e| e.id == call_id).map(CallView)
    }

    pub(crate) fn iter_calls(&self) -> impl Iterator<Item = CallView<'_>> {
        self.calls.iter().map(CallView)
    }

    pub(crate) const fn call_count(&self) -> usize {
        self.calls.len()
    }

    /// Feed recorder power reading from C++ into the call entry.
    /// Used after stopping a recorder to update signal/noise metrics.
    pub(crate) fn feed_recorder_power(&mut self, call_id: CallId, pwr: f64, was_idle: bool) {
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            if was_idle {
                entry.noise = trunk_recorder_sys::signal_from_ffi(pwr);
            } else {
                entry.signal = trunk_recorder_sys::signal_from_ffi(pwr);
            }
        }
    }

    /// Expire a trunked call due to recorder write timeout.
    /// Returns effects to conclude and remove the call.
    pub(crate) fn expire_call(&self, call_id: CallId) -> Effects {
        let mut effects = Effects::new();
        if let Some(entry) = self.get_call(call_id)
            && matches!(entry.state, CallState::Recording | CallState::Idle)
        {
            effects.push(Effect::EmitCallEnd {
                call_id,
                reason: CallEndReason::WriteTimeout,
            });
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
        systems.insert(
            1,
            SystemPolicy {
                short_name: "test".into(),
                record_unknown: true,
                ..Default::default()
            },
        );
        CallManagerConfig {
            call_timeout: 1.0,
            new_call_from_update: false,
            record_uu_v_calls: false,
            default_mode: DefaultMode::Digital,
            temp_dir: String::new(),
            systems,
            adaptive_freq_correction: false,
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
        let names: Vec<&str> = effects
            .iter()
            .map(|e| match e {
                Effect::CreateCall { .. } => "CreateCall",
                Effect::RequestRecording { .. } => "RequestRecording",
                Effect::StopRecording { .. } => "StopRecording",
                Effect::SetFreq { .. } => "SetFreq",
                Effect::SetSource { .. } => "SetSource",
                Effect::EmitCallStart { .. } => "EmitCallStart",
                Effect::EmitCallEnd { .. } => "EmitCallEnd",
                Effect::RotateTransmission { .. } => "RotateTransmission",
                Effect::EmitCallsActive => "EmitCallsActive",
                Effect::EmitSignal { .. } => "EmitSignal",
                Effect::EmitUnitEvent { .. } => "EmitUnitEvent",
                Effect::EmitSetupSystem { .. } => "EmitSetupSystem",
                Effect::EmitSetupRecorder { .. } => "EmitSetupRecorder",
                Effect::AddControlChannel { .. } => "AddControlChannel",
                Effect::ConventionalRestart { .. } => "ConventionalRestart",
                Effect::InitRecorder { .. } => "InitRecorder",
                Effect::EnableRecorder { .. } => "EnableRecorder",
                Effect::DisableRecorder { .. } => "DisableRecorder",
                Effect::RemoveCall { .. } => "RemoveCall",
                Effect::UpdateSquelch { .. } => "UpdateSquelch",
                Effect::Warn { .. } => "Warn",
            })
            .collect();

        let mut name_iter = names.iter();
        for &exp in expected {
            let found = name_iter.any(|&n| n == exp);
            assert!(found, "expected {exp} in effects, got: {names:?}");
        }
    }

    /// Return the CallId from a CreateCall effect.
    fn extract_create_call_id(effects: &[Effect]) -> CallId {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::CreateCall { call_id } => Some(*call_id),
                _ => None,
            })
            .expect("no CreateCall effect found")
    }

    // ── Grant creates a call ──

    #[test]
    fn grant_creates_call_with_recording_request() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);

        let effects = sm.on_message(&msg);

        assert_effects_contain_ordered(
            &effects,
            &[
                "CreateCall",
                "RequestRecording",
                "EmitCallStart",
                "EmitCallsActive",
            ],
        );
        assert_eq!(sm.calls.len(), 1);
        assert_eq!(sm.calls[0].talkgroup, 100);
    }

    // ── Grant on existing call with same freq updates source ──

    #[test]
    fn grant_same_freq_updates_source() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let _effects = sm.on_message(&msg);

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
        let _effects = sm.on_recording_result(
            call_id,
            RecordingResult::Started {
                rec_num: RecorderNum(0),
            },
        );

        let msg2 = grant_msg(100, 852_000_000.0);
        let effects = sm.on_message(&msg2);

        assert_effects_contain_ordered(&effects, &["StopRecording", "SetFreq", "RequestRecording"]);
    }

    // ── A channel granted to another talkgroup ends the call on it ──

    fn tdma_grant(talkgroup: i64, freq_hz: f64, slot: i32) -> TrunkMessage {
        TrunkMessage {
            phase2_tdma: true,
            tdma_slot: slot,
            ..grant_msg(talkgroup, freq_hz)
        }
    }

    fn start(sm: &mut TrunkStateMachine, msg: &TrunkMessage) -> CallId {
        let call_id = extract_create_call_id(&sm.on_message(msg));
        let _ = sm.on_recording_result(
            call_id,
            RecordingResult::Started {
                rec_num: RecorderNum(5),
            },
        );
        call_id
    }

    fn ended(effects: &[Effect]) -> Vec<(CallId, CallEndReason)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::EmitCallEnd { call_id, reason } => Some((*call_id, *reason)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn grant_of_the_channel_to_another_talkgroup_ends_the_call() {
        let mut sm = TrunkStateMachine::new(test_config());
        let a = start(&mut sm, &grant_msg(100, 851_000_000.0));

        let effects = sm.on_message(&grant_msg(200, 851_000_000.0));

        assert_eq!(ended(&effects), [(a, CallEndReason::ChannelReassigned)]);
        // The old call ends, and frees its recorder, before the new one asks.
        assert_effects_contain_ordered(
            &effects,
            &[
                "EmitCallEnd",
                "RemoveCall",
                "CreateCall",
                "RequestRecording",
            ],
        );
    }

    #[test]
    fn tdma_reassignment_ends_only_the_call_on_that_slot() {
        let mut sm = TrunkStateMachine::new(test_config());
        let slot0 = start(&mut sm, &tdma_grant(100, 851_000_000.0, 0));
        let _slot1 = start(&mut sm, &tdma_grant(101, 851_000_000.0, 1));

        let effects = sm.on_message(&tdma_grant(200, 851_000_000.0, 0));
        assert_eq!(ended(&effects), [(slot0, CallEndReason::ChannelReassigned)]);

        // A phase 1 grant takes the whole channel.
        let mut sm = TrunkStateMachine::new(test_config());
        let slot0 = start(&mut sm, &tdma_grant(100, 851_000_000.0, 0));
        let slot1 = start(&mut sm, &tdma_grant(101, 851_000_000.0, 1));
        let effects = sm.on_message(&grant_msg(200, 851_000_000.0));
        assert_eq!(
            ended(&effects),
            [
                (slot0, CallEndReason::ChannelReassigned),
                (slot1, CallEndReason::ChannelReassigned)
            ]
        );
    }

    #[test]
    fn patched_talkgroups_share_a_channel() {
        let mut sm = TrunkStateMachine::new(test_config());
        sm.patch_tables.insert(1, PatchTable::new());
        let now = sm.clock().now();
        sm.patch_tables
            .get_mut(&1)
            .unwrap()
            .update(900, 100, 200, 0, now);
        let _ = start(&mut sm, &grant_msg(100, 851_000_000.0));

        let effects = sm.on_message(&grant_msg(200, 851_000_000.0));

        assert_eq!(ended(&effects), []);
    }

    #[test]
    fn an_update_naming_the_channel_does_not_end_the_call() {
        // Updates repeat for calls already set up, so one for another
        // talkgroup can be stale; only a grant says the channel moved.
        let mut config = test_config();
        config.new_call_from_update = true;
        let mut sm = TrunkStateMachine::new(config);
        let _ = start(&mut sm, &grant_msg(100, 851_000_000.0));

        let update = TrunkMessage {
            message_type: MessageType::Update,
            ..grant_msg(200, 851_000_000.0)
        };
        assert_eq!(ended(&sm.on_message(&update)), []);
    }

    #[test]
    fn a_talkgroup_moving_to_another_slot_retunes_its_call() {
        let mut sm = TrunkStateMachine::new(test_config());
        let a = start(&mut sm, &tdma_grant(100, 851_000_000.0, 0));

        let effects = sm.on_message(&tdma_grant(100, 852_000_000.0, 1));

        assert_eq!(ended(&effects), []);
        assert_effects_contain_ordered(&effects, &["StopRecording", "SetFreq", "RequestRecording"]);
        assert_eq!(sm.calls.len(), 1);
        let entry = sm.get_call(a).unwrap();
        assert_eq!(
            (entry.freq, entry.tdma_slot),
            (Freq::from_hz(852_000_000.0), 1)
        );

        // Same frequency, other slot: still a retune, not a second call.
        let _ = sm.on_recording_result(
            a,
            RecordingResult::Started {
                rec_num: RecorderNum(6),
            },
        );
        let effects = sm.on_message(&tdma_grant(100, 852_000_000.0, 0));
        assert_effects_contain_ordered(&effects, &["StopRecording", "RequestRecording"]);
        assert_eq!(sm.calls.len(), 1);
        assert_eq!(sm.get_call(a).unwrap().tdma_slot, 0);
    }

    // ── Recording result updates state ──

    #[test]
    fn recording_result_success_sets_recording_state() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);

        let effects = sm.on_recording_result(
            call_id,
            RecordingResult::Started {
                rec_num: RecorderNum(5),
            },
        );

        assert_eq!(sm.calls[0].state, CallState::Recording);
        assert_eq!(sm.calls[0].recorder, Some(RecorderNum(5)));
        assert_effects_contain_ordered(&effects, &["EmitSetupRecorder"]);
    }

    // ── In-band source change → per-speaker transmission boundary ──

    /// Drive a trunked call into Recording and return its `CallId`.
    fn recording_trunked_call(sm: &mut TrunkStateMachine) -> CallId {
        let effects = sm.on_message(&grant_msg(100, 851_000_000.0));
        let call_id = extract_create_call_id(&effects);
        let _ = sm.on_recording_result(
            call_id,
            RecordingResult::Started {
                rec_num: RecorderNum(5),
            },
        );
        call_id
    }

    #[test]
    fn crypto_records_algid_and_marks_encrypted() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        assert_eq!(sm.calls[0].alg_id, None);

        sm.on_crypto(call_id, 0x84, 0x1234);
        assert!(sm.calls[0].encrypted);
        assert_eq!(sm.calls[0].alg_id, Some(0x84));
        assert_eq!(sm.calls[0].key_id, Some(0x1234));

        // Unknown call ids are ignored.
        sm.on_crypto(CallId::from_u64(u64::MAX), 0xAA, 1);
        assert_eq!(sm.calls[0].alg_id, Some(0x84));
        assert_eq!(sm.alg_id_for(call_id), Some(0x84));

        // A later call on the same talkgroup that never decoded an ESS (e.g.
        // rejected at the grant) is labelled from the talkgroup cache.
        let (sys, tg) = (sm.calls[0].sys_num, sm.calls[0].talkgroup);
        sm.calls.push(CallEntry {
            sys_num: sys,
            talkgroup: tg,
            ..CallEntry::default()
        });
        let other = sm.calls[1].id;
        assert_eq!(sm.calls[1].alg_id, None);
        assert_eq!(sm.alg_id_for(other), Some(0x84));
    }

    #[test]
    fn source_change_cuts_transmission_boundary() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        // Past MIN_TRANSMISSION so a real boundary is cut.
        sm.calls[0].segment_start = Instant::now() - Duration::from_secs(2);

        // A different unit keys up mid-call: end this transmission, start the next.
        let effects = sm.on_source_change(call_id, 2002);
        assert_effects_contain_ordered(
            &effects,
            &[
                "EmitCallEnd",
                "RotateTransmission",
                "SetSource",
                "EmitSignal",
                "EmitCallStart",
            ],
        );
        assert_eq!(sm.calls[0].current_source_id, 2002);

        // The same source re-firing every superframe must not cut another boundary.
        assert!(sm.on_source_change(call_id, 2002).is_empty());
        // Unknown source (encrypted / no-LC) must not cut a boundary.
        assert!(sm.on_source_change(call_id, -1).is_empty());
    }

    // ── Control channel grant on a recording call → per-PTT boundary ──

    fn key_up(sm: &mut TrunkStateMachine, source: i64) -> Effects {
        sm.on_message(&TrunkMessage {
            source,
            ..grant_msg(100, 851_000_000.0)
        })
    }

    fn cut_at_key_up(effects: &Effects, source: i64) -> bool {
        matches!(
            effects.as_ref(),
            [
                Effect::EmitCallEnd {
                    reason: CallEndReason::KeyUp,
                    ..
                },
                Effect::RotateTransmission { source_id, .. },
                Effect::SetSource { .. },
                Effect::EmitSignal { .. },
                Effect::EmitCallStart { .. },
            ] if *source_id == source
        )
    }

    #[test]
    fn a_grant_naming_another_handset_cuts_a_transmission() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        sm.calls[0].segment_start = Instant::now() - Duration::from_secs(2);
        assert!(cut_at_key_up(&key_up(&mut sm, 2002), 2002));
        assert_eq!(sm.calls[0].current_source_id, 2002);
        // The in-band source change that follows matches and adds nothing.
        assert!(sm.on_source_change(call_id, 2002).is_empty());
    }

    #[test]
    fn the_same_handset_keying_again_cuts_only_after_its_transmission_ended() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        sm.calls[0].segment_start = Instant::now() - Duration::from_secs(2);
        // A repeated grant within one key-up: no boundary.
        assert!(
            !key_up(&mut sm, 100)
                .iter()
                .any(|e| matches!(e, Effect::EmitCallEnd { .. }))
        );
        // The voice channel saw a terminator; the next grant is a new key-up.
        sm.on_ptt_ended(call_id);
        assert!(cut_at_key_up(&key_up(&mut sm, 100), 100));
        // As the executor applies the RotateTransmission effect.
        sm.on_transmission_rotated(call_id, 100);
        assert!(!sm.calls[0].ptt_ended, "a new transmission starts unended");
    }

    #[test]
    fn key_ups_within_the_minimum_transmission_only_reroute() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = recording_trunked_call(&mut sm);
        let effects = key_up(&mut sm, 2002);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::EmitCallEnd { .. }))
        );
        assert_eq!(sm.calls[0].current_source_id, 2002);
    }

    #[test]
    fn cut_transmission_policies_emit_exact_vectors() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);

        // Span (trunked per-speaker): logical boundary on a running recorder.
        let span = sm.cut_transmission(call_id, RecorderPolicy::Span { source_id: 42 });
        assert!(matches!(
            span.as_ref(),
            [
                Effect::EmitCallEnd {
                    reason: CallEndReason::SourceChange,
                    ..
                },
                Effect::RotateTransmission { source_id: 42, .. },
                Effect::SetSource { source_id: 42, .. },
                Effect::EmitSignal { source_id: 42, .. },
                Effect::EmitCallStart { .. },
            ]
        ));

        // Recycle (conventional): stop/restart the recorder, no eager CallStart.
        let recycle = sm.cut_transmission(call_id, RecorderPolicy::Recycle);
        assert!(matches!(
            recycle.as_ref(),
            [
                Effect::EmitCallEnd {
                    reason: CallEndReason::Conventional,
                    ..
                },
                Effect::ConventionalRestart { .. },
                Effect::EmitSetupRecorder { .. },
                Effect::EnableRecorder { .. },
            ]
        ));
        assert!(
            !recycle
                .iter()
                .any(|e| matches!(e, Effect::EmitCallStart { .. })),
            "Recycle must not eagerly start the next transmission"
        );
    }

    #[test]
    fn source_change_below_min_transmission_only_reroutes() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        // segment_start is fresh (< MIN_TRANSMISSION): a rapid hand-off must
        // only re-route the source, not fragment into a sub-second transmission.
        let effects = sm.on_source_change(call_id, 2002);
        assert_effects_contain_ordered(&effects, &["SetSource", "EmitSignal"]);
        assert!(
            !effects.iter().any(|e| matches!(
                e,
                Effect::EmitCallEnd { .. }
                    | Effect::EmitCallStart { .. }
                    | Effect::RotateTransmission { .. }
            )),
            "below MIN_TRANSMISSION must not cut a transmission boundary"
        );
        assert_eq!(sm.calls[0].current_source_id, 2002);
    }

    #[test]
    fn regrant_with_new_speaker_on_parked_call_cuts_boundary() {
        let mut sm = TrunkStateMachine::new(test_config());
        // Recording, speaker 100 (recording_trunked_call grants source 100).
        let call_id = recording_trunked_call(&mut sm);
        assert_eq!(sm.calls[0].current_source_id, 100);

        // PTT release: TDULC parks the call Idle (recorder kept warm), no event.
        let tdulc = TrunkMessage {
            message_type: MessageType::Tdulc,
            ..grant_msg(100, 851_000_000.0)
        };
        assert!(sm.on_message(&tdulc).is_empty());
        assert_eq!(sm.calls[0].state, CallState::Idle);

        // A different unit (200) keys up: the regrant must cut a per-speaker
        // boundary for speaker 100, not silently merge 100 and 200 into one
        // transmission.
        let regrant = TrunkMessage {
            source: 200,
            ..grant_msg(100, 851_000_000.0)
        };
        let effects = sm.on_message(&regrant);
        assert_effects_contain_ordered(
            &effects,
            &[
                "EmitCallEnd",
                "RotateTransmission",
                "SetSource",
                "EmitSignal",
                "EmitCallStart",
            ],
        );
        // The new source is tracked synchronously so a following control-channel
        // update for the same unit dedups.
        assert_eq!(sm.calls[0].current_source_id, 200);
        // EmitCallEnd is ordered before RotateTransmission, so the conclude still
        // observes the parked Idle state (the executor processes effects in
        // order). Simulate the RotateTransmission effect the executor applies
        // between EmitCallEnd and EmitCallStart.
        let end_pos = effects
            .iter()
            .position(|e| matches!(e, Effect::EmitCallEnd { .. }))
            .unwrap();
        let rotate_pos = effects
            .iter()
            .position(|e| matches!(e, Effect::RotateTransmission { .. }))
            .unwrap();
        assert!(end_pos < rotate_pos, "EmitCallEnd must precede the rotate");
        assert_eq!(sm.calls[0].state, CallState::Idle);
        sm.on_transmission_rotated(call_id, 200);
        // After the rotate: Idle → Recording, prior speaker's segments cleared.
        assert_eq!(sm.calls[0].state, CallState::Recording);
        assert!(sm.calls[0].segments.is_empty());
    }

    #[test]
    fn regrant_with_same_speaker_on_parked_call_does_not_cut_boundary() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);

        let tdulc = TrunkMessage {
            message_type: MessageType::Tdulc,
            ..grant_msg(100, 851_000_000.0)
        };
        let _ = sm.on_message(&tdulc);
        assert_eq!(sm.calls[0].state, CallState::Idle);

        // Same unit (100) re-keys: this is one speaker's PTT cycle, so resume in
        // place — no boundary (no CallEnd/CallStart) — and stay coalesced.
        let regrant = grant_msg(100, 851_000_000.0); // source 100
        let effects = sm.on_message(&regrant);
        assert!(
            !effects.iter().any(|e| matches!(
                e,
                Effect::EmitCallEnd { .. }
                    | Effect::EmitCallStart { .. }
                    | Effect::RotateTransmission { .. }
            )),
            "same-speaker re-key must not cut a transmission boundary"
        );
        assert_eq!(sm.calls[0].state, CallState::Recording);
        let _ = call_id;
    }

    #[test]
    fn source_change_ignored_when_not_recording() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        sm.calls[0].segment_start = Instant::now() - Duration::from_secs(2);
        sm.calls[0].state = CallState::Idle;
        assert!(sm.on_source_change(call_id, 2002).is_empty());
    }

    #[test]
    fn recording_result_no_source_rejects() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);

        let result_effects = sm.on_recording_result(call_id, RecordingResult::NoSource);

        assert_eq!(
            sm.calls[0].state,
            CallState::Rejected(RejectReason::NoSource)
        );
        assert!(result_effects.is_empty());
    }

    #[test]
    fn recording_result_no_recorder_rejects() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);

        let effects = sm.on_recording_result(call_id, RecordingResult::NoRecorder);

        assert_eq!(
            sm.calls[0].state,
            CallState::Rejected(RejectReason::NoRecorder)
        );
        assert!(effects.is_empty());
    }

    // ── TDULC transitions Recording → Idle ──

    #[test]
    fn tdulc_transitions_recording_to_idle() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        let _effects = sm.on_recording_result(
            call_id,
            RecordingResult::Started {
                rec_num: RecorderNum(0),
            },
        );

        let tdulc = TrunkMessage {
            message_type: MessageType::Tdulc,
            talkgroup: 100,
            sys_num: 1,
            ..grant_msg(100, 0.0)
        };
        let effects = sm.on_message(&tdulc);

        // TDULC only transitions state — no effects emitted
        assert!(effects.is_empty());
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
        let _effects = sm.on_recording_result(
            call_id,
            RecordingResult::Started {
                rec_num: RecorderNum(0),
            },
        );

        // TDULC → Idle
        let tdulc = TrunkMessage {
            message_type: MessageType::Tdulc,
            talkgroup: 100,
            sys_num: 1,
            ..grant_msg(100, 0.0)
        };
        let _effects = sm.on_message(&tdulc);
        assert_eq!(sm.calls[0].state, CallState::Idle);

        // Grant again → Recording
        let _effects = sm.on_message(&msg);
        assert_eq!(sm.calls[0].state, CallState::Recording);
    }

    // ── Tick: timed-out call emits EmitCallEnd BEFORE RemoveCall ──

    #[test]
    fn tick_emits_call_end_before_remove() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        let _effects = sm.on_recording_result(
            call_id,
            RecordingResult::Started {
                rec_num: RecorderNum(0),
            },
        );

        // Force the call to be timed out
        sm.calls[0].last_update = Instant::now() - std::time::Duration::from_secs(10);

        let effects = sm.tick();

        // EmitCallEnd must come before RemoveCall
        let end_pos = effects
            .iter()
            .position(|e| matches!(e, Effect::EmitCallEnd { .. }));
        let remove_pos = effects
            .iter()
            .position(|e| matches!(e, Effect::RemoveCall { .. }));

        assert!(end_pos.is_some(), "expected EmitCallEnd in tick effects");
        assert!(remove_pos.is_some(), "expected RemoveCall in tick effects");
        assert!(
            end_pos.unwrap() < remove_pos.unwrap(),
            "EmitCallEnd must come before RemoveCall, got end={} remove={}",
            end_pos.unwrap(),
            remove_pos.unwrap()
        );

        assert!(
            effects.iter().any(|e| matches!(e, Effect::EmitCallsActive)),
            "tick must emit EmitCallsActive when a call is removed"
        );

        // Call entry must still exist until RemoveCall is processed
        assert!(
            sm.get_call(call_id).is_some(),
            "entry must exist until RemoveCall is executed"
        );
    }

    #[test]
    fn tick_emits_call_end_before_remove_for_rejected() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        let _effects = sm.on_recording_result(call_id, RecordingResult::NoSource);

        sm.calls[0].last_update = Instant::now() - std::time::Duration::from_secs(10);

        let effects = sm.tick();

        let end_pos = effects
            .iter()
            .position(|e| matches!(e, Effect::EmitCallEnd { .. }));
        let remove_pos = effects
            .iter()
            .position(|e| matches!(e, Effect::RemoveCall { .. }));

        assert!(end_pos.is_some());
        assert!(remove_pos.is_some());
        assert!(end_pos.unwrap() < remove_pos.unwrap());
        assert!(effects.iter().any(|e| matches!(e, Effect::EmitCallsActive)));
    }

    // ── Tick: call within timeout is NOT removed ──

    #[test]
    fn tick_does_not_remove_active_call() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        let _effects = sm.on_recording_result(
            call_id,
            RecordingResult::Started {
                rec_num: RecorderNum(0),
            },
        );

        let effects = sm.tick();

        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::RemoveCall { .. }))
        );
        assert_eq!(sm.calls.len(), 1);
    }

    // ── conclude_all emits EmitCallEnd BEFORE RemoveCall ──

    #[test]
    fn conclude_all_emits_call_end_before_remove() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let effects = sm.on_message(&msg);
        let call_id = extract_create_call_id(&effects);
        let _effects = sm.on_recording_result(
            call_id,
            RecordingResult::Started {
                rec_num: RecorderNum(0),
            },
        );

        let effects = sm.conclude_all();

        let end_pos = effects
            .iter()
            .position(|e| matches!(e, Effect::EmitCallEnd { .. }));
        let remove_pos = effects
            .iter()
            .position(|e| matches!(e, Effect::RemoveCall { .. }));

        assert!(end_pos.is_some(), "expected EmitCallEnd");
        assert!(remove_pos.is_some(), "expected RemoveCall");
        assert!(
            end_pos.unwrap() < remove_pos.unwrap(),
            "EmitCallEnd must come before RemoveCall"
        );
        assert!(effects.iter().any(|e| matches!(e, Effect::EmitCallsActive)));

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

        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::RemoveCall { .. }))
        );
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

        // Per-transmission conclude: end → restart C++ → setup → enable. No
        // eager EmitCallStart — the next squelch open starts the next call.
        assert_effects_contain_ordered(
            &effects,
            &[
                "EmitCallEnd",
                "ConventionalRestart",
                "EmitSetupRecorder",
                "EnableRecorder",
            ],
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::EmitCallStart { .. })),
            "idle timeout must not eagerly emit CallStart"
        );

        // Recorder must still be set after timeout — the executor conclude path
        // must NOT clear it, or tick_conventional will skip this call forever.
        // State stays Recording in the pure state machine; the executor's
        // ConventionalRestart re-arms it to Idle.
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

        // Simulate restart with a new recorder number (C++ may reassign). The
        // channel re-arms to Idle, waiting for the next squelch open.
        sm.on_conventional_restarted(call_id, Some(RecorderNum(13)));

        let entry = sm.get_call(call_id).unwrap();
        assert_eq!(entry.recorder, Some(RecorderNum(13)));
        assert_eq!(entry.state, CallState::Idle);

        // Squelch open on the NEW recorder must match this call → Recording
        let _effects = sm.on_squelch(RecorderNum(13), false, -20.0);
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Recording);

        // Squelch on the OLD recorder must NOT match (no state change)
        let _effects = sm.on_squelch(RecorderNum(12), true, -30.0);
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Recording);
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

        let effects = sm.on_squelch(RecorderNum(5), true, -30.0);

        // Debounce: squelch close is deferred, state stays Recording
        assert!(effects.is_empty());
        assert_eq!(sm.calls[0].state, CallState::Recording);
        assert!(sm.calls[0].squelch_closed_at.is_some());

        // Finalize debounce → transitions to Idle with segment pushed
        sm.calls[0].finalize_squelch_debounce(Instant::now());
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

        let effects = sm.on_squelch(RecorderNum(5), false, -20.0);

        assert_effects_contain_ordered(&effects, &["EmitCallStart", "EmitCallsActive"]);
        assert_eq!(sm.calls[0].state, CallState::Recording);
    }

    #[test]
    fn squelch_reopen_does_not_emit_call_start() {
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

        // First open — emits CallStart
        let effects = sm.on_squelch(RecorderNum(5), false, -20.0);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::EmitCallStart { .. }))
        );

        // Close
        let _effects = sm.on_squelch(RecorderNum(5), true, -30.0);

        // Re-open before the debounce conclude — should NOT emit CallStart
        // again (a brief un-key under the debounce window stays one call).
        let effects = sm.on_squelch(RecorderNum(5), false, -20.0);
        assert!(effects.is_empty());
        assert_eq!(sm.calls[0].state, CallState::Recording);
    }

    #[test]
    fn always_on_conventional_conclude_per_transmission() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(5)));

        // First keyed transmission: squelch open → CallStart + Recording.
        let effects = sm.on_squelch(RecorderNum(5), false, -20.0);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::EmitCallStart { .. }))
        );
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Recording);

        // Squelch close arms the debounce (stays Recording, no segment yet).
        let _ = sm.on_squelch(RecorderNum(5), true, -30.0);
        assert!(sm.get_call(call_id).unwrap().squelch_closed_at.is_some());

        // Debounce conclude ends this transmission as its own call and clears
        // the debounce marker. No eager CallStart.
        let effects = sm.conclude_conventional_transmission(call_id);
        assert_effects_contain_ordered(
            &effects,
            &[
                "EmitCallEnd",
                "ConventionalRestart",
                "EmitSetupRecorder",
                "EnableRecorder",
            ],
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::EmitCallStart { .. }))
        );
        assert!(sm.get_call(call_id).unwrap().squelch_closed_at.is_none());

        // Executor re-arms to Idle via ConventionalRestart.
        sm.on_conventional_restarted(call_id, Some(RecorderNum(5)));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);

        // Second keyed transmission: the next open is a fresh CallStart.
        let effects = sm.on_squelch(RecorderNum(5), false, -20.0);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::EmitCallStart { .. })),
            "each squelch open after a conclude starts a new call"
        );
    }

    // ── expire_call ──

    #[test]
    fn expire_call_recording_emits_call_end() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let _effects = sm.on_message(&msg);
        let call_id = sm.calls[0].id;
        sm.calls[0].state = CallState::Recording;

        let effects = sm.expire_call(call_id);

        assert_effects_contain_ordered(
            &effects,
            &[
                "EmitCallEnd",
                "EmitSetupRecorder",
                "RemoveCall",
                "EmitCallsActive",
            ],
        );
    }

    #[test]
    fn expire_call_rejected_skips_call_end() {
        let mut sm = TrunkStateMachine::new(test_config());
        let msg = grant_msg(100, 851_000_000.0);
        let _effects = sm.on_message(&msg);
        let call_id = sm.calls[0].id;
        sm.calls[0].state = CallState::Rejected(RejectReason::Encrypted);

        let effects = sm.expire_call(call_id);

        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::EmitCallEnd { .. }))
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::RemoveCall { .. }))
        );
        assert!(effects.iter().any(|e| matches!(e, Effect::EmitCallsActive)));
    }

    // ── Update message handling ──

    #[test]
    fn update_with_source_change_emits_signal_not_call_start() {
        let mut sm = TrunkStateMachine::new(test_config());

        // Create an existing call via grant
        let msg = grant_msg(100, 851_000_000.0);
        let _effects = sm.on_message(&msg);
        assert_eq!(sm.calls.len(), 1);

        // Send Update with a different source unit
        let mut update = grant_msg(100, 851_000_000.0);
        update.message_type = MessageType::Update;
        update.source = 200; // different from grant's source=100

        let effects = sm.on_message(&update);

        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetSource { source_id: 200, .. }))
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::EmitSignal { source_id: 200, .. }))
        );
        // A source change on an already-started call must NOT re-emit
        // EmitCallStart — that produced duplicate downstream call_starts which
        // orphaned the Discord embed (keyed by talkgroup).
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::EmitCallStart { .. }))
        );
    }

    #[test]
    fn update_same_source_produces_no_effects() {
        let mut sm = TrunkStateMachine::new(test_config());

        let msg = grant_msg(100, 851_000_000.0);
        let _effects = sm.on_message(&msg);

        // Update with same source
        let mut update = grant_msg(100, 851_000_000.0);
        update.message_type = MessageType::Update;
        // source=100, same as grant

        let effects = sm.on_message(&update);

        assert!(effects.is_empty());
    }

    #[test]
    fn update_sets_encrypted_flag() {
        let mut sm = TrunkStateMachine::new(test_config());

        let msg = grant_msg(100, 851_000_000.0);
        let _effects = sm.on_message(&msg);
        assert!(!sm.calls[0].encrypted);

        let mut update = grant_msg(100, 851_000_000.0);
        update.message_type = MessageType::Update;
        update.encrypted = true;

        let _effects = sm.on_message(&update);

        assert!(sm.calls[0].encrypted);
    }

    #[test]
    fn uuv_update_ignored_when_disabled() {
        let mut sm = TrunkStateMachine::new(test_config());
        // record_uu_v_calls defaults to false in test_config

        let msg = grant_msg(100, 851_000_000.0);
        let _effects = sm.on_message(&msg);

        let mut update = grant_msg(100, 851_000_000.0);
        update.message_type = MessageType::UuVUpdate;
        update.source = 200;

        let effects = sm.on_message(&update);

        assert!(effects.is_empty());
    }

    #[test]
    fn uuv_update_processed_when_enabled() {
        let mut config = test_config();
        config.record_uu_v_calls = true;
        let mut sm = TrunkStateMachine::new(config);

        let msg = grant_msg(100, 851_000_000.0);
        let _effects = sm.on_message(&msg);

        let mut update = grant_msg(100, 851_000_000.0);
        update.message_type = MessageType::UuVUpdate;
        update.source = 200;

        let effects = sm.on_message(&update);

        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetSource { source_id: 200, .. }))
        );
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
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::RequestRecording { .. }))
        );
        assert_eq!(
            sm.calls[0].state,
            CallState::Rejected(RejectReason::Encrypted)
        );
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
    fn signal_detection_starts_disabled_with_init() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, effects) =
            sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));

        // Signal-detection channels start Disabled with only InitRecorder (no EnableRecorder)
        assert_effects_contain_ordered(&effects, &["InitRecorder"]);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::EnableRecorder { .. }))
        );
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);
    }

    #[test]
    fn signal_detection_idle_timeout_concludes_and_restarts() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        sm.calls[0].state = CallState::Recording;

        let effects = sm.on_conventional_idle_timeout(call_id);

        // Recording state triggers CallEnd, then restart for fresh WAV, then disable
        assert_effects_contain_ordered(
            &effects,
            &["EmitCallEnd", "ConventionalRestart", "DisableRecorder"],
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::EmitCallStart { .. }))
        );

        // State hasn't changed yet — the executor processes ConventionalRestart
        // and calls on_conventional_restarted() which resets state to Disabled.
        sm.on_conventional_restarted(call_id, Some(RecorderNum(12)));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);
        assert!(sm.get_call(call_id).unwrap().segments.is_empty());
    }

    #[test]
    fn signal_detection_idle_timeout_then_redetect_enables_recorder() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        sm.calls[0].state = CallState::Recording;

        // Idle timeout → effects include restart + disable
        let effects = sm.on_conventional_idle_timeout(call_id);
        assert_effects_contain_ordered(&effects, &["ConventionalRestart", "DisableRecorder"]);

        // Simulate executor processing ConventionalRestart → state becomes Disabled
        sm.on_conventional_restarted(call_id, Some(RecorderNum(12)));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);

        // Signal detected again → Idle + EnableRecorder
        let signals = vec![trunk_recorder_sys::DetectedSignal {
            center_freq: 1_000_000.0,
            max_rssi: -20,
            threshold: -45,
        }];
        let effects = sm.on_signals_detected(487_000_000.0, &signals);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::EnableRecorder { .. }))
        );
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
    }

    #[test]
    fn signal_detection_idle_timeout_clears_idle_since() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        sm.calls[0].state = CallState::Idle;
        sm.calls[0].idle_since = Some(Instant::now());

        let _effects = sm.on_conventional_idle_timeout(call_id);

        // idle_since is cleared by on_conventional_restarted(), which the
        // executor calls during ConventionalRestart effect processing.
        sm.on_conventional_restarted(call_id, Some(RecorderNum(12)));
        let entry = sm.get_call(call_id).unwrap();
        assert!(entry.idle_since.is_none());
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

        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::EnableRecorder { .. }))
        );
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
        // No premature idle timer — wait for squelch to open/close first
        assert!(sm.get_call(call_id).unwrap().idle_since.is_none());
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
        let (call_id, effects) =
            sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));

        // Non-signal-detection channels start Idle with InitRecorder + EnableRecorder
        assert_effects_contain_ordered(&effects, &["InitRecorder", "EnableRecorder"]);
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

        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::ConventionalRestart { .. }))
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::EnableRecorder { .. }))
        );
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

        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::EnableRecorder { .. }))
        );
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
    }

    #[test]
    fn signal_detection_idle_timeout_no_call_end_without_activity() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        // Idle with no segments — signal detected but squelch never opened
        sm.calls[0].state = CallState::Idle;

        let effects = sm.on_conventional_idle_timeout(call_id);

        // No audio was captured so no CallEnd, but still restart + disable
        assert_effects_contain_ordered(&effects, &["ConventionalRestart", "DisableRecorder"]);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::EmitCallEnd { .. }))
        );

        // State transitions to Disabled after on_conventional_restarted()
        sm.on_conventional_restarted(call_id, Some(RecorderNum(12)));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);
    }

    #[test]
    fn signal_detection_idle_timeout_emits_call_end_when_recording() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));
        // Actively recording when timeout fires (no segments pushed yet)
        sm.calls[0].state = CallState::Recording;

        let effects = sm.on_conventional_idle_timeout(call_id);

        // Recording state triggers CallEnd even without segments
        assert_effects_contain_ordered(
            &effects,
            &["EmitCallEnd", "ConventionalRestart", "DisableRecorder"],
        );

        // State transitions to Disabled after on_conventional_restarted()
        sm.on_conventional_restarted(call_id, Some(RecorderNum(12)));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);
    }

    #[test]
    fn signal_detection_full_lifecycle() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, effects) =
            sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));

        // 1. Starts Disabled with InitRecorder
        assert_effects_contain_ordered(&effects, &["InitRecorder"]);
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);

        // 2. Signal detected → Idle + EnableRecorder, no idle timer
        let signals = vec![trunk_recorder_sys::DetectedSignal {
            center_freq: 1_000_000.0,
            max_rssi: -20,
            threshold: -45,
        }];
        let effects = sm.on_signals_detected(487_000_000.0, &signals);
        assert_effects_contain_ordered(&effects, &["EnableRecorder"]);
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
        assert!(sm.get_call(call_id).unwrap().idle_since.is_none());

        // 3. Squelch opens → Recording + CallStart
        let effects = sm.on_squelch(RecorderNum(12), false, -35.0);
        assert_effects_contain_ordered(&effects, &["EmitCallStart", "EmitCallsActive"]);
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Recording);

        // 4. Squelch closes → debounce pending, still Recording
        let effects = sm.on_squelch(RecorderNum(12), true, -80.0);
        assert!(effects.is_empty());
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Recording);

        // Finalize debounce → Idle, idle timer starts
        sm.calls
            .iter_mut()
            .find(|e| e.id == call_id)
            .unwrap()
            .finalize_squelch_debounce(Instant::now());
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
        assert!(sm.get_call(call_id).unwrap().idle_since.is_some());

        // 5. Idle timeout → conclude call, restart, disable
        let effects = sm.on_conventional_idle_timeout(call_id);
        assert_effects_contain_ordered(
            &effects,
            &["EmitCallEnd", "ConventionalRestart", "DisableRecorder"],
        );
        // Simulate executor processing ConventionalRestart
        sm.on_conventional_restarted(call_id, Some(RecorderNum(12)));
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Disabled);
        assert!(sm.get_call(call_id).unwrap().segments.is_empty());

        // 6. Signal detected again → Idle + EnableRecorder
        let effects = sm.on_signals_detected(487_000_000.0, &signals);
        assert_effects_contain_ordered(&effects, &["EnableRecorder"]);
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);

        // 7. Squelch opens again → new CallStart (segments were cleared)
        let effects = sm.on_squelch(RecorderNum(12), false, -35.0);
        assert_effects_contain_ordered(&effects, &["EmitCallStart", "EmitCallsActive"]);
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Recording);
    }

    #[test]
    fn signal_detection_enable_allows_indefinite_wait() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = signal_detection_channel();
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(12)));

        // Signal detected → Idle with no idle timer
        let signals = vec![trunk_recorder_sys::DetectedSignal {
            center_freq: 1_000_000.0,
            max_rssi: -20,
            threshold: -45,
        }];
        let _effects = sm.on_signals_detected(487_000_000.0, &signals);
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
        assert!(sm.get_call(call_id).unwrap().idle_since.is_none());

        // tick_conventional should NOT time this out (idle_since is None)
        let effects = sm.tick();
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::DisableRecorder { .. }))
        );
        assert_eq!(sm.get_call(call_id).unwrap().state, CallState::Idle);
    }

    // ── ControlChannel / Sysid / Status / UnitEvent messages ──

    #[test]
    fn control_channel_message_emits_add_control_channel() {
        let mut sm = TrunkStateMachine::new(test_config());
        let mut msg = grant_msg(0, 851_000_000.0);
        msg.message_type = MessageType::ControlChannel;

        let effects = sm.on_message(&msg);

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::AddControlChannel { sys_num: 1, .. }
        ));
    }

    #[test]
    fn sysid_message_emits_setup_system() {
        let mut sm = TrunkStateMachine::new(test_config());
        let mut msg = grant_msg(0, 0.0);
        msg.message_type = MessageType::Sysid;
        msg.sys_rfss = 42;
        msg.sys_site_id = 7;

        let effects = sm.on_message(&msg);

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::EmitSetupSystem { sys_num: 1 }
        ));
    }

    #[test]
    fn status_message_updates_identity_and_emits_setup() {
        let mut sm = TrunkStateMachine::new(test_config());
        let mut msg = grant_msg(0, 0.0);
        msg.message_type = MessageType::Status;
        msg.sys_id = 123;
        msg.wacn = 456;
        msg.nac = 789;

        let effects = sm.on_message(&msg);

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::EmitSetupSystem { sys_num: 1 }
        ));

        let id = sm.system_identities.get(&1).unwrap();
        assert_eq!(id.sys_id, 123);
        assert_eq!(id.wacn, 456);
        assert_eq!(id.nac, 789);
    }

    #[test]
    fn registration_message_emits_unit_event() {
        let mut sm = TrunkStateMachine::new(test_config());
        let mut msg = grant_msg(0, 0.0);
        msg.message_type = MessageType::Registration;
        msg.source = 42;

        let effects = sm.on_message(&msg);

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::EmitUnitEvent {
                type_: UnitEventType::Registration,
                source_id: 42,
                ..
            }
        ));
    }

    #[test]
    fn affiliation_message_includes_talkgroup() {
        let mut sm = TrunkStateMachine::new(test_config());
        let mut msg = grant_msg(0, 0.0);
        msg.message_type = MessageType::Affiliation;
        msg.source = 10;
        msg.talkgroup = 200;

        let effects = sm.on_message(&msg);

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::EmitUnitEvent {
                type_: UnitEventType::Affiliation,
                source_id: 10,
                talkgroup: 200,
                ..
            }
        ));
    }

    #[test]
    fn invalid_cc_message_produces_no_effects() {
        let mut sm = TrunkStateMachine::new(test_config());
        let mut msg = grant_msg(0, 0.0);
        msg.message_type = MessageType::InvalidCcMessage;

        let effects = sm.on_message(&msg);

        assert!(effects.is_empty());
    }

    // ── Stuck-recording backstop ──

    #[test]
    fn stuck_recording_backstop_forces_idle_timeout() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (_call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(5)));

        // Simulate squelch opening → Recording, then time passing with no close.
        sm.calls[0].state = CallState::Recording;
        sm.calls[0].segment_start =
            Instant::now() - STUCK_RECORDING_BACKSTOP - Duration::from_secs(1);

        let effects = sm.tick();

        // Should force an idle timeout (restart cycle).
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::ConventionalRestart { .. })),
            "backstop should trigger restart: {effects:?}"
        );
    }

    #[test]
    fn stuck_recording_backstop_does_not_fire_with_squelch_activity() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (_call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(5)));

        // Recording for longer than backstop, BUT squelch has closed (pending debounce).
        sm.calls[0].state = CallState::Recording;
        sm.calls[0].segment_start =
            Instant::now() - STUCK_RECORDING_BACKSTOP - Duration::from_secs(1);
        sm.calls[0].squelch_closed_at = Some(Instant::now());

        let effects = sm.tick();

        // Backstop should NOT fire — squelch is active (pending close).
        // The debounce finalization may push a segment and transition to Idle,
        // but it should not be from the backstop.
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::ConventionalRestart { .. })),
            "backstop should not fire when squelch has recent activity: {effects:?}"
        );
    }

    #[test]
    fn stuck_recording_backstop_does_not_fire_with_segments() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (_call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(5)));

        // Recording for longer than backstop, BUT has completed segments
        // (meaning squelch has been cycling normally).
        sm.calls[0].state = CallState::Recording;
        sm.calls[0].segment_start =
            Instant::now() - STUCK_RECORDING_BACKSTOP - Duration::from_secs(1);
        sm.calls[0].segments.push(CallSegment {
            start: Instant::now() - Duration::from_secs(60),
            end: Instant::now() - Duration::from_secs(30),
            freq: Freq::from_hz(488_000_000.0),
            rec_num: RecorderNum(5),
        });

        let effects = sm.tick();

        // Backstop should NOT fire — there are completed segments, so squelch
        // has been closing normally. This is just a long call.
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::ConventionalRestart { .. })),
            "backstop should not fire when segments exist: {effects:?}"
        );
    }

    #[test]
    fn stuck_recording_backstop_does_not_fire_before_timeout() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (_call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(5)));

        // Recording but not yet past backstop duration.
        sm.calls[0].state = CallState::Recording;
        // segment_start is fresh (just created) — well within 30 seconds.

        let effects = sm.tick();

        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::ConventionalRestart { .. })),
            "backstop should not fire before timeout: {effects:?}"
        );
    }

    // ── Adaptive squelch integration ──

    #[test]
    fn squelch_events_feed_adaptive_tracker() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            squelch_db: -55.0,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (_call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(5)));

        // Feed bimodal power readings: noise at -50 (just above configured),
        // signal at -35. Otsu should place the threshold above configured,
        // triggering an UpdateSquelch effect.
        let mut got_update = false;
        for i in 0..400 {
            let (muted, power) = if i % 3 == 0 {
                (false, -35.0)
            } else {
                (true, -50.0)
            };
            let effects = sm.on_squelch(RecorderNum(5), muted, power);
            if effects
                .iter()
                .any(|e| matches!(e, Effect::UpdateSquelch { .. }))
            {
                got_update = true;
            }
        }
        assert!(got_update, "should have emitted UpdateSquelch");
    }

    #[test]
    fn digital_channel_has_no_adaptive_tracker() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: false,
            squelch_db: -50.0,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (_call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(5)));

        assert!(sm.calls[0].adaptive_squelch.is_none());

        for i in 0..400 {
            let (muted, power) = if i % 3 == 0 {
                (false, -35.0)
            } else {
                (true, -55.0)
            };
            let effects = sm.on_squelch(RecorderNum(5), muted, power);
            assert!(
                !effects
                    .iter()
                    .any(|e| matches!(e, Effect::UpdateSquelch { .. })),
                "digital channel should never emit UpdateSquelch"
            );
        }
    }

    #[test]
    fn adaptive_squelch_preserved_across_restart() {
        let mut sm = TrunkStateMachine::new(test_config());
        let channel = ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            is_analog: true,
            squelch_db: -50.0,
            ..Default::default()
        };
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, _) = sm.add_conventional_call(&channel, 1, &policy, Some(RecorderNum(5)));

        // Feed some readings to warm up the tracker via squelch events.
        for _ in 0..100 {
            let _ = sm.on_squelch(RecorderNum(5), true, -55.0);
        }

        // Restart should preserve the tracker (not reset it).
        sm.on_conventional_restarted(call_id, Some(RecorderNum(6)));

        assert!(
            sm.calls[0].adaptive_squelch.is_some(),
            "tracker should survive restart"
        );
    }
}
