//! The voice pipeline and vocoders against op25, frozen: every fixture here
//! was taken from op25's own frame assembler and vocoders (driven directly
//! from C++, since deleted) while they were in the tree, and the Rust had
//! matched them sample for sample on 16k random frames a phase and 375
//! tapped calls. The hashes are of the 16-bit samples, little endian
//! (FNV-1a 64).

use std::collections::BTreeMap;

use trunk_p25::framer::{Duid, ideal};
use trunk_p25::tdma::AmbeParams;
use trunk_p25::tdma::ambe::AmbeCodeword;
use trunk_p25::testgen::{self, Rng};
use trunk_p25::types::Nac;
use trunk_p25::voice::signalling::{ALGID_CLEAR, encode};
use trunk_p25::voice::{EncryptionSync, ImbeFrame, ImbeParams};

use super::*;

const NAC: Nac = Nac::new(0x38d).unwrap();
const LC: [u8; 9] = [0x00, 0x00, 0x00, 0x00, 0x03, 0x61, 0x12, 0x34, 0x56];

fn scrambler() -> Scrambler {
    Scrambler::new(0xbee00, 0x5e4, Nac::new(0x5e1).unwrap())
}

fn fnv(pcm: &[i16]) -> u64 {
    pcm.iter()
        .flat_map(|s| s.to_le_bytes())
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}

/// One frame's vocoder input as op25 would have been given it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Input {
    /// Phase 1: the parameters and error counts, or encrypted (silent).
    Imbe(Option<([u32; 8], u32, u32)>),
    Ambe([u8; 36], bool),
}

impl Input {
    fn of(v: Vocode) -> Self {
        match v {
            Vocode::Imbe { frame, e0, et } => Self::Imbe(Some((frame.params.to_op25(), e0, et))),
            Vocode::Silence => Self::Imbe(None),
            Vocode::Ambe {
                dibits, encrypted, ..
            } => Self::Ambe(dibits, encrypted),
        }
    }

    fn vocode(self) -> Vocode {
        match self {
            // op25's inputs alone: as op25 would see them, nothing soft.
            Self::Imbe(Some((u, e0, et))) => Vocode::Imbe {
                frame: ImbeFrame {
                    params: ImbeParams::from_op25(u),
                    errors: et,
                    errors_c0: e0,
                    reliability: f32::INFINITY,
                    unprotected: f32::INFINITY,
                },
                e0,
                et,
            },
            Self::Imbe(None) => Vocode::Silence,
            Self::Ambe(dibits, encrypted) => Vocode::Ambe {
                dibits,
                frame: None,
                encrypted,
            },
        }
    }
}

/// A call's audio, vocoder inputs, in-band sources (each change) and
/// terminators.
#[derive(Default)]
struct Call {
    pcm: Vec<i16>,
    inputs: Vec<Input>,
    sources: Vec<i64>,
    terminators: usize,
}

fn call_of(out: &[CallOutput]) -> Call {
    let mut c = Call::default();
    for o in out {
        match o {
            CallOutput::Audio { pcm, frame } => {
                c.pcm.extend(pcm);
                c.inputs.push(Input::of(*frame));
            }
            CallOutput::Event(CallEvent::Source(s)) => {
                if c.sources.last() != Some(&i64::from(*s)) {
                    c.sources.push(i64::from(*s));
                }
            }
            CallOutput::Event(CallEvent::Terminator(_)) => c.terminators += 1,
            CallOutput::Event(_) => {}
        }
    }
    c
}

fn phase1_call(rng: &mut Rng, algid: u8, superframes: usize) -> Vec<u8> {
    let mut d = vec![0u8; 100];
    d.extend(testgen::hdu(NAC, &[0; 9], 0, algid, 0, 865));
    for _ in 0..superframes {
        for duid in [Duid::Ldu1, Duid::Ldu2] {
            let params: [ImbeParams; 9] = std::array::from_fn(|_| testgen::random_imbe(rng));
            let signalling = if duid == Duid::Ldu1 {
                encode::link_control_ldu1(&LC)
            } else {
                encode::encryption_sync(&[0; 9], algid, 0)
            };
            d.extend(testgen::ldu(NAC, duid, &params, &signalling));
        }
    }
    d.extend(testgen::tdu(NAC));
    d.extend(vec![0u8; 400]);
    d
}

/// Three phase 1 calls (clear, encrypted, clear): op25 vocoded 126 frames
/// (the encrypted ones silent), heard one source, saw three terminators.
#[test]
fn a_clean_phase1_call_sounds_as_op25_did() {
    let mut rng = Rng::new(11);
    let mut d = phase1_call(&mut rng, ALGID_CLEAR, 4);
    d.extend(phase1_call(&mut rng, 0x84, 1));
    d.extend(phase1_call(&mut rng, ALGID_CLEAR, 2));
    let mut p = VoicePipeline::new(24_000.0);
    p.start_call(Modulation::Cqpsk, false, None, 0);
    let mut out = Vec::new();
    p.process_symbols(&ideal(&d), &mut out);
    let us = call_of(&out);
    assert_eq!(us.pcm.len(), 7 * 18 * SAMPLES_PER_FRAME);
    assert_eq!(us.inputs.len(), 126);
    assert_eq!(fnv(&us.pcm), 0xb149_104a_67e0_b4d9);
    assert_eq!(us.sources, [0x12_3456]);
    assert_eq!(us.terminators, 3);
}

/// A phase 2 call: op25 acquired four frames after this pipeline and then
/// vocoded the same 100 inputs; the same inputs make the same audio.
#[test]
fn a_clean_phase2_call_sounds_as_op25_did() {
    let mut rng = Rng::new(12);
    let mut sent = testgen::tdma::Sent::default();
    let es = EncryptionSync {
        mi: [0; 9],
        algid: ALGID_CLEAR,
        keyid: 0,
    };
    let d = testgen::tdma::call(&mut rng, &scrambler(), &es, 0, 6, &mut sent);
    let mut p = VoicePipeline::new(24_000.0);
    p.start_call(Modulation::Cqpsk, true, Some(scrambler()), 0);
    let mut out = Vec::new();
    p.process_symbols(&ideal(&d), &mut out);
    let us = call_of(&out);
    assert!(us.pcm.iter().any(|&s| s != 0));
    assert_eq!(us.inputs.len(), 104);
    assert_eq!(fnv(&vocode_fresh(&us.inputs[4..])), 0x4531_1b30_91bd_37a2);
    assert_eq!(us.sources, Vec::<i64>::new());
    assert_eq!(us.terminators, 0);
}

/// `inputs` through a new synthesiser.
fn vocode_fresh(inputs: &[Input]) -> Vec<i16> {
    let mut p = VoicePipeline::new(24_000.0);
    inputs.iter().flat_map(|i| p.vocode(i.vocode())).collect()
}

// ── A capture's voice taps ──

fn read_cf32(path: &str) -> Vec<Complex32> {
    let raw = std::fs::read(path).unwrap();
    raw.as_chunks::<8>()
        .0
        .iter()
        .map(|c| {
            Complex32::new(
                f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            )
        })
        .collect()
}

/// With `VOICE_CORPUS=<dir>`, appends each call's clear frames to
/// `<dir>/imbe.txt` (op25's `u`, one frame a line) or `<dir>/ambe.txt` (36
/// dibits a line), calls separated by a blank line: real speech for
/// trunk-p25's `vocoder_noise` and `vocoder_ab` examples.
fn dump_corpus(inputs: &[Input], phase2: bool) {
    use std::io::Write as _;
    let Ok(dir) = std::env::var("VOICE_CORPUS") else {
        return;
    };
    let path = format!("{dir}/{}.txt", if phase2 { "ambe" } else { "imbe" });
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    for i in inputs {
        let line: Vec<String> = match i {
            Input::Imbe(Some((u, _, _))) => u.iter().map(ToString::to_string).collect(),
            Input::Ambe(d, false) => d.iter().map(ToString::to_string).collect(),
            _ => continue,
        };
        writeln!(f, "{}", line.join(" ")).unwrap();
    }
    writeln!(f).unwrap();
}

/// Every voice call of a tapped replay (`v_*.out.cf32`) through the
/// pipeline: `TRUNK_P25_TAPS=<dir> cargo test -p trunk-recorder-sys
/// --release --lib voice_capture -- --ignored --nocapture`. Reports what
/// each made; with `VOICE_CORPUS` writes the speech corpus.
#[test]
#[ignore = "needs TRUNK_P25_TAPS"]
fn voice_capture_decodes() {
    let dir = std::env::var("TRUNK_P25_TAPS").expect("TRUNK_P25_TAPS");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with("v_") && n.ends_with(".json")
        })
        .collect();
    files.sort();
    let mut calls = [0usize; 2];
    for path in files {
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let base = path.with_extension("");
        let Ok(_) = std::fs::metadata(format!("{}.out.cf32", base.display())) else {
            continue;
        };
        let mut iq = read_cf32(&format!("{}.out.cf32", base.display()));
        let rate = meta["out_rate"].as_f64().unwrap();
        let phase2 = meta["tdma"].as_bool().unwrap();
        let slot = u8::try_from(meta["slot"].as_u64().unwrap()).unwrap();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        iq.extend(vec![Complex32::new(0.0, 0.0); (rate / 2.0) as usize]);
        let mut p = VoicePipeline::new(rate);
        p.start_call(Modulation::Cqpsk, phase2, Some(scrambler()), slot);
        let mut out = Vec::new();
        for chunk in iq.chunks(4096) {
            p.process(chunk, &mut out);
        }
        let us = call_of(&out);
        dump_corpus(&us.inputs, phase2);
        calls[usize::from(phase2)] += 1;
        #[allow(clippy::cast_precision_loss)]
        let audible = us
            .pcm
            .chunks(SAMPLES_PER_FRAME)
            .filter(|f| f.iter().any(|&s| s.unsigned_abs() >= 16))
            .count() as f64
            / 50.0;
        println!(
            "{}: {} frames, {audible:.1} s audible, sources {:?}, {} terminators",
            base.file_name().unwrap().to_string_lossy(),
            us.inputs.len(),
            us.sources,
            us.terminators,
        );
    }
    println!("phase 1 calls {} phase 2 calls {}", calls[0], calls[1]);
    assert!(calls[0] + calls[1] > 0);
}

// ── The vocoders against op25's, frozen ──

/// How a run of frames is damaged.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Clean,
    Noisy,
    /// Enough errors to repeat frames and, held, to mute them.
    Bad,
}

impl Channel {
    fn pick(rng: &mut Rng) -> Self {
        [Self::Clean, Self::Noisy, Self::Bad][rng.below(3)]
    }
}

const RUN: usize = 40;
const FRAMES: usize = 2000;

/// Phase 1 frames (op25's `u`, e0, et) in runs of clean, noisy or bad;
/// only bad runs send b̂0 > 207, which is no pitch.
fn imbe_frames(rng: &mut Rng, n: usize) -> Vec<([u32; 8], u32, u32)> {
    let mut out = Vec::new();
    while out.len() < n {
        let channel = Channel::pick(rng);
        for _ in 0..RUN {
            let u = loop {
                let u = testgen::random_imbe(rng).to_op25();
                let b0 = ((u[0] >> 4) & 0xfc) | ((u[7] >> 1) & 3);
                if b0 <= 207 || channel == Channel::Bad {
                    break u;
                }
            };
            let (e0, more) = match channel {
                Channel::Clean => (0, rng.below(2)),
                Channel::Noisy => (rng.below(2), rng.below(8)),
                Channel::Bad => (rng.below(5), 4 + rng.below(24)),
            };
            #[allow(clippy::cast_possible_truncation)]
            out.push((u, e0 as u32, (e0 + more) as u32));
        }
    }
    out
}

/// op25's audio of `imbe_frames` for seeds 100.., as (hash, audible
/// frames).
const IMBE_FIXTURES: [(u64, usize); 8] = [
    (0x26fb_983b_b93b_2397, 1615),
    (0x5b0e_8b95_4f66_698a, 1409),
    (0xce30_5e75_32f6_8b61, 1441),
    (0x527a_22fa_032a_bb40, 1526),
    (0x5f7a_f43c_74eb_9a4a, 1345),
    (0x15ee_0121_2d9f_c851, 1433),
    (0xdc16_66a4_f7cd_200d, 1217),
    (0xd9e0_30ce_b13b_08d5, 1306),
];

#[test]
fn the_imbe_vocoder_is_op25s() {
    for (seed, &(hash, audible)) in IMBE_FIXTURES.iter().enumerate() {
        let mut rng = Rng::new(100 + seed as u64);
        let mut v = ImbeVocoder::new();
        let mut pcm = Vec::with_capacity(FRAMES * SAMPLES_PER_FRAME);
        let mut heard = 0;
        for (u, e0, et) in imbe_frames(&mut rng, FRAMES) {
            let got = v.decode(&u, e0, et);
            heard += usize::from(got.is_some_and(|p| p.iter().any(|&s| s != 0)));
            pcm.extend(got.unwrap_or([0; SAMPLES_PER_FRAME]));
        }
        assert_eq!((fnv(&pcm), heard), (hash, audible), "seed {seed}");
    }
}

/// Tone indices, valid (single, DTMF, KNOX, call progress, silence) and
/// not.
const TONES: [u16; 16] = [
    5, 64, 122, 128, 135, 143, 144, 159, 160, 163, 255, 0, 4, 123, 164, 254,
];

/// A tone frame's parameters: the index sent four times, the amplitude.
fn tone_params(id: [u16; 4], amplitude: u16) -> AmbeParams {
    AmbeParams([
        0xfc0 | (amplitude >> 1),
        (id[0] << 4) | (id[1] >> 4),
        ((id[1] & 0xf) << 7) | (id[2] >> 1),
        ((id[2] & 1) << 13) | (id[3] << 5) | ((amplitude & 1) << 4),
    ])
}

/// `p` with quantiser index b0 set to `b0`.
fn with_b0(AmbeParams(mut u): AmbeParams, b0: u16) -> AmbeParams {
    u[0] = (u[0] & !(0xf << 8)) | ((b0 >> 3) << 8);
    u[3] = (u[3] & !(0x7 << 9)) | ((b0 & 7) << 9);
    AmbeParams(u)
}

/// Phase 2 frames (36 dibits, encrypted): voice, silence, erasures and tone
/// frames (runs of one tone as sent, some with an index sent wrong), in runs
/// clean, noisy or bad (bits flipped in the 72), some encrypted.
fn ambe_frames(rng: &mut Rng, n: usize) -> Vec<([u8; 36], bool)> {
    let mut out = Vec::new();
    while out.len() < n {
        let channel = Channel::pick(rng);
        let encrypted = rng.below(8) == 0;
        let tone = (rng.below(4) == 0).then(|| TONES[rng.below(TONES.len())]);
        for _ in 0..RUN {
            #[allow(clippy::cast_possible_truncation)]
            let amplitude = rng.below(128) as u16;
            let random = testgen::tdma::random_ambe(rng);
            #[allow(clippy::cast_possible_truncation)]
            let p = match (tone, rng.below(10)) {
                (Some(id), 0) => {
                    let mut ids = [id; 4];
                    ids[rng.below(4)] = TONES[rng.below(TONES.len())];
                    tone_params(ids, amplitude)
                }
                (Some(id), _) => tone_params([id; 4], amplitude),
                (None, 0) => with_b0(random, 124 + rng.below(2) as u16),
                (None, 1) => with_b0(random, 120 + rng.below(8) as u16),
                (None, 2) => tone_params([TONES[rng.below(TONES.len())]; 4], amplitude),
                (None, _) => with_b0(random, rng.below(120) as u16),
            };
            let cw = p.encode();
            let mut bits: [bool; 72] = std::array::from_fn(|j| cw.bit(j));
            let flips = match channel {
                Channel::Clean => 0,
                Channel::Noisy => rng.below(3),
                Channel::Bad => rng.below(9),
            };
            for _ in 0..flips {
                bits[rng.below(72)] ^= true;
            }
            let dibits =
                std::array::from_fn(|d| u8::from(bits[2 * d]) * 2 + u8::from(bits[2 * d + 1]));
            out.push((dibits, encrypted && rng.below(4) != 0));
        }
    }
    out
}

/// op25's audio of `ambe_frames` for seeds 200.., as (hash, frames op25
/// did not mute, encrypted ones aside).
const AMBE_FIXTURES: [(u64, usize); 8] = [
    (0xb7f7_ff2e_4324_bcb5, 1526),
    (0x7a65_4c36_6059_7eb1, 1573),
    (0x0428_e840_702b_bfe6, 1517),
    (0xfa8b_c05a_2e69_a510, 1610),
    (0x47a0_d6da_675a_3a30, 1541),
    (0x5138_e97a_d73f_ca00, 1583),
    (0x18d7_1499_b578_acee, 1712),
    (0x718a_9a25_a12b_3224, 1552),
];

#[test]
fn the_ambe_vocoder_is_op25s() {
    // Frames played and muted by what op25 decodes them as: voice, silence,
    // erasure (b0 120–123, 126, 127) and tone.
    let mut heard: BTreeMap<&str, [usize; 2]> = BTreeMap::new();
    for (seed, &(hash, audible)) in AMBE_FIXTURES.iter().enumerate() {
        let mut rng = Rng::new(200 + seed as u64);
        let mut v = AmbeVocoder::new();
        let mut pcm = Vec::with_capacity(FRAMES * SAMPLES_PER_FRAME);
        let mut played = 0;
        for (dibits, encrypted) in ambe_frames(&mut rng, FRAMES) {
            let got = if encrypted {
                v.encrypted(&dibits);
                None
            } else {
                v.decode(&dibits)
            };
            played += usize::from(got.is_some());
            if !encrypted {
                let (AmbeParams(u), _) = AmbeCodeword::from_dibits(&dibits).decode_op25();
                let b0 = ((u[0] >> 5) & 0x78) + ((u[3] >> 9) & 0x7);
                let kind = match b0 {
                    _ if (u[0] >> 6) & 0x3f == 63 && u[3] % 16 == 0 => "tone",
                    124 | 125 => "silence",
                    120.. => "erasure",
                    _ => "voice",
                };
                heard.entry(kind).or_default()[usize::from(got.is_none())] += 1;
            }
            pcm.extend(got.unwrap_or([0; SAMPLES_PER_FRAME]));
        }
        assert_eq!((fnv(&pcm), played), (hash, audible), "seed {seed}");
    }
    // Each kind both heard and muted (an erasure is heard as a repeat).
    assert_eq!(heard.len(), 4, "{heard:?}");
    assert!(heard.values().all(|&[a, m]| a > 0 && m > 0), "{heard:?}");
}
