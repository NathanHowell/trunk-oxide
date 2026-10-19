//! Every P25 voice call's outcome, tallied by system and phase for metrics.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use crate::types::TrCallReport;

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

/// Cumulative call counts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CallTally {
    pub calls: u64,
    pub audio_seconds: f64,
    pub transmissions: u64,
    pub errors: u64,
    pub spikes: u64,
    /// Calls found encrypted.
    pub encrypted: u64,
    /// Voice frames the vocoder repeated, and muted, for errors.
    pub repeated: u64,
    pub muted: u64,
}

static CALLS: Mutex<BTreeMap<(String, Phase), CallTally>> = Mutex::new(BTreeMap::new());

pub fn observe_call(report: &TrCallReport) {
    let phase = if report.phase2 {
        Phase::Two
    } else {
        Phase::One
    };
    let call = CallTally {
        calls: 1,
        audio_seconds: report.audio_seconds,
        transmissions: u64::try_from(report.transmissions).unwrap_or(0),
        errors: u64::try_from(report.errors).unwrap_or(0),
        spikes: u64::try_from(report.spikes).unwrap_or(0),
        encrypted: u64::from(report.algid != trunk_p25::voice::signalling::ALGID_CLEAR),
        repeated: u64::try_from(report.repeated).unwrap_or(0),
        muted: u64::try_from(report.muted).unwrap_or(0),
    };
    CALLS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry((report.system.clone(), phase))
        .or_default()
        .add(&call);
}

impl CallTally {
    fn add(&mut self, o: &Self) {
        self.calls += o.calls;
        self.audio_seconds += o.audio_seconds;
        self.transmissions += o.transmissions;
        self.errors += o.errors;
        self.spikes += o.spikes;
        self.encrypted += o.encrypted;
        self.repeated += o.repeated;
        self.muted += o.muted;
    }
}

/// Every call so far, by system and phase.
pub fn call_tally() -> BTreeMap<(String, Phase), CallTally> {
    CALLS.lock().unwrap_or_else(PoisonError::into_inner).clone()
}
