//! Soft symbols to frames: frame sync, NID, status-symbol removal.
//!
//! Sync is a hard match within [`SYNC_MAX_ERRORS`] bits, like op25, relaxed to
//! [`SYNC_FLYWHEEL_ERRORS`] where the previous frame says the next one starts.
//! The NID is decoded with the hard BCH decoder until the NAC is learned
//! (two agreeing NIDs with ≤ 4 corrections); after that each NID is matched
//! against the seven valid codewords for that NAC with soft distances, which
//! keeps working far below the BCH decoder's limit.

use crate::bch;
use crate::symbols::{FRAME_SYNC, LEVELS, SYNC_DIBITS, dist2, is_status, slice};
use crate::types::Nac;

/// Bit errors allowed in a sync found by search.
pub const SYNC_MAX_ERRORS: u32 = 6;
/// Bit errors allowed in a sync at the position the last frame predicts.
pub const SYNC_FLYWHEEL_ERRORS: u32 = 12;
/// Bit errors a hard NID decode may correct before we trust it to learn the
/// NAC (op25's limit for every NID).
const NID_LEARN_ERRORS: u32 = 4;
/// Bit errors (hard decisions against the chosen codeword) allowed in a NID
/// matched against the learned NAC. Codewords sharing a NAC are ≥ 23 apart.
const NID_ML_ERRORS: u32 = 16;

const NID_START: usize = SYNC_DIBITS;
/// Sync, NID and the status dibit inside the NID.
pub const HEADER_DIBITS: usize = SYNC_DIBITS + 33;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Duid {
    Hdu = 0,
    Tdu = 3,
    Ldu1 = 5,
    Tsdu = 7,
    Ldu2 = 10,
    Pdu = 12,
    TduLc = 15,
}

impl Duid {
    pub const ALL: [Self; 7] = [
        Self::Hdu,
        Self::Tdu,
        Self::Ldu1,
        Self::Tsdu,
        Self::Ldu2,
        Self::Pdu,
        Self::TduLc,
    ];

    pub const fn from_bits(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Hdu,
            3 => Self::Tdu,
            5 => Self::Ldu1,
            7 => Self::Tsdu,
            10 => Self::Ldu2,
            12 => Self::Pdu,
            15 => Self::TduLc,
            _ => return None,
        })
    }

    /// The NID's 64th bit (TIA-102.BAAC): set for voice frames only.
    pub const fn parity(self) -> u64 {
        match self {
            Self::Ldu1 | Self::Ldu2 => 1,
            _ => 0,
        }
    }

    /// Frame length in dibits including sync, NID and status. PDUs grow once
    /// their header says how many blocks follow.
    pub const fn dibits(self) -> usize {
        match self {
            Self::Hdu => 396,
            Self::Tdu => 72,
            Self::Ldu1 | Self::Ldu2 => 864,
            Self::Tsdu => 360,
            Self::Pdu => frame_dibits_for(1),
            Self::TduLc => 216,
        }
    }
}

/// Dibits (including status) a frame needs to carry `blocks` 98-dibit blocks.
pub const fn frame_dibits_for(blocks: usize) -> usize {
    let data = SYNC_DIBITS + 32 + 98 * blocks;
    let mut total = 0;
    let mut have = 0;
    while have < data {
        if !is_status(total) {
            have += 1;
        }
        total += 1;
    }
    // A status dibit that falls right after the data still belongs to it.
    if is_status(total) {
        total += 1;
    }
    total
}

/// The 64-bit NID: BCH codeword of NAC and DUID, then the parity bit.
pub const fn nid_word(nac: Nac, duid: Duid) -> u64 {
    (bch::encode((nac.get() << 4) | duid as u16) << 1) | duid.parity()
}

/// A complete (or truncated) frame.
#[derive(Clone, Debug)]
pub struct Frame {
    pub duid: Duid,
    pub nac: Nac,
    /// Symbol index of the first sync dibit.
    pub start: u64,
    /// Soft symbols after the NID, status symbols removed.
    pub body: Vec<f32>,
    /// Every soft symbol of the frame from the first sync dibit, status
    /// included (the coordinates of the TIA bit tables).
    pub raw: Vec<f32>,
    pub nid_errors: u32,
    pub sync_errors: u32,
    /// The frame ended early (a new sync arrived first).
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FramerStats {
    pub syncs: u64,
    pub flywheel_syncs: u64,
    pub nid_ok_hard: u64,
    pub nid_ok_ml: u64,
    pub nid_failed: u64,
    pub frames: u64,
    /// Frames cut short by the next sync.
    pub truncated: u64,
    /// Bit errors summed over accepted syncs and NIDs.
    pub sync_bit_errors: u64,
    pub nid_bit_errors: u64,
}

struct Collecting {
    start: u64,
    sync_errors: u32,
    /// Dibits of the frame seen so far (sync included).
    seen: usize,
    nid: Vec<f32>,
    header: Option<(Duid, Nac, u32)>,
    body: Vec<f32>,
    raw: Vec<f32>,
    total: usize,
}

pub struct Framer {
    n: u64,
    acc: u64,
    /// The last `SYNC_DIBITS` soft symbols, a ring indexed by `n`.
    recent: [f32; SYNC_DIBITS],
    cur: Option<Collecting>,
    /// Where the frame just finished says the next sync starts.
    expect_sync_at: Option<u64>,
    nac: Option<Nac>,
    nac_candidate: Option<Nac>,
    /// The NAC must match this when set (op25 `set_nac`).
    required_nac: Option<Nac>,
    pub stats: FramerStats,
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framer {
    pub const fn new() -> Self {
        Self {
            n: 0,
            acc: 0,
            recent: [0.0; SYNC_DIBITS],
            cur: None,
            expect_sync_at: None,
            nac: None,
            nac_candidate: None,
            required_nac: None,
            stats: FramerStats {
                syncs: 0,
                flywheel_syncs: 0,
                nid_ok_hard: 0,
                nid_ok_ml: 0,
                nid_failed: 0,
                frames: 0,
                truncated: 0,
                sync_bit_errors: 0,
                nid_bit_errors: 0,
            },
        }
    }

    /// Pin the NAC (skips learning; frames with another NAC are dropped).
    pub const fn set_nac(&mut self, nac: Option<Nac>) {
        self.required_nac = nac;
        self.nac = nac;
    }

    pub const fn nac(&self) -> Option<Nac> {
        self.nac
    }

    /// Forget the frame in flight, timing and the learned NAC (after a
    /// retune). The symbol count and the counters carry on.
    pub fn reset(&mut self) {
        *self = Self {
            n: self.n,
            stats: self.stats,
            required_nac: self.required_nac,
            nac: self.required_nac,
            ..Self::new()
        };
    }

    /// Feed one soft symbol; completed frames are appended to `out`.
    pub fn push(&mut self, s: f32, out: &mut Vec<Frame>) {
        let idx = self.n;
        self.n += 1;
        self.acc = ((self.acc << 2) | u64::from(slice(s))) & ((1 << 48) - 1);
        #[allow(clippy::cast_possible_truncation)]
        {
            self.recent[(idx % SYNC_DIBITS as u64) as usize] = s;
        }

        if let Some(cur) = &mut self.cur {
            let k = cur.seen;
            cur.seen += 1;
            cur.raw.push(s);
            if (NID_START..HEADER_DIBITS).contains(&k) {
                if !is_status(k) {
                    cur.nid.push(s);
                }
            } else if k >= HEADER_DIBITS && !is_status(k) {
                cur.body.push(s);
            }
        }

        if self.cur.as_ref().is_some_and(|c| c.seen == HEADER_DIBITS) {
            self.finish_header();
        }

        // Sync search runs all the time; a sync ends whatever was in flight.
        let errors = (self.acc ^ FRAME_SYNC).count_ones();
        let start = (idx + 1).saturating_sub(SYNC_DIBITS as u64);
        let at_expected = self.expect_sync_at == Some(start)
            || self
                .cur
                .as_ref()
                .is_some_and(|c| c.header.is_some() && c.start + c.total as u64 == start);
        if self.n >= SYNC_DIBITS as u64
            && (errors <= SYNC_MAX_ERRORS || (at_expected && errors <= SYNC_FLYWHEEL_ERRORS))
        {
            if let Some(cur) = self.cur.take() {
                Self::emit(cur, true, &mut self.stats, out);
            }
            self.stats.syncs += 1;
            if errors > SYNC_MAX_ERRORS {
                self.stats.flywheel_syncs += 1;
            }
            self.expect_sync_at = None;
            self.cur = Some(Collecting {
                start,
                sync_errors: errors,
                seen: SYNC_DIBITS,
                nid: Vec::with_capacity(32),
                header: None,
                body: Vec::new(),
                raw: self.recent_sync(),
                total: 0,
            });
            return;
        }

        if self
            .cur
            .as_ref()
            .is_some_and(|c| c.header.is_some() && c.seen >= c.total)
            && let Some(cur) = self.cur.take()
        {
            self.expect_sync_at = Some(cur.start + cur.seen as u64);
            Self::emit(cur, false, &mut self.stats, out);
        }
    }

    /// The sync just received, oldest first.
    fn recent_sync(&self) -> Vec<f32> {
        #[allow(clippy::cast_possible_truncation)]
        let start = (self.n % SYNC_DIBITS as u64) as usize;
        self.recent[start..]
            .iter()
            .chain(&self.recent[..start])
            .copied()
            .collect()
    }

    /// Lengthen the frame in flight (a PDU header announced its blocks).
    pub fn extend_current(&mut self, total_dibits: usize) {
        if let Some(cur) = &mut self.cur {
            cur.total = cur.total.max(total_dibits);
        }
    }

    /// The earliest symbol a frame may still claim: the start of the frame in
    /// flight, or of a sync that may be arriving. Everything before it is
    /// settled.
    pub fn settled_before(&self) -> u64 {
        self.cur.as_ref().map_or_else(
            || self.n.saturating_sub(SYNC_DIBITS as u64 - 1),
            |c| c.start,
        )
    }

    /// Symbols seen so far.
    pub const fn position(&self) -> u64 {
        self.n
    }

    /// The frame in flight, once its header is known: (duid, body so far).
    pub fn current(&self) -> Option<(Duid, &[f32])> {
        let cur = self.cur.as_ref()?;
        cur.header.map(|(d, _, _)| (d, cur.body.as_slice()))
    }

    fn emit(cur: Collecting, truncated: bool, stats: &mut FramerStats, out: &mut Vec<Frame>) {
        let Some((duid, nac, nid_errors)) = cur.header else {
            return;
        };
        stats.frames += 1;
        let truncated = truncated && cur.seen < cur.total;
        stats.truncated += u64::from(truncated);
        stats.sync_bit_errors += u64::from(cur.sync_errors);
        stats.nid_bit_errors += u64::from(nid_errors);
        out.push(Frame {
            duid,
            nac,
            start: cur.start,
            body: cur.body,
            raw: cur.raw,
            nid_errors,
            sync_errors: cur.sync_errors,
            truncated,
        });
    }

    fn finish_header(&mut self) {
        let Some(cur) = &mut self.cur else { return };
        if let Some(NidResult {
            duid,
            nac,
            errors,
            ml,
        }) = decode_nid(&cur.nid, self.nac, &mut self.nac_candidate)
        {
            if self.required_nac.is_some_and(|r| r != nac) {
                self.stats.nid_failed += 1;
                self.cur = None;
                return;
            }
            if ml {
                self.stats.nid_ok_ml += 1;
            } else {
                self.stats.nid_ok_hard += 1;
            }
            cur.header = Some((duid, nac, errors));
            cur.total = duid.dibits();
            if self.required_nac.is_none() && !ml {
                // Learn (or re-learn) the NAC from two agreeing NIDs.
                if self.nac != Some(nac) {
                    if errors <= NID_LEARN_ERRORS && self.nac_candidate == Some(nac) {
                        self.nac = Some(nac);
                    } else if errors <= NID_LEARN_ERRORS {
                        self.nac_candidate = Some(nac);
                    }
                }
            }
        } else {
            self.stats.nid_failed += 1;
            self.cur = None;
        }
    }
}

struct NidResult {
    duid: Duid,
    nac: Nac,
    errors: u32,
    ml: bool,
}

fn decode_nid(
    soft: &[f32],
    learned: Option<Nac>,
    candidate: &mut Option<Nac>,
) -> Option<NidResult> {
    debug_assert_eq!(soft.len(), 32);
    let hard = soft
        .iter()
        .fold(0u64, |acc, &s| (acc << 2) | u64::from(slice(s)));

    let hard_result = bch::decode(hard >> 1).and_then(|(cw, errors)| {
        let info = bch::info(cw);
        let nac = Nac::new(info >> 4)?;
        #[allow(clippy::cast_possible_truncation)]
        let duid = Duid::from_bits((info & 0xf) as u8)?;
        (hard & 1 == duid.parity()).then_some(NidResult {
            duid,
            nac,
            errors,
            ml: false,
        })
    });

    let Some(nac) = learned else {
        return hard_result.filter(|r| r.errors <= NID_LEARN_ERRORS);
    };

    // A clean NID for another NAC: let it through so the framer can relearn
    // (a retune to another site), but it must be as clean as a learning NID.
    if let Some(r) = &hard_result
        && r.nac != nac
    {
        if r.errors <= NID_LEARN_ERRORS {
            *candidate = Some(r.nac);
            return hard_result;
        }
        return None;
    }

    let mut best: Option<(f32, Duid, u64)> = None;
    for duid in Duid::ALL {
        let word = nid_word(nac, duid);
        let metric: f32 = soft
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                #[allow(clippy::cast_possible_truncation)]
                let d = ((word >> (62 - 2 * i)) & 3) as u8;
                dist2(s, d)
            })
            .sum();
        if best.is_none_or(|(m, _, _)| metric < m) {
            best = Some((metric, duid, word));
        }
    }
    let (_, duid, word) = best?;
    let errors = (word ^ hard).count_ones();
    (errors <= NID_ML_ERRORS).then_some(NidResult {
        duid,
        nac,
        errors,
        ml: true,
    })
}

/// Ideal soft symbols for a dibit stream.
pub fn ideal(dibits: &[u8]) -> Vec<f32> {
    dibits.iter().map(|&d| LEVELS[usize::from(d)]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::{self, Rng};

    const NAC: Nac = Nac::new(0x38d).unwrap();
    const OTHER: Nac = Nac::new(0x123).unwrap();

    fn run(framer: &mut Framer, dibits: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        for s in ideal(dibits) {
            framer.push(s, &mut out);
        }
        out
    }

    #[test]
    fn frame_lengths_are_consistent_with_status_spacing() {
        assert_eq!(frame_dibits_for(3), 360);
        assert_eq!(testgen::tsdu(NAC, &[[0; 10]; 3]).len(), 360);
        assert_eq!(testgen::nid_info(NAC, Duid::Tsdu), (NAC.get() << 4) | 7);
    }

    #[test]
    fn back_to_back_tsdus_are_framed() {
        let mut rng = Rng::new(11);
        let mut stream = vec![2u8; 100];
        let mut sent = Vec::new();
        for _ in 0..5 {
            let t = testgen::random_tsbks(&mut rng, 3);
            stream.extend(testgen::tsdu(NAC, &t));
            sent.push(t);
        }
        stream.extend(vec![0u8; 400]);
        let mut f = Framer::new();
        let frames = run(&mut f, &stream);
        assert_eq!(frames.len(), 5);
        for (fr, t) in frames.iter().zip(&sent) {
            assert_eq!((fr.duid, fr.nac, fr.truncated), (Duid::Tsdu, NAC, false));
            assert_eq!(fr.body.len(), 3 * 98);
            assert_eq!(fr.raw.len(), 360);
            assert_eq!(
                fr.raw[..SYNC_DIBITS],
                ideal(&crate::symbols::sync_dibits())[..]
            );
            let d = crate::trellis::decode(&fr.body.as_chunks().0[0]);
            assert_eq!(&d.data[..10], &testgen::tsbk_block(&t[0], false)[..10]);
        }
        assert_eq!(f.nac(), Some(NAC));
    }

    #[test]
    fn learned_nac_rescues_badly_damaged_nids() {
        let mut rng = Rng::new(12);
        let mut stream = Vec::new();
        for i in 0..6 {
            let mut fr = testgen::tsdu(NAC, &testgen::random_tsbks(&mut rng, 3));
            if i >= 2 {
                // Eight dibit errors (well past op25's 4-bit BCH limit) in the NID.
                for j in 0..8 {
                    let p = 25 + 4 * j;
                    fr[p] ^= 1;
                }
            }
            stream.extend(fr);
        }
        let mut f = Framer::new();
        let frames = run(&mut f, &stream);
        assert_eq!(frames.len(), 6);
        assert!(f.stats.nid_ok_ml >= 4);
    }

    #[test]
    fn another_nac_is_relearned() {
        let mut rng = Rng::new(13);
        let mut stream = Vec::new();
        for nac in [NAC, NAC, NAC, OTHER, OTHER, OTHER] {
            stream.extend(testgen::tsdu(nac, &testgen::random_tsbks(&mut rng, 3)));
        }
        let mut f = Framer::new();
        let frames = run(&mut f, &stream);
        assert_eq!(
            frames.iter().map(|f| f.nac).collect::<Vec<_>>(),
            [NAC, NAC, NAC, OTHER, OTHER, OTHER]
        );
        assert_eq!(f.nac(), Some(OTHER));
    }

    #[test]
    fn flywheel_accepts_a_damaged_sync_where_one_is_due() {
        let mut rng = Rng::new(14);
        let mut stream = Vec::new();
        for i in 0..3 {
            let mut fr = testgen::tsdu(NAC, &testgen::random_tsbks(&mut rng, 3));
            if i == 2 {
                for p in [0, 3, 6, 9, 12] {
                    fr[p] ^= 3; // 5 dibits, 10 bit errors
                }
            }
            stream.extend(fr);
        }
        let mut f = Framer::new();
        let frames = run(&mut f, &stream);
        assert_eq!(frames.len(), 3);
        assert_eq!(f.stats.flywheel_syncs, 1);
    }
}
