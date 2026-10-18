//! Every P25 decoder's output by implementation, and in shadow mode how the
//! two implementations compare on the same signal.
//!
//! C++ reports each control-channel message (op25's wire form, from whichever
//! implementation decoded it) and each voice call's per-decoder account. With
//! only a primary running these are just counted, so `impl` labels exist for
//! both implementations whichever is in use. With a shadow running:
//!
//! - control channels: a message one implementation decoded is matched with
//!   the same message (type and payload) from the other within [`WINDOW`];
//!   matched is `Both`, unmatched once the window passes is `Only(impl)`;
//! - voice calls: the source units each account names are compared
//!   (`Both` / `Only(impl)` per unit), and so is whether each produced audio.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::ffi;
use crate::p25::{Impl, Role};

/// How long a message waits for its counterpart from the other decoder.
/// op25's chain and trunk-p25 deliver the same frame within a few hundred
/// milliseconds of each other.
pub const WINDOW: Duration = Duration::from_secs(2);

/// A call has audio if it produced at least this much.
const AUDIBLE_SECONDS: f64 = 0.5;

impl From<ffi::TrP25Decoder> for Impl {
    fn from(d: ffi::TrP25Decoder) -> Self {
        if d == ffi::TrP25Decoder::Op25 {
            Self::Op25
        } else {
            Self::Rust
        }
    }
}

impl Impl {
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Rust => Self::Op25,
            Self::Op25 => Self::Rust,
        }
    }
}

impl Role {
    pub const fn of(primary: bool) -> Self {
        if primary { Self::Primary } else { Self::Shadow }
    }
}

/// A control-channel message's kind, from its op25 type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MessageKind {
    Tsbk,
    Pdu,
    Tdu,
    TduLc,
    Ldu,
    Timeout,
    Other,
}

impl MessageKind {
    pub const fn from_op25(msg_type: i64) -> Self {
        match msg_type {
            7 => Self::Tsbk,
            12 => Self::Pdu,
            3 => Self::Tdu,
            15 => Self::TduLc,
            5 | 10 => Self::Ldu,
            0xffff => Self::Timeout,
            _ => Self::Other,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Tsbk => "tsbk",
            Self::Pdu => "pdu",
            Self::Tdu => "tdu",
            Self::TduLc => "tdulc",
            Self::Ldu => "ldu",
            Self::Timeout => "timeout",
            Self::Other => "other",
        }
    }
}

/// How the two implementations compared on one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Outcome {
    Both,
    /// Only this implementation had it.
    Only(Impl),
}

impl Outcome {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Only(_) => "only",
        }
    }

    /// The implementation an `Only` belongs to.
    pub const fn implementation(self) -> Option<Impl> {
        match self {
            Self::Both => None,
            Self::Only(i) => Some(i),
        }
    }
}

/// Cumulative counts for one control channel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CcTally {
    pub messages: BTreeMap<(Impl, Role, MessageKind), u64>,
    /// Matching across implementations; only while a shadow runs.
    pub agreement: BTreeMap<(MessageKind, Outcome), u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CcShadowReport {
    pub system: String,
    pub freq_hz: f64,
    /// The implementation in each role, as last observed.
    pub roles: BTreeMap<Role, Impl>,
    pub tally: CcTally,
}

struct Pending {
    at: Instant,
    kind: MessageKind,
    key: u64,
}

#[derive(Default)]
struct CcState {
    roles: BTreeMap<Role, Impl>,
    pending: [VecDeque<Pending>; 2],
    tally: CcTally,
}

const fn slot(i: Impl) -> usize {
    match i {
        Impl::Rust => 0,
        Impl::Op25 => 1,
    }
}

impl CcState {
    fn shadowed(&self) -> bool {
        self.roles.contains_key(&Role::Shadow)
    }

    /// Messages whose window has passed unmatched; counted only once a shadow
    /// runs (without one, every message would be "only" the primary's).
    fn expire(&mut self, now: Instant) {
        let shadowed = self.shadowed();
        for i in [Impl::Rust, Impl::Op25] {
            let q = &mut self.pending[slot(i)];
            while q.front().is_some_and(|p| now.duration_since(p.at) > WINDOW) {
                if let Some(p) = q.pop_front()
                    && shadowed
                {
                    *self
                        .tally
                        .agreement
                        .entry((p.kind, Outcome::Only(i)))
                        .or_default() += 1;
                }
            }
        }
    }

    fn observe(
        &mut self,
        now: Instant,
        implementation: Impl,
        role: Role,
        msg_type: i64,
        payload: &[u8],
    ) {
        let kind = MessageKind::from_op25(msg_type);
        self.roles.insert(role, implementation);
        *self
            .tally
            .messages
            .entry((implementation, role, kind))
            .or_default() += 1;
        self.expire(now);
        if kind == MessageKind::Timeout {
            return;
        }
        let mut h = DefaultHasher::new();
        (msg_type, payload).hash(&mut h);
        let key = h.finish();
        let other = &mut self.pending[slot(implementation.other())];
        if let Some(at) = other.iter().position(|p| p.key == key) {
            other.remove(at);
            *self
                .tally
                .agreement
                .entry((kind, Outcome::Both))
                .or_default() += 1;
            // (A match needs both decoders, so a shadow is running.)
        } else {
            self.pending[slot(implementation)].push_back(Pending { at: now, kind, key });
        }
    }
}

/// Keyed by system and the frequency's bits.
static CONTROL: Mutex<BTreeMap<(String, u64), CcState>> = Mutex::new(BTreeMap::new());

pub fn observe_cc_message(
    system: &str,
    freq_hz: f64,
    implementation: Impl,
    role: Role,
    msg_type: i64,
    payload: &[u8],
) {
    let mut cc = CONTROL.lock().unwrap_or_else(PoisonError::into_inner);
    cc.entry((system.to_owned(), freq_hz.to_bits()))
        .or_default()
        .observe(Instant::now(), implementation, role, msg_type, payload);
}

/// Every control channel seen so far.
pub fn control_channels() -> Vec<CcShadowReport> {
    let now = Instant::now();
    let mut cc = CONTROL.lock().unwrap_or_else(PoisonError::into_inner);
    cc.iter_mut()
        .map(|((system, freq), s)| {
            s.expire(now);
            CcShadowReport {
                system: system.clone(),
                freq_hz: f64::from_bits(*freq),
                roles: s.roles.clone(),
                tally: s.tally.clone(),
            }
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Phase {
    One,
    Two,
}

impl Phase {
    pub const fn label(self) -> &'static str {
        match self {
            Self::One => "1",
            Self::Two => "2",
        }
    }
}

/// Cumulative per-decoder call counts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CallTally {
    pub calls: u64,
    pub audio_seconds: f64,
    pub transmissions: u64,
    pub errors: u64,
    pub spikes: u64,
}

/// What a call comparison is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CallItem {
    /// A source unit named by a transmission.
    Source,
    /// The call produced audio (≥ 0.5 s).
    Audio,
}

impl CallItem {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Audio => "audio",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VoiceTally {
    pub calls: BTreeMap<(String, Phase, Impl, Role), CallTally>,
    /// With the implementations in their roles: (system, phase, primary,
    /// shadow, item, outcome).
    pub agreement: BTreeMap<(String, Phase, Impl, Impl, CallItem, Outcome), u64>,
}

static VOICE: Mutex<Option<VoiceTally>> = Mutex::new(None);

#[allow(clippy::significant_drop_tightening)] // the tally is updated throughout
pub fn observe_call(report: &ffi::TrCallReport) {
    let phase = if report.phase2 {
        Phase::Two
    } else {
        Phase::One
    };
    let system = report.system.clone();
    let mut voice = VOICE.lock().unwrap_or_else(PoisonError::into_inner);
    let tally = voice.get_or_insert_with(VoiceTally::default);
    for a in &report.accounts {
        let t = tally
            .calls
            .entry((system.clone(), phase, a.decoder.into(), Role::of(a.primary)))
            .or_default();
        t.calls += 1;
        t.audio_seconds += a.audio_seconds;
        t.transmissions += u64::try_from(a.transmissions).unwrap_or(0);
        t.errors += u64::try_from(a.errors).unwrap_or(0);
        t.spikes += u64::try_from(a.spikes).unwrap_or(0);
    }
    let (Some(p), Some(s)) = (
        report.accounts.iter().find(|a| a.primary),
        report.accounts.iter().find(|a| !a.primary),
    ) else {
        return;
    };
    let (pi, si): (Impl, Impl) = (p.decoder.into(), s.decoder.into());
    let mut count = |item: CallItem, outcome: Outcome, n: u64| {
        if n > 0 {
            *tally
                .agreement
                .entry((system.clone(), phase, pi, si, item, outcome))
                .or_default() += n;
        }
    };
    let units = |a: &ffi::TrCallAccount| -> Vec<i64> {
        let mut u: Vec<i64> = a.sources.iter().copied().filter(|&s| s > 0).collect();
        u.sort_unstable();
        u.dedup();
        u
    };
    let (pu, su) = (units(p), units(s));
    let both = pu.iter().filter(|u| su.contains(u)).count() as u64;
    count(CallItem::Source, Outcome::Both, both);
    count(CallItem::Source, Outcome::Only(pi), pu.len() as u64 - both);
    count(CallItem::Source, Outcome::Only(si), su.len() as u64 - both);
    match (
        p.audio_seconds >= AUDIBLE_SECONDS,
        s.audio_seconds >= AUDIBLE_SECONDS,
    ) {
        (true, true) => count(CallItem::Audio, Outcome::Both, 1),
        (true, false) => count(CallItem::Audio, Outcome::Only(pi), 1),
        (false, true) => count(CallItem::Audio, Outcome::Only(si), 1),
        (false, false) => {}
    }
}

/// Every voice call so far.
pub fn voice_tally() -> VoiceTally {
    VOICE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_across_implementations() {
        let t0 = Instant::now();
        let mut s = CcState::default();
        let tsbk = [5u8, 0xe1, 0x3b, 0, 0, 0xbe, 0xe0, 5, 0xe4, 0x16, 0x70, 0x70];
        let other = [5u8, 0xe1, 0x02, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        s.observe(t0, Impl::Rust, Role::Primary, 7, &tsbk);
        s.observe(t0, Impl::Op25, Role::Shadow, 7, &other);
        s.observe(
            t0 + Duration::from_millis(300),
            Impl::Op25,
            Role::Shadow,
            7,
            &tsbk,
        );
        s.observe(
            t0 + Duration::from_millis(400),
            Impl::Rust,
            Role::Primary,
            7,
            &tsbk,
        );
        s.observe(
            t0 + Duration::from_millis(500),
            Impl::Rust,
            Role::Primary,
            0xffff,
            &[],
        );
        s.expire(t0 + Duration::from_secs(5));
        let a = &s.tally.agreement;
        assert_eq!(a.get(&(MessageKind::Tsbk, Outcome::Both)), Some(&1));
        assert_eq!(
            a.get(&(MessageKind::Tsbk, Outcome::Only(Impl::Rust))),
            Some(&1)
        );
        assert_eq!(
            a.get(&(MessageKind::Tsbk, Outcome::Only(Impl::Op25))),
            Some(&1)
        );
        assert_eq!(a.get(&(MessageKind::Timeout, Outcome::Both)), None);
        assert_eq!(
            s.tally
                .messages
                .get(&(Impl::Rust, Role::Primary, MessageKind::Tsbk)),
            Some(&2)
        );
    }

    #[test]
    fn without_a_shadow_messages_are_only_counted() {
        let mut s = CcState::default();
        s.observe(Instant::now(), Impl::Op25, Role::Primary, 7, &[1, 2, 3]);
        s.expire(Instant::now() + Duration::from_secs(5));
        assert!(s.tally.agreement.is_empty());
        assert_eq!(s.tally.messages.values().sum::<u64>(), 1);
    }
}
