//! Phase 2: AMBE+2 half-rate frames to audio, as op25 makes it
//! (`p25p2_tdma::handle_voice_frame`): mbelib's dequantiser
//! (`mbe_dequantizeAmbe2250Parms`, `mbe_dequantizeAmbeTone`) and op25's
//! repeat and mute rules, synthesised by op25's software IMBE synthesiser.
//!
//! mbelib keeps a current and a previous model and copies one to the other
//! after every frame, so between frames they are equal: one model is kept
//! here, replaced when a frame dequantises, and a repeat (mbelib's
//! `mbe_useLastMbeParms`) renders it again.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops,
    clippy::manual_midpoint,
    clippy::needless_range_loop,
    clippy::many_single_char_names,
    clippy::unreadable_literal,
    reason = "a port of mbelib: its equations' names, indexing and constants, its int-to-float \
              conversions, and no fused multiply-adds or other rewritten arithmetic"
)]

use std::f64::consts::{LN_2, PI, SQRT_2};
use std::ops::RangeInclusive;

use super::synth::{
    Errors, HARMONICS, Pcm, SAMPLES_PER_FRAME, Synthesizer, Thresholds, op25_energy,
};
use super::tables::{
    AMBE_BLOCKS, AMBE_GAIN, AMBE_HOC5, AMBE_HOC6, AMBE_HOC7, AMBE_HOC8, AMBE_L, AMBE_PRBA24,
    AMBE_PRBA58, AMBE_VUV, AMBE_W0,
};
use super::{Decision, PLAY_MARGIN, Policy, Spectrum};
use crate::tdma::ambe::{AmbeCodeword, AmbeFrame, AmbeParams};

/// A recorder's phase 2 vocoder, kept across calls as op25 keeps its own.
pub struct AmbeVocoder {
    policy: Policy,
    /// The model last rendered.
    model: Model,
    /// The model the next frame is predicted from: under
    /// [`Policy::Standard`] the last voice frame's (BABA-1 Eqs. 26, 43),
    /// otherwise `model`, as mbelib predicts from a silence frame too.
    prediction: Model,
    tone: Tone,
    /// Running bit error rate; frames are muted above [`MAX_ERROR_RATE`].
    error_rate: f64,
    /// Frames rejected in a row.
    rejected: u32,
    /// Whether the last frame rendered was a tone.
    tone_frame: bool,
    last: Decision,
    synth: Synthesizer,
}

/// The error rate ε<sub>R</sub> above which frames are muted (TIA-102.BABA-1 §5.7).
const MAX_ERROR_RATE: f64 = 0.096;

/// ε<sub>R</sub>'s decay from frame to frame (BABA-1 Eq. 56).
const ERROR_RATE_DECAY: f64 = 0.95;
/// ε<sub>R</sub>'s weight on the frame's errors ε<sub>T</sub> (BABA-1 Eq. 56).
const ERROR_RATE_PER_ERROR: f64 = 0.001064;

/// Consecutive invalid frames at which the decoder mutes instead of
/// repeating (BABA-1 §5.7): three repeats, then silence.
const MUTE_AFTER_INVALID: u32 = 4;

/// b0 of an erasure frame (BABA-1 Table 4).
const ERASURE_B0: RangeInclusive<usize> = 120..=123;
/// b0 of a silence frame (BABA-1 Table 4).
const SILENCE_B0: RangeInclusive<usize> = 124..=125;
/// The least b0 of a tone frame (BABA-1 Table 4). One that reaches the
/// voice path failed `ToneFrame::of`'s tests (trunk-oxide-13y.3).
const FIRST_TONE_B0: usize = 126;

/// A silence frame's pitch period, ω0 = 2π/32 (BABA-1 Eq. 1).
const SILENCE_PERIOD: f64 = 32.0;
/// A silence frame's L (BABA-1 Eq. 2).
const SILENCE_L: usize = 14;

/// Frame repeat (BABA-1 §5.6): ε0 ≥ 4, or ε0 ≥ 2 (Eq. 57) and ε<sub>T</sub> ≥ 6
/// (Eq. 58).
const REPEAT_E0: u32 = 4;
const REPEAT_E0_WITH_TOTAL: u32 = 2;
const REPEAT_TOTAL: u32 = 6;

/// Whether a frame's error counts call for a frame repeat (BABA-1 §5.6).
/// The thresholds are the spec's; the counts given them are op25's, not
/// the bits corrected or detected (trunk-oxide-13y.2, see
/// [`crate::tdma::ambe`]'s `op25_golay_23`).
const fn repeat_for_errors(e0: u32, e1: u32) -> bool {
    e0 >= REPEAT_E0 || (e0 >= REPEAT_E0_WITH_TOTAL && e0 + e1 >= REPEAT_TOTAL)
}

/// The previous frame's gain's weight in this one's (BABA-1 Eq. 26).
const GAIN_PREDICTION: f32 = 0.5;
/// The previous frame's log amplitudes' weight in the prediction
/// (BABA-1 Eq. 43).
const AMPLITUDE_PREDICTION: f32 = 0.65;
/// Elements of each higher order coefficient vector, so C̃i,k for
/// 3 ≤ k ≤ 6 (BABA-1 §4.3.2; Eq. 37 as printed says k ≤ 4, a misprint).
const HOC_LEN: usize = 4;
const LAST_HOC_K: usize = 2 + HOC_LEN;
/// An unvoiced amplitude's scale, 0.2046/√ω0 (BABA-1 Eq. 46).
const UNVOICED_GAIN: f32 = 0.2046;
/// exp(0.693 Λ) for 2^Λ (BABA-1 Eq. 46).
const LOG_AMPLITUDE_SCALE: f32 = 0.693;

/// u0[11:6] of a tone frame (BABA-1 §7, Table 10).
const TONE_MARKER: u16 = 63;
/// A single tone's frequency step, f1 = 31.25 ID Hz (BABA-1 Table 9).
const SINGLE_TONE_STEP_HZ: f64 = 31.25;
/// op25's linear tone amplitude, AD × 75, shared between a dual tone's two.
const OP25_TONE_AMPLITUDE_SCALE: u16 = 75;
/// BABA-1 §7.2: AD is a log amplitude, 127 the A-to-D's largest sinusoid
/// (+3.17 dBm0, full scale) and 0 at −87.13 dBm0, so 0.711 dB a step. Each
/// of a dual tone's two is at that level, as the AMBE-3000R plays them
/// (measured by JMBE: 0.708 dB a step, AD 127 within 0.3 dB of full scale).
const TONE_AD_MAX: u16 = 127;
const TONE_AD_STEP_DB: f64 = (3.17 + 87.13) / 127.0;
const FULL_SCALE: f64 = 32767.0;

/// The error rate and errors op25 gives the adaptive smoothing for phase
/// 2: none, so it never reacts to errors (trunk-oxide-13y.5; BABA §9 takes
/// ε<sub>R</sub> and ε<sub>T</sub>).
const OP25_SMOOTHING_ERROR_RATE: f32 = 0.0;
const OP25_SMOOTHING_ERRORS: f32 = 0.0;

impl Default for AmbeVocoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AmbeVocoder {
    /// op25's rules ([`Policy::Op25`]).
    pub fn new() -> Self {
        Self::with_policy(Policy::Op25)
    }

    pub fn with_policy(policy: Policy) -> Self {
        Self {
            policy,
            model: Model::MBELIB_INITIAL,
            prediction: Model::MBELIB_INITIAL,
            tone: Tone {
                logarithmic: policy == Policy::Standard,
                ..Tone::default()
            },
            error_rate: 0.0,
            rejected: 0,
            tone_frame: false,
            last: Decision::Played,
            synth: Synthesizer::new(),
        }
    }

    /// What became of the last frame.
    pub const fn last(&self) -> Decision {
        self.last
    }

    /// The model the last frame was played from, unless muted or a tone.
    pub fn spectrum(&self) -> Option<Spectrum> {
        (self.last != Decision::Muted && !self.tone_frame).then(|| Spectrum {
            w0: self.model.w0,
            amplitudes: self.model.ml[1..=self.model.l].to_vec(),
        })
    }

    /// One clear frame: its 36 descrambled dibits as op25 receives them,
    /// and the frame decoded here from the same burst, if there is one
    /// (without it the policy is op25's). `None` if the frame is muted.
    pub fn vocode(&mut self, dibits: &[u8; 36], frame: Option<&AmbeFrame>) -> Option<Pcm> {
        match (self.policy, frame) {
            (Policy::Soft, Some(f)) => self.soft(f),
            _ => self.decode(dibits),
        }
    }

    /// [`Policy::Soft`]: the frame as decoded if its decision had margin,
    /// else the last again (three times at most), else silence.
    fn soft(&mut self, frame: &AmbeFrame) -> Option<Pcm> {
        let render = if frame.reliability >= PLAY_MARGIN {
            self.decide(frame.params, [0, 0])
        } else {
            self.reject(true)
        };
        render.then(|| self.play())
    }

    /// One frame as op25 takes it, its 36 descrambled dibits as received,
    /// under op25's rules (but for [`Policy::Standard`]'s departures from
    /// them). `None` if it is muted (silence) for errors or too many
    /// repeats.
    pub fn decode(&mut self, dibits: &[u8; 36]) -> Option<Pcm> {
        let (u, errors) = self.receive(dibits);
        // op25's order: the frame is taken in (its model kept, the repeat
        // count reset) before the error rate mutes it, where BABA-1 §5.7
        // mutes with the repeat's update instead (trunk-oxide-13y.8).
        let render = self.decide(u, errors) && self.error_rate <= MAX_ERROR_RATE;
        if !render {
            self.last = Decision::Muted;
        }
        render.then(|| self.play())
    }

    /// What is heard: the tone or the model.
    fn play(&mut self) -> Pcm {
        if self.tone_frame {
            self.tone.render()
        } else {
            self.render()
        }
    }

    /// Takes a frame in: a tone, voice, or neither (rejected, so repeated
    /// or muted). Whether anything is to be rendered.
    fn decide(&mut self, u: AmbeParams, errors: [u32; 2]) -> bool {
        let standard = self.policy == Policy::Standard;
        match ToneFrame::of(u, errors, standard) {
            ToneFrame::Tone { id, amplitude } => {
                self.tone.set(id, amplitude);
                self.accept(true)
            }
            ToneFrame::Erased => self.reject(self.tone_frame),
            ToneFrame::Voice => {
                let prediction = if standard {
                    &self.prediction
                } else {
                    &self.model
                };
                let b = indices(u);
                match prediction.dequantize(&b, errors) {
                    Some(model) => {
                        // mbelib predicts the next frame from a silence
                        // frame too; BABA-1 Eq. 26 and §4.4.3 from voice
                        // frames only (trunk-oxide-13y.1).
                        if !SILENCE_B0.contains(&b[0]) {
                            self.prediction = model;
                        }
                        self.model = model;
                        self.accept(false)
                    }
                    None => self.reject(!self.tone_frame),
                }
            }
        }
    }

    /// An encrypted frame: no audio, but its errors count toward the rate.
    pub fn encrypted(&mut self, dibits: &[u8; 36]) {
        self.receive(dibits);
    }

    fn receive(&mut self, dibits: &[u8; 36]) -> (AmbeParams, [u32; 2]) {
        let (u, errors) = AmbeCodeword::from_dibits(dibits).decode_op25();
        self.error_rate = ERROR_RATE_DECAY * self.error_rate
            + ERROR_RATE_PER_ERROR * f64::from(errors[0] + errors[1]);
        (u, errors)
    }

    const fn accept(&mut self, tone: bool) -> bool {
        self.tone_frame = tone;
        self.rejected = 0;
        self.last = Decision::Played;
        true
    }

    /// A frame that would not decode: the last one again if fewer than four
    /// have been rejected in a row and `repeatable` (the last frame was of
    /// the same kind), else silence.
    ///
    /// `repeatable` is op25's: BABA-1 §5.6 and §7.3 repeat whatever the
    /// last frame was (trunk-oxide-13y.6). Muting is silence, not §5.7's
    /// comfort noise.
    const fn reject(&mut self, repeatable: bool) -> bool {
        self.rejected += 1;
        if self.rejected < MUTE_AFTER_INVALID && repeatable {
            self.last = Decision::Repeated;
            true
        } else {
            self.tone_frame = false;
            self.last = Decision::Muted;
            false
        }
    }

    /// The model through op25's synthesiser (`decode_tap`).
    fn render(&mut self) -> Pcm {
        let m = &self.model;
        let s = &mut self.synth;
        s.l = m.l;
        s.w0 = m.w0;
        let cur = s.current();
        cur.voiced[1..=m.l].copy_from_slice(&m.voiced[1..=m.l]);
        cur.mu[1..=m.l].copy_from_slice(&m.ml[1..=m.l]);
        let rm0 = s.enhance();
        let errors = Errors {
            rate: OP25_SMOOTHING_ERROR_RATE,
            total: OP25_SMOOTHING_ERRORS,
            c4: OP25_SMOOTHING_ERRORS,
        };
        s.smooth(Thresholds::op25(op25_energy(rm0), errors));
        let pcm = s.synthesize();
        s.advance();
        pcm
    }
}

/// mbelib's model of a frame.
#[derive(Clone, Copy)]
struct Model {
    w0: f32,
    l: usize,
    voiced: [bool; HARMONICS],
    ml: [f32; HARMONICS],
    /// One past the last harmonic, where mbelib reads the next field of its
    /// struct (always zero) for the prediction at L = 56.
    log2_ml: [f32; HARMONICS + 1],
    gamma: f32,
}

impl Model {
    /// mbelib's (`mbe_initMbeParms`): log2 Ml = 0 and L = 30, where
    /// BABA-1 §4.4.3 starts from Λ̃l(−1) = 1 and L̃(−1) = 15
    /// (trunk-oxide-13y.7). γ̃(−1) = 0 is the spec's (Eq. 26).
    const MBELIB_INITIAL: Self = Self {
        w0: 0.09378,
        l: 30,
        voiced: [false; HARMONICS],
        ml: [0.0; HARMONICS],
        log2_ml: [0.0; HARMONICS + 1],
        gamma: 0.0,
    };

    /// The next frame's model from its quantiser indices b0–b8, predicted
    /// from this one's; `None` for an erasure (too many errors, or b0 a
    /// value no voice frame has).
    #[allow(clippy::many_single_char_names)]
    #[allow(clippy::too_many_lines)] // BABA-1 §4.4, equation by equation
    fn dequantize(&self, b: &[usize; 9], [e0, e1]: [u32; 2]) -> Option<Self> {
        if ERASURE_B0.contains(&b[0]) || repeat_for_errors(e0, e1) || b[0] >= FIRST_TONE_B0 {
            return None;
        }
        let mut cur = *self;
        if SILENCE_B0.contains(&b[0]) {
            // Silence (BABA-1 Eqs. 1–3).
            cur.w0 = ((2.0 * PI) / SILENCE_PERIOD) as f32;
            cur.l = SILENCE_L;
            cur.voiced[1..=SILENCE_L].fill(false);
        } else {
            let f0 = AMBE_W0[b[0]];
            cur.w0 = (f64::from(f0 * 2.0) * PI) as f32;
            cur.l = usize::from(AMBE_L[b[0]]);
            for l in 1..=cur.l {
                let band = (l as f32 * 16.0 * f0) as usize;
                cur.voiced[l] = AMBE_VUV[b[1]][band];
            }
        }
        let l_cur = cur.l;
        let unvoiced_gain = UNVOICED_GAIN / cur.w0.sqrt();
        cur.gamma = AMBE_GAIN[b[2]] + GAIN_PREDICTION * self.gamma;

        // The PRBA vector, and from it each block's first two coefficients.
        let mut gm = [0f32; 9];
        gm[2..=4].copy_from_slice(&AMBE_PRBA24[b[3]]);
        gm[5..=8].copy_from_slice(&AMBE_PRBA58[b[4]]);
        let mut ri = [0f32; 9];
        for i in 1..=8 {
            let mut sum = 0f32;
            for m in 1..=8 {
                let am = if m == 1 { 1.0f32 } else { 2.0 };
                let arg = (PI * (m - 1) as f64 * f64::from(i as f32 - 0.5)) / 8.0;
                sum += am * gm[m] * (arg as f32).cos();
            }
            ri[i] = sum;
        }
        let rconst = (1.0 / (2.0 * SQRT_2)) as f32;
        let mut cik = [[0f32; 18]; 5];
        for i in 1..=4 {
            cik[i][1] = 0.5 * (ri[2 * i - 1] + ri[2 * i]);
            cik[i][2] = rconst * (ri[2 * i - 1] - ri[2 * i]);
        }

        // The higher order coefficients, and each block's inverse DCT.
        let ji = AMBE_BLOCKS[l_cur].map(usize::from);
        let hoc: [&[f32; 4]; 4] = [
            &AMBE_HOC5[b[5]],
            &AMBE_HOC6[b[6]],
            &AMBE_HOC7[b[7]],
            &AMBE_HOC8[b[8]],
        ];
        for i in 1..=4 {
            for k in 3..=ji[i - 1] {
                cik[i][k] = if k > LAST_HOC_K {
                    0.0
                } else {
                    hoc[i - 1][k - 3]
                };
            }
        }
        let mut tl = [0f32; HARMONICS];
        let mut l = 1;
        for i in 1..=4 {
            let n = ji[i - 1];
            for j in 1..=n {
                let mut sum = 0f32;
                for k in 1..=n {
                    let ak = if k == 1 { 1.0f32 } else { 2.0 };
                    let arg = (PI * (k - 1) as f64 * f64::from(j as f32 - 0.5)) / n as f64;
                    sum += ak * cik[i][k] * (arg as f32).cos();
                }
                tl[l] = sum;
                l += 1;
            }
        }

        // log2 Ml: the residuals plus the prediction from this frame's,
        // stretched to the new L.
        let mut prev_log2 = self.log2_ml;
        for l in self.l + 1..=l_cur {
            prev_log2[l] = prev_log2[self.l];
        }
        prev_log2[0] = prev_log2[1];
        let mut kl = [0usize; HARMONICS];
        let mut deltal = [0f32; HARMONICS];
        let mut sum43 = 0f32;
        for l in 1..=l_cur {
            let flokl = (self.l as f32 / l_cur as f32) * l as f32;
            kl[l] = flokl as usize;
            deltal[l] = flokl - kl[l] as f32;
            sum43 += (1.0 - deltal[l]) * prev_log2[kl[l]] + deltal[l] * prev_log2[kl[l] + 1];
        }
        sum43 *= AMPLITUDE_PREDICTION / l_cur as f32;
        let mut sum42 = 0f32;
        for &t in &tl[1..=l_cur] {
            sum42 += t;
        }
        sum42 /= l_cur as f32;
        let big_gamma =
            (f64::from(cur.gamma) - 0.5 * ((l_cur as f64).ln() / LN_2) - f64::from(sum42)) as f32;
        for l in 1..=l_cur {
            let c1 = AMPLITUDE_PREDICTION * (1.0 - deltal[l]) * prev_log2[kl[l]];
            let c2 = AMPLITUDE_PREDICTION * deltal[l] * prev_log2[kl[l] + 1];
            cur.log2_ml[l] = tl[l] + c1 + c2 - sum43 + big_gamma;
            let ml = f64::from(LOG_AMPLITUDE_SCALE * cur.log2_ml[l]).exp();
            cur.ml[l] = if cur.voiced[l] {
                ml as f32
            } else {
                (f64::from(unvoiced_gain) * ml) as f32
            };
        }
        Some(cur)
    }
}

/// op25's quantiser indices b0–b8 from u0–u3 (`p25p2_vf::unpack_b`).
fn indices(AmbeParams(u): AmbeParams) -> [usize; 9] {
    let u = u.map(usize::from);
    [
        ((u[0] >> 5) & 0x78) + ((u[3] >> 9) & 0x7),
        ((u[0] >> 3) & 0x1e) + ((u[3] >> 13) & 0x1),
        ((u[0] << 1) & 0x1e) + ((u[3] >> 12) & 0x1),
        ((u[1] >> 3) & 0x1fe) + ((u[3] >> 8) & 0x1),
        ((u[1] << 3) & 0x78) + ((u[3] >> 5) & 0x7),
        ((u[2] >> 6) & 0x1e) + ((u[3] >> 4) & 0x1),
        ((u[2] >> 3) & 0x0e) + ((u[3] >> 3) & 0x1),
        (u[2] & 0x0e) + ((u[3] >> 2) & 0x1),
        ((u[2] << 2) & 0x04) + (u[3] & 0x3),
    ]
}

/// What kind of frame a frame is (`mbe_dequantizeAmbeTone`).
enum ToneFrame {
    Voice,
    /// A tone frame too damaged to trust, or for no tone there is.
    Erased,
    Tone {
        id: u16,
        amplitude: u16,
    },
}

impl ToneFrame {
    /// `standard`: by BABA-1 §7's test alone.
    fn of(AmbeParams(u): AmbeParams, [e0, e1]: [u32; 2], standard: bool) -> Self {
        // The marker is BABA-1 §7's test; u3[3:0] = 0 is mbelib's besides
        // (§7 sends it, but detects a tone by u0 alone): with an error
        // there, a tone frame falls through as voice (trunk-oxide-13y.3).
        if (u[0] >> 6) & 0x3f != TONE_MARKER || (!standard && u[3] & 0xf != 0) {
            return Self::Voice;
        }
        let amplitude = ((u[0] & 0x3f) << 1) + ((u[3] >> 4) & 0x1);
        // The index is sent four times; three must agree.
        let id = [
            (u[1] & 0xfff) >> 4,
            ((u[1] & 0xf) << 4) + ((u[2] >> 7) & 0xf),
            ((u[2] & 0x7f) << 1) + ((u[3] >> 13) & 0x1),
            (u[3] & 0x1fe0) >> 5,
        ];
        if repeat_for_errors(e0, e1) {
            return Self::Erased;
        }
        let Some(agreed) = id
            .iter()
            .copied()
            .find(|&x| id.iter().filter(|&&y| y == x).count() >= 3)
        else {
            return Self::Erased;
        };
        match tone_frequencies(agreed) {
            Some(_) => Self::Tone {
                id: agreed,
                amplitude,
            },
            None => Self::Erased,
        }
    }
}

/// A tone's two frequencies (Hz), or `None` for an index no tone has.
/// Index 255 is silence.
fn tone_frequencies(id: u16) -> Option<(f32, f32)> {
    let pair = |a: u16, b: u16| Some((f32::from(a), f32::from(b)));
    match id {
        // Single tones.
        5..=122 => {
            let f = (SINGLE_TONE_STEP_HZ * f64::from(id)) as f32;
            Some((f, f))
        }
        // DTMF.
        128 => pair(1336, 941),
        129 => pair(1209, 697),
        130 => pair(1336, 697),
        131 => pair(1477, 697),
        132 => pair(1209, 770),
        133 => pair(1336, 770),
        134 => pair(1477, 770),
        135 => pair(1209, 852),
        136 => pair(1336, 852),
        137 => pair(1477, 852),
        138 => pair(1633, 697),
        139 => pair(1633, 770),
        140 => pair(1633, 852),
        141 => pair(1633, 941),
        142 => pair(1209, 941),
        143 => pair(1477, 941),
        // KNOX.
        144 => pair(1162, 820),
        145 => pair(1052, 606),
        146 => pair(1162, 606),
        147 => pair(1279, 606),
        148 => pair(1052, 672),
        149 => pair(1162, 672),
        150 => pair(1279, 672),
        151 => pair(1052, 743),
        152 => pair(1162, 743),
        153 => pair(1279, 743),
        154 => pair(1430, 606),
        155 => pair(1430, 672),
        156 => pair(1430, 743),
        157 => pair(1430, 820),
        158 => pair(1052, 820),
        159 => pair(1279, 820),
        // Call progress.
        160 => pair(440, 350),
        161 => pair(480, 440),
        162 => pair(620, 480),
        163 => pair(490, 350),
        255 => pair(0, 0),
        _ => None,
    }
}

/// The tone being played, and how far into it (op25's `mbe_tone`).
#[derive(Default)]
struct Tone {
    id: u16,
    amplitude: u16,
    /// Samples played since it started.
    n: i32,
    /// The amplitude is BABA-1's log AD, else op25's linear one.
    logarithmic: bool,
}

impl Tone {
    /// A tone frame: the same tone continues, a new one starts over.
    const fn set(&mut self, id: u16, amplitude: u16) {
        if self.id != id {
            self.id = id;
            self.n = 0;
        }
        self.amplitude = amplitude;
    }

    /// The next frame of the tone (op25's `decode_tone`).
    fn render(&mut self) -> Pcm {
        let (f1, f2) = tone_frequencies(self.id).unwrap_or((0.0, 0.0));
        if f1 == 0.0 && f2 == 0.0 {
            return [0; SAMPLES_PER_FRAME];
        }
        let step1 = (2.0 * PI * f64::from(f1) / 8000.0) as f32;
        let step2 = (2.0 * PI * f64::from(f2) / 8000.0) as f32;
        if self.logarithmic {
            let below = f64::from(TONE_AD_MAX - self.amplitude.min(TONE_AD_MAX));
            let peak = (FULL_SCALE * 10f64.powf(-TONE_AD_STEP_DB * below / 20.0)) as f32;
            #[allow(clippy::float_cmp, reason = "a single tone is sent as two equal ones")]
            let dual = f1 != f2;
            return std::array::from_fn(|_| {
                let n = self.n as f32;
                self.n += 1;
                let second = if dual { (n * step2).sin() } else { 0.0 };
                (peak * ((n * step1).sin() + second)) as i16
            });
        }
        let amplitude = f32::from(self.amplitude * OP25_TONE_AMPLITUDE_SCALE);
        std::array::from_fn(|_| {
            let n = self.n as f32;
            self.n += 1;
            (amplitude * ((n * step1).sin() / 2.0 + (n * step2).sin() / 2.0)) as i16
        })
    }
}
