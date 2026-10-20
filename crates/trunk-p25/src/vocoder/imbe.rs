//! Phase 1: IMBE full-rate parameters to audio (op25's
//! `software_imbe_decoder::decode_fullrate`, after TIA-102.BABA).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops,
    clippy::manual_midpoint,
    clippy::needless_range_loop,
    clippy::many_single_char_names,
    clippy::unreadable_literal,
    reason = "a port of op25: its equations' names, indexing and constants, its int-to-float \
              conversions, and no fused multiply-adds or other rewritten arithmetic"
)]

use std::f64::consts::PI;

use super::synth::{Errors, HARMONICS, Pcm, Synthesizer, Thresholds};
use super::tables::{BIT_COUNT, BMT_B, BMT_N, GAIN_LEVELS, STEP_SIZE};
use super::{Decision, PLAY_MARGIN, Policy, Spectrum};
use crate::voice::ImbeFrame;

/// A recorder's phase 1 vocoder, kept across calls as op25 keeps its own.
pub struct ImbeVocoder {
    policy: Policy,
    synth: Synthesizer,
    /// Running bit error rate.
    error_rate: f32,
    /// Frames [`Policy::Soft`] has repeated in a row.
    repeats: u32,
    last: Decision,
}

impl Default for ImbeVocoder {
    fn default() -> Self {
        Self::new()
    }
}

/// The largest valid pitch index b̂0; larger values are reserved and the
/// frame is repeated (TIA-102.BABA §7.7).
const MAX_PITCH_INDEX: u32 = 207;
/// The error rate's decay and weight per error: `ε_R(0) = 0.95 ε_R(−1) +
/// 0.000365 ε_T` (BABA Eq. 96).
const EPS_R_DECAY: f64 = 0.95;
const EPS_R_PER_ERROR: f64 = 0.000365;
/// Frames are muted above this error rate (BABA §7.8).
const MUTE_EPS_R: f64 = 0.0875;
/// Frame repeat thresholds: `ε0 ≥ 2` (Eq. 97) and `ε_T ≥ 10 + 40 ε_R`
/// (Eq. 98). The spec repeats when both hold, op25 when either does.
const REPEAT_EPS_0: u32 = 2;
const REPEAT_EPS_T_BASE: f32 = 10.0;
const REPEAT_EPS_T_PER_RATE: f32 = 40.0;
/// [`Policy::Soft`]'s repeats in a row before it mutes, as op25's. BABA has
/// no limit (§7.7): the error rate mutes a bad enough run (§7.8).
const SOFT_MAX_REPEATS: u32 = 3;

/// b̂0 of a frame: the pitch, 0..=207 for voice. Its low two bits are u7's
/// bits 2..1 as sent (BABA §7.1, Fig. 22). op25 shifts u7 left a bit
/// first, and so reads bits 1..0.
const fn b0(u: &[u32; 8]) -> u32 {
    ((u[0] >> 4) & 0xfc) | ((u[7] >> 1) & 0x3)
}

impl ImbeVocoder {
    /// [`Policy::Hard`].
    pub fn new() -> Self {
        Self::with_policy(Policy::Hard)
    }

    pub fn with_policy(policy: Policy) -> Self {
        Self {
            policy,
            synth: Synthesizer::new(),
            error_rate: 0.0,
            repeats: 0,
            last: Decision::Played,
        }
    }

    /// What became of the last frame.
    pub const fn last(&self) -> Decision {
        self.last
    }

    /// The model the last frame was played from, unless muted (or nothing
    /// was yet decoded to repeat).
    pub fn spectrum(&self) -> Option<Spectrum> {
        (self.last != Decision::Muted && self.synth.old_l > 0).then(|| Spectrum {
            w0: self.synth.old_w0,
            amplitudes: self.synth.played().mu[1..=self.synth.old_l].to_vec(),
        })
    }

    /// One frame as decoded here. `None` if the frame is muted.
    pub fn vocode(&mut self, frame: &ImbeFrame) -> Option<Pcm> {
        match self.policy {
            Policy::Hard => self.decode(
                &frame.params.0.map(u32::from),
                frame.errors_c0,
                frame.errors_c4,
                frame.errors,
            ),
            Policy::Soft => self.soft(frame),
        }
    }

    /// [`Policy::Soft`]: the frame as decoded if its decision had margin,
    /// else the last again (three times at most), else silence.
    fn soft(&mut self, frame: &ImbeFrame) -> Option<Pcm> {
        let u = frame.params.0.map(u32::from);
        self.last = if frame.reliability >= PLAY_MARGIN && b0(&u) <= MAX_PITCH_INDEX {
            self.repeats = 0;
            self.decode_model(&u);
            self.synth.enhance();
            self.synth.smooth(Thresholds::CLEAN);
            Decision::Played
        } else {
            self.repeats += 1;
            if self.repeats <= SOFT_MAX_REPEATS {
                self.repeat_last();
                self.synth.smooth(Thresholds::CLEAN);
                Decision::Repeated
            } else {
                Decision::Muted
            }
        };
        let pcm = (self.last != Decision::Muted).then(|| self.synth.synthesize());
        self.synth.advance();
        pcm
    }

    /// One frame by the standard's rules (BABA §7.7–7.8, §9): its
    /// `u[0..8]` as sent and the bits corrected in c0 (`e0`), in c4 (`e4`)
    /// and in all (`et`), whatever the policy. `None` if the frame is muted
    /// for errors (silence).
    pub fn decode(&mut self, u: &[u32; 8], e0: u32, e4: u32, et: u32) -> Option<Pcm> {
        let b0 = b0(u);
        self.error_rate =
            (EPS_R_DECAY * f64::from(self.error_rate) + EPS_R_PER_ERROR * f64::from(et)) as f32;
        // Eqs. 97 and 98 both.
        let invalid = e0 >= REPEAT_EPS_0
            && et as f32 >= REPEAT_EPS_T_BASE + REPEAT_EPS_T_PER_RATE * self.error_rate;
        let mut rm0 = None;
        self.last = if f64::from(self.error_rate) > MUTE_EPS_R {
            // Muting (7.8), with the repeat's update.
            self.repeat_last();
            Decision::Muted
        } else if b0 > MAX_PITCH_INDEX || invalid {
            // Repeat (7.7), as many in a row as come: op25 mutes the
            // fourth.
            self.repeat_last();
            Decision::Repeated
        } else {
            self.decode_model(u);
            rm0 = Some(self.synth.enhance());
            Decision::Played
        };
        let pcm = (self.last != Decision::Muted).then(|| {
            let errors = Errors {
                rate: self.error_rate,
                total: et as f32,
                c4: e4 as f32,
            };
            let thresholds = self.synth.thresholds(rm0, errors);
            self.synth.smooth(thresholds);
            self.synth.synthesize()
        });
        self.synth.advance();
        pcm
    }

    /// The previous frame's model again (its phases excepted).
    const fn repeat_last(&mut self) {
        let s = &mut self.synth;
        s.w0 = s.old_w0;
        s.l = s.old_l;
        let old = *s.previous();
        let cur = s.current();
        cur.voiced = old.voiced;
        cur.mu = old.mu;
        cur.m = old.m;
        cur.log2_mu = old.log2_mu;
    }

    /// The frame's fundamental, voicing and spectral amplitudes.
    fn decode_model(&mut self, u: &[u32; 8]) {
        let b = self.rearrange(u);
        let s = &mut self.synth;
        let l = s.l;
        let k = if l > 36 { 12 } else { l.div_ceil(3) };
        let cur = s.current();
        for ell in 1..=l {
            let band = if ell <= 36 { ell.div_ceil(3) } else { 12 };
            cur.voiced[ell] = b[1] & (1 << (k - band)) != 0;
        }
        self.decode_amplitudes(&b);
    }

    /// The fundamental and L from b̂0, and the quantised values b̂ from u
    /// by the bit allocation for that L. u7's bits are where BABA §7.1 puts
    /// them in u7 as sent (op25's shifted u7 read them one bit low).
    fn rearrange(&mut self, u: &[u32; 8]) -> [i32; HARMONICS + 1] {
        let b0 = ((u[0] / 16) & 0xfc) | ((u[7] / 2) & 3);
        let s = &mut self.synth;
        s.w0 = (4.0 * PI / (f64::from(b0) + 39.5)) as f32;
        let l = (0.9254 * f64::from(((PI / f64::from(s.w0)) + 0.25) as f32).floor()) as usize;
        // b̂0 ≤ 207 here, so 9 ≤ L ≤ 56.
        debug_assert!((9..=56).contains(&l), "L {l}");
        s.l = l;

        let mut b = [0i32; HARMONICS + 1];
        b[2] = ((u[0] & 0x38) | ((u[7] / 8) & 1)) as i32;
        // u0's low three bits, u1–u3's twelve, u4–u6's eleven and three of
        // u7's, most significant first.
        let bits = [(0, 3), (1, 12), (2, 12), (3, 12), (4, 11), (5, 11), (6, 11)]
            .into_iter()
            .flat_map(|(v, n)| (0..n).rev().map(move |i| (u[v] >> i) & 1 == 1))
            .chain((4..=6).rev().map(|i| (u[7] >> i) & 1 == 1));
        let row = (l - 9) * 75;
        for (seq, set) in (row..).zip(bits) {
            if set {
                b[usize::from(BMT_N[seq])] += i32::from(BMT_B[seq]);
            }
        }
        b
    }

    /// The spectral amplitudes from the gains and residuals in b̂2.., each
    /// added to the prediction from the previous frame's.
    #[allow(clippy::many_single_char_names)]
    fn decode_amplitudes(&mut self, b: &[i32; HARMONICS + 1]) {
        let s = &mut self.synth;
        let l = s.l;
        let mut p3 = ((l - 1) * (l - 2)) / 2 - 28;
        // Each value in turn: b̂ less half its range, times its step.
        let mut dequantize = |b: i32| {
            let (i, half) = (p3, (f32::from(BIT_COUNT[p3]) - 1.0).exp2());
            p3 += 1;
            (f64::from(STEP_SIZE[i]) * (f64::from(b as f32 - half) + 0.5)) as f32
        };

        // The gain vector, and from it each block's first coefficient.
        let mut g = [0f32; 7];
        g[1] = GAIN_LEVELS[b[2] as usize];
        for eye in 2..=6 {
            g[eye] = dequantize(b[eye + 1]);
        }
        let mut c = [[0f32; 11]; 7];
        for eye in 1..=6 {
            let mut t = g[1];
            for em in 2..=6 {
                let cos = (PI * (em - 1) as f64 * (eye as f64 - 0.5) / 6.0).cos();
                t = (f64::from(t) + f64::from(2.0 * g[em]) * cos) as f32;
            }
            c[eye][1] = t;
        }

        // The rest of each block's coefficients, and the residuals T.
        let j: [usize; 7] = std::array::from_fn(|eye| (l + eye - 1) / 6);
        let mut ell = 8;
        for eye in 1..=6 {
            for kay in 2..=j[eye] {
                c[eye][kay] = dequantize(b[ell]);
                ell += 1;
            }
        }
        let mut t = [0f32; HARMONICS];
        let mut ell = 0;
        for eye in 1..=6 {
            for jay in 1..=j[eye] {
                let mut sum = c[eye][1];
                for kay in 2..=j[eye] {
                    let cos = (PI * (kay - 1) as f64 * (jay as f64 - 0.5) / j[eye] as f64).cos();
                    sum = (f64::from(sum) + f64::from(2.0 * c[eye][kay]) * cos) as f32;
                }
                ell += 1;
                t[ell] = sum;
            }
        }
        debug_assert_eq!(ell, l);

        // The prediction from the previous frame, stretched to this L. The
        // first frame predicts from L = 30, all ones.
        if s.old_l == 0 {
            s.old_l = 30;
            s.previous().log2_mu = [1.0; HARMONICS + 1];
        }
        let old_l = s.old_l;
        let old = s.previous();
        for ell in old_l + 1..=l + 1 {
            old.log2_mu[ell] = old.log2_mu[old_l];
        }
        let old_log2 = old.log2_mu;
        let rho: f32 = if l <= 15 {
            0.4
        } else if l <= 24 {
            (0.03 * l as f64 - 0.05) as f32
        } else {
            0.7
        };
        let cur = s.current();
        let mut mean = 0f32;
        for ell in 1..=l {
            let tk = (old_l as f32 / l as f32) * ell as f32;
            let k = tk as usize;
            let delta = tk - k as f32;
            cur.mu[ell] = rho * ((1.0 - delta) * old_log2[k] + delta * old_log2[k + 1]);
            mean += cur.mu[ell];
        }
        mean /= l as f32;
        for ell in 1..=l {
            cur.log2_mu[ell] = t[ell] + cur.mu[ell] - mean;
            // op25's powf(2, x); glibc's exp2f agrees for every |x| < 128.
            cur.mu[ell] = cur.log2_mu[ell].exp2();
        }
    }
}
