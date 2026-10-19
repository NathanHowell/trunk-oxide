//! Blind A/B listening clips for the vocoder policies: a tapped call under
//! injected noise, vocoded as shipped (`Policy::Op25`) and with
//! `Policy::Soft`, both halves of one clip with a gap between, which half
//! is which drawn at random and written to the key.
//!
//! Not every call: each call at each noise level is scored by how far the
//! two policies' played spectra diverge (log-spectral distance against the
//! clean frames, frame by frame), the clip is cut from the window where
//! they diverge most, and only the few calls that diverge most, plus one
//! typical of the rest, are written. Where the policies agree there is
//! nothing to hear.
//!
//! `VOCODER_CORPUS=<dir> cargo run --release -p trunk-p25 --example
//! vocoder_ab -- <out dir>`
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
mod common;

use std::fmt::Write as _;

use common::{Corpus, channel, log_spectral_distance};
use trunk_p25::tdma::AmbeParams;
use trunk_p25::tdma::ambe;
use trunk_p25::testgen::Rng;
use trunk_p25::vocoder::{AmbeVocoder, ImbeVocoder, Pcm, Policy, SAMPLES_PER_FRAME};
use trunk_p25::voice::ImbeParams;
use trunk_p25::voice::imbe;

const SIGMAS: [f64; 2] = [0.5, 0.6];
/// Frames a clip's half holds (7 s).
const WINDOW: usize = 350;
/// Silence between the halves.
const GAP_FRAMES: usize = 30;
/// Clips a phase gets: the most divergent calls, then one typical.
const MOST_DIVERGENT: [usize; 2] = [3, 2];
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
    /// Mean LSD of each policy over the window.
    lsd: [f64; 2],
    a: Vec<i16>,
    b: Vec<i16>,
}

/// Per frame: both policies' audio and their distance from the clean frame.
struct Frame {
    pcm: [Option<Pcm>; 2],
    lsd: [f64; 2],
}

fn candidate(phase: u8, call: usize, sigma: f64, frames: &[Frame]) -> Option<Candidate> {
    if frames.len() < WINDOW {
        return None;
    }
    let d: Vec<f64> = frames.iter().map(|f| (f.lsd[0] - f.lsd[1]).abs()).collect();
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

fn phase1(call: &[ImbeParams], sigma: f64, rng: &mut Rng) -> Vec<Frame> {
    let mut clean = ImbeVocoder::new();
    let mut v = [
        ImbeVocoder::with_policy(Policy::Op25),
        ImbeVocoder::with_policy(Policy::Soft),
    ];
    call.iter()
        .filter_map(|p| {
            clean.decode(&p.to_op25(), 0, 0);
            let reference = clean.spectrum()?;
            let (llr, _) = channel::<144>(|j| p.encode().bit(j), sigma, rng);
            let frame = imbe::decode(&llr);
            let pcm = [0, 1].map(|k| v[k].vocode(&frame, 0, 0));
            let lsd = [0, 1].map(|k| log_spectral_distance(v[k].spectrum().as_ref(), &reference));
            Some(Frame { pcm, lsd })
        })
        .collect()
}

fn phase2(call: &[AmbeParams], sigma: f64, rng: &mut Rng) -> Vec<Frame> {
    let mut clean = AmbeVocoder::new();
    let mut v = [
        AmbeVocoder::with_policy(Policy::Op25),
        AmbeVocoder::with_policy(Policy::Soft),
    ];
    call.iter()
        .filter_map(|p| {
            let cw = p.encode();
            let (_, clean_dibits) = channel::<72>(|j| cw.bit(j), 0.0, rng);
            clean.decode(clean_dibits.as_slice().try_into().unwrap());
            let reference = clean.spectrum()?;
            let (llr, dibits) = channel::<72>(|j| cw.bit(j), sigma, rng);
            let dibits: [u8; 36] = dibits.try_into().unwrap();
            let frame = ambe::decode(&llr);
            let pcm = [0, 1].map(|k| v[k].vocode(&dibits, Some(&frame)));
            let lsd = [0, 1].map(|k| log_spectral_distance(v[k].spectrum().as_ref(), &reference));
            Some(Frame { pcm, lsd })
        })
        .collect()
}

/// The most divergent candidates of a phase (distinct calls), then the
/// median of those that differ at all.
fn pick(mut all: Vec<Candidate>, most: usize) -> Vec<(Candidate, &'static str)> {
    all.sort_by(|x, y| y.divergence.total_cmp(&x.divergence));
    let mut chosen = Vec::new();
    let mut calls = Vec::new();
    let mut rest = Vec::new();
    for c in all {
        if chosen.len() < most && !calls.contains(&c.call) {
            calls.push(c.call);
            chosen.push((c, "most divergent"));
        } else {
            rest.push(c);
        }
    }
    let differing: Vec<Candidate> = rest.into_iter().filter(|c| c.differing >= 10).collect();
    if !differing.is_empty() {
        let mid = differing.len() / 2;
        chosen.push((differing.into_iter().nth(mid).unwrap(), "typical"));
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
    let out = std::env::args().nth(1).expect("an output directory");
    std::fs::create_dir_all(&out).unwrap();
    let corpus = Corpus::load().expect("VOCODER_CORPUS");
    let mut rng = Rng::new(21);
    let mut chosen = Vec::new();
    for (phase, most) in [(1u8, MOST_DIVERGENT[0]), (2, MOST_DIVERGENT[1])] {
        let mut all = Vec::new();
        for sigma in SIGMAS {
            let calls = if phase == 1 {
                corpus.imbe.len()
            } else {
                corpus.ambe.len()
            };
            for call in 0..calls {
                let frames = if phase == 1 {
                    phase1(&corpus.imbe[call], sigma, &mut rng)
                } else {
                    phase2(&corpus.ambe[call], sigma, &mut rng)
                };
                all.extend(candidate(phase, call, sigma, &frames));
            }
        }
        println!("phase {phase}: {} candidates", all.len());
        chosen.extend(pick(all, most));
    }

    // Sides drawn from the clock, so the key is the only record.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let mut sides = Rng::new(seed);
    let mut key = String::from("[\n");
    for (i, (c, why)) in chosen.iter().enumerate() {
        let soft_first = sides.next_u64().is_multiple_of(2);
        let (first, second) = if soft_first {
            (&c.b, &c.a)
        } else {
            (&c.a, &c.b)
        };
        let mut pcm = first.clone();
        pcm.extend(std::iter::repeat_n(0i16, GAP_FRAMES * SAMPLES_PER_FRAME));
        pcm.extend(second);
        let file = format!("clip_{:02}.wav", i + 1);
        wav(&format!("{out}/{file}"), &pcm);
        let (left, right) = if soft_first {
            ("soft", "op25")
        } else {
            ("op25", "soft")
        };
        let _ = writeln!(
            key,
            "  {{\"file\": \"{file}\", \"phase\": {}, \"sigma\": {}, \"call\": {}, \"start_s\": {:.1}, \"why\": \"{why}\", \"left\": \"{left}\", \"right\": \"{right}\", \"divergence\": {:.1}, \"differing_frames\": {}, \"lsd_op25\": {:.2}, \"lsd_soft\": {:.2}}}{}",
            c.phase,
            c.sigma,
            c.call,
            c.start as f64 / 50.0,
            c.divergence,
            c.differing,
            c.lsd[0],
            c.lsd[1],
            if i + 1 < chosen.len() { "," } else { "" }
        );
        println!(
            "{file}: phase {} sigma {} call {} ({why}); {} of {WINDOW} frames differ, LSD op25 {:.1} soft {:.1}",
            c.phase, c.sigma, c.call, c.differing, c.lsd[0], c.lsd[1]
        );
    }
    key.push_str("]\n");
    std::fs::write(format!("{out}/key.json"), key).unwrap();
}
