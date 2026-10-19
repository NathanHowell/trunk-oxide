use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};

use crate::allocation::{self, Diff, Priority, Want};
use crate::clock::Clock;

use trunk_config::{SystemType, TalkgroupMode};

use trunk_radio::{
    Channel, ChannelNum, Encryption, OpenError, Opener, Squelch, TrunkMessage, Trunking, Voice,
};

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
    pub(crate) const fn talkgroup(&self) -> Talkgroup {
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
    /// The kind of channel it records on (empty if it is not recorded).
    pub(crate) fn kind(&self) -> &'static str {
        self.0.recipe.as_ref().map_or("", |r| r.kind())
    }
    /// The rate of the audio its channel records, if it is recorded.
    pub(crate) fn audio_rate(&self) -> Option<f64> {
        self.0.recipe.as_ref().and_then(|r| r.audio_rate())
    }
    /// The unit talking, if known.
    pub(crate) const fn current_source(&self) -> Option<u32> {
        self.0.current_source
    }
    pub(crate) const fn signal(&self) -> Option<f64> {
        self.0.signal
    }
    pub(crate) const fn noise(&self) -> Option<f64> {
        self.0.noise
    }
    pub(crate) const fn service_priority(&self) -> u8 {
        self.0.service_priority
    }
    /// The channel it has, or last had.
    pub(crate) const fn channel(&self) -> Option<ChannelNum> {
        self.0.channel
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
}

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

pub(crate) use trunk_radio::CallId;

static NEXT_CALL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A call id no call has had.
fn next_call_id() -> CallId {
    CallId(NEXT_CALL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// Monotonic call number counter, replacing C++'s `Call::call_counter`.
static NEXT_CALL_NUM: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub(crate) fn next_call_num() -> i64 {
    NEXT_CALL_NUM.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn system_time_to_unix(t: SystemTime) -> i64 {
    crate::clock::unix_seconds(t)
}

/// A talkgroup (or, on a unit-to-unit call, the called unit), as the air
/// names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Talkgroup(pub u32);

impl std::fmt::Display for Talkgroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<Talkgroup> for i64 {
    fn from(tg: Talkgroup) -> Self {
        Self::from(tg.0)
    }
}

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
    /// On its channel, a transmission on.
    Recording,
    /// On its channel, nothing on (a conventional channel between
    /// transmissions; a trunked call between segments).
    Idle,
    /// A signal-detection channel with nothing detected: it wants no
    /// channel.
    Disabled,
    /// Wants a channel; every one is held by a call that outranks it.
    Waiting,
    Rejected(RejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectReason {
    UnknownTg,
    IgnoredTg,
    /// No source covers its channel.
    NoSource,
    /// Its channel could not be opened (see [`OpenError`]).
    NoChannel,
    Encrypted,
    Duplicate,
    Superseded,
}

impl From<&OpenError> for RejectReason {
    fn from(e: &OpenError) -> Self {
        match e {
            OpenError::NoSource { .. } => Self::NoSource,
            OpenError::AlreadyOpen(_) | OpenError::NoSystem(_) | OpenError::NoScramblingMask => {
                Self::NoChannel
            }
        }
    }
}

impl CallState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            CallState::Recording => "Recording",
            CallState::Idle => "Idle",
            CallState::Disabled => "Disabled",
            CallState::Waiting => "Waiting",
            CallState::Rejected(_) => "Rejected",
        }
    }

    pub(crate) const fn to_proto(self) -> i32 {
        use trunk_oxide_grpc::proto::CallState as ProtoCallState;
        match self {
            CallState::Recording => ProtoCallState::Recording as i32,
            CallState::Idle | CallState::Disabled => ProtoCallState::Idle as i32,
            CallState::Waiting => ProtoCallState::Waiting as i32,
            CallState::Rejected(RejectReason::UnknownTg) => {
                ProtoCallState::RejectedUnknownTg as i32
            }
            CallState::Rejected(RejectReason::IgnoredTg) => {
                ProtoCallState::RejectedIgnoredTg as i32
            }
            CallState::Rejected(RejectReason::NoSource) => ProtoCallState::RejectedNoSource as i32,
            CallState::Rejected(RejectReason::NoChannel) => {
                ProtoCallState::RejectedNoChannel as i32
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
            CallState::Waiting => write!(f, "Waiting"),
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
            Self::NoChannel => "no_channel",
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
            RejectReason::NoChannel => write!(f, "NO CHANNEL"),
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
    pub(crate) channel: Option<ChannelNum>,
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

#[derive(Clone, Debug)]
pub struct TalkgroupPolicy {
    pub mode: TalkgroupMode,
    /// Its claim on a channel (lower wins; -1: never recorded).
    pub priority: i32,
    pub preferred_nac: u64,
    /// How its calls are opened.
    pub recipe: Arc<dyn Opener>,
}

#[derive(Clone, Debug)]
pub struct ConventionalChannel {
    pub talkgroup: Talkgroup,
    pub freq: Freq,
    /// Its claim on a channel.
    pub priority: Priority,
    pub signal_detection: bool,
    /// As configured, for the metrics (an analog channel's gate has its
    /// own).
    pub squelch_db: f64,
    /// How it is opened: its kind, tone and squelch.
    pub recipe: Arc<dyn Opener>,
}

#[derive(Clone, Debug, Default)]
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
    /// How its control channels are opened (none on a conventional system).
    pub control_recipe: Option<Arc<dyn Opener>>,
    /// The control channels' claim on a channel.
    pub control_priority: Priority,
    /// An emergency call's claim, when it beats its talkgroup's.
    pub emergency_priority: Priority,
    /// How calls of talkgroups it does not list are opened, at what priority.
    pub default_recipe: Option<Arc<dyn Opener>>,
    pub default_priority: Priority,
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

#[derive(Clone, Debug)]
pub struct CallManagerConfig {
    pub call_timeout: f64,
    pub new_call_from_update: bool,
    pub record_uu_v_calls: bool,
    /// How many channels may be open at once: control channels, trunked
    /// calls and conventional channels alike.
    pub max_channels: usize,
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
}

/// A control channel: a standing claim on a channel for its system.
pub(crate) struct ControlChannel {
    pub(crate) sys_num: i32,
    pub(crate) freq: Freq,
    pub(crate) channel: Option<ChannelNum>,
}

// ── Patch Table ──

/// Talkgroups patched into supergroups, each member with when the control
/// channel last said so.
pub(crate) struct PatchTable {
    patches: HashMap<Talkgroup, HashMap<Talkgroup, Instant>>,
}

impl PatchTable {
    pub(crate) fn new() -> Self {
        Self {
            patches: HashMap::new(),
        }
    }

    /// The supergroup's members (the supergroup itself among them).
    fn members(p: &trunk_radio::Patch) -> impl Iterator<Item = Talkgroup> {
        std::iter::once(p.supergroup)
            .chain(p.groups)
            .filter(|&g| g != 0)
            .map(|g| Talkgroup(u32::from(g)))
    }

    pub(crate) fn update(&mut self, p: &trunk_radio::Patch, now: Instant) {
        let patch = self
            .patches
            .entry(Talkgroup(u32::from(p.supergroup)))
            .or_default();
        for tg in Self::members(p) {
            patch.insert(tg, now);
        }
    }

    pub(crate) fn delete(&mut self, p: &trunk_radio::Patch) {
        let sg = Talkgroup(u32::from(p.supergroup));
        if let Some(patch) = self.patches.get_mut(&sg) {
            for tg in Self::members(p).filter(|&tg| tg != sg) {
                patch.remove(&tg);
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
            let tgids: Vec<u32> = members.keys().map(|t| t.0).collect();
            tracing::info!(supergroup = sg.0, ?tgids, "active patch");
        }
    }

    /// Every talkgroup patched with `talkgroup` (it among them), or none.
    pub(crate) fn get_patch(&self, talkgroup: Talkgroup) -> Vec<Talkgroup> {
        self.patches
            .values()
            .find(|patch| patch.contains_key(&talkgroup))
            .map(|patch| patch.keys().copied().collect())
            .unwrap_or_default()
    }
}

// ── Call Entry ──

pub(crate) struct CallEntry {
    pub(crate) id: CallId,
    pub(crate) call_num: i64,
    pub(crate) talkgroup: Talkgroup,
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
    /// The encryption the voice channel confirmed (ESS `ALGID`, e.g. `0x84`
    /// AES-256, and `KeyID`); `None` until it does (only encrypted calls we
    /// hold no key for).
    pub(crate) encryption: Option<Encryption>,
    /// How its channel is opened, once it is accepted for recording.
    pub(crate) recipe: Option<Arc<dyn Opener>>,
    /// The unit talking, if known.
    pub(crate) current_source: Option<u32>,
    pub(crate) signal: Option<f64>,
    pub(crate) noise: Option<f64>,
    /// The grant's service-option priority (0–7, for the wire): not its
    /// claim on a channel, which is its talkgroup's.
    pub(crate) service_priority: u8,
    /// The channel it has, or last had.
    pub(crate) channel: Option<ChannelNum>,
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
}

impl Default for CallEntry {
    fn default() -> Self {
        Self {
            id: next_call_id(),
            call_num: next_call_num(),
            talkgroup: Talkgroup(0),
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
            encryption: None,
            recipe: None,
            current_source: None,
            signal: None,
            noise: None,
            service_priority: 0,
            channel: None,
            last_update: Instant::now(),
            refreshes: 0,
            ptt_ended: false,
            max_duration: 0.0,
            segments: Vec::new(),
            segment_start: Instant::now(),
            idle_since: None,
            call_mode: CallMode::Conventional,
            signal_detection: false,
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
            CallMode::Conventional => None,
        }
    }

    /// The grant names exactly this call's channel and slot.
    fn on_channel(&self, v: &Voice) -> bool {
        self.freq == v.freq()
            && self.phase2_tdma == v.phase2_tdma()
            && self.tdma_slot == v.tdma_slot()
    }

    /// The grant needs this call's channel: the same frequency, and the same
    /// slot when both are TDMA (a phase 1 grant takes both slots).
    fn shares_channel(&self, v: &Voice) -> bool {
        self.freq == v.freq()
            && (!(self.phase2_tdma && v.phase2_tdma()) || self.tdma_slot == v.tdma_slot())
    }

    /// End the current keyed transmission *without* emitting a boundary: push
    /// the in-progress segment, reset `segment_start`, and transition Recording
    /// → Idle, leaving the idle timeout to conclude the call (a
    /// signal-detection channel's squelch close).
    fn close_segment_and_idle(&mut self, now: Instant) {
        self.push_segment(now);
        self.segment_start = now;
        self.state = CallState::Idle;
        self.idle_since = Some(now);
    }

    /// Close the segment in progress at `now`.
    fn push_segment(&mut self, now: Instant) {
        self.segments.push(CallSegment {
            freq: self.freq,
            start: self.segment_start,
            end: now,
            channel: self.channel,
        });
    }

    /// Start the call's next logical transmission: a fresh `call_num` /
    /// `start_time` and no segments. What it is recorded on is the
    /// caller's business.
    fn rotate_bookkeeping(&mut self, now: Instant) {
        self.call_num = next_call_num();
        self.start_time = now;
        self.segments.clear();
        self.segment_start = now;
        self.ptt_ended = false;
        self.refreshes = 0;
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

    /// The call as the radio opens a channel for it (at its displayed
    /// frequency; the executor applies the clock correction).
    fn radio_call(&self) -> trunk_radio::Call {
        trunk_radio::Call {
            id: self.id,
            sys_num: self.sys_num,
            talkgroup: self.talkgroup.0,
            channel: Channel {
                freq_hz: self.freq.as_hz(),
                slot: self
                    .phase2_tdma
                    .then(|| u8::try_from(self.tdma_slot).unwrap_or(0)),
            },
        }
    }
}

/// A call accepted for recording: how its channel is opened, and its claim.
struct Admission {
    recipe: Arc<dyn Opener>,
    priority: Priority,
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
    /// Channel write/keepalive timeout expired the call.
    WriteTimeout,
    /// A conventional channel's transmission is over.
    Conventional,
    /// Trunked per-speaker boundary: the in-band source (unit) changed mid-call.
    /// A *logical* boundary — the channel keeps running and audio stays
    /// continuous; only the CallEnd/CallStart event pair is emitted.
    SourceChange,
    /// Trunked per-PTT boundary: a control channel grant for a call already
    /// recording, i.e. a new key-up (another handset, or the same one after
    /// the voice channel ended its transmission). Logical, like
    /// [`Self::SourceChange`].
    KeyUp,
    /// The control channel granted the call's channel (and TDMA slot) to
    /// another talkgroup, so the call is over: left running, its channel
    /// would record the new talkgroup until a timeout.
    ChannelReassigned,
    /// A call that outranks it took its channel.
    Evicted,
    /// Process shutdown drained all calls.
    Shutdown,
}

impl CallEndReason {
    /// Ends that close the channel whether or not anyone is talking: with
    /// voice still arriving, the transmission was cut short.
    pub(crate) const fn can_cut_speech(self) -> bool {
        matches!(
            self,
            Self::GrantTimeout
                | Self::IdleTimeout
                | Self::WriteTimeout
                | Self::Superseded
                | Self::ChannelReassigned
                | Self::Evicted
        )
    }

    /// A logical boundary inside a call that goes on, on the same channel.
    pub(crate) const fn is_boundary(self) -> bool {
        matches!(self, Self::SourceChange | Self::KeyUp)
    }

    /// As the wire says it.
    pub(crate) const fn to_proto(self) -> trunk_oxide_grpc::proto::CallEndReason {
        use trunk_oxide_grpc::proto::CallEndReason as P;
        match self {
            Self::GrantTimeout => P::GrantTimeout,
            Self::IdleTimeout => P::IdleTimeout,
            Self::Superseded => P::Superseded,
            Self::WriteTimeout => P::WriteTimeout,
            Self::Conventional => P::Conventional,
            Self::SourceChange => P::SourceChange,
            Self::KeyUp => P::KeyUp,
            Self::ChannelReassigned => P::ChannelReassigned,
            Self::Evicted => P::Evicted,
            Self::Shutdown => P::Shutdown,
        }
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
            Self::Evicted => "evicted",
            Self::Shutdown => "shutdown",
        }
    }
}

/// What ends a transmission inside a call (`cut_transmission`).
///
/// A call is one channel (and one downstream embed) holding an ordered list
/// of transmissions, one continuously keyed speaker each. A cut ends the
/// current one on the wire and starts the next; what differs is who said
/// so, and whether the channel's recording goes with it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Cut {
    /// In band, another unit is talking (trunked). Logical: the recording
    /// runs on, the source is re-routed, the next transmission opens at
    /// once.
    Speaker { source: u32 },
    /// As [`Self::Speaker`], at a new key-up the control channel granted.
    KeyUp { source: Option<u32> },
    /// A conventional channel's carrier dropped: its recording ends with
    /// the transmission and the next starts; the next squelch open begins
    /// the next transmission as its own CallStart.
    Carrier,
}

impl Cut {
    /// The `EmitCallEnd` reason carried by the cut.
    const fn end_reason(self) -> CallEndReason {
        match self {
            Self::Speaker { .. } => CallEndReason::SourceChange,
            Self::KeyUp { .. } => CallEndReason::KeyUp,
            Self::Carrier => CallEndReason::Conventional,
        }
    }
}

#[derive(Debug)]
pub(crate) enum Effect {
    // ── Channels ──
    /// The call (or control channel) was given a channel: open it, then
    /// report with [`TrunkStateMachine::on_opened`].
    Open {
        call_id: CallId,
    },
    /// Its channel goes: close it. What it recorded belongs to the call's
    /// next [`Self::EmitCallEnd`].
    Close {
        call_id: CallId,
    },
    /// End the recording on its channel and start the next, the channel
    /// staying open (a conventional transmission is over). What it
    /// recorded belongs to the next [`Self::EmitCallEnd`].
    Rotate {
        call_id: CallId,
    },
    SetSource {
        call_id: CallId,
        source_id: u32,
    },
    // ── Calls ──
    CreateCall {
        call_id: CallId,
    },
    SetFreq {
        call_id: CallId,
        freq: Freq,
    },
    EmitCallStart {
        call_id: CallId,
    },
    EmitCallEnd {
        call_id: CallId,
        reason: CallEndReason,
    },
    /// Advance the call's bookkeeping to its next transmission (fresh
    /// `call_num`, no segments) with [`TrunkStateMachine::on_next_transmission`]:
    /// a trunked call stays on the air under `source_id`; a conventional
    /// one (`None`) waits for its next squelch open. Ordered after the
    /// closing `EmitCallEnd` (which reads the old `call_num`/segments) and
    /// before any opening `EmitCallStart` (which reads the new one).
    NextTransmission {
        call_id: CallId,
        source: Option<u32>,
    },
    EmitCallsActive,
    EmitSignal {
        call_id: CallId,
        source_id: u32,
    },
    EmitUnitEvent {
        sys_num: i32,
        type_: UnitEventType,
        source_id: u32,
        /// For an affiliation, an answer request or a location.
        talkgroup: Option<Talkgroup>,
    },
    EmitSetupSystem {
        sys_num: i32,
    },
    RemoveCall {
        call_id: CallId,
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

pub(crate) use trunk_radio::UnitEventType;

// ── What the state machine reads off a voice grant or update ──

/// A voice grant or update as the state machine compares it with its calls.
trait VoiceExt {
    fn freq(&self) -> Freq;
    fn tdma_slot(&self) -> i32;
    fn phase2_tdma(&self) -> bool;
    fn tg(&self) -> Talkgroup;
    /// The unit talking, when the message names one.
    fn unit(&self) -> Option<u32>;
}

impl VoiceExt for Voice {
    fn freq(&self) -> Freq {
        Freq::from_hz(self.channel.freq_hz)
    }
    fn tdma_slot(&self) -> i32 {
        self.channel.slot.map_or(0, i32::from)
    }
    fn phase2_tdma(&self) -> bool {
        self.channel.slot.is_some()
    }
    fn tg(&self) -> Talkgroup {
        Talkgroup(self.talkgroup)
    }
    fn unit(&self) -> Option<u32> {
        self.source.filter(|&u| u != 0)
    }
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
    known_alg_ids: HashMap<(i32, Talkgroup), u8>,
    /// The control channels, by the id their claims are made under.
    controls: BTreeMap<CallId, ControlChannel>,
    /// Who wants a channel (calls and control channels).
    wants: BTreeMap<CallId, Want>,
    /// Who has one: the allocation the radio is in (or is being put in by
    /// the effects on their way).
    held: BTreeSet<CallId>,
    /// Every time the state machine reads (see [`crate::clock`]).
    clock: Clock,
}

impl TrunkStateMachine {
    #[cfg(test)]
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
                        sys_id: sp.sys_id,
                        wacn: sp.wacn,
                        nac: sp.nac,
                        rfss: 0,
                        site_id: 0,
                    },
                )
            })
            .collect();
        let mut systems: Vec<_> = config.systems.iter().collect();
        systems.sort_by_key(|(sys_num, _)| **sys_num);
        let mut controls = BTreeMap::new();
        let mut wants = BTreeMap::new();
        let now = clock.now();
        for (&sys_num, sp) in systems {
            if sp.control_recipe.is_none() {
                continue;
            }
            for &freq in &sp.control_channels {
                let id = next_call_id();
                controls.insert(
                    id,
                    ControlChannel {
                        sys_num,
                        freq,
                        channel: None,
                    },
                );
                wants.insert(
                    id,
                    Want {
                        priority: sp.control_priority,
                        since: now,
                    },
                );
            }
        }
        Self {
            clock,
            calls: Vec::new(),
            config,
            patch_tables,
            system_identities,
            known_alg_ids: HashMap::new(),
            controls,
            wants,
            held: BTreeSet::new(),
        }
    }

    // ── Channels ──

    /// Open the control channels (and whatever else has a claim): the
    /// first allocation.
    pub(crate) fn start(&mut self) -> Effects {
        self.settled(Effects::new())
    }

    /// `id` wants a channel at `priority`; a claim it already had keeps its
    /// place in the queue.
    fn want(&mut self, id: CallId, priority: Priority) {
        let since = self.clock.now();
        self.wants
            .entry(id)
            .and_modify(|w| w.priority = priority)
            .or_insert(Want { priority, since });
    }

    /// `id` wants no channel; the one it has is closed.
    fn unwant(&mut self, id: CallId, effects: &mut Effects) {
        self.wants.remove(&id);
        if self.held.remove(&id) {
            effects.push(Effect::Close { call_id: id });
        }
    }

    /// Bring the channels in line with the claims: who falls out of the
    /// allocation is evicted, who enters it is admitted.
    fn settle(&mut self, effects: &mut Effects) {
        let next = allocation::allocate(&self.wants, &self.held, self.config.max_channels);
        let Diff { close, open } = allocation::diff(&self.held, &next);
        self.held = next;
        for id in close {
            self.evict(id, effects);
        }
        for id in open {
            self.admit(id, effects);
        }
    }

    /// `effects`, then whatever settling the channels takes.
    fn settled(&mut self, mut effects: Effects) -> Effects {
        self.settle(&mut effects);
        effects
    }

    /// `id` keeps its claim but lost its channel to one that outranks it.
    fn evict(&mut self, id: CallId, effects: &mut Effects) {
        effects.push(Effect::Close { call_id: id });
        if let Some(cc) = self.controls.get(&id) {
            tracing::warn!(
                sys_num = cc.sys_num, freq = %cc.freq,
                "control channel evicted by calls that outrank it"
            );
            return;
        }
        let Some(entry) = self.calls.iter_mut().find(|e| e.id == id) else {
            return;
        };
        tracing::info!(
            short_name = %entry.short_name,
            talkgroup = entry.talkgroup.0,
            freq = %entry.freq,
            "evicted by a call that outranks it"
        );
        if entry.is_trunked() {
            // The talkgroup's next grant is a new call.
            self.wants.remove(&id);
            effects.push(Effect::EmitCallEnd {
                call_id: id,
                reason: CallEndReason::Evicted,
            });
            effects.push(Effect::RemoveCall { call_id: id });
            effects.push(Effect::EmitCallsActive);
        } else if entry.state == CallState::Recording {
            effects.push(Effect::EmitCallEnd {
                call_id: id,
                reason: CallEndReason::Evicted,
            });
            effects.push(Effect::NextTransmission {
                call_id: id,
                source: None,
            });
        } else {
            entry.state = CallState::Waiting;
            entry.idle_since = None;
        }
    }

    /// `id` was given a channel.
    fn admit(&mut self, id: CallId, effects: &mut Effects) {
        effects.push(Effect::Open { call_id: id });
        let now = self.clock.now();
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == id) {
            entry.segment_start = now;
            entry.idle_since = None;
            entry.state = if entry.is_trunked() {
                CallState::Recording
            } else {
                CallState::Idle
            };
        }
    }

    /// What the executor opens for `id`: how, and for which call (at its
    /// displayed frequency).
    pub(crate) fn opening(&self, id: CallId) -> Option<(Arc<dyn Opener>, trunk_radio::Call)> {
        if let Some(cc) = self.controls.get(&id) {
            let recipe = self
                .config
                .systems
                .get(&cc.sys_num)?
                .control_recipe
                .clone()?;
            return Some((
                recipe,
                trunk_radio::Call {
                    id,
                    sys_num: cc.sys_num,
                    talkgroup: 0,
                    channel: Channel {
                        freq_hz: cc.freq.as_hz(),
                        slot: None,
                    },
                },
            ));
        }
        let entry = self.get_call(id)?;
        Some((entry.recipe.clone()?, entry.radio_call()))
    }

    /// The executor opened `id`'s channel, or could not.
    pub(crate) fn on_opened(
        &mut self,
        id: CallId,
        opened: Result<ChannelNum, OpenError>,
    ) -> Effects {
        let error = match opened {
            Ok(num) => {
                if let Some(cc) = self.controls.get_mut(&id) {
                    cc.channel = Some(num);
                    tracing::info!(sys_num = cc.sys_num, freq = %cc.freq, channel = %num, "control channel decoder started");
                } else if let Some(entry) = self.calls.iter_mut().find(|e| e.id == id) {
                    entry.channel = Some(num);
                    tracing::info!(
                        short_name = %entry.short_name,
                        channel = %num,
                        talkgroup = entry.talkgroup.0,
                        freq = %entry.freq,
                        tdma = entry.phase2_tdma,
                        slot = entry.tdma_slot,
                        kind = entry.recipe.as_ref().map_or("", |r| r.kind()),
                        "starting recorder"
                    );
                }
                return Effects::new();
            }
            Err(e) => e,
        };
        // Nothing was opened: give the claim (and its place) up.
        self.wants.remove(&id);
        self.held.remove(&id);
        if let Some(cc) = self.controls.remove(&id) {
            tracing::warn!(sys_num = cc.sys_num, freq = %cc.freq, %error, "cannot open the control channel; skipping");
        } else if let Some(entry) = self.calls.iter_mut().find(|e| e.id == id) {
            tracing::error!(
                talkgroup = entry.talkgroup.0,
                sys_num = entry.sys_num,
                freq = %entry.freq,
                %error,
                "not recording: cannot open the channel"
            );
            entry.state = CallState::Rejected(RejectReason::from(&error));
        }
        self.settled(Effects::new())
    }

    /// End call `id` now: its channel closes, the wire hears why, and it
    /// is forgotten.
    fn end_call(&mut self, id: CallId, reason: CallEndReason, effects: &mut Effects) {
        self.unwant(id, effects);
        effects.push(Effect::EmitCallEnd {
            call_id: id,
            reason,
        });
        effects.push(Effect::RemoveCall { call_id: id });
    }

    // ── Top-level message dispatch ──

    pub(crate) fn on_message(&mut self, msg: &TrunkMessage) -> Effects {
        let now = self.clock.now();
        let sys_num = msg.sys_num;
        let unit_event = |type_, source_id: u32, talkgroup: Option<Talkgroup>| -> Effects {
            Effect::EmitUnitEvent {
                sys_num,
                type_,
                source_id,
                talkgroup,
            }
            .into()
        };
        let group = |g: u16| Some(Talkgroup(u32::from(g)));
        let effects = match msg.kind {
            // Another control channel of the site: decoded only if
            // configured; the wire hears of it with the message itself.
            Trunking::Invalid
            | Trunking::Unknown
            | Trunking::Tdulc
            | Trunking::ControlChannel { .. } => Effects::new(),

            Trunking::PatchAdd(p) => {
                if let Some(pt) = self.patch_tables.get_mut(&sys_num) {
                    pt.update(&p, now);
                }
                Effects::new()
            }
            Trunking::PatchDelete(p) => {
                if let Some(pt) = self.patch_tables.get_mut(&sys_num) {
                    pt.delete(&p);
                }
                Effects::new()
            }

            Trunking::Site { rfss, site, .. } => {
                if let Some(id) = self.system_identities.get_mut(&sys_num) {
                    id.rfss = i32::from(rfss);
                    id.site_id = i32::from(site);
                }
                Effect::EmitSetupSystem { sys_num }.into()
            }
            Trunking::Network { wacn, sys_id, .. } => {
                if let Some(id) = self.system_identities.get_mut(&sys_num) {
                    id.sys_id = u64::from(sys_id);
                    id.wacn = u64::from(wacn);
                    id.nac = u64::from(msg.nac);
                }
                Effect::EmitSetupSystem { sys_num }.into()
            }

            // Unit events (stateless)
            Trunking::Registration { unit } => unit_event(UnitEventType::Registration, unit, None),
            Trunking::Deregistration { unit } => {
                unit_event(UnitEventType::Deregistration, unit, None)
            }
            Trunking::Affiliation { group: g, unit } => {
                unit_event(UnitEventType::Affiliation, unit, group(g))
            }
            Trunking::Acknowledge { unit, .. } => {
                unit_event(UnitEventType::Acknowledge, unit, None)
            }
            Trunking::Location { group: g, unit } => {
                unit_event(UnitEventType::Location, unit, group(g))
            }
            Trunking::DataGrant { unit, .. } => unit_event(UnitEventType::DataGrant, unit, None),
            Trunking::AnswerRequest(v) => unit_event(
                UnitEventType::AnswerRequest,
                v.source.unwrap_or(0),
                Some(v.tg()),
            ),

            // Call lifecycle
            Trunking::Grant(v) => self.on_grant(sys_num, msg.nac, &v, true),
            Trunking::Update(v) => {
                if self.config.new_call_from_update {
                    self.on_grant(sys_num, msg.nac, &v, false)
                } else {
                    self.on_update(sys_num, &v)
                }
            }
            Trunking::UnitGrant(v) => {
                if self.config.record_uu_v_calls {
                    self.on_grant(sys_num, msg.nac, &v, true)
                } else {
                    Effects::new()
                }
            }
            Trunking::UnitUpdate(v) => {
                if self.config.record_uu_v_calls {
                    self.on_update(sys_num, &v)
                } else {
                    Effects::new()
                }
            }
        };
        self.settled(effects)
    }

    // ── Grant handling ──

    /// Ends the calls whose channel a grant gives to another talkgroup. The
    /// site only reassigns a channel once the call on it has ended, so these
    /// calls are over; left alone, their channels would record the new
    /// talkgroup until a timeout. Talkgroups patched together share a channel
    /// legitimately and are left alone.
    fn end_reassigned_calls(&mut self, sys_num: i32, v: &Voice, effects: &mut Effects) {
        let now = self.clock.now();
        let patched = self
            .patch_tables
            .get(&sys_num)
            .map(|pt| pt.get_patch(v.tg()))
            .unwrap_or_default();
        let ended: Vec<CallId> = self
            .calls
            .iter()
            .filter(|entry| {
                entry.is_trunked()
                    && entry.sys_num == sys_num
                    && entry.talkgroup != v.tg()
                    && entry.shares_channel(v)
                    && !patched.contains(&entry.talkgroup)
            })
            .inspect(|entry| {
                tracing::info!(
                    short_name = %entry.short_name,
                    talkgroup = entry.talkgroup.0,
                    to_talkgroup = v.tg().0,
                    freq = %entry.freq,
                    slot = entry.tdma_slot,
                    since_update_s = now.saturating_duration_since(entry.last_update).as_secs_f64(),
                    "channel granted to another talkgroup: ending call"
                );
            })
            .map(|entry| entry.id)
            .collect();
        for &id in &ended {
            self.end_call(id, CallEndReason::ChannelReassigned, effects);
        }
        if !ended.is_empty() {
            effects.push(Effect::EmitCallsActive);
        }
    }

    /// A grant (or, with `new_call_from_update`, an update) of a voice
    /// channel on system `sys_num`, heard with `nac`.
    fn on_grant(&mut self, sys_num: i32, nac: u16, v: &Voice, is_grant_message: bool) -> Effects {
        let now = self.clock.now();
        let mut effects = Effects::new();
        if is_grant_message {
            self.end_reassigned_calls(sys_num, v, &mut effects);
        }
        let source = v.unit();

        // A talkgroup has one call per system. A grant on another channel or
        // TDMA slot moves it there (the retune below), rather than starting a
        // second call and leaving the first running on the old channel.
        let existing_idx = self
            .calls
            .iter()
            .position(|entry| entry.talkgroup == v.tg() && entry.sys_num == sys_num);

        if let Some(idx) = existing_idx {
            let same_channel = self.calls[idx].on_channel(v);
            if same_channel {
                let entry = &mut self.calls[idx];

                // A grant for a call already recording is a new key-up: from
                // another handset (the grant names it), or from the same one
                // after the voice channel ended its transmission. Each is its
                // own transmission. The grant usually beats the in-band source
                // change, which then matches and is ignored, so cut here.
                let other_handset = source.is_some() && source != entry.current_source;
                if is_grant_message
                    && entry.is_trunked()
                    && entry.state == CallState::Recording
                    && (other_handset || entry.ptt_ended)
                    && now.saturating_duration_since(entry.segment_start) >= MIN_TRANSMISSION
                {
                    let call_id = entry.id;
                    let unit = source.or(entry.current_source);
                    entry.current_source = unit;
                    tracing::info!(
                        short_name = %entry.short_name,
                        talkgroup = entry.talkgroup.0,
                        source = ?unit,
                        other_handset,
                        "new key-up on a recording call: cutting per-PTT transmission boundary"
                    );
                    effects.extend(self.cut_transmission(call_id, Cut::KeyUp { source: unit }));
                    return effects;
                }

                // Source changed: re-route to the new source (SetSource +
                // EmitSignal inside update_call_source). Do NOT emit
                // EmitCallStart — this is the same logical call already in
                // progress, and a duplicate call_start orphans/duplicates the
                // downstream embed (it is keyed by talkgroup, not call id).
                self.update_call_source(idx, source, &mut effects);
            } else {
                self.retune(idx, sys_num, v, &mut effects);
            }
            return effects;
        }

        // New call
        let site = self.system_identities.get(&sys_num);
        let (wacn, rfss, site_id) = site.map_or((0, 0, 0), |s| (s.wacn, s.rfss, s.site_id));
        let multisite = self.check_multisite(v.tg(), sys_num, nac, wacn, rfss, site_id);
        let policy = self.config.systems.get(&sys_num).unwrap_or(&DEFAULT_POLICY);

        let new_entry = CallEntry {
            start_time: now,
            last_update: now,
            segment_start: now,
            talkgroup: v.tg(),
            sys_num,
            short_name: policy.short_name.clone(),
            tdma_slot: v.tdma_slot(),
            phase2_tdma: v.phase2_tdma(),
            mode: v.options.packet,
            duplex: v.options.duplex,
            emergency: v.options.emergency,
            freq: v.freq(),
            encrypted: v.options.encrypted,
            current_source: source,
            service_priority: v.options.priority,
            max_duration: policy.max_duration,
            call_mode: CallMode::Trunked(TrunkedFields {
                multi_site: policy.multi_site,
                wacn,
                rfss,
                site_id,
                multi_site_name: policy.multi_site_name.clone(),
                conversation_mode: policy.conversation_mode,
            }),
            ..Default::default()
        };
        let call_id = new_entry.id;

        self.calls.push(new_entry);
        let new_idx = self.calls.len() - 1;

        effects.push(Effect::CreateCall { call_id });

        if multisite.is_duplicate && !multisite.is_superseding {
            self.calls[new_idx].state = CallState::Rejected(RejectReason::Duplicate);
        } else {
            let assessed = self.assess(new_idx, sys_num, v);
            if assessed.is_ok() && !is_grant_message {
                tracing::info!(talkgroup = v.tg().0, sys_num, "this was an UPDATE");
            }
            self.claim(new_idx, assessed, &mut effects);
            // Settle now, so the call is on its channel (or waiting) when
            // the wire hears it start.
            self.settle(&mut effects);
            if multisite.is_superseding
                && self.held.contains(&call_id)
                && let Some(orig_idx) = multisite.original_idx
            {
                let orig = &mut self.calls[orig_idx];
                orig.state = CallState::Rejected(RejectReason::Superseded);
                let orig_id = orig.id;
                self.end_call(orig_id, CallEndReason::Superseded, &mut effects);
            }
        }

        effects.push(Effect::EmitCallStart { call_id });
        effects.push(Effect::EmitCallsActive);

        effects
    }

    /// Move the call at `idx` to the channel (or slot) `v` names. A call on
    /// a channel keeps its place: its old channel closes and its new one
    /// opens under the same claim.
    fn retune(&mut self, idx: usize, sys_num: i32, v: &Voice, effects: &mut Effects) {
        let now = self.clock.now();
        let entry = &mut self.calls[idx];
        let id = entry.id;
        if entry.state == CallState::Recording {
            entry.push_segment(now);
        }
        entry.segment_start = now;
        entry.idle_since = None;
        entry.freq = v.freq();
        entry.tdma_slot = v.tdma_slot();
        entry.phase2_tdma = v.phase2_tdma();
        // Refresh the tracked source for the new freq so the next grant
        // on it isn't misread as a source change (which would emit a
        // spurious SetSource/EmitSignal).
        entry.current_source = v.unit();
        entry.last_update = now;
        let was_held = self.held.contains(&id);
        if was_held {
            effects.push(Effect::Close { call_id: id });
        }
        effects.push(Effect::SetFreq {
            call_id: id,
            freq: v.freq(),
        });
        let assessed = self.assess(idx, sys_num, v);
        match (was_held, assessed) {
            (true, Ok(a)) => {
                let entry = &mut self.calls[idx];
                entry.recipe = Some(a.recipe);
                entry.state = CallState::Recording;
                self.want(id, a.priority);
                effects.push(Effect::Open { call_id: id });
            }
            (true, Err(reason)) => {
                // Closed above.
                self.calls[idx].state = CallState::Rejected(reason);
                self.wants.remove(&id);
                self.held.remove(&id);
            }
            (false, assessed) => self.claim(idx, assessed, effects),
        }
    }

    // ── Update handling ──

    fn on_update(&mut self, sys_num: i32, v: &Voice) -> Effects {
        let now = self.clock.now();
        let mut effects = Effects::new();

        for entry in &mut self.calls {
            if entry.talkgroup == v.tg() && entry.sys_num == sys_num && entry.on_channel(v) {
                if v.options.encrypted {
                    entry.encrypted = true;
                }

                // Source changed: re-route only (SetSource + EmitSignal). No
                // EmitCallStart for an already-started call — see on_grant.
                Self::update_call_source_entry(entry, v.unit(), &mut effects, now);
            }
        }

        effects
    }

    // ── Source tracking ──

    fn update_call_source(
        &mut self,
        idx: usize,
        source: Option<u32>,
        effects: &mut Effects,
    ) -> bool {
        let now = self.clock.now();
        let entry = &mut self.calls[idx];
        Self::update_call_source_entry(entry, source, effects, now)
    }

    fn update_call_source_entry(
        entry: &mut CallEntry,
        source: Option<u32>,
        effects: &mut Effects,
        now: Instant,
    ) -> bool {
        entry.last_update = now;
        entry.refreshes += 1;

        let Some(source) = source.filter(|&u| Some(u) != entry.current_source) else {
            return false;
        };

        entry.current_source = Some(source);
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
    /// op25 on the voice channel (keyed by its [`CallId`]). The
    /// ESS is authoritative, so this also marks the call encrypted. Unknown
    /// calls are ignored.
    pub(crate) fn on_crypto(&mut self, call_id: CallId, encryption: Encryption) {
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            entry.encrypted = true;
            entry.encryption = Some(encryption);
            self.known_alg_ids
                .insert((entry.sys_num, entry.talkgroup), encryption.algid);
        }
    }

    /// The call's own decoded `ALGID`; for a call rejected as encrypted at
    /// the grant (never recorded, so none of its own), the last one seen on
    /// its talkgroup. A clear call has none, whatever its talkgroup did before.
    pub(crate) fn alg_id_for(&self, call_id: CallId) -> Option<u8> {
        let entry = self.calls.iter().find(|e| e.id == call_id)?;
        entry.encryption.map(|e| e.algid).or_else(|| {
            (entry.state == CallState::Rejected(RejectReason::Encrypted))
                .then(|| {
                    self.known_alg_ids
                        .get(&(entry.sys_num, entry.talkgroup))
                        .copied()
                })
                .flatten()
        })
    }

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

    /// Handle an in-band source (unit) change decoded on the voice channel
    /// (keyed by its [`CallId`]). Unlike the control-channel source updates
    /// in [`Self::update_call_source_entry`], this cuts a real per-speaker
    /// **transmission boundary** so each unit's audio becomes its own
    /// CallStart/CallEnd pair, on a channel that runs on (the [`CallId`] is
    /// unchanged so audio routing and Discord coalescing carry across).
    pub(crate) fn on_source_change(&mut self, call_id: CallId, source: u32) -> Effects {
        let now = self.clock.now();
        let mut effects = Effects::new();
        let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) else {
            return effects;
        };

        // Only trunked, only while actively recording. Ignore no-op repeats:
        // the tag re-fires every superframe, and the control channel may have
        // already moved `current_source` — dedup against it so we never emit
        // a double boundary.
        if !entry.is_trunked()
            || entry.state != CallState::Recording
            || source == 0
            || entry.current_source == Some(source)
        {
            return effects;
        }

        // Below the minimum transmission length, treat it as a plain re-route
        // (SetSource + EmitSignal) rather than fragmenting rapid A↔B↔A hand-offs
        // into sub-second transmissions.
        if now.saturating_duration_since(entry.segment_start) < MIN_TRANSMISSION {
            Self::update_call_source_entry(entry, Some(source), &mut effects, now);
            return effects;
        }

        // Real per-speaker boundary: close the current transmission and open
        // the next on the same channel. Set `current_source` now so a
        // following control-channel update for the same unit dedups.
        entry.current_source = Some(source);
        tracing::info!(
            short_name = %entry.short_name,
            talkgroup = entry.talkgroup.0,
            source,
            "in-band source change: cutting per-speaker transmission boundary"
        );
        self.cut_transmission(call_id, Cut::Speaker { source })
    }

    /// Cut a transmission boundary: end the current transmission on the wire
    /// and start the next, the one verb shared by trunked per-speaker and
    /// per-PTT boundaries and conventional keyed transmissions; the
    /// [`Cut`] is all that differs.
    ///
    /// Effect ordering is the load-bearing coalescing contract — `EmitCallEnd`
    /// MUST precede `NextTransmission` (the executor reads the old
    /// `call_num`/segments while concluding) and any `EmitCallStart`; a
    /// `Rotate` precedes them both, so the recording's held audio goes out
    /// before the end. Reordering corrupts the downstream embed with no
    /// compile error.
    fn cut_transmission(&mut self, call_id: CallId, cut: Cut) -> Effects {
        let mut effects = Effects::new();
        let end = Effect::EmitCallEnd {
            call_id,
            reason: cut.end_reason(),
        };
        match cut {
            Cut::Speaker { source: unit } | Cut::KeyUp { source: Some(unit) } => {
                effects.push(end);
                effects.push(Effect::NextTransmission {
                    call_id,
                    source: Some(unit),
                });
                effects.push(Effect::SetSource {
                    call_id,
                    source_id: unit,
                });
                effects.push(Effect::EmitSignal {
                    call_id,
                    source_id: unit,
                });
                effects.push(Effect::EmitCallStart { call_id });
            }
            Cut::KeyUp { source: None } => {
                effects.push(end);
                effects.push(Effect::NextTransmission {
                    call_id,
                    source: None,
                });
                effects.push(Effect::EmitCallStart { call_id });
            }
            Cut::Carrier => {
                effects.push(Effect::Rotate { call_id });
                effects.push(end);
                effects.push(Effect::NextTransmission {
                    call_id,
                    source: None,
                });
            }
        }
        effects
    }

    /// Apply an [`Effect::NextTransmission`]: a fresh `call_num`, no
    /// segments. A trunked call stays on the air under the new speaker (if
    /// known); a conventional one waits for its next transmission: on its
    /// channel if it has one, for one if it wants one, else disabled.
    pub(crate) fn on_next_transmission(&mut self, call_id: CallId, source: Option<u32>) {
        let now = self.clock.now();
        let (wants, holds) = (
            self.wants.contains_key(&call_id),
            self.held.contains(&call_id),
        );
        let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) else {
            return;
        };
        entry.rotate_bookkeeping(now);
        match &entry.call_mode {
            CallMode::Trunked(_) => entry.current_source = source.or(entry.current_source),
            CallMode::Conventional => {
                entry.last_update = now;
                entry.idle_since = None;
                entry.state = match (wants, holds) {
                    (false, _) => CallState::Disabled,
                    (true, true) => CallState::Idle,
                    (true, false) => CallState::Waiting,
                };
            }
        }
    }

    // ── Recording policy ──

    /// Whether the call at `entry_idx` on `v` is recorded: how and at what
    /// priority, or why not.
    fn assess(
        &mut self,
        entry_idx: usize,
        sys_num: i32,
        v: &Voice,
    ) -> Result<Admission, RejectReason> {
        let tg = v.tg();

        let Some(sys_policy) = self.config.systems.get(&sys_num) else {
            return Err(RejectReason::UnknownTg);
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
                if !sys_policy.hide_unknown {
                    tracing::info!(
                        talkgroup = tg.0,
                        sys_num,
                        "not recording: TG not in talkgroup file"
                    );
                }
                return Err(RejectReason::UnknownTg);
            }
        }

        // 2. Encryption check
        let is_encrypted =
            v.options.encrypted || tg_policy.is_some_and(|tg| tg.mode.is_encrypted());
        if is_encrypted {
            entry.encrypted = true;
            if !sys_policy.monitor_encrypted {
                if !sys_policy.hide_encrypted {
                    tracing::info!(
                        talkgroup = tg.0,
                        sys_num,
                        freq = %v.freq(),
                        slot = v.tdma_slot(),
                        "not recording: encrypted"
                    );
                }
                return Err(RejectReason::Encrypted);
            }
        }

        // 3. Its claim: the talkgroup's, or the best of a patch's members.
        let priority = tg_policy.map_or(sys_policy.default_priority, |tgp| {
            let mut p = tgp.priority;
            for ptg in &patch_tgs {
                if let Some(ptgp) = sys_policy.talkgroups.get(ptg)
                    && ptgp.priority >= 0
                    && ptgp.priority < p
                {
                    p = ptgp.priority;
                }
            }
            Priority(p)
        });

        // 4. Priority -1 means never.
        if priority == Priority(-1) {
            tracing::info!(
                talkgroup = tg.0,
                sys_num,
                "not recording: priority disabled (-1)"
            );
            return Err(RejectReason::IgnoredTg);
        }

        // 5. An emergency claims at the emergency priority, if that is better.
        let priority = if entry.emergency {
            priority.min(sys_policy.emergency_priority)
        } else {
            priority
        };

        let recipe = tg_policy
            .map(|t| Arc::clone(&t.recipe))
            .or_else(|| sys_policy.default_recipe.clone())
            .ok_or(RejectReason::NoChannel)?;
        Ok(Admission { recipe, priority })
    }

    /// Apply an assessment to the call at `idx`: a claim on a channel, or a
    /// rejection.
    fn claim(
        &mut self,
        idx: usize,
        assessed: Result<Admission, RejectReason>,
        effects: &mut Effects,
    ) {
        let id = self.calls[idx].id;
        match assessed {
            Ok(a) => {
                let entry = &mut self.calls[idx];
                entry.recipe = Some(a.recipe);
                if !self.held.contains(&id) {
                    entry.state = CallState::Waiting;
                }
                self.want(id, a.priority);
            }
            Err(reason) => {
                self.calls[idx].state = CallState::Rejected(reason);
                self.unwant(id, effects);
            }
        }
    }

    // ── Multisite duplicate detection (pure) ──

    /// Whether a new call on `sys_num` (a site of `wacn`, `rfss`/`site_id`,
    /// heard with `nac`) duplicates or supersedes one on another site.
    #[allow(clippy::cast_sign_loss)]
    fn check_multisite(
        &self,
        talkgroup: Talkgroup,
        sys_num: i32,
        nac: u16,
        wacn: u64,
        rfss: i32,
        site_id: i32,
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
                if trunked.wacn != wacn {
                    continue;
                }

                let call_rfss_site = trunked.rfss as u64 * 10000 + trunked.site_id as u64;
                let msg_rfss_site = rfss as u64 * 10000 + site_id as u64;

                if msg_rfss_site == call_rfss_site {
                    continue;
                }

                result.is_duplicate = true;
                result.original_idx = Some(idx);

                let call_preferred_nac = self.get_preferred_nac(entry.sys_num, talkgroup);
                if call_preferred_nac != call_rfss_site && u64::from(nac) == msg_rfss_site {
                    result.is_superseding = true;
                }
            } else {
                let msg_multi_site_name = self
                    .config
                    .systems
                    .get(&sys_num)
                    .map_or("", |p| p.multi_site_name.as_str());
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

        let stale: Vec<(CallId, CallState)> = self
            .calls
            .iter()
            .filter(|e| {
                !e.is_conventional()
                    && now.saturating_duration_since(e.last_update).as_secs_f64() > timeout
            })
            .map(|e| (e.id, e.state))
            .collect();
        for &(id, state) in &stale {
            let reason = if state == CallState::Recording {
                CallEndReason::GrantTimeout
            } else {
                CallEndReason::IdleTimeout
            };
            self.end_call(id, reason, &mut effects);
        }
        if !stale.is_empty() {
            effects.push(Effect::EmitCallsActive);
        }

        effects.extend(self.tick_conventional());
        self.settled(effects)
    }

    fn tick_conventional(&mut self) -> Effects {
        let now = self.clock.now();
        let timeout = self.config.call_timeout;
        let timed_out: Vec<CallId> = self
            .calls
            .iter()
            .filter(|entry| {
                if !entry.is_conventional() || !self.held.contains(&entry.id) {
                    return false;
                }
                let idle_timed_out = entry
                    .idle_since
                    .is_some_and(|t| now.saturating_duration_since(t).as_secs_f64() > timeout);
                let elapsed = now
                    .saturating_duration_since(entry.start_time)
                    .as_secs_f64();
                let max_duration_exceeded =
                    entry.max_duration > 0.0 && elapsed > entry.max_duration;
                // Safety backstop: recording continuously without any squelch
                // close event, the noise floor is likely above the threshold.
                let stuck_recording = entry.state == CallState::Recording
                    && entry.segments.is_empty()
                    && now.saturating_duration_since(entry.segment_start)
                        > STUCK_RECORDING_BACKSTOP;
                idle_timed_out || max_duration_exceeded || stuck_recording
            })
            .map(|entry| entry.id)
            .collect();

        let mut effects = Effects::new();
        for call_id in timed_out {
            effects.extend(self.on_conventional_idle_timeout(call_id));
        }
        effects
    }

    /// A conventional channel's time is up: a signal-detection channel
    /// concludes its detection and gives its channel up; an always-on one
    /// (only here through the stuck-recording and max-duration backstops)
    /// concludes its transmission like any other.
    pub(crate) fn on_conventional_idle_timeout(&mut self, call_id: CallId) -> Effects {
        let Some(entry) = self.calls.iter().find(|e| e.id == call_id) else {
            return Effects::new();
        };
        if !entry.signal_detection {
            return self.cut_transmission(call_id, Cut::Carrier);
        }
        let recorded = !entry.segments.is_empty() || entry.state == CallState::Recording;
        let mut effects = Effects::new();
        self.unwant(call_id, &mut effects);
        if recorded {
            effects.push(Effect::EmitCallEnd {
                call_id,
                reason: CallEndReason::Conventional,
            });
        }
        effects.push(Effect::NextTransmission {
            call_id,
            source: None,
        });
        effects
    }

    // ── Squelch handling ──

    /// A conventional channel's transmission detector opened or closed. A
    /// close ends the keyed transmission at once: on an always-on channel
    /// it concludes the call (one CallStart/CallEnd per key-up, like a
    /// trunked grant); on a signal-detection channel it closes the segment
    /// and lets the idle timeout conclude the detection cycle. The detectors
    /// have their own hold times, so no debounce is needed here.
    pub(crate) fn on_squelch(
        &mut self,
        call_id: CallId,
        squelch: Squelch,
        power_db: Option<f64>,
    ) -> Effects {
        let now = self.clock.now();
        let Some(entry) = self
            .calls
            .iter_mut()
            .find(|e| e.id == call_id && e.is_conventional())
        else {
            return Effects::new();
        };
        let mut effects = Effects::new();
        match squelch {
            Squelch::Close(_) => {
                entry.noise = power_db;
                if entry.state == CallState::Recording {
                    if entry.signal_detection {
                        entry.close_segment_and_idle(now);
                    } else {
                        effects.extend(self.cut_transmission(call_id, Cut::Carrier));
                    }
                } else if entry.idle_since.is_none() {
                    entry.idle_since = Some(now);
                }
            }
            Squelch::Open => {
                entry.signal = power_db;
                entry.idle_since = None;
                if entry.state == CallState::Idle {
                    let first_open = entry.segments.is_empty();
                    entry.state = CallState::Recording;
                    entry.segment_start = now;
                    if first_open {
                        effects.push(Effect::EmitCallStart { call_id });
                        effects.push(Effect::EmitCallsActive);
                    }
                }
            }
        }
        effects
    }

    // ── Signal detection ──

    pub(crate) fn on_signals_detected(
        &mut self,
        source_center_freq: f64,
        signals: &[trunk_radio::DetectedSignal],
    ) -> Effects {
        let mut detected = Vec::new();
        for signal in signals {
            let abs_freq = source_center_freq + signal.center_freq;
            for entry in &mut self.calls {
                if entry.is_conventional()
                    && entry.signal_detection
                    && entry.state == CallState::Disabled
                    && (entry.freq.as_hz() - abs_freq).abs() < 30_000.0
                {
                    entry.state = CallState::Waiting;
                    entry.idle_since = None;
                    detected.push(entry.id);
                }
            }
        }
        for id in detected {
            let priority = self.conventional_priority(id);
            self.want(id, priority);
        }
        self.settled(Effects::new())
    }

    /// A conventional channel's claim on a channel.
    fn conventional_priority(&self, id: CallId) -> Priority {
        self.get_call(id)
            .and_then(|e| {
                self.config
                    .systems
                    .get(&e.sys_num)?
                    .channels
                    .iter()
                    .find(|c| c.talkgroup == e.talkgroup && c.freq == e.freq)
            })
            .map_or(Priority::UNSET, |c| c.priority)
    }

    // ── Conventional call addition ──

    /// A conventional channel's call: it claims a channel at once, or (with
    /// signal detection) once something is detected on it.
    pub(crate) fn add_conventional_call(
        &mut self,
        channel: &ConventionalChannel,
        sys_num: i32,
        policy: &SystemPolicy,
    ) -> (CallId, Effects) {
        let now = self.clock.now();
        let entry = CallEntry {
            start_time: now,
            last_update: now,
            segment_start: now,
            talkgroup: channel.talkgroup,
            sys_num,
            short_name: policy.short_name.clone(),
            freq: channel.freq,
            recipe: Some(Arc::clone(&channel.recipe)),
            max_duration: policy.max_duration,
            state: if channel.signal_detection {
                CallState::Disabled
            } else {
                CallState::Waiting
            },
            signal_detection: channel.signal_detection,
            ..Default::default()
        };
        let call_id = entry.id;
        self.calls.push(entry);
        if !channel.signal_detection {
            self.want(call_id, channel.priority);
        }
        (call_id, self.settled(Effects::new()))
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
                .map_or("unknown", |sp| sp.short_name.as_str());
            pt.log_patches(short_name);
        }
    }

    // ── Conclude all (shutdown) ──

    pub(crate) fn conclude_all(&mut self) -> Effects {
        let mut effects = Effects::new();
        let calls: Vec<(CallId, CallState)> = self.calls.iter().map(|e| (e.id, e.state)).collect();
        for (id, state) in calls {
            self.unwant(id, &mut effects);
            if matches!(
                state,
                CallState::Recording | CallState::Idle | CallState::Disabled
            ) {
                effects.push(Effect::EmitCallEnd {
                    call_id: id,
                    reason: CallEndReason::Shutdown,
                });
            }
            effects.push(Effect::RemoveCall { call_id: id });
        }
        let controls: Vec<CallId> = self.controls.keys().copied().collect();
        for id in controls {
            self.unwant(id, &mut effects);
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

    /// The control channel `id` names, if it is one.
    pub(crate) fn control(&self, id: CallId) -> Option<&ControlChannel> {
        self.controls.get(&id)
    }

    /// Channels held, of `max_channels`.
    pub(crate) fn channels_held(&self) -> usize {
        self.held.len()
    }

    /// Calls (and control channels) waiting for a channel.
    pub(crate) fn waiting(&self) -> usize {
        self.wants.len().saturating_sub(self.held.len())
    }

    /// A power reading from the call's channel as it closed: the signal, or
    /// the noise floor if nothing was on.
    pub(crate) fn feed_channel_power(
        &mut self,
        call_id: CallId,
        power_db: Option<f64>,
        was_idle: bool,
    ) {
        if let Some(entry) = self.calls.iter_mut().find(|e| e.id == call_id) {
            if was_idle {
                entry.noise = power_db;
            } else {
                entry.signal = power_db;
            }
        }
    }

    /// Expire a trunked call whose channel stopped writing audio.
    pub(crate) fn expire_call(&mut self, call_id: CallId) -> Effects {
        let mut effects = Effects::new();
        let live = self
            .get_call(call_id)
            .is_some_and(|e| matches!(e.state, CallState::Recording | CallState::Idle));
        self.unwant(call_id, &mut effects);
        if live {
            effects.push(Effect::EmitCallEnd {
                call_id,
                reason: CallEndReason::WriteTimeout,
            });
        }
        effects.push(Effect::RemoveCall { call_id });
        effects.push(Effect::EmitCallsActive);
        self.settled(effects)
    }

    /// Forget the call (its channel is already closed, its end sent).
    pub(crate) fn remove_call(&mut self, call_id: CallId) {
        debug_assert!(
            !self.held.contains(&call_id),
            "removing a call on a channel"
        );
        self.calls.retain(|e| e.id != call_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunk_radio::{
        AnalogFm, AnalogParams, CloseReason, Keying, Options, P25Control, P25Voice, Recipe,
    };

    fn p25() -> Arc<dyn Opener> {
        Recipe::<P25Voice>::opener(Keying::Trunked)
    }

    fn talkgroup(priority: i32) -> TalkgroupPolicy {
        TalkgroupPolicy {
            mode: TalkgroupMode::default(),
            priority,
            preferred_nac: 0,
            recipe: p25(),
        }
    }

    /// System 1: unknown talkgroups recorded at priority 5; talkgroups 1
    /// (dispatch) and 3 (public works) listed.
    fn test_config() -> CallManagerConfig {
        let mut systems = HashMap::new();
        systems.insert(
            1,
            SystemPolicy {
                short_name: "test".into(),
                record_unknown: true,
                default_recipe: Some(p25()),
                default_priority: Priority(5),
                emergency_priority: Priority(0),
                talkgroups: [(Talkgroup(1), talkgroup(1)), (Talkgroup(3), talkgroup(3))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        CallManagerConfig {
            call_timeout: 1.0,
            new_call_from_update: false,
            record_uu_v_calls: false,
            max_channels: 8,
            temp_dir: String::new(),
            systems,
            adaptive_freq_correction: false,
        }
    }

    /// A message of `kind` on system 1.
    fn message(kind: Trunking) -> TrunkMessage {
        TrunkMessage {
            sys_num: 1,
            nac: 0,
            opcode: 0,
            kind,
        }
    }

    /// A grant of `freq_hz` (phase 1) to `talkgroup` for unit 100.
    fn grant_msg(talkgroup: u32, freq_hz: f64) -> TrunkMessage {
        message(Trunking::Grant(voice(talkgroup, freq_hz, None, Some(100))))
    }

    fn voice(talkgroup: u32, freq_hz: f64, slot: Option<u8>, source: Option<u32>) -> Voice {
        Voice {
            channel: Channel { freq_hz, slot },
            talkgroup,
            source,
            options: Options::default(),
        }
    }

    /// `m` as a grant from `source` instead.
    fn from_source(m: &TrunkMessage, source: u32) -> TrunkMessage {
        let Trunking::Grant(v) = m.kind else {
            panic!("not a grant")
        };
        message(Trunking::Grant(Voice {
            source: Some(source),
            ..v
        }))
    }

    /// `m` with its options changed by `f`.
    fn with_options(m: &TrunkMessage, f: impl FnOnce(&mut Options)) -> TrunkMessage {
        let Trunking::Grant(mut v) = m.kind else {
            panic!("not a grant")
        };
        f(&mut v.options);
        message(Trunking::Grant(v))
    }

    fn encrypted(m: &TrunkMessage) -> TrunkMessage {
        with_options(m, |o| o.encrypted = true)
    }

    fn name(e: &Effect) -> &'static str {
        match e {
            Effect::Open { .. } => "Open",
            Effect::Close { .. } => "Close",
            Effect::Rotate { .. } => "Rotate",
            Effect::SetSource { .. } => "SetSource",
            Effect::CreateCall { .. } => "CreateCall",
            Effect::SetFreq { .. } => "SetFreq",
            Effect::EmitCallStart { .. } => "EmitCallStart",
            Effect::EmitCallEnd { .. } => "EmitCallEnd",
            Effect::NextTransmission { .. } => "NextTransmission",
            Effect::EmitCallsActive => "EmitCallsActive",
            Effect::EmitSignal { .. } => "EmitSignal",
            Effect::EmitUnitEvent { .. } => "EmitUnitEvent",
            Effect::EmitSetupSystem { .. } => "EmitSetupSystem",
            Effect::RemoveCall { .. } => "RemoveCall",
        }
    }

    fn names(effects: &[Effect]) -> Vec<&'static str> {
        effects.iter().map(name).collect()
    }

    /// Verify effects contain the expected variant names in order.
    /// Only checks the variants listed — extra effects in between are allowed.
    fn assert_effects_contain_ordered(effects: &[Effect], expected: &[&str]) {
        let names = names(effects);
        let mut name_iter = names.iter();
        for &exp in expected {
            let found = name_iter.any(|&n| n == exp);
            assert!(found, "expected {exp} in effects, got: {names:?}");
        }
    }

    fn has(effects: &[Effect], what: &str) -> bool {
        effects.iter().any(|e| name(e) == what)
    }

    /// The channel effects, in order.
    fn channel_effects(effects: &[Effect]) -> Vec<(&'static str, CallId)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Open { call_id } => Some(("Open", *call_id)),
                Effect::Close { call_id } => Some(("Close", *call_id)),
                Effect::Rotate { call_id } => Some(("Rotate", *call_id)),
                _ => None,
            })
            .collect()
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

    fn ended(effects: &[Effect]) -> Vec<(CallId, CallEndReason)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::EmitCallEnd { call_id, reason } => Some((*call_id, *reason)),
                _ => None,
            })
            .collect()
    }

    /// Apply `effects` as the executor would where the state machine hears
    /// back: every open succeeds, transmissions advance, calls are removed.
    /// Returns everything that was done, follow-ups included.
    fn exec(sm: &mut TrunkStateMachine, effects: Effects) -> Vec<Effect> {
        let mut done = Vec::new();
        for e in effects {
            match e {
                Effect::Open { call_id } => {
                    let followup = sm.on_opened(call_id, Ok(ChannelNum(5)));
                    done.push(e);
                    done.extend(exec(sm, followup));
                    continue;
                }
                Effect::NextTransmission { call_id, source } => {
                    sm.on_next_transmission(call_id, source);
                }
                Effect::RemoveCall { call_id } => sm.remove_call(call_id),
                _ => {}
            }
            done.push(e);
        }
        done
    }

    /// Start a call with `msg` and put it on its channel.
    fn start(sm: &mut TrunkStateMachine, msg: &TrunkMessage) -> CallId {
        let effects = sm.on_message(msg);
        let call_id = extract_create_call_id(&effects);
        exec(sm, effects);
        call_id
    }

    fn state(sm: &TrunkStateMachine, id: CallId) -> CallState {
        sm.get_call(id).unwrap().state
    }

    // ── Grants ──

    #[test]
    fn a_grant_creates_a_call_on_its_own_channel() {
        let mut sm = TrunkStateMachine::new(test_config());
        let effects = sm.on_message(&grant_msg(100, 851_000_000.0));
        assert_eq!(
            names(&effects),
            ["CreateCall", "Open", "EmitCallStart", "EmitCallsActive"]
        );
        let id = sm.calls[0].id;
        assert_eq!(
            state(&sm, id),
            CallState::Recording,
            "on the air when it starts"
        );
        let (recipe, call) = sm.opening(id).unwrap();
        assert_eq!(recipe.kind(), "p25");
        assert_eq!(
            (call.talkgroup, call.channel.freq_hz, call.channel.slot),
            (100, 851_000_000.0, None)
        );
        exec(&mut sm, effects);
        assert_eq!(sm.calls[0].channel, Some(ChannelNum(5)));
    }

    #[test]
    fn grant_same_freq_updates_source() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = start(&mut sm, &grant_msg(100, 851_000_000.0));
        let effects = sm.on_message(&from_source(&grant_msg(100, 851_000_000.0), 200));
        assert_eq!(names(&effects), ["SetSource", "EmitSignal"]);
        assert_eq!(sm.calls.len(), 1);
    }

    #[test]
    fn a_retune_moves_the_call_to_its_new_channel() {
        let mut sm = TrunkStateMachine::new(test_config());
        let a = start(&mut sm, &grant_msg(100, 851_000_000.0));
        let effects = sm.on_message(&grant_msg(100, 852_000_000.0));
        assert_eq!(names(&effects), ["Close", "SetFreq", "Open"]);
        assert_eq!(state(&sm, a), CallState::Recording);
        assert_eq!(
            sm.get_call(a).unwrap().segments.len(),
            1,
            "the old one closed"
        );
    }

    #[test]
    fn a_retune_keeps_the_calls_place_in_the_queue() {
        let mut config = test_config();
        config.max_channels = 1;
        let sys = config.systems.get_mut(&1).unwrap();
        sys.talkgroups.insert(Talkgroup(30), talkgroup(3));
        let mut sm = TrunkStateMachine::new(config);
        let a = start(&mut sm, &grant_msg(3, 851_000_000.0));
        let w = start(&mut sm, &grant_msg(30, 854_000_000.0));
        assert_eq!(state(&sm, w), CallState::Waiting);
        // an equal that has waited longer than the holder has held
        let held_since = sm.wants[&a].since;
        sm.wants.get_mut(&w).unwrap().since = held_since - Duration::from_secs(1);
        // the holder moves: it keeps its channel
        let effects = sm.on_message(&grant_msg(3, 852_000_000.0));
        assert_eq!(channel_effects(&effects), [("Close", a), ("Open", a)]);
        assert_eq!(state(&sm, w), CallState::Waiting);
    }

    // ── A channel granted to another talkgroup ends the call on it ──

    fn tdma_grant(talkgroup: u32, freq_hz: f64, slot: i32) -> TrunkMessage {
        message(Trunking::Grant(voice(
            talkgroup,
            freq_hz,
            Some(u8::try_from(slot).unwrap()),
            Some(100),
        )))
    }

    #[test]
    fn grant_of_the_channel_to_another_talkgroup_ends_the_call() {
        let mut sm = TrunkStateMachine::new(test_config());
        let a = start(&mut sm, &grant_msg(100, 851_000_000.0));

        let effects = sm.on_message(&grant_msg(200, 851_000_000.0));

        assert_eq!(ended(&effects), [(a, CallEndReason::ChannelReassigned)]);
        // The old call's channel closes, and its end is sent, before the new
        // one opens.
        assert_effects_contain_ordered(
            &effects,
            &["Close", "EmitCallEnd", "RemoveCall", "CreateCall", "Open"],
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
        sm.patch_tables.get_mut(&1).unwrap().update(
            &trunk_radio::Patch {
                supergroup: 900,
                groups: [100, 200, 0],
            },
            now,
        );
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

        let update = message(Trunking::Update(voice(200, 851_000_000.0, None, Some(100))));
        assert_eq!(ended(&sm.on_message(&update)), []);
    }

    #[test]
    fn a_talkgroup_moving_to_another_slot_retunes_its_call() {
        let mut sm = TrunkStateMachine::new(test_config());
        let a = start(&mut sm, &tdma_grant(100, 851_000_000.0, 0));

        let effects = sm.on_message(&tdma_grant(100, 852_000_000.0, 1));

        assert_eq!(ended(&effects), []);
        assert_effects_contain_ordered(&effects, &["Close", "SetFreq", "Open"]);
        assert_eq!(sm.calls.len(), 1);
        let entry = sm.get_call(a).unwrap();
        assert_eq!(
            (entry.freq, entry.tdma_slot),
            (Freq::from_hz(852_000_000.0), 1)
        );
        assert_eq!(sm.opening(a).unwrap().1.channel.slot, Some(1));

        // Same frequency, other slot: still a retune, not a second call.
        exec(&mut sm, effects);
        let effects = sm.on_message(&tdma_grant(100, 852_000_000.0, 0));
        assert_effects_contain_ordered(&effects, &["Close", "Open"]);
        assert_eq!(sm.calls.len(), 1);
        assert_eq!(sm.get_call(a).unwrap().tdma_slot, 0);
    }

    // ── Who gets a channel ──

    #[test]
    fn a_call_waits_for_a_channel_and_gets_the_first_one_freed() {
        let mut config = test_config();
        config.max_channels = 1;
        let mut sm = TrunkStateMachine::new(config);
        let a = start(&mut sm, &grant_msg(3, 851_000_000.0));
        // the same priority: the holder keeps its channel
        let effects = sm.on_message(&grant_msg(300, 852_000_000.0));
        let b = extract_create_call_id(&effects);
        assert!(!has(&effects, "Open") && !has(&effects, "Close"));
        assert_effects_contain_ordered(&effects, &["CreateCall", "EmitCallStart"]);
        assert_eq!(state(&sm, b), CallState::Waiting);
        assert_eq!((sm.channels_held(), sm.waiting()), (1, 1));
        exec(&mut sm, effects);
        // the holder's call ends: the waiting one gets its channel
        sm.calls[0].last_update = Instant::now() - Duration::from_secs(10);
        sm.calls[1].last_update = Instant::now();
        let effects = sm.tick();
        assert_eq!(channel_effects(&effects), [("Close", a), ("Open", b)]);
        assert_eq!(ended(&effects), [(a, CallEndReason::GrantTimeout)]);
        assert_eq!(state(&sm, b), CallState::Recording);
    }

    #[test]
    fn a_call_that_outranks_a_holder_evicts_it() {
        let mut config = test_config();
        config.max_channels = 2;
        let mut sm = TrunkStateMachine::new(config);
        let unknown = start(&mut sm, &grant_msg(200, 851_000_000.0)); // 5
        let works = start(&mut sm, &grant_msg(3, 852_000_000.0)); // 3
        let effects = sm.on_message(&grant_msg(1, 853_000_000.0)); // dispatch, 1
        let dispatch = extract_create_call_id(&effects);
        assert_eq!(
            channel_effects(&effects),
            [("Close", unknown), ("Open", dispatch)],
            "the lowest claim goes"
        );
        assert_eq!(ended(&effects), [(unknown, CallEndReason::Evicted)]);
        assert_effects_contain_ordered(
            &effects,
            &[
                "Close",
                "EmitCallEnd",
                "RemoveCall",
                "Open",
                "EmitCallStart",
            ],
        );
        exec(&mut sm, effects);
        assert!(
            sm.get_call(unknown).is_none(),
            "its next grant is a new call"
        );
        assert_eq!(state(&sm, works), CallState::Recording);
        assert_eq!(state(&sm, dispatch), CallState::Recording);
        // the evicted talkgroup's next grant waits behind both
        let effects = sm.on_message(&grant_msg(200, 851_000_000.0));
        let again = extract_create_call_id(&effects);
        assert_ne!(again, unknown);
        assert_eq!(state(&sm, again), CallState::Waiting);
    }

    #[test]
    fn an_emergency_claims_at_the_emergency_priority() {
        let mut config = test_config();
        config.max_channels = 1;
        let mut sm = TrunkStateMachine::new(config);
        let dispatch = start(&mut sm, &grant_msg(1, 851_000_000.0));
        let effects = sm.on_message(&with_options(&grant_msg(3, 852_000_000.0), |o| {
            o.emergency = true;
        }));
        let emergency = extract_create_call_id(&effects);
        assert_eq!(
            channel_effects(&effects),
            [("Close", dispatch), ("Open", emergency)]
        );
    }

    #[test]
    fn a_call_whose_channel_cannot_open_is_rejected_and_frees_its_place() {
        let mut config = test_config();
        config.max_channels = 1;
        let mut sm = TrunkStateMachine::new(config);
        let effects = sm.on_message(&grant_msg(3, 851_000_000.0));
        let a = extract_create_call_id(&effects);
        let waiting = start(&mut sm, &grant_msg(300, 852_000_000.0));
        assert_eq!(state(&sm, waiting), CallState::Waiting);
        let followup = sm.on_opened(
            a,
            Err(OpenError::NoSource {
                freq_hz: 851e6,
                kind: "p25",
            }),
        );
        assert_eq!(state(&sm, a), CallState::Rejected(RejectReason::NoSource));
        assert_eq!(channel_effects(&followup), [("Open", waiting)]);
        assert_eq!(state(&sm, waiting), CallState::Recording);
        let _ = sm.on_opened(waiting, Err(OpenError::NoScramblingMask));
        assert_eq!(
            state(&sm, waiting),
            CallState::Rejected(RejectReason::NoChannel)
        );
        assert_eq!((sm.channels_held(), sm.waiting()), (0, 0));
    }

    #[test]
    fn control_channels_hold_channels_at_their_priority() {
        let mut config = test_config();
        config.max_channels = 2;
        let sys = config.systems.get_mut(&1).unwrap();
        sys.control_channels = vec![Freq::from_hz(851_012_500.0), Freq::from_hz(851_037_500.0)];
        sys.control_recipe = Some(Recipe::<P25Control>::opener(()));
        sys.control_priority = Priority(0);
        let mut sm = TrunkStateMachine::new(config);
        let effects = sm.start();
        let ccs: Vec<CallId> = channel_effects(&effects)
            .iter()
            .map(|(_, id)| *id)
            .collect();
        assert_eq!(ccs.len(), 2);
        let (recipe, call) = sm.opening(ccs[0]).unwrap();
        assert_eq!((recipe.kind(), call.talkgroup), ("p25-control", 0));
        assert_eq!(
            sm.control(ccs[1]).unwrap().freq,
            Freq::from_hz(851_037_500.0)
        );
        exec(&mut sm, effects);
        // every channel is a control channel's: a call waits
        let a = start(&mut sm, &grant_msg(1, 852_000_000.0));
        assert_eq!(state(&sm, a), CallState::Waiting);
        // one cannot be opened: its claim goes and the call gets the channel
        let followup = sm.on_opened(ccs[1], Err(OpenError::NoSystem(1)));
        assert_eq!(channel_effects(&followup), [("Open", a)]);
        assert!(sm.control(ccs[1]).is_none());
        // at shutdown the control channel closes too
        let effects = sm.conclude_all();
        assert_eq!(channel_effects(&effects), [("Close", a), ("Close", ccs[0])]);
    }

    #[test]
    fn a_control_channel_outranked_by_calls_is_evicted_and_comes_back() {
        let mut config = test_config();
        config.max_channels = 1;
        let sys = config.systems.get_mut(&1).unwrap();
        sys.control_channels = vec![Freq::from_hz(851_012_500.0)];
        sys.control_recipe = Some(Recipe::<P25Control>::opener(()));
        sys.control_priority = Priority(2);
        let mut sm = TrunkStateMachine::new(config);
        let cc = channel_effects(&sm.start())[0].1;
        let effects = sm.on_message(&grant_msg(1, 852_000_000.0));
        let dispatch = extract_create_call_id(&effects);
        assert_eq!(
            channel_effects(&effects),
            [("Close", cc), ("Open", dispatch)]
        );
        assert!(
            ended(&effects).is_empty(),
            "a control channel has no call to end"
        );
        exec(&mut sm, effects);
        sm.calls[0].last_update = Instant::now() - Duration::from_secs(10);
        let effects = sm.tick();
        assert_eq!(
            channel_effects(&effects),
            [("Close", dispatch), ("Open", cc)]
        );
    }

    // ── In-band source change → per-speaker transmission boundary ──

    /// Drive a trunked call into Recording and return its `CallId`.
    fn recording_trunked_call(sm: &mut TrunkStateMachine) -> CallId {
        start(sm, &grant_msg(100, 851_000_000.0))
    }

    #[test]
    fn crypto_records_algid_and_marks_encrypted() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        assert_eq!(sm.calls[0].encryption, None);

        let aes = Encryption {
            algid: 0x84,
            keyid: 0x1234,
        };
        sm.on_crypto(call_id, aes);
        assert!(sm.calls[0].encrypted);
        assert_eq!(sm.calls[0].encryption, Some(aes));

        // Unknown call ids are ignored.
        sm.on_crypto(
            CallId(u64::MAX),
            Encryption {
                algid: 0xAA,
                keyid: 1,
            },
        );
        assert_eq!(sm.calls[0].encryption, Some(aes));
        assert_eq!(sm.alg_id_for(call_id), Some(0x84));

        // A later call on the same talkgroup rejected as encrypted at the
        // grant (never recorded, so no ESS) is labelled from the talkgroup.
        let (sys, tg) = (sm.calls[0].sys_num, sm.calls[0].talkgroup);
        sm.calls.push(CallEntry {
            sys_num: sys,
            talkgroup: tg,
            state: CallState::Rejected(RejectReason::Encrypted),
            ..CallEntry::default()
        });
        let rejected = sm.calls[1].id;
        assert_eq!(sm.calls[1].encryption, None);
        assert_eq!(sm.alg_id_for(rejected), Some(0x84));

        // A clear call on it is not: one ESS (perhaps a miscorrection) does
        // not make the talkgroup's later calls encrypted.
        sm.calls.push(CallEntry {
            sys_num: sys,
            talkgroup: tg,
            ..CallEntry::default()
        });
        let clear = sm.calls[2].id;
        assert_eq!(sm.alg_id_for(clear), None);
    }

    #[test]
    fn source_change_cuts_transmission_boundary() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        // Past MIN_TRANSMISSION so a real boundary is cut.
        sm.calls[0].segment_start = Instant::now() - Duration::from_secs(2);

        // A different unit keys up mid-call: end this transmission, start the next.
        let effects = sm.on_source_change(call_id, 2002);
        assert_eq!(
            names(&effects),
            [
                "EmitCallEnd",
                "NextTransmission",
                "SetSource",
                "EmitSignal",
                "EmitCallStart",
            ]
        );
        assert_eq!(sm.calls[0].current_source, Some(2002));

        // The same source re-firing every superframe must not cut another boundary.
        assert!(sm.on_source_change(call_id, 2002).is_empty());
        // No unit (encrypted / no-LC frames) must not cut a boundary.
        assert!(sm.on_source_change(call_id, 0).is_empty());
    }

    // ── Control channel grant on a recording call → per-PTT boundary ──

    fn key_up(sm: &mut TrunkStateMachine, source: u32) -> Effects {
        sm.on_message(&from_source(&grant_msg(100, 851_000_000.0), source))
    }

    fn cut_at_key_up(effects: &Effects, source: u32) -> bool {
        matches!(
            effects.as_ref(),
            [
                Effect::EmitCallEnd {
                    reason: CallEndReason::KeyUp,
                    ..
                },
                Effect::NextTransmission { source: Some(s), .. },
                Effect::SetSource { .. },
                Effect::EmitSignal { .. },
                Effect::EmitCallStart { .. },
            ] if *s == source
        )
    }

    #[test]
    fn a_grant_naming_another_handset_cuts_a_transmission() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        sm.calls[0].segment_start = Instant::now() - Duration::from_secs(2);
        assert!(cut_at_key_up(&key_up(&mut sm, 2002), 2002));
        assert_eq!(sm.calls[0].current_source, Some(2002));
        // The in-band source change that follows matches and adds nothing.
        assert!(sm.on_source_change(call_id, 2002).is_empty());
    }

    #[test]
    fn the_same_handset_keying_again_cuts_only_after_its_transmission_ended() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        sm.calls[0].segment_start = Instant::now() - Duration::from_secs(2);
        // A repeated grant within one key-up: no boundary.
        assert!(!has(&key_up(&mut sm, 100), "EmitCallEnd"));
        // The voice channel saw a terminator; the next grant is a new key-up.
        sm.on_ptt_ended(call_id);
        let effects = key_up(&mut sm, 100);
        assert!(cut_at_key_up(&effects, 100));
        exec(&mut sm, effects);
        assert!(!sm.calls[0].ptt_ended, "a new transmission starts unended");
        assert_eq!(state(&sm, call_id), CallState::Recording);
    }

    #[test]
    fn key_ups_within_the_minimum_transmission_only_reroute() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = recording_trunked_call(&mut sm);
        let effects = key_up(&mut sm, 2002);
        assert!(!has(&effects, "EmitCallEnd"));
        assert_eq!(sm.calls[0].current_source, Some(2002));
    }

    #[test]
    fn cuts_emit_exact_vectors() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);

        // A speaker change: a logical boundary on a channel that runs on.
        let speaker = sm.cut_transmission(call_id, Cut::Speaker { source: 42 });
        assert!(matches!(
            speaker.as_ref(),
            [
                Effect::EmitCallEnd {
                    reason: CallEndReason::SourceChange,
                    ..
                },
                Effect::NextTransmission {
                    source: Some(42),
                    ..
                },
                Effect::SetSource { source_id: 42, .. },
                Effect::EmitSignal { source_id: 42, .. },
                Effect::EmitCallStart { .. },
            ]
        ));

        // A carrier drop: the recording rotates first, so its held audio
        // goes out before the end; no eager CallStart.
        let carrier = sm.cut_transmission(call_id, Cut::Carrier);
        assert!(matches!(
            carrier.as_ref(),
            [
                Effect::Rotate { .. },
                Effect::EmitCallEnd {
                    reason: CallEndReason::Conventional,
                    ..
                },
                Effect::NextTransmission { source: None, .. },
            ]
        ));
    }

    #[test]
    fn source_change_below_min_transmission_only_reroutes() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        // segment_start is fresh (< MIN_TRANSMISSION): a rapid hand-off must
        // only re-route the source, not fragment into a sub-second transmission.
        let effects = sm.on_source_change(call_id, 2002);
        assert_eq!(names(&effects), ["SetSource", "EmitSignal"]);
        assert_eq!(sm.calls[0].current_source, Some(2002));
    }

    #[test]
    fn source_change_ignored_when_not_recording() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        sm.calls[0].segment_start = Instant::now() - Duration::from_secs(2);
        sm.calls[0].state = CallState::Idle;
        assert!(sm.on_source_change(call_id, 2002).is_empty());
    }

    // ── Tick: timed-out call closes its channel, ends, then is removed ──

    #[test]
    fn tick_closes_the_channel_and_ends_the_call_before_removing_it() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        sm.calls[0].last_update = Instant::now() - Duration::from_secs(10);

        let effects = sm.tick();

        assert_eq!(
            names(&effects),
            ["Close", "EmitCallEnd", "RemoveCall", "EmitCallsActive"]
        );
        assert_eq!(ended(&effects), [(call_id, CallEndReason::GrantTimeout)]);
        // Call entry must still exist until RemoveCall is processed
        assert!(sm.get_call(call_id).is_some());
        exec(&mut sm, effects);
        assert!(sm.get_call(call_id).is_none());
    }

    #[test]
    fn a_rejected_call_times_out_without_a_channel() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = start(&mut sm, &encrypted(&grant_msg(100, 851_000_000.0)));
        sm.calls[0].last_update = Instant::now() - Duration::from_secs(10);

        let effects = sm.tick();

        assert_eq!(
            names(&effects),
            ["EmitCallEnd", "RemoveCall", "EmitCallsActive"]
        );
        assert_eq!(ended(&effects), [(call_id, CallEndReason::IdleTimeout)]);
    }

    #[test]
    fn tick_does_not_remove_active_call() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = recording_trunked_call(&mut sm);
        assert!(!has(&sm.tick(), "RemoveCall"));
        assert_eq!(sm.calls.len(), 1);
    }

    #[test]
    fn conclude_all_closes_ends_and_removes() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        let effects = sm.conclude_all();
        assert_eq!(
            names(&effects),
            ["Close", "EmitCallEnd", "RemoveCall", "EmitCallsActive"]
        );
        assert_eq!(ended(&effects), [(call_id, CallEndReason::Shutdown)]);
        assert!(sm.get_call(call_id).is_some());
    }

    #[test]
    fn remove_call_removes_entry() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = start(&mut sm, &encrypted(&grant_msg(100, 851_000_000.0)));
        assert_eq!(sm.calls.len(), 1);
        sm.remove_call(call_id);
        assert_eq!(sm.calls.len(), 0);
    }

    // ── Conventional ──

    fn conventional(signal_detection: bool) -> ConventionalChannel {
        ConventionalChannel {
            talkgroup: Talkgroup(1),
            freq: Freq::from_hz(488_000_000.0),
            priority: Priority(1),
            signal_detection,
            squelch_db: -50.0,
            recipe: Recipe::<AnalogFm>::opener(AnalogParams::default()),
        }
    }

    /// A conventional channel's call, on its channel.
    fn add_conventional(sm: &mut TrunkStateMachine, signal_detection: bool) -> CallId {
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, effects) =
            sm.add_conventional_call(&conventional(signal_detection), 1, &policy);
        exec(sm, effects);
        call_id
    }

    #[test]
    fn an_always_on_channel_opens_at_once_and_waits_for_a_transmission() {
        let mut sm = TrunkStateMachine::new(test_config());
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, effects) = sm.add_conventional_call(&conventional(false), 1, &policy);
        assert_eq!(channel_effects(&effects), [("Open", call_id)]);
        assert_eq!(state(&sm, call_id), CallState::Idle);
        assert_eq!(sm.opening(call_id).unwrap().0.kind(), "analog");
    }

    #[test]
    fn tick_skips_conventional_calls() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, false);
        sm.calls[0].last_update = Instant::now() - Duration::from_secs(10);
        assert!(!has(&sm.tick(), "RemoveCall"));
        assert!(sm.get_call(call_id).is_some());
    }

    #[test]
    fn an_always_on_backstop_concludes_the_transmission_on_its_channel() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, false);
        sm.calls[0].state = CallState::Recording;

        let effects = sm.on_conventional_idle_timeout(call_id);

        // Per-transmission conclude: the recording rotates, the end goes out,
        // the channel stays open. No eager EmitCallStart.
        assert_eq!(
            names(&effects),
            ["Rotate", "EmitCallEnd", "NextTransmission"]
        );
        exec(&mut sm, effects);
        let entry = sm.get_call(call_id).unwrap();
        assert_eq!(entry.channel, Some(ChannelNum(5)), "still on its channel");
        assert_eq!(entry.state, CallState::Idle);
    }

    #[test]
    fn squelch_events_are_matched_by_call_id() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, false);
        let _ = sm.on_squelch(call_id, Squelch::Open, Some(-20.0));
        assert_eq!(state(&sm, call_id), CallState::Recording);
        assert_eq!(sm.get_call(call_id).unwrap().signal, Some(-20.0));
        // another call's event changes nothing
        assert!(
            sm.on_squelch(
                CallId(u64::MAX),
                Squelch::Close(CloseReason::Quieting),
                None
            )
            .is_empty()
        );
        assert_eq!(state(&sm, call_id), CallState::Recording);
        // a trunked call's are ignored
        let trunked = recording_trunked_call(&mut sm);
        assert!(sm.on_squelch(trunked, Squelch::Open, None).is_empty());
    }

    #[test]
    fn squelch_muted_on_a_signal_detection_channel_closes_the_segment() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, true);
        sm.calls[0].state = CallState::Recording;

        let effects = sm.on_squelch(call_id, Squelch::Close(CloseReason::Power), Some(-30.0));
        assert!(effects.is_empty());
        assert_eq!(sm.calls[0].state, CallState::Idle);
        assert_eq!(sm.calls[0].segments.len(), 1);
        assert!(sm.calls[0].idle_since.is_some());
        assert_eq!(sm.calls[0].noise, Some(-30.0));
    }

    #[test]
    fn always_on_conventional_conclude_per_transmission() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, false);

        // First keyed transmission: squelch open → CallStart + Recording.
        let effects = sm.on_squelch(call_id, Squelch::Open, Some(-20.0));
        assert_eq!(names(&effects), ["EmitCallStart", "EmitCallsActive"]);
        assert_eq!(state(&sm, call_id), CallState::Recording);

        // The close ends this transmission as its own call. No eager
        // CallStart.
        let effects = sm.on_squelch(call_id, Squelch::Close(CloseReason::Quieting), Some(-30.0));
        assert_eq!(
            names(&effects),
            ["Rotate", "EmitCallEnd", "NextTransmission"]
        );
        let call_num = sm.get_call(call_id).unwrap().call_num;
        exec(&mut sm, effects);
        assert_eq!(state(&sm, call_id), CallState::Idle);
        assert_ne!(sm.get_call(call_id).unwrap().call_num, call_num);

        // Second keyed transmission: the next open is a fresh CallStart.
        let effects = sm.on_squelch(call_id, Squelch::Open, Some(-20.0));
        assert!(has(&effects, "EmitCallStart"));
    }

    #[test]
    fn an_evicted_conventional_channel_ends_its_transmission_and_waits() {
        let mut config = test_config();
        config.max_channels = 1;
        let mut sm = TrunkStateMachine::new(config);
        let call_id = add_conventional(&mut sm, false); // priority 1
        let _ = sm.on_squelch(call_id, Squelch::Open, None);
        let effects = sm.on_message(&with_options(&grant_msg(3, 851_000_000.0), |o| {
            o.emergency = true; // 0
        }));
        let emergency = extract_create_call_id(&effects);
        assert_eq!(
            channel_effects(&effects),
            [("Close", call_id), ("Open", emergency)]
        );
        assert_eq!(ended(&effects), [(call_id, CallEndReason::Evicted)]);
        exec(&mut sm, effects);
        assert_eq!(state(&sm, call_id), CallState::Waiting);
        // the emergency ends: the channel comes back, waiting for a carrier
        sm.calls[1].last_update = Instant::now() - Duration::from_secs(10);
        let effects = sm.tick();
        assert_eq!(
            channel_effects(&effects),
            [("Close", emergency), ("Open", call_id)]
        );
        assert_eq!(state(&sm, call_id), CallState::Idle);
    }

    // ── Signal detection ──

    fn detected(sm: &mut TrunkStateMachine, offset_hz: f64) -> Vec<Effect> {
        let signals = vec![trunk_radio::DetectedSignal {
            center_freq: offset_hz,
            max_rssi: -20,
            threshold: -45,
        }];
        let effects = sm.on_signals_detected(487_000_000.0, &signals);
        exec(sm, effects)
    }

    #[test]
    fn a_signal_detection_channel_wants_no_channel_until_something_is_detected() {
        let mut sm = TrunkStateMachine::new(test_config());
        let policy = sm.config.systems.get(&1).unwrap().clone();
        let (call_id, effects) = sm.add_conventional_call(&conventional(true), 1, &policy);
        assert!(effects.is_empty());
        assert_eq!(state(&sm, call_id), CallState::Disabled);
        assert_eq!(sm.waiting(), 0);
    }

    #[test]
    fn signals_detected_open_the_matching_channel() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, true);
        // Source center 487 MHz, signal at +1 MHz offset → 488 MHz absolute
        let effects = detected(&mut sm, 1_000_000.0);
        assert_eq!(channel_effects(&effects), [("Open", call_id)]);
        assert_eq!(state(&sm, call_id), CallState::Idle);
        // No premature idle timer — wait for squelch to open/close first
        assert!(sm.get_call(call_id).unwrap().idle_since.is_none());
    }

    #[test]
    fn signals_detected_no_match_when_freq_too_far() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, true);
        // Signal at 490 MHz, channel at 488 MHz — 2 MHz apart
        assert!(detected(&mut sm, 3_000_000.0).is_empty());
        assert_eq!(state(&sm, call_id), CallState::Disabled);
    }

    #[test]
    fn signals_detected_within_tolerance() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, true);
        // 10 kHz off the channel
        assert!(!detected(&mut sm, 1_010_000.0).is_empty());
        assert_eq!(state(&sm, call_id), CallState::Idle);
    }

    #[test]
    fn signals_detected_skips_channels_already_on() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, true);
        let _ = detected(&mut sm, 1_000_000.0);
        sm.calls[0].state = CallState::Recording;
        assert!(detected(&mut sm, 1_000_000.0).is_empty());
        assert_eq!(state(&sm, call_id), CallState::Recording);
    }

    #[test]
    fn a_signal_detection_timeout_without_activity_just_closes() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, true);
        let _ = detected(&mut sm, 1_000_000.0);
        sm.calls[0].idle_since = Some(Instant::now());

        let effects = sm.on_conventional_idle_timeout(call_id);
        assert_eq!(names(&effects), ["Close", "NextTransmission"]);
        exec(&mut sm, effects);
        let entry = sm.get_call(call_id).unwrap();
        assert_eq!(entry.state, CallState::Disabled);
        assert!(entry.idle_since.is_none());
    }

    #[test]
    fn a_signal_detection_timeout_while_recording_ends_the_call() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, true);
        let _ = detected(&mut sm, 1_000_000.0);
        sm.calls[0].state = CallState::Recording;

        let effects = sm.on_conventional_idle_timeout(call_id);
        // The channel closes first, so its held audio precedes the end.
        assert_eq!(
            names(&effects),
            ["Close", "EmitCallEnd", "NextTransmission"]
        );
        assert!(!has(&effects, "EmitCallStart"));
        exec(&mut sm, effects);
        assert_eq!(state(&sm, call_id), CallState::Disabled);
        assert!(sm.get_call(call_id).unwrap().segments.is_empty());
    }

    #[test]
    fn signal_detection_full_lifecycle() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, true);
        assert_eq!(state(&sm, call_id), CallState::Disabled);

        // 1. Signal detected → its channel opens, no idle timer
        assert_eq!(
            channel_effects(&detected(&mut sm, 1_000_000.0)),
            [("Open", call_id)]
        );
        assert_eq!(state(&sm, call_id), CallState::Idle);

        // 2. Squelch opens → Recording + CallStart
        let effects = sm.on_squelch(call_id, Squelch::Open, Some(-35.0));
        assert_eq!(names(&effects), ["EmitCallStart", "EmitCallsActive"]);

        // 3. Squelch closes → Idle, idle timer starts
        assert!(
            sm.on_squelch(call_id, Squelch::Close(CloseReason::Power), Some(-80.0))
                .is_empty()
        );
        assert!(sm.get_call(call_id).unwrap().idle_since.is_some());

        // 4. Idle timeout → the channel closes, the call concludes
        let effects = sm.on_conventional_idle_timeout(call_id);
        assert_eq!(
            names(&effects),
            ["Close", "EmitCallEnd", "NextTransmission"]
        );
        exec(&mut sm, effects);
        assert_eq!(state(&sm, call_id), CallState::Disabled);

        // 5. Detected again → open again; the next open is a new CallStart
        assert_eq!(
            channel_effects(&detected(&mut sm, 1_000_000.0)),
            [("Open", call_id)]
        );
        let effects = sm.on_squelch(call_id, Squelch::Open, Some(-35.0));
        assert_eq!(names(&effects), ["EmitCallStart", "EmitCallsActive"]);
    }

    #[test]
    fn a_detected_channel_waits_indefinitely_for_its_first_open() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = add_conventional(&mut sm, true);
        let _ = detected(&mut sm, 1_000_000.0);
        // tick_conventional must not time this out (idle_since is None)
        assert!(!has(&sm.tick(), "Close"));
        assert_eq!(state(&sm, call_id), CallState::Idle);
    }

    // ── Stuck-recording backstop ──

    #[test]
    fn stuck_recording_backstop_forces_a_conclusion() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = add_conventional(&mut sm, false);
        sm.calls[0].state = CallState::Recording;
        sm.calls[0].segment_start =
            Instant::now() - STUCK_RECORDING_BACKSTOP - Duration::from_secs(1);
        assert!(has(&sm.tick(), "Rotate"));
    }

    #[test]
    fn stuck_recording_backstop_does_not_fire_with_segments() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = add_conventional(&mut sm, false);
        // Recording for longer than backstop, BUT has completed segments
        // (meaning squelch has been cycling normally).
        sm.calls[0].state = CallState::Recording;
        sm.calls[0].segment_start =
            Instant::now() - STUCK_RECORDING_BACKSTOP - Duration::from_secs(1);
        sm.calls[0].segments.push(CallSegment {
            start: Instant::now() - Duration::from_secs(60),
            end: Instant::now() - Duration::from_secs(30),
            freq: Freq::from_hz(488_000_000.0),
            channel: Some(ChannelNum(5)),
        });
        assert!(!has(&sm.tick(), "Rotate"));
    }

    #[test]
    fn stuck_recording_backstop_does_not_fire_before_timeout() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = add_conventional(&mut sm, false);
        sm.calls[0].state = CallState::Recording;
        assert!(!has(&sm.tick(), "Rotate"));
    }

    // ── expire_call ──

    #[test]
    fn expire_call_recording_closes_and_ends() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = recording_trunked_call(&mut sm);
        let effects = sm.expire_call(call_id);
        assert_eq!(
            names(&effects),
            ["Close", "EmitCallEnd", "RemoveCall", "EmitCallsActive"]
        );
        assert_eq!(ended(&effects), [(call_id, CallEndReason::WriteTimeout)]);
    }

    #[test]
    fn expire_call_rejected_skips_call_end() {
        let mut sm = TrunkStateMachine::new(test_config());
        let call_id = start(&mut sm, &encrypted(&grant_msg(100, 851_000_000.0)));
        let effects = sm.expire_call(call_id);
        assert_eq!(names(&effects), ["RemoveCall", "EmitCallsActive"]);
    }

    // ── Update message handling ──

    #[test]
    fn update_with_source_change_emits_signal_not_call_start() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = start(&mut sm, &grant_msg(100, 851_000_000.0));

        let update = message(Trunking::Update(voice(100, 851_000_000.0, None, Some(200))));
        let effects = sm.on_message(&update);

        // A source change on an already-started call must NOT re-emit
        // EmitCallStart — that produced duplicate downstream call_starts which
        // orphaned the Discord embed (keyed by talkgroup).
        assert!(matches!(
            effects.as_ref(),
            [
                Effect::SetSource { source_id: 200, .. },
                Effect::EmitSignal { source_id: 200, .. }
            ]
        ));
    }

    #[test]
    fn update_same_source_produces_no_effects() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = start(&mut sm, &grant_msg(100, 851_000_000.0));
        let update = message(Trunking::Update(voice(100, 851_000_000.0, None, Some(100))));
        assert!(sm.on_message(&update).is_empty());
    }

    #[test]
    fn update_sets_encrypted_flag() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = start(&mut sm, &grant_msg(100, 851_000_000.0));
        assert!(!sm.calls[0].encrypted);

        let update = message(Trunking::Update(Voice {
            options: Options {
                encrypted: true,
                ..Options::default()
            },
            ..voice(100, 851_000_000.0, None, Some(100))
        }));
        let _effects = sm.on_message(&update);

        assert!(sm.calls[0].encrypted);
    }

    #[test]
    fn uuv_update_ignored_when_disabled() {
        let mut sm = TrunkStateMachine::new(test_config());
        let _ = start(&mut sm, &grant_msg(100, 851_000_000.0));
        let update = message(Trunking::UnitUpdate(voice(
            100,
            851_000_000.0,
            None,
            Some(200),
        )));
        assert!(sm.on_message(&update).is_empty());
    }

    #[test]
    fn uuv_update_processed_when_enabled() {
        let mut config = test_config();
        config.record_uu_v_calls = true;
        let mut sm = TrunkStateMachine::new(config);
        let _ = start(&mut sm, &grant_msg(100, 851_000_000.0));
        let update = message(Trunking::UnitUpdate(voice(
            100,
            851_000_000.0,
            None,
            Some(200),
        )));
        let effects = sm.on_message(&update);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetSource { source_id: 200, .. }))
        );
    }

    // ── Rejections ──

    #[test]
    fn encrypted_call_rejected_when_not_monitoring() {
        let mut sm = TrunkStateMachine::new(test_config());
        let effects = sm.on_message(&encrypted(&grant_msg(100, 851_000_000.0)));
        assert_eq!(
            names(&effects),
            ["CreateCall", "EmitCallStart", "EmitCallsActive"]
        );
        assert_eq!(
            sm.calls[0].state,
            CallState::Rejected(RejectReason::Encrypted)
        );
    }

    #[test]
    fn a_never_priority_talkgroup_is_ignored_and_unknown_ones_follow_policy() {
        let mut config = test_config();
        config
            .systems
            .get_mut(&1)
            .unwrap()
            .talkgroups
            .insert(Talkgroup(9), talkgroup(-1));
        let mut sm = TrunkStateMachine::new(config.clone());
        let ignored = start(&mut sm, &grant_msg(9, 851_000_000.0));
        assert_eq!(
            state(&sm, ignored),
            CallState::Rejected(RejectReason::IgnoredTg)
        );
        config.systems.get_mut(&1).unwrap().record_unknown = false;
        let mut sm = TrunkStateMachine::new(config);
        let unknown = start(&mut sm, &grant_msg(200, 851_000_000.0));
        assert_eq!(
            state(&sm, unknown),
            CallState::Rejected(RejectReason::UnknownTg)
        );
    }

    // ── Sysid / Status / UnitEvent messages ──

    #[test]
    fn another_control_channel_named_changes_nothing() {
        let mut sm = TrunkStateMachine::new(test_config());
        let effects = sm.on_message(&message(Trunking::ControlChannel {
            freq_hz: 851_000_000.0,
        }));
        assert!(effects.is_empty());
    }

    #[test]
    fn sysid_message_emits_setup_system() {
        let mut sm = TrunkStateMachine::new(test_config());
        let effects = sm.on_message(&message(Trunking::Site {
            sys_id: 0,
            rfss: 42,
            site: 7,
        }));
        assert!(matches!(
            effects.as_ref(),
            [Effect::EmitSetupSystem { sys_num: 1 }]
        ));
    }

    #[test]
    fn status_message_updates_identity_and_emits_setup() {
        let mut sm = TrunkStateMachine::new(test_config());
        let effects = sm.on_message(&TrunkMessage {
            nac: 789,
            ..message(Trunking::Network {
                freq_hz: 0.0,
                wacn: 456,
                sys_id: 123,
            })
        });
        assert!(matches!(
            effects.as_ref(),
            [Effect::EmitSetupSystem { sys_num: 1 }]
        ));
        let id = sm.system_identities.get(&1).unwrap();
        assert_eq!((id.sys_id, id.wacn, id.nac), (123, 456, 789));
    }

    #[test]
    fn registration_message_emits_unit_event() {
        let mut sm = TrunkStateMachine::new(test_config());
        let effects = sm.on_message(&message(Trunking::Registration { unit: 42 }));
        assert!(matches!(
            effects.as_ref(),
            [Effect::EmitUnitEvent {
                type_: UnitEventType::Registration,
                source_id: 42,
                ..
            }]
        ));
    }

    #[test]
    fn affiliation_message_includes_talkgroup() {
        let mut sm = TrunkStateMachine::new(test_config());
        let effects = sm.on_message(&message(Trunking::Affiliation {
            group: 200,
            unit: 10,
        }));
        assert!(matches!(
            effects.as_ref(),
            [Effect::EmitUnitEvent {
                type_: UnitEventType::Affiliation,
                source_id: 10,
                talkgroup: Some(Talkgroup(200)),
                ..
            }]
        ));
    }

    #[test]
    fn invalid_cc_message_produces_no_effects() {
        let mut sm = TrunkStateMachine::new(test_config());
        assert!(sm.on_message(&message(Trunking::Invalid)).is_empty());
    }
}
