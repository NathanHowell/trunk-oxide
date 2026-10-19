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

use super::synth::{HARMONICS, Pcm, SAMPLES_PER_FRAME, Synthesizer};
use super::tables::{
    AMBE_BLOCKS, AMBE_GAIN, AMBE_HOC5, AMBE_HOC6, AMBE_HOC7, AMBE_HOC8, AMBE_L, AMBE_PRBA24,
    AMBE_PRBA58, AMBE_VUV, AMBE_W0,
};
use super::{Decision, PLAY_MARGIN, Policy, Spectrum};
use crate::tdma::ambe::{AmbeCodeword, AmbeFrame, AmbeParams};

/// A recorder's phase 2 vocoder, kept across calls as op25 keeps its own.
pub struct AmbeVocoder {
    policy: Policy,
    model: Model,
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

const MAX_ERROR_RATE: f64 = 0.096;

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
            model: Model::INITIAL,
            tone: Tone::default(),
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
    /// under op25's rules whatever the policy. `None` if it is muted
    /// (silence) for errors or too many repeats.
    pub fn decode(&mut self, dibits: &[u8; 36]) -> Option<Pcm> {
        let (u, errors) = self.receive(dibits);
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
        match ToneFrame::of(u, errors) {
            ToneFrame::Tone { id, amplitude } => {
                self.tone.set(id, amplitude);
                self.accept(true)
            }
            ToneFrame::Erased => self.reject(self.tone_frame),
            ToneFrame::Voice => match self.model.dequantize(&indices(u), errors) {
                Some(model) => {
                    self.model = model;
                    self.accept(false)
                }
                None => self.reject(!self.tone_frame),
            },
        }
    }

    /// An encrypted frame: no audio, but its errors count toward the rate.
    pub fn encrypted(&mut self, dibits: &[u8; 36]) {
        self.receive(dibits);
    }

    fn receive(&mut self, dibits: &[u8; 36]) -> (AmbeParams, [u32; 2]) {
        let (u, errors) = AmbeCodeword::from_dibits(dibits).decode_op25();
        self.error_rate = 0.95 * self.error_rate + 0.001064 * f64::from(errors[0] + errors[1]);
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
    const fn reject(&mut self, repeatable: bool) -> bool {
        self.rejected += 1;
        if self.rejected < 4 && repeatable {
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
        let se = s.enhance();
        s.smooth(se, 0.0, 0.0);
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
    /// `mbe_initMbeParms`.
    const INITIAL: Self = Self {
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
    fn dequantize(&self, b: &[usize; 9], [e0, e1]: [u32; 2]) -> Option<Self> {
        if (120..=123).contains(&b[0]) || e0 >= 4 || (e0 >= 2 && e0 + e1 >= 6) || b[0] >= 126 {
            return None;
        }
        let mut cur = *self;
        if b[0] >= 124 {
            // Silence.
            cur.w0 = ((2.0 * PI) / 32.0) as f32;
            cur.l = 14;
            cur.voiced[1..=14].fill(false);
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
        let unvoiced_gain = 0.2046f32 / cur.w0.sqrt();
        cur.gamma = AMBE_GAIN[b[2]] + 0.5 * self.gamma;

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
                cik[i][k] = if k > 6 { 0.0 } else { hoc[i - 1][k - 3] };
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
        sum43 *= 0.65 / l_cur as f32;
        let mut sum42 = 0f32;
        for &t in &tl[1..=l_cur] {
            sum42 += t;
        }
        sum42 /= l_cur as f32;
        let big_gamma =
            (f64::from(cur.gamma) - 0.5 * ((l_cur as f64).ln() / LN_2) - f64::from(sum42)) as f32;
        for l in 1..=l_cur {
            let c1 = 0.65 * (1.0 - deltal[l]) * prev_log2[kl[l]];
            let c2 = 0.65 * deltal[l] * prev_log2[kl[l] + 1];
            cur.log2_ml[l] = tl[l] + c1 + c2 - sum43 + big_gamma;
            let ml = f64::from(0.693f32 * cur.log2_ml[l]).exp();
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
    fn of(AmbeParams(u): AmbeParams, [e0, e1]: [u32; 2]) -> Self {
        if (u[0] >> 6) & 0x3f != 63 || u[3] & 0xf != 0 {
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
        if e0 >= 4 || (e0 >= 2 && e0 + e1 >= 6) {
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
            let f = (31.25 * f64::from(id)) as f32;
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
        let amplitude = f32::from(self.amplitude * 75);
        std::array::from_fn(|_| {
            let n = self.n as f32;
            self.n += 1;
            (amplitude * ((n * step1).sin() / 2.0 + (n * step2).sin() / 2.0)) as i16
        })
    }
}
