//! Burst framing: soft symbols to bursts, each with its superframe slot.
//!
//! op25 hunts for the S-ISCH word in every 180-dibit window (≤ 4 bit errors),
//! keeps framing for ten bursts after each one it finds, and takes the slot
//! from the last I-ISCH it could look up (≤ 7 bit errors) — so a weak
//! channel, whose sync words are rarely clean, keeps falling out of sync.
//!
//! Here sync is acquired the same way, but then held as a flywheel: bursts
//! are 180 dibits apart, every burst's ISCH is checked at the expected place
//! and one symbol either side (to follow timing slips), and is decoded
//! maximum-likelihood against only the words its slot allows. The slot phase
//! comes from the first I-ISCH; bursts seen before it are held, then released
//! with their slots. Lock is lost only after a run of ISCHs that look like
//! noise.

use std::collections::VecDeque;

use super::isch::{self, BITS as ISCH_BITS, Isch, IschDecision, S_ISCH};
use super::layout::{BURST_DIBITS, BURSTS_PER_SUPERFRAME, ISCH_DIBITS};
use crate::symbols::{bit_llrs, slice};

/// op25's sync tolerance.
const SYNC_ERRORS: u32 = 4;
/// Hard bit errors in an ISCH (of 40) that count as noise.
const NOISE_ERRORS: u32 = 12;
/// Consecutive noise-like ISCHs that end lock.
const MAX_MISSES: u32 = 6;
/// An I-ISCH this clean, announcing another slot, re-phases the superframe.
const REPHASE_ERRORS: u32 = 2;
/// How much cheaper (total |LLR|) an ISCH one symbol off must be to move the
/// burst boundary: about two bits' worth. Swept on the golden taps; anything
/// from 4 to 16 does as well, never moving does markedly worse.
const SLIP_EVIDENCE: f32 = 8.0;
/// Bursts held while the superframe phase is unknown.
const MAX_PENDING: usize = 4;

/// One burst: 180 soft symbols from the first ISCH dibit.
#[derive(Clone, Debug, PartialEq)]
pub struct Burst {
    /// Symbol index of the first ISCH dibit.
    pub start: u64,
    /// Superframe slot, 0–11.
    pub slot: usize,
    /// Bursts since lock, counting slots (so 12 per superframe).
    pub sequence: u64,
    pub isch: IschDecision,
    pub soft: Vec<f32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BurstFramerStats {
    pub acquisitions: u64,
    pub losses: u64,
    pub bursts: u64,
    /// Bursts whose ISCH came out at another offset than expected.
    pub slips: u64,
    pub rephases: u64,
    /// Hard bit errors in the ISCH words of emitted bursts.
    pub isch_errors: u64,
}

#[derive(Clone, Debug)]
enum State {
    Search,
    Locked {
        /// Where the next burst's ISCH should start.
        next: u64,
        /// The next burst's slot, once known.
        slot: Option<usize>,
        sequence: u64,
        misses: u32,
        /// Bursts awaiting the superframe phase.
        pending: Vec<Burst>,
    },
}

#[derive(Clone, Debug)]
pub struct BurstFramer {
    buf: VecDeque<f32>,
    /// Symbol index of `buf[0]`.
    buf_start: u64,
    state: State,
    pub stats: BurstFramerStats,
}

impl Default for BurstFramer {
    fn default() -> Self {
        Self::new()
    }
}

fn isch_llrs(soft: &[f32]) -> [f32; ISCH_BITS] {
    std::array::from_fn(|i| bit_llrs(soft[i / 2])[i % 2])
}

impl BurstFramer {
    pub const fn new() -> Self {
        Self {
            buf: VecDeque::new(),
            buf_start: 0,
            state: State::Search,
            stats: BurstFramerStats {
                acquisitions: 0,
                losses: 0,
                bursts: 0,
                slips: 0,
                rephases: 0,
                isch_errors: 0,
            },
        }
    }

    /// Drop sync (a new call). The symbol count carries on.
    pub fn reset(&mut self) {
        self.state = State::Search;
    }

    /// Symbols taken in.
    pub fn position(&self) -> u64 {
        self.buf_start + self.buf.len() as u64
    }

    pub const fn locked(&self) -> bool {
        matches!(self.state, State::Locked { .. })
    }

    /// Every symbol before this index is final: no burst will claim it.
    pub fn settled_before(&self) -> u64 {
        match &self.state {
            State::Search => self.position().saturating_sub(ISCH_DIBITS as u64),
            State::Locked { next, pending, .. } => {
                pending.first().map_or(*next, |b| b.start).saturating_sub(1)
            }
        }
    }

    fn window(&self, start: u64, len: usize) -> Option<Vec<f32>> {
        let from = usize::try_from(start.checked_sub(self.buf_start)?).ok()?;
        (from + len <= self.buf.len()).then(|| self.buf.range(from..from + len).copied().collect())
    }

    pub fn push(&mut self, s: f32, out: &mut Vec<Burst>) {
        self.buf.push_back(s);
        match self.state {
            State::Search => self.search(),
            State::Locked { next, .. } => {
                // The burst, and one symbol beyond its start for the late check.
                if self.position() > next + BURST_DIBITS as u64 {
                    self.frame(out);
                }
            }
        }
        self.trim();
    }

    fn search(&mut self) {
        let n = self.buf.len();
        if n < ISCH_DIBITS {
            return;
        }
        let word = self
            .buf
            .range(n - ISCH_DIBITS..)
            .fold(0u64, |w, &s| (w << 2) | u64::from(slice(s)));
        if (word ^ S_ISCH).count_ones() <= SYNC_ERRORS {
            self.stats.acquisitions += 1;
            self.state = State::Locked {
                next: self.position() - ISCH_DIBITS as u64,
                slot: None,
                sequence: 0,
                misses: 0,
                pending: Vec::new(),
            };
        }
    }

    fn decide(&self, at: u64, slot: Option<usize>) -> Option<IschDecision> {
        let llr = isch_llrs(&self.window(at, ISCH_DIBITS)?);
        Some(slot.map_or_else(|| isch::decode(&llr), |s| isch::decode_for_slot(&llr, s)))
    }

    fn frame(&mut self, out: &mut Vec<Burst>) {
        let State::Locked {
            next,
            slot,
            sequence,
            ..
        } = self.state.clone()
        else {
            return;
        };
        // The ISCH at the expected place or one symbol either side.
        let mut best: Option<(u64, IschDecision)> = None;
        for at in [next, next.saturating_sub(1), next + 1] {
            if let Some(d) = self.decide(at, slot) {
                let better = best.is_none_or(|(_, b)| d.cost + SLIP_EVIDENCE < b.cost);
                if better {
                    best = Some((at, d));
                }
            }
        }
        let Some((start, mut decision)) = best else {
            return;
        };
        if start != next {
            self.stats.slips += 1;
        }
        let mut slot = slot;
        // An unconstrained look: a clean I-ISCH fixes (or corrects) the phase.
        let free = self.decide(start, None);
        if let Some(f) = free
            && let Isch::Info(info) = f.isch
            && let Some(s) = info.slot()
            && f.errors <= REPHASE_ERRORS
            && slot != Some(s)
        {
            if slot.is_some() {
                self.stats.rephases += 1;
            }
            slot = Some(s);
            decision = f;
        }
        let soft = self.window(start, BURST_DIBITS).unwrap_or_default();
        let State::Locked {
            next,
            slot: next_slot,
            sequence: next_sequence,
            misses,
            pending,
        } = &mut self.state
        else {
            return;
        };
        *next = start + BURST_DIBITS as u64;
        *misses = if decision.errors >= NOISE_ERRORS {
            *misses + 1
        } else {
            0
        };
        let burst = Burst {
            start,
            slot: slot.unwrap_or(0),
            sequence,
            isch: decision,
            soft,
        };
        if let Some(s) = slot {
            // Release held bursts, counting back from this one.
            let held = pending.len();
            for (k, mut b) in pending.drain(..).enumerate() {
                let back = held - k;
                b.slot = (s + BURSTS_PER_SUPERFRAME * back - back) % BURSTS_PER_SUPERFRAME;
                b.isch = isch::decode_for_slot(&isch_llrs(&b.soft), b.slot);
                b.sequence = sequence - back as u64;
                self.stats.bursts += 1;
                self.stats.isch_errors += u64::from(b.isch.errors);
                out.push(b);
            }
            self.stats.bursts += 1;
            self.stats.isch_errors += u64::from(burst.isch.errors);
            out.push(burst);
            *next_slot = Some((s + 1) % BURSTS_PER_SUPERFRAME);
        } else {
            pending.push(burst);
            if pending.len() > MAX_PENDING {
                pending.remove(0);
            }
        }
        *next_sequence = sequence + 1;
        if *misses >= MAX_MISSES {
            self.stats.losses += 1;
            self.state = State::Search;
        }
    }

    fn trim(&mut self) {
        let keep_from = match &self.state {
            State::Search => self.position().saturating_sub(ISCH_DIBITS as u64),
            State::Locked { next, pending, .. } => {
                pending.first().map_or(*next, |b| b.start).saturating_sub(2)
            }
        };
        while self.buf_start < keep_from && !self.buf.is_empty() {
            self.buf.pop_front();
            self.buf_start += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framer::ideal;
    use crate::tdma::isch::IschInfo;
    use crate::testgen::Rng;

    /// A superframe of bursts with correct ISCHs and random bodies.
    pub fn bursts(rng: &mut Rng, first_slot: usize, count: usize) -> Vec<u8> {
        let mut d = Vec::new();
        for k in 0..count {
            let slot = (first_slot + k) % 12;
            let isch = if crate::tdma::layout::is_sync_slot(slot) {
                Isch::Sync
            } else {
                #[allow(clippy::cast_possible_truncation)]
                Isch::Info(IschInfo::new(((slot & 3) << 5 | (slot / 4) << 3) as u8))
            };
            let cw = isch.codeword();
            #[allow(clippy::cast_possible_truncation)]
            d.extend((0..20).map(|i| ((cw >> (38 - 2 * i)) & 3) as u8));
            #[allow(clippy::cast_possible_truncation)]
            d.extend((0..160).map(|_| (rng.next_u64() & 3) as u8));
        }
        d
    }

    #[test]
    fn frames_a_clean_stream_with_slots() {
        let mut rng = Rng::new(21);
        #[allow(clippy::cast_possible_truncation)]
        let mut d: Vec<u8> = (0..77).map(|_| (rng.next_u64() & 3) as u8).collect();
        d.extend(bursts(&mut rng, 2, 40));
        let mut f = BurstFramer::new();
        let mut out = Vec::new();
        for s in ideal(&d) {
            f.push(s, &mut out);
        }
        // Acquired at the first S-ISCH (slot 2); the last burst needs one
        // more symbol.
        assert_eq!(out.len(), 39);
        for (k, b) in out.iter().enumerate() {
            assert_eq!(b.start, 77 + 180 * k as u64);
            assert_eq!(b.slot, (2 + k) % 12);
            assert_eq!(b.isch.errors, 0);
            assert_eq!(b.soft.len(), 180);
        }
        assert_eq!(f.stats.slips, 0);
    }

    #[test]
    fn follows_a_slip_and_survives_a_fade() {
        let mut rng = Rng::new(22);
        let mut d = bursts(&mut rng, 6, 20);
        d.remove(180 * 8 + 50); // a symbol lost
        for x in &mut d[180 * 12..180 * 14] {
            *x ^= (rng.next_u64() & 3) as u8; // two bursts of garbage
        }
        let mut f = BurstFramer::new();
        let mut out = Vec::new();
        for s in ideal(&d) {
            f.push(s, &mut out);
        }
        assert_eq!(f.stats.slips, 1);
        assert_eq!(f.stats.losses, 0);
        let slots: Vec<usize> = out.iter().map(|b| b.slot).collect();
        let want: Vec<usize> = (0..slots.len()).map(|k| (6 + k) % 12).collect();
        assert_eq!(slots, want);
    }
}
