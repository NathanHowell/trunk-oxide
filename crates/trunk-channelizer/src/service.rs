//! Thread-safe front end for embedding the channelizer in a streaming host
//! (the GNU Radio block in trunk-recorder).
//!
//! Control calls (`set_slot`, `set_slot_filter`) may come from any thread and
//! only touch a small mailbox. `process` runs on the data thread, drains the
//! mailbox *between blocks* so a retune can never race the DSP, and copies
//! each active slot's output into a caller-provided slot-indexed buffer. A
//! slot is one consumer's port: it has a profile, a frequency and a filter,
//! and the filter persists while the slot is disabled.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::Result;
use num_complex::Complex32;

use crate::channelizer::{ChannelId, Channelizer};
use crate::conditioner::{Conditioner, ConditionerParams, SquelchEvent, SquelchMode};
use crate::config::{Config, ProfileId};
use crate::detector::{Detection, DetectorConfig, SignalDetector};

/// When a slot's conditioner (AGC, FLL) starts over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResetPolicy {
    /// Never: state carries across disable/enable and retunes, as the GNU
    /// Radio blocks it replaces did.
    #[default]
    Never,
    /// When a disabled slot is enabled.
    Enable,
    /// On enable, and whenever an enabled slot is retuned.
    Retune,
}

/// Conditioner settings shared by every slot of a [`SharedChannelizer`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ConditioningSettings {
    pub params: ConditionerParams,
    pub reset: ResetPolicy,
}

/// How a slot is conditioned (see [`SharedChannelizer::set_slot_conditioning`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConditioningSpec {
    pub sps: u32,
    pub excess_bw: f32,
    /// The slot's channel rate and the rate the consumer wants; a resampler
    /// runs first when `out_rate < in_rate`.
    pub in_rate: f64,
    pub out_rate: f64,
    /// Power squelch before the AGC.
    pub squelch: Option<SquelchMode>,
}

/// Consumes a slot's conditioned output on the DSP thread: a protocol
/// decoder (P25 control channel, …) that lives beside the conditioner, so its
/// input never crosses into GNU Radio. Results leave through whatever shared
/// state the implementation holds.
pub trait ChannelDecoder: Send + std::fmt::Debug {
    /// One block of the slot's conditioned output.
    fn process(&mut self, iq: &[Complex32]);
    /// The slot was retuned to another channel: drop timing and lock state.
    fn retune(&mut self);
}

/// A retune smaller than this (fine tuning) keeps the decoder's state.
pub const DECODER_RETUNE_HZ: f64 = 1_000.0;

/// `xlat_channelizer::get_freq_error` scales the FLL by 24 kHz whatever the
/// channel rate; kept so the reported error means the same thing.
const FREQ_ERROR_RATE: f64 = 24_000.0;

/// What a conditioned slot reports; readable from any thread.
#[derive(Debug)]
pub struct SlotStats {
    freq_error_hz: AtomicI32,
    /// `f64` bits; NaN until the first block.
    power_db: AtomicU64,
    squelch_open: AtomicBool,
    events: Mutex<VecDeque<SquelchEvent>>,
}

impl SlotStats {
    fn new() -> Self {
        Self {
            freq_error_hz: AtomicI32::new(0),
            power_db: AtomicU64::new(f64::NAN.to_bits()),
            squelch_open: AtomicBool::new(true),
            events: Mutex::new(VecDeque::new()),
        }
    }
    pub fn squelch_open(&self) -> bool {
        self.squelch_open.load(Ordering::Relaxed)
    }
    /// Oldest squelch transition not yet taken.
    pub fn next_event(&self) -> Option<SquelchEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }
    pub fn freq_error_hz(&self) -> i32 {
        self.freq_error_hz.load(Ordering::Relaxed)
    }
    pub fn power_db(&self) -> f64 {
        f64::from_bits(self.power_db.load(Ordering::Relaxed))
    }
}

#[derive(Debug)]
enum Cmd {
    /// Enable (or retune) a slot at an offset from the source centre.
    Set {
        slot: usize,
        profile: ProfileId,
        freq_hz: f64,
    },
    /// Set the slot's filter; it persists across [`Cmd::Clear`] and retunes.
    Filter {
        slot: usize,
        cutoff_hz: f64,
        transition_hz: f64,
    },
    Clear {
        slot: usize,
    },
    /// Condition the slot's output (or change its samples per symbol).
    Condition {
        slot: usize,
        spec: ConditioningSpec,
        stats: Arc<SlotStats>,
    },
    SquelchDb {
        slot: usize,
        db: f64,
    },
    Decoder {
        slot: usize,
        decoder: Option<Box<dyn ChannelDecoder>>,
    },
    /// Run (or stop) the wideband signal detector on the source input.
    Detector(Option<DetectorConfig>),
}

/// Per-slot result of one [`SharedChannelizer::process`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Slots rejected because the requested offset is outside the passband.
    pub rejected: u64,
    /// Blocks processed.
    pub blocks: u64,
    /// Blocks whose DSP took longer than the block's real-time duration
    /// (`hop / sample_rate`): the channelizer fell behind the sample stream.
    pub overruns: u64,
    /// Total DSP time across all blocks, in nanoseconds.
    pub busy_ns: u64,
    /// Longest single block since the last [`SharedChannelizer::take_counters`].
    pub max_ns: u64,
    /// Channels currently enabled.
    pub active: u64,
}

pub struct SharedChannelizer {
    cfg: Config,
    cond: ConditioningSettings,
    stats: Mutex<Vec<Option<Arc<SlotStats>>>>,
    mailbox: Mutex<Vec<Cmd>>,
    dsp: Mutex<Dsp>,
    /// What the signal detector found, until taken.
    detections: Mutex<Vec<Detection>>,
}

struct Slot {
    id: Option<ChannelId>,
    /// `(cutoff_hz, transition_hz)`; `None` = the profile's default.
    filter: Option<(f64, f64)>,
    freq_hz: f64,
    cond: Option<(Conditioner, ConditioningSpec, Arc<SlotStats>)>,
    decoder: Option<Box<dyn ChannelDecoder>>,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            id: None,
            filter: None,
            freq_hz: 0.0,
            cond: None,
            decoder: None,
        }
    }
}

struct Dsp {
    ch: Channelizer,
    slots: Vec<Slot>,
    counters: Counters,
    detector: Option<SignalDetector>,
    found: Vec<Detection>,
    /// Conditioned output of one slot, for [`SharedChannelizer::run`].
    scratch: Vec<Complex32>,
}

impl SharedChannelizer {
    pub fn new(cfg: Config) -> Result<Self> {
        Self::with_conditioning(cfg, ConditioningSettings::default())
    }

    pub fn with_conditioning(cfg: Config, cond: ConditioningSettings) -> Result<Self> {
        let ch = Channelizer::new(cfg.clone())?;
        Ok(Self {
            cfg,
            cond,
            stats: Mutex::new(Vec::new()),
            mailbox: Mutex::new(Vec::new()),
            dsp: Mutex::new(Dsp {
                ch,
                slots: Vec::new(),
                counters: Counters::default(),
                detector: None,
                found: Vec::new(),
                scratch: Vec::new(),
            }),
            detections: Mutex::new(Vec::new()),
        })
    }

    /// Look for signals across the whole source input (see
    /// [`SignalDetector`]); `None` stops. Findings wait in
    /// [`Self::take_detections`].
    pub fn set_signal_detector(&self, cfg: Option<DetectorConfig>) {
        self.post(Cmd::Detector(cfg));
    }

    /// Everything the signal detector has found since the last call.
    pub fn take_detections(&self) -> Vec<Detection> {
        std::mem::take(
            &mut *self
                .detections
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    pub const fn config(&self) -> &Config {
        &self.cfg
    }

    /// Enable `slot` as a `profile` channel at `freq_hz` from the source
    /// centre, or retune it if already enabled. Applied before the next block.
    pub fn set_slot(&self, slot: usize, profile: ProfileId, freq_hz: f64) {
        self.post(Cmd::Set {
            slot,
            profile,
            freq_hz,
        });
    }

    /// Give `slot` its own filter. Remembered while the slot is disabled.
    pub fn set_slot_filter(&self, slot: usize, cutoff_hz: f64, transition_hz: f64) {
        self.post(Cmd::Filter {
            slot,
            cutoff_hz,
            transition_hz,
        });
    }

    /// Condition `slot`'s output: resample (if `spec` asks), squelch (if it
    /// asks), AGC and the band-edge FLL. Calling it again changes the
    /// samples per symbol and the squelch mode in place; other changes start
    /// a new conditioner. It persists while the slot is disabled.
    pub fn set_slot_conditioning(&self, slot: usize, spec: ConditioningSpec) {
        let stats = {
            let mut v = self.stats.lock().unwrap_or_else(PoisonError::into_inner);
            if v.len() <= slot {
                v.resize(slot + 1, None);
            }
            Arc::clone(v[slot].get_or_insert_with(|| Arc::new(SlotStats::new())))
        };
        self.post(Cmd::Condition { slot, spec, stats });
    }

    /// Squelch threshold of a conditioned slot (dB).
    pub fn set_slot_squelch_db(&self, slot: usize, db: f64) {
        self.post(Cmd::SquelchDb { slot, db });
    }

    /// False while a conditioned slot's squelch is closed.
    pub fn slot_squelch_open(&self, slot: usize) -> bool {
        self.slot_stats(slot).is_none_or(|s| s.squelch_open())
    }

    /// Oldest squelch transition of a slot not yet taken.
    pub fn next_squelch_event(&self, slot: usize) -> Option<SquelchEvent> {
        self.slot_stats(slot).and_then(|s| s.next_event())
    }

    /// FLL frequency error (Hz) of a conditioned slot, 0 if none.
    pub fn slot_freq_error_hz(&self, slot: usize) -> i32 {
        self.slot_stats(slot).map_or(0, |s| s.freq_error_hz())
    }

    /// Smoothed input power (dB) of a conditioned slot; NaN if none yet.
    pub fn slot_power_db(&self, slot: usize) -> f64 {
        self.slot_stats(slot).map_or(f64::NAN, |s| s.power_db())
    }

    fn slot_stats(&self, slot: usize) -> Option<Arc<SlotStats>> {
        self.stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(slot)
            .cloned()
            .flatten()
    }

    /// Run `decoder` on `slot`'s conditioned output (or stop, with `None`).
    /// It persists while the slot is disabled and is told about retunes of
    /// more than [`DECODER_RETUNE_HZ`].
    pub fn set_slot_decoder(&self, slot: usize, decoder: Option<Box<dyn ChannelDecoder>>) {
        self.post(Cmd::Decoder { slot, decoder });
    }

    /// Disable `slot` (it produces no output from the next block on).
    pub fn clear_slot(&self, slot: usize) {
        self.post(Cmd::Clear { slot });
    }

    fn post(&self, cmd: Cmd) {
        self.mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(cmd);
    }

    pub fn counters(&self) -> Counters {
        self.dsp
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .counters
    }

    /// Like [`Self::counters`], but also resets `max_ns` so the next call
    /// reports the longest block of the interval in between.
    pub fn take_counters(&self) -> Counters {
        let mut dsp = self
            .dsp
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = dsp.counters;
        dsp.counters.max_ns = 0;
        snapshot
    }

    /// Real-time duration of one block, in nanoseconds (the overrun threshold).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn block_budget_ns(&self) -> u64 {
        (self.cfg.hop() as f64 / self.cfg.sample_rate * 1e9) as u64
    }

    /// Consume one block ([`Config::hop`] samples). For every active slot
    /// `s < produced.len()`, write its `out_per_block(profile)` samples to
    /// `out[s * stride ..]` (`stride` = [`Config::max_out_per_block`]) and set
    /// `produced[s]` to that count; other entries of `produced` are set to 0.
    ///
    /// # Panics
    /// If `input.len() != hop` or `out.len() < produced.len() * stride`.
    pub fn process(&self, input: &[Complex32], out: &mut [Complex32], produced: &mut [u32]) {
        let stride = self.cfg.max_out_per_block();
        assert!(
            out.len() >= produced.len() * stride,
            "output buffer too small"
        );
        self.run_block(input, Some((out, produced)));
    }

    /// Consume one block and run every slot's conditioner and decoder,
    /// keeping no output: for hosts whose decoders consume the slots.
    ///
    /// # Panics
    /// If `input.len() != hop`.
    pub fn run(&self, input: &[Complex32]) {
        self.run_block(input, None);
    }

    fn run_block(&self, input: &[Complex32], sink: Option<(&mut [Complex32], &mut [u32])>) {
        let stride = self.cfg.max_out_per_block();
        let cmds = std::mem::take(
            &mut *self
                .mailbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut dsp = self
            .dsp
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Dsp {
            ch,
            slots,
            counters,
            detector,
            found,
            scratch,
        } = &mut *dsp;
        let started = Instant::now();

        for cmd in cmds {
            match cmd {
                Cmd::Set {
                    slot,
                    profile,
                    freq_hz,
                } => {
                    if slots.len() <= slot {
                        slots.resize_with(slot + 1, Slot::empty);
                    }
                    let s = &mut slots[slot];
                    let was_enabled = s.id.is_some();
                    let moved = (s.freq_hz - freq_hz).abs() > f64::EPSILON;
                    let jumped = (s.freq_hz - freq_hz).abs() > DECODER_RETUNE_HZ;
                    // A slot keeps its profile while enabled; changing it
                    // means a new channel.
                    let same =
                        s.id.and_then(|id| ch.profile_of(id))
                            .is_some_and(|p| p == profile);
                    let applied = if let (true, Some(id)) = (same, s.id) {
                        ch.retune(id, freq_hz).is_ok()
                    } else {
                        if let Some(id) = s.id.take() {
                            ch.remove_channel(id);
                        }
                        match ch.add_channel(profile, freq_hz) {
                            Ok(id) => {
                                s.id = Some(id);
                                if let Some((c, t)) = s.filter
                                    && ch.set_filter(id, c, t).is_err()
                                {
                                    counters.rejected += 1;
                                }
                                true
                            }
                            Err(_) => false,
                        }
                    };
                    if !applied {
                        counters.rejected += 1;
                        if let Some(id) = s.id.take() {
                            ch.remove_channel(id);
                        }
                    } else {
                        s.freq_hz = freq_hz;
                        let reset = match self.cond.reset {
                            ResetPolicy::Never => false,
                            ResetPolicy::Enable => !was_enabled,
                            ResetPolicy::Retune => !was_enabled || moved,
                        };
                        if let (true, Some((c, _, _))) = (reset, s.cond.as_mut()) {
                            c.reset();
                        }
                        if let (true, Some(d)) = (jumped, s.decoder.as_mut()) {
                            d.retune();
                        }
                    }
                }
                Cmd::Filter {
                    slot,
                    cutoff_hz,
                    transition_hz,
                } => {
                    if slots.len() <= slot {
                        slots.resize_with(slot + 1, Slot::empty);
                    }
                    let s = &mut slots[slot];
                    s.filter = Some((cutoff_hz, transition_hz));
                    if let Some(id) = s.id
                        && ch.set_filter(id, cutoff_hz, transition_hz).is_err()
                    {
                        counters.rejected += 1;
                    }
                }
                Cmd::Condition { slot, spec, stats } => {
                    if slots.len() <= slot {
                        slots.resize_with(slot + 1, Slot::empty);
                    }
                    let s = &mut slots[slot];
                    let in_place = s.cond.as_ref().is_some_and(|(_, old, _)| {
                        old.excess_bw.to_bits() == spec.excess_bw.to_bits()
                            && old.in_rate.to_bits() == spec.in_rate.to_bits()
                            && old.out_rate.to_bits() == spec.out_rate.to_bits()
                            && old.squelch.is_some() == spec.squelch.is_some()
                    });
                    if let (true, Some((c, old, _))) = (in_place, s.cond.as_mut()) {
                        c.set_samples_per_symbol(spec.sps);
                        if let Some(m) = spec.squelch {
                            c.set_squelch_mode(m);
                        }
                        *old = spec;
                    } else {
                        s.cond = Some((build_conditioner(&spec, self.cond.params), spec, stats));
                    }
                }
                Cmd::SquelchDb { slot, db } => {
                    if let Some((c, _, _)) = slots.get_mut(slot).and_then(|s| s.cond.as_mut()) {
                        c.set_squelch_db(db);
                    }
                }
                Cmd::Decoder { slot, decoder } => {
                    if slots.len() <= slot {
                        slots.resize_with(slot + 1, Slot::empty);
                    }
                    slots[slot].decoder = decoder;
                }
                Cmd::Clear { slot } => {
                    if let Some(id) = slots.get_mut(slot).and_then(|s| s.id.take()) {
                        ch.remove_channel(id);
                    }
                }
                Cmd::Detector(cfg) => {
                    *detector = cfg.map(|c| SignalDetector::new(self.cfg.sample_rate, c));
                }
            }
        }

        if let Some(d) = detector.as_mut() {
            found.clear();
            d.push(input, found);
            if !found.is_empty() {
                self.detections
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .append(found);
            }
        }
        ch.process_block(input);
        counters.blocks += 1;

        let run_slot = |s: &mut Slot, dst: &mut [Complex32]| -> Option<usize> {
            let o = ch.output(s.id?)?;
            let n = match s.cond.as_mut() {
                Some((c, _, stats)) => {
                    let n = c.process_into(o, dst);
                    publish(c, stats);
                    n
                }
                None => {
                    dst[..o.len()].copy_from_slice(o);
                    o.len()
                }
            };
            if let Some(d) = s.decoder.as_mut() {
                d.process(&dst[..n]);
            }
            Some(n)
        };
        match sink {
            Some((out, produced)) => {
                produced.fill(0);
                for (slot, s) in slots.iter_mut().enumerate().take(produced.len()) {
                    let dst = &mut out[slot * stride..(slot + 1) * stride];
                    if let Some(n) = run_slot(s, dst) {
                        produced[slot] = u32::try_from(n).unwrap_or(u32::MAX);
                    }
                }
            }
            None => {
                if scratch.len() < stride {
                    scratch.resize(stride, Complex32::ZERO);
                }
                for s in slots.iter_mut() {
                    run_slot(s, scratch);
                }
            }
        }

        let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        counters.busy_ns = counters.busy_ns.saturating_add(elapsed);
        counters.max_ns = counters.max_ns.max(elapsed);
        counters.overruns += u64::from(elapsed > self.block_budget_ns());
        counters.active = slots.iter().filter(|s| s.id.is_some()).count() as u64;
    }
}

fn build_conditioner(spec: &ConditioningSpec, params: ConditionerParams) -> Conditioner {
    #[allow(clippy::cast_possible_truncation)]
    let mut c = Conditioner::new(spec.sps, spec.excess_bw, params)
        .with_resampler((spec.out_rate / spec.in_rate) as f32);
    if let Some(m) = spec.squelch {
        // callback_pwr_squelch_cc starts at 0 dB until the recorder sets it
        c = c.with_squelch(m, 0.0);
    }
    c
}

fn publish(c: &mut Conditioner, stats: &SlotStats) {
    stats
        .freq_error_hz
        .store(c.freq_error_hz(FREQ_ERROR_RATE), Ordering::Relaxed);
    stats.power_db.store(
        c.power_db().unwrap_or(f64::NAN).to_bits(),
        Ordering::Relaxed,
    );
    stats
        .squelch_open
        .store(c.squelch_open(), Ordering::Relaxed);
    let mut ev = c.take_events().peekable();
    if ev.peek().is_some() {
        stats
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(ev);
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use super::*;
    use crate::config::{NARROW, WIDE};

    fn tone(cfg: &Config, f: f64, start: usize, len: usize) -> Vec<Complex32> {
        (start..start + len)
            .map(|n| Complex32::from_polar(1.0, (TAU * f * n as f64 / cfg.sample_rate) as f32))
            .collect()
    }

    #[test]
    fn slots_follow_commands_between_blocks() {
        let cfg = Config::standard(2_496_000.0).unwrap();
        let (hop, stride) = (cfg.hop(), cfg.max_out_per_block());
        let sc = SharedChannelizer::new(cfg.clone()).unwrap();
        let mut out = vec![Complex32::ZERO; 4 * stride];
        let mut produced = [0u32; 4];

        // nothing enabled -> nothing produced
        sc.process(&tone(&cfg, 0.0, 0, hop), &mut out, &mut produced);
        assert_eq!(produced, [0; 4]);

        let (opb_n, opb_w) = (cfg.out_per_block(NARROW), cfg.out_per_block(WIDE));
        assert!(opb_w > opb_n, "wide slots produce more samples per block");
        sc.set_slot(2, NARROW, 100_000.0);
        sc.set_slot(1, WIDE, -200_000.0);
        let mut input = tone(&cfg, 100_700.0, hop, hop);
        for (x, y) in input.iter_mut().zip(tone(&cfg, -199_300.0, hop, hop)) {
            *x += y;
        }
        sc.process(&input, &mut out, &mut produced);
        assert_eq!(produced, [0, opb_w as u32, opb_n as u32, 0]);
        // first block after enabling is valid immediately: ~700 Hz tone, unity gain
        let mid = &out[2 * stride + opb_n / 2..2 * stride + opb_n];
        assert!(mid.iter().all(|c| (c.norm() - 1.0).abs() < 2e-3));
        let mid = &out[stride + opb_w / 2..stride + opb_w];
        assert!(mid.iter().all(|c| (c.norm() - 1.0).abs() < 2e-3));

        sc.clear_slot(2);
        sc.process(&tone(&cfg, 0.0, 2 * hop, hop), &mut out, &mut produced);
        assert_eq!(produced, [0, opb_w as u32, 0, 0]);
    }

    #[test]
    fn slot_filter_persists_across_disable_and_changing_profile_replaces_the_channel() {
        let cfg = Config::standard(2_496_000.0).unwrap();
        let (hop, stride) = (cfg.hop(), cfg.max_out_per_block());
        let sc = SharedChannelizer::new(cfg.clone()).unwrap();
        let mut out = vec![Complex32::ZERO; stride];
        let mut produced = [0u32; 1];
        // 3.5 kHz off centre: passes the default filters, not a 2 kHz one
        let mut blk = 0;
        let mut level = |sc: &SharedChannelizer, out: &mut [Complex32], produced: &mut [u32]| {
            sc.process(&tone(&cfg, 103_500.0, blk * hop, hop), out, produced);
            blk += 1;
            let n = produced[0] as usize;
            out[n / 2..n]
                .iter()
                .map(|c| f64::from(c.norm()))
                .sum::<f64>()
                / (n - n / 2) as f64
        };
        sc.set_slot_filter(0, 2_000.0, 1_500.0);
        sc.set_slot(0, WIDE, 100_000.0);
        assert!(level(&sc, &mut out, &mut produced) < 1e-3);
        sc.clear_slot(0);
        level(&sc, &mut out, &mut produced);
        assert_eq!(produced[0], 0);
        sc.set_slot(0, WIDE, 100_000.0);
        assert!(
            level(&sc, &mut out, &mut produced) < 1e-3,
            "filter survived disable"
        );
        // same slot, other profile: new channel, same slot filter (2 kHz)
        sc.set_slot(0, NARROW, 100_000.0);
        level(&sc, &mut out, &mut produced);
        assert_eq!(produced[0] as usize, cfg.out_per_block(NARROW));
        assert_eq!(sc.counters().rejected, 0);
    }

    #[test]
    fn out_of_passband_slot_is_rejected_and_counted() {
        let cfg = Config::standard(912_000.0).unwrap();
        let (hop, stride) = (cfg.hop(), cfg.max_out_per_block());
        let sc = SharedChannelizer::new(cfg.clone()).unwrap();
        let mut out = vec![Complex32::ZERO; 2 * stride];
        let mut produced = [0u32; 2];
        sc.set_slot(1, NARROW, 460_000.0);
        sc.process(&tone(&cfg, 0.0, 0, hop), &mut out, &mut produced);
        assert_eq!(produced, [0; 2]);
        assert_eq!(sc.counters().rejected, 1);
    }

    #[test]
    fn timing_counters_track_blocks_and_reset_max() {
        let cfg = Config::standard(912_000.0).unwrap();
        let (hop, stride) = (cfg.hop(), cfg.max_out_per_block());
        let sc = SharedChannelizer::new(cfg.clone()).unwrap();
        let mut out = vec![Complex32::ZERO; stride];
        let mut produced = [0u32; 1];
        sc.set_slot(0, NARROW, 50_000.0);
        for i in 0..3 {
            sc.process(&tone(&cfg, 50_000.0, i * hop, hop), &mut out, &mut produced);
        }
        let c = sc.take_counters();
        assert_eq!((c.blocks, c.active), (3, 1));
        assert!(c.busy_ns > 0 && c.max_ns > 0 && c.max_ns <= c.busy_ns);
        assert_eq!(sc.take_counters().max_ns, 0);
        assert_eq!(sc.counters().blocks, 3);
        assert!(sc.block_budget_ns() > 1_000_000);
    }

    #[test]
    fn conditioned_slot_reports_power_and_levels_output() {
        let cfg = Config::standard(912_000.0).unwrap();
        let (hop, stride) = (cfg.hop(), cfg.max_out_per_block());
        let sc = SharedChannelizer::new(cfg.clone()).unwrap();
        let mut out = vec![Complex32::ZERO; stride];
        let mut produced = [0u32; 1];
        assert!(sc.slot_power_db(0).is_nan());
        sc.set_slot_conditioning(
            0,
            ConditioningSpec {
                sps: 5,
                excess_bw: 0.2,
                in_rate: cfg.out_rate(NARROW),
                out_rate: cfg.out_rate(NARROW),
                squelch: None,
            },
        );
        sc.set_slot(0, NARROW, 50_000.0);
        for i in 0..40 {
            let mut x = tone(&cfg, 50_500.0, i * hop, hop);
            x.iter_mut().for_each(|v| *v *= 0.05);
            sc.process(&x, &mut out, &mut produced);
        }
        let n = produced[0] as usize;
        let rms = (out[..n].iter().map(|c| c.norm_sqr()).sum::<f32>() / n as f32).sqrt();
        assert!((rms - 0.85).abs() < 0.05, "AGC output rms {rms}");
        let db = sc.slot_power_db(0);
        // the probe's average is slow (alpha 1e-4), so it is still settling
        assert!((-35.0..-26.0).contains(&db), "power {db} dB");
    }

    #[test]
    fn analog_slot_resamples_squelches_and_reports_transitions() {
        let cfg = Config::standard(912_000.0).unwrap();
        let (hop, stride) = (cfg.hop(), cfg.max_out_per_block());
        let in_rate = cfg.out_rate(WIDE);
        let sc = SharedChannelizer::new(cfg.clone()).unwrap();
        let mut out = vec![Complex32::ZERO; stride];
        let mut produced = [0u32; 1];
        sc.set_slot_conditioning(
            0,
            ConditioningSpec {
                sps: 2,
                excess_bw: 0.2,
                in_rate,
                out_rate: 96_000.0,
                squelch: Some(SquelchMode::ANALOG),
            },
        );
        sc.set_slot_squelch_db(0, -40.0);
        sc.set_slot(0, WIDE, 100_000.0);
        let (mut total_in, mut total_out) = (0usize, 0usize);
        // 20 blocks of carrier at -20 dB, then 20 of silence
        for i in 0..40 {
            let mut x = tone(&cfg, 101_000.0, i * hop, hop);
            let amp = if i < 20 { 0.1 } else { 0.0 };
            x.iter_mut().for_each(|v| *v *= amp);
            sc.process(&x, &mut out, &mut produced);
            total_in += cfg.out_per_block(WIDE);
            total_out += produced[0] as usize;
            if i == 10 {
                assert!(sc.slot_squelch_open(0));
            }
        }
        let ratio = total_out as f64 / total_in as f64;
        assert!((ratio - 96_000.0 / in_rate).abs() < 1e-3, "rate {ratio}");
        assert!(!sc.slot_squelch_open(0));
        let open = sc.next_squelch_event(0).unwrap();
        let close = sc.next_squelch_event(0).unwrap();
        assert!(!open.muted && close.muted);
        assert!(open.pwr_db > -40.0 && close.pwr_db < -40.0);
        assert!(sc.next_squelch_event(0).is_none());
    }

    #[test]
    fn signal_detector_runs_on_the_source_input_and_reports_through_the_mailbox() {
        let cfg = Config::standard(912_000.0).unwrap();
        let (hop, stride) = (cfg.hop(), cfg.max_out_per_block());
        let sc = SharedChannelizer::new(cfg.clone()).unwrap();
        let mut out = vec![Complex32::ZERO; stride];
        let mut produced = [0u32; 1];
        sc.set_signal_detector(Some(DetectorConfig {
            interval_s: 0.0,
            ..DetectorConfig::default()
        }));
        let mut x = tone(&cfg, 150_000.0, 0, hop);
        x.iter_mut().for_each(|v| *v *= 0.3);
        sc.process(&x, &mut out, &mut produced);
        let found = sc.take_detections();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!((found[0].center_hz - 150_000.0).abs() < 1_000.0);
        assert!(sc.take_detections().is_empty());
        sc.set_signal_detector(None);
        sc.process(&x, &mut out, &mut produced);
        assert!(sc.take_detections().is_empty());
    }

    #[derive(Debug, Default)]
    struct Seen {
        samples: usize,
        retunes: usize,
    }

    #[derive(Debug)]
    struct Recorder(Arc<Mutex<Seen>>);

    impl ChannelDecoder for Recorder {
        fn process(&mut self, iq: &[Complex32]) {
            self.0.lock().unwrap().samples += iq.len();
        }
        fn retune(&mut self) {
            self.0.lock().unwrap().retunes += 1;
        }
    }

    #[test]
    fn decoder_sees_the_slot_output_and_real_retunes_only() {
        let cfg = Config::standard(2_496_000.0).unwrap();
        let (hop, stride) = (cfg.hop(), cfg.max_out_per_block());
        let sc = SharedChannelizer::new(cfg.clone()).unwrap();
        let mut out = vec![Complex32::ZERO; 2 * stride];
        let mut produced = [0u32; 2];
        let seen = Arc::new(Mutex::new(Seen::default()));
        sc.set_slot_decoder(1, Some(Box::new(Recorder(Arc::clone(&seen)))));
        sc.set_slot(1, NARROW, 100_000.0);
        let mut total = 0;
        for i in 0..3 {
            if i == 1 {
                sc.set_slot(1, NARROW, 100_300.0); // fine tune: kept
            }
            if i == 2 {
                sc.set_slot(1, NARROW, 250_000.0); // another channel
            }
            sc.process(
                &tone(&cfg, 100_700.0, i * hop, hop),
                &mut out,
                &mut produced,
            );
            total += produced[1] as usize;
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.samples, total);
        // Enabling counts as a jump from 0 Hz, plus the 150 kHz move.
        assert_eq!(seen.retunes, 2);

        sc.set_slot_decoder(1, None);
        sc.process(
            &tone(&cfg, 100_700.0, 3 * hop, hop),
            &mut out,
            &mut produced,
        );
    }
}
