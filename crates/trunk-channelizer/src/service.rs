//! Thread-safe front end for embedding the channelizer in a streaming host
//! (the GNU Radio block in trunk-recorder).
//!
//! Control calls (`set_slot`, `set_slot_filter`) may come from any thread and
//! only touch a small mailbox. `process` runs on the data thread, drains the
//! mailbox *between blocks* so a retune can never race the DSP, and copies
//! each active slot's output into a caller-provided slot-indexed buffer. A
//! slot is one consumer's port: it has a profile, a frequency and a filter,
//! and the filter persists while the slot is disabled.

use std::sync::Mutex;
use std::time::Instant;

use anyhow::Result;
use num_complex::Complex32;

use crate::channelizer::{ChannelId, Channelizer};
use crate::config::{Config, ProfileId};

#[derive(Clone, Copy, Debug)]
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
    mailbox: Mutex<Vec<Cmd>>,
    dsp: Mutex<Dsp>,
}

#[derive(Clone, Copy)]
struct Slot {
    id: Option<ChannelId>,
    /// `(cutoff_hz, transition_hz)`; `None` = the profile's default.
    filter: Option<(f64, f64)>,
}

struct Dsp {
    ch: Channelizer,
    slots: Vec<Slot>,
    counters: Counters,
}

impl SharedChannelizer {
    pub fn new(cfg: Config) -> Result<Self> {
        let ch = Channelizer::new(cfg.clone())?;
        Ok(Self {
            cfg,
            mailbox: Mutex::new(Vec::new()),
            dsp: Mutex::new(Dsp {
                ch,
                slots: Vec::new(),
                counters: Counters::default(),
            }),
        })
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
                        slots.resize(
                            slot + 1,
                            Slot {
                                id: None,
                                filter: None,
                            },
                        );
                    }
                    let s = &mut slots[slot];
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
                    }
                }
                Cmd::Filter {
                    slot,
                    cutoff_hz,
                    transition_hz,
                } => {
                    if slots.len() <= slot {
                        slots.resize(
                            slot + 1,
                            Slot {
                                id: None,
                                filter: None,
                            },
                        );
                    }
                    let s = &mut slots[slot];
                    s.filter = Some((cutoff_hz, transition_hz));
                    if let Some(id) = s.id
                        && ch.set_filter(id, cutoff_hz, transition_hz).is_err()
                    {
                        counters.rejected += 1;
                    }
                }
                Cmd::Clear { slot } => {
                    if let Some(id) = slots.get_mut(slot).and_then(|s| s.id.take()) {
                        ch.remove_channel(id);
                    }
                }
            }
        }

        ch.process_block(input);
        counters.blocks += 1;

        produced.fill(0);
        for (slot, s) in slots.iter().enumerate().take(produced.len()) {
            if let Some(id) = s.id
                && let Some(o) = ch.output(id)
            {
                out[slot * stride..slot * stride + o.len()].copy_from_slice(o);
                produced[slot] = u32::try_from(o.len()).unwrap_or(u32::MAX);
            }
        }

        let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        counters.busy_ns = counters.busy_ns.saturating_add(elapsed);
        counters.max_ns = counters.max_ns.max(elapsed);
        counters.overruns += u64::from(elapsed > self.block_budget_ns());
        counters.active = slots.iter().filter(|s| s.id.is_some()).count() as u64;
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
}
