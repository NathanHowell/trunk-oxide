//! Blind A/B listening clips between two vocoder policies: tapped calls,
//! clean or under injected noise, vocoded both ways, the two halves of one
//! clip with a pause between. Each clip is written twice, the halves
//! swapped, so a listener's preference for a side cancels out; the order
//! of the clips is shuffled and the key records which half is which.
//!
//! A clip is short (`WINDOW`, 2 s) and cut where the policies diverge
//! most: each call at each noise level is scored frame by frame by how far
//! the two policies' played spectra are apart (log-spectral distance), and
//! the window with the most of it is the clip, so what differs is the
//! thing heard. Only the calls that diverge most are kept, a few per noise
//! level. Each policy's distance from its own clean decoding is reported
//! beside it.
//!
//! `VOCODER_CORPUS=<dir> cargo run --release -p trunk-p25 --example
//! vocoder_ab -- <out dir> <test>...`, a test being
//! `<name>:<phase>:<policy a>:<policy b>:<σ>,<σ>..` (policies `hard`,
//! `soft`), e.g. `soft:1:hard:soft:0.4,0.6`.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
mod common;

use std::fmt::Write as _;

use common::{Corpus, channel, log_spectral_distance};
use trunk_p25::tdma::AmbeParams;
use trunk_p25::tdma::ambe;
use trunk_p25::testgen::Rng;
use trunk_p25::vocoder::{
    AmbeVocoder, Decision, ImbeVocoder, Pcm, Policy, SAMPLES_PER_FRAME, Spectrum,
};
use trunk_p25::voice::ImbeParams;
use trunk_p25::voice::imbe;

/// Frames a clip's half holds (2 s).
const WINDOW: usize = 100;
/// How far apart the windows tried start (frames).
const WINDOW_STEP: usize = 5;
/// Silence between the halves.
const GAP_FRAMES: usize = 30;
/// Each half fades in and out over this many samples (10 ms), so a cut
/// mid-word does not click.
const FADE: usize = 80;
/// Clips per test and noise level: the calls that diverge most.
const PER_SIGMA: usize = 2;
/// Fewer frames differing than this and there is nothing to hear.
const MIN_DIFFERING: usize = 10;
const RATE: u32 = 8000;

/// One call under one noise level, both ways.
struct Candidate {
    call: usize,
    sigma: f64,
    /// Where the window starts (frames).
    start: usize,
    /// Σ distance between the two over the window.
    divergence: f64,
    /// Frames in the window whose audio differs.
    differing: usize,
    /// Mean LSD of each policy from its own clean decoding, over the window.
    lsd: [f64; 2],
    pcm: [Vec<i16>; 2],
}

/// Per frame: both policies' audio, each one's distance from its own
/// clean decoding, and the distance between them.
struct Frame {
    pcm: [Option<Pcm>; 2],
    lsd: [f64; 2],
    apart: f64,
    /// Each policy's: played as decoded, or not.
    played: [bool; 2],
}

impl Frame {
    #[allow(clippy::large_types_passed_by_value, reason = "kept in the frame")]
    fn new(
        pcm: [Option<Pcm>; 2],
        played: &[Option<Spectrum>; 2],
        clean: [&Spectrum; 2],
        decisions: [Decision; 2],
    ) -> Self {
        let lsd = [0, 1].map(|k| log_spectral_distance(played[k].as_ref(), clean[k]));
        let apart = match played {
            [_, Some(b)] => log_spectral_distance(played[0].as_ref(), b),
            [Some(a), None] => log_spectral_distance(None, a),
            [None, None] => 0.0,
        };
        Self {
            pcm,
            lsd,
            apart,
            played: decisions.map(|d| d == Decision::Played),
        }
    }
}

fn candidate(call: usize, sigma: f64, frames: &[Frame]) -> Option<Candidate> {
    if frames.len() < WINDOW {
        return None;
    }
    let d: Vec<f64> = frames.iter().map(|f| f.apart).collect();
    let (start, divergence) = (0..=frames.len() - WINDOW)
        .step_by(WINDOW_STEP)
        .map(|s| (s, d[s..s + WINDOW].iter().sum::<f64>()))
        .max_by(|x, y| x.1.total_cmp(&y.1))?;
    let window = &frames[start..start + WINDOW];
    let silence = [0i16; SAMPLES_PER_FRAME];
    let audio = |k: usize| -> Vec<i16> {
        let mut pcm: Vec<i16> = window
            .iter()
            .flat_map(|f| f.pcm[k].unwrap_or(silence))
            .collect();
        let n = pcm.len();
        for i in 0..FADE {
            let g = i as f32 / FADE as f32;
            pcm[i] = (f32::from(pcm[i]) * g) as i16;
            pcm[n - 1 - i] = (f32::from(pcm[n - 1 - i]) * g) as i16;
        }
        pcm
    };
    Some(Candidate {
        call,
        sigma,
        start,
        divergence,
        differing: window.iter().filter(|f| f.pcm[0] != f.pcm[1]).count(),
        lsd: [0, 1].map(|k| window.iter().map(|f| f.lsd[k]).sum::<f64>() / WINDOW as f64),
        pcm: [audio(0), audio(1)],
    })
}

fn phase1(call: &[ImbeParams], sigma: f64, policies: [Policy; 2], rng: &mut Rng) -> Vec<Frame> {
    let mut clean = policies.map(ImbeVocoder::with_policy);
    let mut v = policies.map(ImbeVocoder::with_policy);
    call.iter()
        .filter_map(|p| {
            let (llr, _) = channel::<144>(|j| p.encode().bit(j), 0.0, rng);
            let sent = imbe::decode(&llr);
            let reference = [0, 1].map(|k| {
                clean[k].vocode(&sent, 0, 0);
                clean[k].spectrum()
            });
            let [Some(r0), Some(r1)] = &reference else {
                return None;
            };
            let (llr, _) = channel::<144>(|j| p.encode().bit(j), sigma, rng);
            let frame = imbe::decode(&llr);
            // e0 = et = 0: what op25 is given after regeneration, as live.
            let pcm = [0, 1].map(|k| v[k].vocode(&frame, 0, 0));
            let decisions = [0, 1].map(|k| v[k].last());
            Some(Frame::new(
                pcm,
                &[0, 1].map(|k| v[k].spectrum()),
                [r0, r1],
                decisions,
            ))
        })
        .collect()
}

fn phase2(call: &[AmbeParams], sigma: f64, policies: [Policy; 2], rng: &mut Rng) -> Vec<Frame> {
    let mut clean = policies.map(AmbeVocoder::with_policy);
    let mut v = policies.map(AmbeVocoder::with_policy);
    call.iter()
        .filter_map(|p| {
            let cw = p.encode();
            let (clean_llr, clean_dibits) = channel::<72>(|j| cw.bit(j), 0.0, rng);
            let clean_dibits: [u8; 36] = clean_dibits.try_into().unwrap();
            let sent = ambe::decode(&clean_llr);
            let reference = [0, 1].map(|k| {
                clean[k].vocode(&clean_dibits, Some(&sent));
                clean[k].spectrum()
            });
            let [Some(r0), Some(r1)] = &reference else {
                return None;
            };
            let (llr, dibits) = channel::<72>(|j| cw.bit(j), sigma, rng);
            let dibits: [u8; 36] = dibits.try_into().unwrap();
            let frame = ambe::decode(&llr);
            let pcm = [0, 1].map(|k| v[k].vocode(&dibits, Some(&frame)));
            let decisions = [0, 1].map(|k| v[k].last());
            Some(Frame::new(
                pcm,
                &[0, 1].map(|k| v[k].spectrum()),
                [r0, r1],
                decisions,
            ))
        })
        .collect()
}

fn policy(name: &str) -> Policy {
    match name {
        "hard" => Policy::Hard,
        "soft" => Policy::Soft,
        _ => panic!("no policy {name}: hard or soft"),
    }
}

/// The `most` candidates that diverge most, from distinct calls.
fn pick(mut all: Vec<Candidate>, most: usize) -> Vec<Candidate> {
    all.sort_by(|x, y| y.divergence.total_cmp(&x.divergence));
    let mut calls = Vec::new();
    all.into_iter()
        .filter(|c| c.differing >= MIN_DIFFERING)
        .filter(|c| {
            let new = !calls.contains(&c.call);
            if new {
                calls.push(c.call);
            }
            new
        })
        .take(most)
        .collect()
}

/// One comparison: two policies on one phase at some noise levels.
struct Test {
    name: String,
    phase: u8,
    names: [String; 2],
    policies: [Policy; 2],
    sigmas: Vec<f64>,
}

impl Test {
    fn parse(spec: &str) -> Self {
        let parts: Vec<&str> = spec.split(':').collect();
        let [name, phase, a, b, sigmas] = parts[..] else {
            panic!("a test is <name>:<phase>:<policy a>:<policy b>:<σ>,..: {spec}");
        };
        Self {
            name: name.to_owned(),
            phase: phase.parse().expect("phase 1 or 2"),
            names: [a.to_owned(), b.to_owned()],
            policies: [policy(a), policy(b)],
            sigmas: sigmas.split(',').map(|s| s.parse().expect("a σ")).collect(),
        }
    }

    /// The test's clips, scored and picked.
    fn run(&self, corpus: &Corpus, rng: &mut Rng) -> Vec<Candidate> {
        let mut chosen = Vec::new();
        for &sigma in &self.sigmas {
            let calls = if self.phase == 1 {
                corpus.imbe.len()
            } else {
                corpus.ambe.len()
            };
            let (mut frames_in, mut differing, mut lsd) = (0usize, 0usize, [0f64; 2]);
            let mut unplayed = [0usize; 2];
            let mut all = Vec::new();
            for call in 0..calls {
                let frames = if self.phase == 1 {
                    phase1(&corpus.imbe[call], sigma, self.policies, rng)
                } else {
                    phase2(&corpus.ambe[call], sigma, self.policies, rng)
                };
                frames_in += frames.len();
                differing += frames.iter().filter(|f| f.pcm[0] != f.pcm[1]).count();
                for f in &frames {
                    for k in 0..2 {
                        lsd[k] += f.lsd[k];
                        unplayed[k] += usize::from(!f.played[k]);
                    }
                }
                all.extend(candidate(call, sigma, &frames));
            }
            println!(
                "{} (phase {}) σ {sigma}: {differing} of {frames_in} frames differ; LSD from own clean {} {:.2} dB ({} repeated or muted), {} {:.2} dB ({})",
                self.name,
                self.phase,
                self.names[0],
                lsd[0] / frames_in as f64,
                unplayed[0],
                self.names[1],
                lsd[1] / frames_in as f64,
                unplayed[1],
            );
            chosen.extend(pick(all, PER_SIGMA));
        }
        chosen
    }
}

fn wav(path: &str, pcm: &[i16]) {
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    let data = (pcm.len() * 2) as u32;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).expect("an output directory");
    let tests: Vec<Test> = args[2..].iter().map(|s| Test::parse(s)).collect();
    assert!(!tests.is_empty(), "no tests");
    std::fs::create_dir_all(out).unwrap();
    let corpus = Corpus::load_frames(true).expect("VOCODER_CORPUS");
    let mut rng = Rng::new(21);
    let chosen: Vec<(&Test, Candidate)> = tests
        .iter()
        .flat_map(|t| t.run(&corpus, &mut rng).into_iter().map(move |c| (t, c)))
        .collect();
    // Every clip both ways round, in an order drawn from the clock, so the
    // key is the only record.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let mut sides = Rng::new(seed);
    let mut order: Vec<(usize, bool)> = (0..chosen.len())
        .flat_map(|i| [(i, false), (i, true)])
        .collect();
    for i in (1..order.len()).rev() {
        order.swap(i, sides.below(i + 1));
    }
    let mut key = String::from("[\n");
    for (n, &(i, swapped)) in order.iter().enumerate() {
        let (t, c) = &chosen[i];
        let [first, second] = if swapped { [1, 0] } else { [0, 1] };
        let mut pcm = c.pcm[first].clone();
        pcm.extend(std::iter::repeat_n(0i16, GAP_FRAMES * SAMPLES_PER_FRAME));
        pcm.extend(&c.pcm[second]);
        let file = format!("clip_{:02}.wav", n + 1);
        wav(&format!("{out}/{file}"), &pcm);
        let _ = writeln!(
            key,
            "  {{\"file\": \"{file}\", \"test\": \"{}\", \"pair\": {i}, \"phase\": {}, \"sigma\": {}, \"call\": {}, \"start_s\": {:.1}, \"left\": \"{}\", \"right\": \"{}\", \"divergence\": {:.1}, \"differing_frames\": {}, \"lsd_left\": {:.2}, \"lsd_right\": {:.2}}}{}",
            t.name,
            t.phase,
            c.sigma,
            c.call,
            c.start as f64 / 50.0,
            t.names[first],
            t.names[second],
            c.divergence,
            c.differing,
            c.lsd[first],
            c.lsd[second],
            if n + 1 < order.len() { "," } else { "" }
        );
        println!(
            "{file}: {} σ {} call {} at {:.1} s, pair {i}; {} of {WINDOW} frames differ, LSD {} {:.2} {} {:.2}",
            t.name,
            c.sigma,
            c.call,
            c.start as f64 / 50.0,
            c.differing,
            t.names[0],
            c.lsd[0],
            t.names[1],
            c.lsd[1]
        );
    }
    key.push_str("]\n");
    std::fs::write(format!("{out}/key.json"), key).unwrap();
}
