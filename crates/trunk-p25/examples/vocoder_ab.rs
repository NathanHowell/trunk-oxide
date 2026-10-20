//! Blind A/B listening clips for two vocoder policies (hard and soft
//! unless named): a tapped call, clean or under injected noise,
//! vocoded both ways, both halves of one clip with a gap between, which
//! half is which drawn at random and written to the key.
//!
//! Not every call: each call at each noise level is scored by how far the
//! two policies' played spectra diverge from each other (log-spectral
//! distance, frame by frame), the clip is cut from the window where they
//! diverge most, and only the few calls that diverge most, plus one typical
//! of the rest, are written, clean and noisy apart. Where the policies
//! agree there is nothing to hear. Each policy's distance from its own
//! clean decoding of the call is reported beside it.
//!
//! `VOCODER_CORPUS=<dir> cargo run --release -p trunk-p25 --example
//! vocoder_ab -- <out dir> [<policy a> <policy b>]` (`hard`, `soft`)
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

const SIGMAS: [f64; 3] = [0.0, 0.5, 0.6];
/// Frames a clip's half holds (7 s).
const WINDOW: usize = 350;
/// Silence between the halves.
const GAP_FRAMES: usize = 30;
/// Clips a phase gets, clean and noisy each: the most divergent calls,
/// then one typical.
const MOST_DIVERGENT: [usize; 2] = [2, 2];
const RATE: u32 = 8000;

/// One call under one noise level, both ways.
struct Candidate {
    phase: u8,
    call: usize,
    sigma: f64,
    /// Where the window starts (frames).
    start: usize,
    /// Σ|`LSD_a` − `LSD_b`| over the window.
    divergence: f64,
    /// Frames in the window whose audio differs.
    differing: usize,
    /// Mean LSD of each policy from its own clean decoding, over the window.
    lsd: [f64; 2],
    a: Vec<i16>,
    b: Vec<i16>,
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

fn candidate(phase: u8, call: usize, sigma: f64, frames: &[Frame]) -> Option<Candidate> {
    if frames.len() < WINDOW {
        return None;
    }
    let d: Vec<f64> = frames.iter().map(|f| f.apart).collect();
    let (start, divergence) = (0..=frames.len() - WINDOW)
        .step_by(25)
        .map(|s| (s, d[s..s + WINDOW].iter().sum::<f64>()))
        .max_by(|x, y| x.1.total_cmp(&y.1))?;
    let window = &frames[start..start + WINDOW];
    let silence = [0i16; SAMPLES_PER_FRAME];
    let audio = |k: usize| -> Vec<i16> {
        window
            .iter()
            .flat_map(|f| f.pcm[k].unwrap_or(silence))
            .collect()
    };
    Some(Candidate {
        phase,
        call,
        sigma,
        start,
        divergence,
        differing: window.iter().filter(|f| f.pcm[0] != f.pcm[1]).count(),
        lsd: [0, 1].map(|k| window.iter().map(|f| f.lsd[k]).sum::<f64>() / WINDOW as f64),
        a: audio(0),
        b: audio(1),
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

/// The most divergent candidates of a phase (distinct calls), then the
/// median of those that differ at all.
fn pick(mut all: Vec<Candidate>, most: usize) -> Vec<(Candidate, &'static str)> {
    all.sort_by(|x, y| y.divergence.total_cmp(&x.divergence));
    let mut chosen = Vec::new();
    let mut calls = Vec::new();
    let mut rest = Vec::new();
    // Where the two agree there is nothing to hear.
    for c in all.into_iter().filter(|c| c.differing >= 10) {
        if chosen.len() < most && !calls.contains(&c.call) {
            calls.push(c.call);
            chosen.push((c, "most divergent"));
        } else {
            rest.push(c);
        }
    }
    if !rest.is_empty() {
        let mid = rest.len() / 2;
        chosen.push((rest.into_iter().nth(mid).unwrap(), "typical"));
    }
    chosen
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
    let names = [
        args.get(2).map_or("hard", String::as_str),
        args.get(3).map_or("soft", String::as_str),
    ];
    let policies = names.map(policy);
    std::fs::create_dir_all(out).unwrap();
    let corpus = Corpus::load_frames(true).expect("VOCODER_CORPUS");
    let mut rng = Rng::new(21);
    let mut chosen = Vec::new();
    for (phase, most) in [(1u8, MOST_DIVERGENT[0]), (2, MOST_DIVERGENT[1])] {
        let mut clean = Vec::new();
        let mut noisy = Vec::new();
        for sigma in SIGMAS {
            let calls = if phase == 1 {
                corpus.imbe.len()
            } else {
                corpus.ambe.len()
            };
            let (mut frames_in, mut differing, mut lsd) = (0usize, 0usize, [0f64; 2]);
            let mut unplayed = [0usize; 2];
            for call in 0..calls {
                let frames = if phase == 1 {
                    phase1(&corpus.imbe[call], sigma, policies, &mut rng)
                } else {
                    phase2(&corpus.ambe[call], sigma, policies, &mut rng)
                };
                frames_in += frames.len();
                differing += frames.iter().filter(|f| f.pcm[0] != f.pcm[1]).count();
                for f in &frames {
                    for k in 0..2 {
                        lsd[k] += f.lsd[k];
                        unplayed[k] += usize::from(!f.played[k]);
                    }
                }
                let to = if sigma == 0.0 { &mut clean } else { &mut noisy };
                to.extend(candidate(phase, call, sigma, &frames));
            }
            println!(
                "phase {phase} sigma {sigma}: {differing} of {frames_in} frames differ; LSD from own clean {} {:.2} dB ({} repeated or muted), {} {:.2} dB ({})",
                names[0],
                lsd[0] / frames_in as f64,
                unplayed[0],
                names[1],
                lsd[1] / frames_in as f64,
                unplayed[1],
            );
        }
        chosen.extend(pick(clean, most));
        chosen.extend(pick(noisy, most));
    }

    // Sides drawn from the clock, so the key is the only record.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let mut sides = Rng::new(seed);
    let mut key = String::from("[\n");
    for (i, (c, why)) in chosen.iter().enumerate() {
        let b_first = sides.next_u64().is_multiple_of(2);
        let (first, second) = if b_first { (&c.b, &c.a) } else { (&c.a, &c.b) };
        let mut pcm = first.clone();
        pcm.extend(std::iter::repeat_n(0i16, GAP_FRAMES * SAMPLES_PER_FRAME));
        pcm.extend(second);
        let file = format!("clip_{:02}.wav", i + 1);
        wav(&format!("{out}/{file}"), &pcm);
        let [left, right] = if b_first { [names[1], names[0]] } else { names };
        let _ = writeln!(
            key,
            "  {{\"file\": \"{file}\", \"phase\": {}, \"sigma\": {}, \"call\": {}, \"start_s\": {:.1}, \"why\": \"{why}\", \"left\": \"{left}\", \"right\": \"{right}\", \"divergence\": {:.1}, \"differing_frames\": {}, \"lsd_{}\": {:.2}, \"lsd_{}\": {:.2}}}{}",
            c.phase,
            c.sigma,
            c.call,
            c.start as f64 / 50.0,
            c.divergence,
            c.differing,
            names[0],
            c.lsd[0],
            names[1],
            c.lsd[1],
            if i + 1 < chosen.len() { "," } else { "" }
        );
        println!(
            "{file}: phase {} sigma {} call {} ({why}); {} of {WINDOW} frames differ, LSD {} {:.1} {} {:.1}",
            c.phase, c.sigma, c.call, c.differing, names[0], c.lsd[0], names[1], c.lsd[1]
        );
    }
    key.push_str("]\n");
    std::fs::write(format!("{out}/key.json"), key).unwrap();
}
