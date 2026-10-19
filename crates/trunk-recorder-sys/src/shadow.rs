//! Every P25 voice decoder's account of each call by implementation, and in
//! shadow mode how the two implementations compare on the same signal.
//!
//! C++ reports each voice call's per-decoder account. With only a primary
//! running these are just counted, so `impl` labels exist for both
//! implementations whichever is in use. With a shadow running, the source
//! units each account names are compared (`Both` / `Only(impl)` per unit),
//! and so are whether each produced audio and the encryption it found.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use crate::ffi;
use crate::p25::{Impl, Role};

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
    /// The call is encrypted (the same ALGID, when both say so).
    Encrypted,
}

impl CallItem {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Audio => "audio",
            Self::Encrypted => "encrypted",
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
    match (p.algid != ALGID_CLEAR, s.algid != ALGID_CLEAR) {
        (true, true) if p.algid == s.algid => count(CallItem::Encrypted, Outcome::Both, 1),
        (true, true) => {
            count(CallItem::Encrypted, Outcome::Only(pi), 1);
            count(CallItem::Encrypted, Outcome::Only(si), 1);
        }
        (true, false) => count(CallItem::Encrypted, Outcome::Only(pi), 1),
        (false, true) => count(CallItem::Encrypted, Outcome::Only(si), 1),
        (false, false) => {}
    }
}

const ALGID_CLEAR: u8 = trunk_p25::voice::signalling::ALGID_CLEAR;

/// Every voice call so far.
pub fn voice_tally() -> VoiceTally {
    VOICE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_default()
}
