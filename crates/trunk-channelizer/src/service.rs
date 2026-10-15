//! Thread-safe front end for embedding the channelizer in a streaming host
//! (the GNU Radio block in trunk-recorder).
//!
//! Control calls (`set_slot`) may come from any thread and only touch a small
//! mailbox. `process` runs on the data thread, drains the mailbox *between
//! blocks* so a retune can never race the DSP, and copies each active slot's
//! output into a caller-provided slot-indexed buffer.

use std::sync::Mutex;

use anyhow::Result;
use num_complex::Complex32;

use crate::channelizer::{ChannelId, Channelizer, Config};

#[derive(Clone, Copy, Debug)]
enum Cmd {
    /// Enable (or retune) a slot at an offset from the source centre.
    Set { slot: usize, freq_hz: f64 },
    Clear { slot: usize },
}

/// Per-slot result of one [`SharedChannelizer::process`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Slots rejected because the requested offset is outside the passband.
    pub rejected: u64,
    /// Blocks processed.
    pub blocks: u64,
}

pub struct SharedChannelizer {
    cfg: Config,
    mailbox: Mutex<Vec<Cmd>>,
    dsp: Mutex<Dsp>,
}

struct Dsp {
    ch: Channelizer,
    slots: Vec<Option<ChannelId>>,
    counters: Counters,
}

impl SharedChannelizer {
    pub fn new(cfg: Config) -> Result<Self> {
        let ch = Channelizer::new(cfg.clone())?;
        Ok(Self { cfg, mailbox: Mutex::new(Vec::new()), dsp: Mutex::new(Dsp { ch, slots: Vec::new(), counters: Counters::default() }) })
    }

    pub const fn config(&self) -> &Config {
        &self.cfg
    }

    /// Enable `slot` at `freq_hz` from the source centre, or retune it if
    /// already enabled. Applied before the next block.
    pub fn set_slot(&self, slot: usize, freq_hz: f64) {
        self.post(Cmd::Set { slot, freq_hz });
    }

    /// Disable `slot` (it produces no output from the next block on).
    pub fn clear_slot(&self, slot: usize) {
        self.post(Cmd::Clear { slot });
    }

    fn post(&self, cmd: Cmd) {
        self.mailbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(cmd);
    }

    pub fn counters(&self) -> Counters {
        self.dsp.lock().unwrap_or_else(std::sync::PoisonError::into_inner).counters
    }

    /// Consume one block ([`Config::hop`] samples). For every active slot
    /// `s < active.len()`, write its [`Config::out_per_block`] samples to
    /// `out[s * opb .. (s+1) * opb]` and set `active[s] = true`; other
    /// entries of `active` are set to `false`.
    ///
    /// # Panics
    /// If `input.len() != hop` or `out.len() < active.len() * out_per_block`.
    pub fn process(&self, input: &[Complex32], out: &mut [Complex32], active: &mut [bool]) {
        let opb = self.cfg.out_per_block();
        assert!(out.len() >= active.len() * opb, "output buffer too small");
        let cmds = std::mem::take(&mut *self.mailbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        let mut dsp = self.dsp.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Dsp { ch, slots, counters } = &mut *dsp;

        for cmd in cmds {
            match cmd {
                Cmd::Set { slot, freq_hz } => {
                    if slots.len() <= slot {
                        slots.resize(slot + 1, None);
                    }
                    let applied = match slots[slot] {
                        Some(id) => ch.retune(id, freq_hz).is_ok(),
                        None => ch.add_channel(freq_hz).map(|id| slots[slot] = Some(id)).is_ok(),
                    };
                    if !applied {
                        counters.rejected += 1;
                        if let Some(id) = slots[slot].take() {
                            ch.remove_channel(id);
                        }
                    }
                }
                Cmd::Clear { slot } => {
                    if let Some(id) = slots.get_mut(slot).and_then(Option::take) {
                        ch.remove_channel(id);
                    }
                }
            }
        }

        ch.process_block(input);
        counters.blocks += 1;

        active.fill(false);
        for (slot, id) in slots.iter().enumerate().take(active.len()) {
            if let Some(id) = id
                && let Some(o) = ch.output(*id)
            {
                out[slot * opb..(slot + 1) * opb].copy_from_slice(o);
                active[slot] = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use super::*;

    fn tone(cfg: &Config, f: f64, start: usize, len: usize) -> Vec<Complex32> {
        (start..start + len).map(|n| Complex32::from_polar(1.0, (TAU * f * n as f64 / cfg.sample_rate) as f32)).collect()
    }

    #[test]
    fn slots_follow_commands_between_blocks() {
        let cfg = Config::p25(2_496_000.0).unwrap();
        let (hop, opb) = (cfg.hop(), cfg.out_per_block());
        let sc = SharedChannelizer::new(cfg.clone()).unwrap();
        let mut out = vec![Complex32::ZERO; 4 * opb];
        let mut active = [false; 4];

        // nothing enabled -> nothing active
        sc.process(&tone(&cfg, 0.0, 0, hop), &mut out, &mut active);
        assert_eq!(active, [false; 4]);

        sc.set_slot(2, 100_000.0);
        sc.process(&tone(&cfg, 100_700.0, hop, hop), &mut out, &mut active);
        assert_eq!(active, [false, false, true, false]);
        // first block after enabling is valid immediately: ~700 Hz tone, unity gain
        let mid = &out[2 * opb + opb / 2..3 * opb];
        assert!(mid.iter().all(|c| (c.norm() - 1.0).abs() < 2e-3));

        sc.clear_slot(2);
        sc.process(&tone(&cfg, 0.0, 2 * hop, hop), &mut out, &mut active);
        assert_eq!(active, [false; 4]);
    }

    #[test]
    fn out_of_passband_slot_is_rejected_and_counted() {
        let cfg = Config::p25(912_000.0).unwrap();
        let (hop, opb) = (cfg.hop(), cfg.out_per_block());
        let sc = SharedChannelizer::new(cfg.clone()).unwrap();
        let mut out = vec![Complex32::ZERO; 2 * opb];
        let mut active = [false; 2];
        sc.set_slot(1, 460_000.0);
        sc.process(&tone(&cfg, 0.0, 0, hop), &mut out, &mut active);
        assert_eq!(active, [false; 2]);
        assert_eq!(sc.counters().rejected, 1);
    }
}
