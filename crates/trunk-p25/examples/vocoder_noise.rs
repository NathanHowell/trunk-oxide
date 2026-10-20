//! The vocoder policies on noisy frames, scored against the clean frames
//! they came from: what each decided (played the right frame, one wrong
//! only in its unprotected bits, one wrong in a protected vector, repeated,
//! muted) and the log-spectral distance of what it played from the clean
//! frame's model (a waveform SNR would not do: one repeat shifts every
//! later harmonic's phase, and the vocoder's output is a model, not a
//! waveform). The measurement behind `Policy::Soft` and its thresholds.
//!
//! `hard` is the path as shipped (`Policy::Hard`): phase 1 the frame as
//! decoded here, phase 2 the hard decoding of the received dibits, each
//! under the standard's rules on the bits it corrected. Each
//! `soft m/u/e` row is the decoder's own frame played when its weakest
//! vector margin ≥ m, its weakest unprotected |LLR| ≥ u and ≤ e bits were
//! corrected, else repeated (three times), then muted.
//!
//! Frames come from `VOCODER_CORPUS=<dir>` (`imbe.txt`, `ambe.txt`, as the
//! trunk-radio `voice_capture` test writes them with
//! `VOICE_CORPUS`): real speech, so a repeat is a real substitute. Without
//! it, random parameters, where a repeat is as wrong as anything.
//!
//! cargo run --release -p trunk-p25 --example `vocoder_noise`
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]
mod common;

use common::{Corpus, Truth, ambe_truth, channel, imbe_truth, log_spectral_distance};
use trunk_p25::tdma::ambe::{self, AmbeCodeword, AmbeFrame, AmbeParams};
use trunk_p25::testgen::Rng;
use trunk_p25::vocoder::{AmbeVocoder, Decision, ImbeVocoder, Policy, Spectrum};
use trunk_p25::voice::ImbeParams;
use trunk_p25::voice::imbe::{self, ImbeFrame};

const SIGMAS: [f64; 5] = [0.3, 0.4, 0.5, 0.6, 0.7];
const SEQUENCES: usize = 40;
const FRAMES: usize = 100;

/// `Soft` gates to try: (margin, unprotected |LLR|, errors).
const GATES: [(f32, f32, u32); 7] = [
    (0.0, 0.0, u32::MAX),
    (2.0, 0.0, u32::MAX),
    (4.0, 0.0, u32::MAX),
    (6.0, 0.0, u32::MAX),
    (2.0, 0.25, u32::MAX),
    (2.0, 0.5, u32::MAX),
    (2.0, 1.0, u32::MAX),
];

#[derive(Default)]
struct Score {
    right: usize,
    unprotected: usize,
    protected: usize,
    repeated: usize,
    muted: usize,
    /// Sum of log-spectral distances (dB) over frames.
    lsd: f64,
    frames: usize,
}

impl Score {
    fn add(
        &mut self,
        decision: Decision,
        truth: Truth,
        played: Option<&Spectrum>,
        clean: &Spectrum,
    ) {
        match (decision, truth) {
            (Decision::Played, Truth::Right) => self.right += 1,
            (Decision::Played, Truth::Unprotected) => self.unprotected += 1,
            (Decision::Played, Truth::Protected) => self.protected += 1,
            (Decision::Repeated, _) => self.repeated += 1,
            (Decision::Muted, _) => self.muted += 1,
        }
        self.lsd += log_spectral_distance(played, clean);
        self.frames += 1;
    }

    fn line(&self) -> String {
        let pct = |n: usize| 100.0 * n as f64 / self.frames as f64;
        format!(
            "played right {:5.1}%  unprotected-wrong {:4.1}%  protected-wrong {:4.1}%  repeated {:4.1}%  muted {:4.1}%  LSD {:5.1} dB",
            pct(self.right),
            pct(self.unprotected),
            pct(self.protected),
            pct(self.repeated),
            pct(self.muted),
            self.lsd / self.frames as f64
        )
    }
}

/// `soft` as the vocoder would decide with gate `g`, by telling it the
/// frame is sure or doubtful.
fn gated_ambe(f: &AmbeFrame, (m, u, e): (f32, f32, u32)) -> AmbeFrame {
    let ok = f.reliability >= m && f.unprotected >= u && f.errors <= e;
    AmbeFrame {
        reliability: if ok { f32::INFINITY } else { 0.0 },
        unprotected: f32::INFINITY,
        errors: 0,
        ..*f
    }
}

fn gated_imbe(f: &ImbeFrame, (m, u, e): (f32, f32, u32)) -> ImbeFrame {
    let ok = f.reliability >= m && f.unprotected >= u && f.errors <= e;
    ImbeFrame {
        reliability: if ok { f32::INFINITY } else { 0.0 },
        unprotected: f32::INFINITY,
        errors: 0,
        errors_c0: 0,
        ..*f
    }
}

fn phase2(calls: &[Vec<AmbeParams>], sigma: f64, rng: &mut Rng) -> Vec<Score> {
    let mut scores: Vec<Score> = (0..=GATES.len()).map(|_| Score::default()).collect();
    for call in calls {
        let mut clean = AmbeVocoder::new();
        let mut shipped = AmbeVocoder::new();
        let mut soft: Vec<AmbeVocoder> = GATES
            .iter()
            .map(|_| AmbeVocoder::with_policy(Policy::Soft))
            .collect();
        for p in call {
            let cw = p.encode();
            let (_, clean_dibits) = channel::<72>(|j| cw.bit(j), 0.0, rng);
            clean.decode(clean_dibits.as_slice().try_into().unwrap());
            let Some(reference) = clean.spectrum() else {
                continue;
            };
            let (llr, dibits) = channel::<72>(|j| cw.bit(j), sigma, rng);
            let dibits: [u8; 36] = dibits.try_into().unwrap();
            let frame = ambe::decode(&llr);

            shipped.decode(&dibits);
            let (heard, _) = AmbeCodeword::from_dibits(&dibits).decode();
            scores[0].add(
                shipped.last(),
                ambe_truth(heard, *p),
                shipped.spectrum().as_ref(),
                &reference,
            );
            for (k, g) in GATES.iter().enumerate() {
                soft[k].vocode(&dibits, Some(&gated_ambe(&frame, *g)));
                scores[k + 1].add(
                    soft[k].last(),
                    ambe_truth(frame.params, *p),
                    soft[k].spectrum().as_ref(),
                    &reference,
                );
            }
        }
    }
    scores
}

fn phase1(calls: &[Vec<ImbeParams>], sigma: f64, rng: &mut Rng) -> Vec<Score> {
    let mut scores: Vec<Score> = (0..=GATES.len()).map(|_| Score::default()).collect();
    for call in calls {
        let mut clean = ImbeVocoder::new();
        let mut shipped = ImbeVocoder::new();
        let mut soft: Vec<ImbeVocoder> = GATES
            .iter()
            .map(|_| ImbeVocoder::with_policy(Policy::Soft))
            .collect();
        for p in call {
            let cw = p.encode();
            clean.decode(&p.0.map(u32::from), 0, 0, 0);
            let Some(reference) = clean.spectrum() else {
                continue;
            };
            let (llr, _) = channel::<144>(|j| cw.bit(j), sigma, rng);
            let frame = imbe::decode(&llr);
            let truth = imbe_truth(&frame.params, p);

            // As shipped.
            shipped.vocode(&frame);
            scores[0].add(
                shipped.last(),
                truth,
                shipped.spectrum().as_ref(),
                &reference,
            );
            for (k, g) in GATES.iter().enumerate() {
                soft[k].vocode(&gated_imbe(&frame, *g));
                scores[k + 1].add(
                    soft[k].last(),
                    truth,
                    soft[k].spectrum().as_ref(),
                    &reference,
                );
            }
        }
    }
    scores
}

fn report(name: &str, source: &str, frames: usize, mut run: impl FnMut(f64) -> Vec<Score>) {
    println!("{name}: {frames} {source} frames a noise level");
    for sigma in SIGMAS {
        let scores = run(sigma);
        println!("  sigma {sigma:.1}");
        println!("    hard           {}", scores[0].line());
        for (k, (m, u, e)) in GATES.iter().enumerate() {
            let e = if *e == u32::MAX {
                "-".to_string()
            } else {
                e.to_string()
            };
            println!("    soft {m:.0}/{u:.1}/{e:<3}   {}", scores[k + 1].line());
        }
    }
}

fn main() {
    let mut rng = Rng::new(7);
    let (corpus, source) = Corpus::load().map_or_else(
        || (Corpus::random(&mut rng, SEQUENCES, FRAMES), "random"),
        |c| (c, "corpus"),
    );
    let n: usize = corpus.imbe.iter().map(Vec::len).sum();
    let mut r = Rng::new(8);
    report("phase 1 (IMBE)", source, n, |sigma| {
        phase1(&corpus.imbe, sigma, &mut r)
    });
    let n: usize = corpus.ambe.iter().map(Vec::len).sum();
    let mut r = Rng::new(9);
    report("phase 2 (AMBE+2)", source, n, |sigma| {
        phase2(&corpus.ambe, sigma, &mut r)
    });
}
