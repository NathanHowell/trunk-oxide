use std::collections::BTreeMap;

use trunk_p25::framer::{Duid, ideal};
use trunk_p25::symbols::slice;
use trunk_p25::tdma::ambe::AmbeCodeword;
use trunk_p25::tdma::{AmbeParams, TdmaRegenerator};
use trunk_p25::testgen::{self, Rng};
use trunk_p25::types::Nac;
use trunk_p25::voice::signalling::{ALGID_CLEAR, encode};
use trunk_p25::voice::{EncryptionSync, ImbeFrame, ImbeParams, Regenerator};

use super::*;

const NAC: Nac = Nac::new(0x38d).unwrap();
const LC: [u8; 9] = [0x00, 0x00, 0x00, 0x00, 0x03, 0x61, 0x12, 0x34, 0x56];

fn scrambler() -> Scrambler {
    Scrambler::new(0xbee00, 0x5e4, Nac::new(0x5e1).unwrap())
}

/// One frame's vocoder input, as both sides report it.
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

/// op25's records ([`ffi::tr_op25_voice_reference_inputs`]) as inputs.
fn inputs_of(records: &[u32], phase2: bool) -> Vec<Input> {
    if phase2 {
        records
            .chunks(37)
            .map(|r| {
                #[allow(clippy::cast_possible_truncation)]
                Input::Ambe(std::array::from_fn(|i| r[i] as u8), r[36] != 0)
            })
            .collect()
    } else {
        records
            .chunks(11)
            .map(|r| Input::Imbe((r[10] == 0).then(|| (std::array::from_fn(|i| r[i]), r[8], r[9]))))
            .collect()
    }
}

/// Audio, vocoder inputs, sources and terminators of a call.
struct Reference {
    pcm: Vec<i16>,
    inputs: Vec<Input>,
    sources: Vec<i64>,
    terminators: usize,
}

fn reference(dibits: &[u8], phase2: bool, slot: i32) -> Reference {
    let mask: Vec<u8> = scrambler().dibits().to_vec();
    let mut r = ffi::tr_op25_voice_reference_new(phase2, slot, &mask);
    for chunk in dibits.chunks(864) {
        ffi::tr_op25_voice_reference_push(r.pin_mut(), chunk);
    }
    Reference {
        pcm: ffi::tr_op25_voice_reference_pcm(&r),
        inputs: inputs_of(&ffi::tr_op25_voice_reference_inputs(&r), phase2),
        sources: ffi::tr_op25_voice_reference_sources(&r),
        terminators: ffi::tr_op25_voice_reference_terminators(&r).len(),
    }
}

/// The pipeline's audio, in-band sources (each change) and terminators.
fn ours(out: &[CallOutput]) -> Reference {
    let mut r = Reference {
        pcm: Vec::new(),
        inputs: Vec::new(),
        sources: Vec::new(),
        terminators: 0,
    };
    for o in out {
        match o {
            CallOutput::Audio { pcm, frame } => {
                r.pcm.extend(pcm);
                r.inputs.push(Input::of(*frame));
            }
            CallOutput::Event(CallEvent::Source(s)) => {
                if r.sources.last() != Some(&i64::from(*s)) {
                    r.sources.push(i64::from(*s));
                }
            }
            CallOutput::Event(CallEvent::Terminator) => r.terminators += 1,
            CallOutput::Event(_) => {}
        }
    }
    r
}

/// Frame-by-frame comparison, for a readable failure.
fn frame_diff(a: &[i16], b: &[i16]) -> String {
    let fa: Vec<&[i16]> = a.chunks(SAMPLES_PER_FRAME).collect();
    let fb: Vec<&[i16]> = b.chunks(SAMPLES_PER_FRAME).collect();
    let differ: Vec<usize> = fa
        .iter()
        .zip(&fb)
        .enumerate()
        .filter(|(_, (x, y))| x != y)
        .map(|(i, _)| i)
        .collect();
    let zero = |f: &[i16]| f.iter().all(|&s| s == 0);
    format!(
        "frames {} vs {}; {} differ, first {:?}; silent {} vs {}",
        fa.len(),
        fb.len(),
        differ.len(),
        &differ[..differ.len().min(8)],
        fa.iter().filter(|f| zero(f)).count(),
        fb.iter().filter(|f| zero(f)).count(),
    )
}

fn dedup(v: &[i64]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for &x in v {
        if out.last() != Some(&x) {
            out.push(x);
        }
    }
    out
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

#[test]
fn a_clean_phase1_call_sounds_as_op25_makes_it() {
    let mut rng = Rng::new(11);
    let mut d = phase1_call(&mut rng, ALGID_CLEAR, 4);
    d.extend(phase1_call(&mut rng, 0x84, 1));
    d.extend(phase1_call(&mut rng, ALGID_CLEAR, 2));
    let op25 = reference(&d, false, 0);
    let mut p = VoicePipeline::new(24_000.0);
    p.start_call(Modulation::Cqpsk, false, None, 0);
    let mut out = Vec::new();
    p.process_symbols(&ideal(&d), &mut out);
    let us = ours(&out);
    assert_eq!(
        us.pcm.len(),
        7 * 18 * SAMPLES_PER_FRAME,
        "{}",
        frame_diff(&op25.pcm, &us.pcm)
    );
    assert!(us.pcm.iter().any(|&s| s != 0));
    assert_eq!(us.inputs, op25.inputs);
    assert!(us.pcm == op25.pcm, "{}", frame_diff(&op25.pcm, &us.pcm));
    assert_eq!(us.sources, dedup(&op25.sources));
    assert_eq!((us.terminators, op25.terminators), (3, 3));
}

#[test]
fn a_clean_phase2_call_sounds_as_op25_makes_it() {
    let mut rng = Rng::new(12);
    let mut sent = testgen::tdma::Sent::default();
    let es = EncryptionSync {
        mi: [0; 9],
        algid: ALGID_CLEAR,
        keyid: 0,
    };
    let d = testgen::tdma::call(&mut rng, &scrambler(), &es, 0, 6, &mut sent);
    let op25 = reference(&d, true, 0);
    let mut p = VoicePipeline::new(24_000.0);
    p.start_call(Modulation::Cqpsk, true, Some(scrambler()), 0);
    let mut out = Vec::new();
    p.process_symbols(&ideal(&d), &mut out);
    let us = ours(&out);
    assert!(us.pcm.iter().any(|&s| s != 0));
    // op25 acquires a burst later; from its first frame the inputs agree.
    let Some(skip) = us.inputs.iter().position(|i| *i == op25.inputs[0]) else {
        panic!("op25's first frame is not ours");
    };
    assert!(skip <= 4, "{skip}");
    let n = op25.inputs.len();
    assert!(us.inputs.len() >= skip + n);
    assert_eq!(us.inputs[skip..skip + n], op25.inputs[..]);
    // And the same inputs from the same start sound the same.
    assert_eq!(vocode_fresh(&op25.inputs), op25.pcm);
}

/// `inputs` through a new synthesiser.
fn vocode_fresh(inputs: &[Input]) -> Vec<i16> {
    let mut p = VoicePipeline::new(24_000.0);
    inputs.iter().flat_map(|i| p.vocode(i.vocode())).collect()
}

// ── Against op25 on a capture's voice taps ──

#[derive(Debug, Default)]
struct Tally {
    calls: usize,
    /// Calls whose audio is op25's sample for sample.
    same_audio: usize,
    /// Calls whose vocoder inputs, as op25 framed them, our vocoders make
    /// into op25's audio sample for sample.
    same_vocoding: usize,
    /// Frames both vocoded from the same input.
    same: usize,
    /// Aligned frames whose inputs differ.
    differ: usize,
    /// Of `differ`: only the encryption (or, phase 1, silence) differs.
    differ_encryption: usize,
    /// Frames only one side vocoded, before op25's first, after its last,
    /// and between.
    ours_only: [usize; 3],
    op25_only: [usize; 3],
    sources_differ: usize,
    /// Ours-only frames by whether their burst was rewritten.
    ours_only_rewritten: BTreeMap<Option<bool>, usize>,
    /// Frames one side vocoded (one-sided, or paired with another frame):
    /// [audible, silent].
    ours_extra_audio: [usize; 2],
    op25_extra_audio: [usize; 2],
    /// Frames both vocoded alike, by who made sound of them: [both, ours
    /// only, op25 only, neither].
    same_audio_frames: [usize; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Same,
    Differ,
    OursOnly,
    Op25Only,
}

/// An edit script between two input sequences (longest common subsequence,
/// substitutions paired up).
#[allow(clippy::many_single_char_names, clippy::suspicious_operation_groupings)]
fn align(ours: &[Input], op25: &[Input]) -> Vec<(Step, Option<Input>, Option<Input>)> {
    let (n, m) = (ours.len(), op25.len());
    let mut l = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if ours[i] == op25[j] {
                l[i + 1][j + 1] + 1
            } else {
                l[i + 1][j].max(l[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < n || j < m {
        if i < n && j < m && ours[i] == op25[j] {
            out.push((Step::Same, Some(ours[i]), Some(op25[j])));
            i += 1;
            j += 1;
        } else if i < n && j < m && l[i + 1][j + 1] == l[i][j] {
            out.push((Step::Differ, Some(ours[i]), Some(op25[j])));
            i += 1;
            j += 1;
        } else if j == m || (i < n && l[i + 1][j] >= l[i][j + 1]) {
            out.push((Step::OursOnly, Some(ours[i]), None));
            i += 1;
        } else {
            out.push((Step::Op25Only, None, Some(op25[j])));
            j += 1;
        }
    }
    out
}

/// Whether two inputs differ only in whether the frame was encrypted.
fn encryption_only(ours: Input, op25: Input) -> bool {
    match (ours, op25) {
        (Input::Imbe(a), Input::Imbe(b)) => a.is_none() != b.is_none(),
        (Input::Ambe(a, enc_a), Input::Ambe(b, enc_b)) => a == b && enc_a != enc_b,
        _ => false,
    }
}

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

/// With `VOICE_CORPUS=<dir>`, appends each call's clear frames as op25
/// framed them to `<dir>/imbe.txt` (op25's `u`, one frame a line) or
/// `<dir>/ambe.txt` (36 dibits a line), calls separated by a blank line:
/// real speech for `examples/vocoder_noise.rs`.
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

/// Every voice call of a tapped replay (`v_*.out.cf32`), op25 after the
/// regenerator against the pipeline: `TRUNK_P25_TAPS=<dir> cargo test -p
/// trunk-recorder-sys --release --lib voice_capture -- --ignored --nocapture`.
#[test]
#[ignore = "needs TRUNK_P25_TAPS"]
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
fn voice_capture_sounds_as_op25_makes_it() {
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
    let mut tally: BTreeMap<&str, Tally> = BTreeMap::new();
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
        // Half a second of nothing lets the regenerators release all.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        iq.extend(vec![Complex32::new(0.0, 0.0); (rate / 2.0) as usize]);

        // op25 after the regenerator.
        let mut symbols = Vec::new();
        if phase2 {
            let mut r = TdmaRegenerator::new(DemodConfig::phase2(rate), Some(scrambler()));
            r.process(&iq, &mut symbols, &mut Vec::new());
            r.flush(&mut symbols);
        } else {
            let mut r = Regenerator::new(DemodConfig::new(rate, Modulation::Cqpsk));
            r.process(&iq, &mut symbols, &mut Vec::new());
        }
        let dibits: Vec<u8> = symbols.iter().map(|&s| slice(s)).collect();
        let op25 = reference(&dibits, phase2, i32::from(slot));
        dump_corpus(&op25.inputs, phase2);

        // Per voice frame of our channel, in order: was its burst rewritten
        // (so op25 read the DUID the Rust decoders decided)?
        let mut rewritten = Vec::new();
        if phase2 {
            let mut dec = TdmaDecoder::new(DemodConfig::phase2(rate), Some(scrambler()));
            dec.process_with(&iq, &mut |_, event, _| {
                if event.channel == slot {
                    let r = trunk_p25::tdma::regenerate::rewritten(&event);
                    rewritten.extend(event.frames().iter().map(|_| r));
                }
            });
        }
        let mut p = VoicePipeline::new(rate);
        p.start_call(Modulation::Cqpsk, phase2, Some(scrambler()), slot);
        let mut out = Vec::new();
        for chunk in iq.chunks(4096) {
            p.process(chunk, &mut out);
        }
        let us = ours(&out);

        let t = tally
            .entry(if phase2 { "phase2" } else { "phase1" })
            .or_default();
        t.calls += 1;
        t.same_audio += usize::from(us.pcm == op25.pcm);
        t.same_vocoding += usize::from(vocode_fresh(&op25.inputs) == op25.pcm);
        let steps = align(&us.inputs, &op25.inputs);
        let first = steps.iter().position(|s| s.1.is_some() && s.2.is_some());
        let last = steps.iter().rposition(|s| s.1.is_some() && s.2.is_some());
        let mut line = Tally::default();
        for (k, st) in steps.iter().enumerate() {
            let place = match (first, last) {
                (Some(f), _) if k < f => 0,
                (_, Some(l)) if k > l => 1,
                _ => 2,
            };
            match st.0 {
                Step::Same => line.same += 1,
                Step::Differ => {
                    line.differ += 1;
                    line.differ_encryption +=
                        usize::from(encryption_only(st.1.unwrap(), st.2.unwrap()));
                }
                Step::OursOnly => line.ours_only[place] += 1,
                Step::Op25Only => line.op25_only[place] += 1,
            }
        }
        if std::env::var("VOICE_DETAIL").is_ok_and(|v| base.to_string_lossy().contains(&v)) {
            let (mut i, mut j) = (0usize, 0usize);
            for st in &steps {
                if st.0 != Step::Same {
                    let detail = match (st.1, st.2) {
                        (Some(Input::Ambe(a, e)), Some(Input::Ambe(b, f))) => format!(
                            "{} dibits differ, encrypted {e}/{f}",
                            a.iter().zip(&b).filter(|(x, y)| x != y).count()
                        ),
                        _ => String::new(),
                    };
                    println!("  ours {i} op25 {j}: {:?} {detail}", st.0);
                }
                i += usize::from(st.1.is_some());
                j += usize::from(st.2.is_some());
            }
        }
        {
            let audible = |pcm: &[i16], k: usize| {
                pcm.chunks(SAMPLES_PER_FRAME)
                    .nth(k)
                    .is_some_and(|f| f.iter().any(|&x| x.unsigned_abs() >= 16))
            };
            let (mut i, mut j) = (0usize, 0usize);
            for st in &steps {
                if st.0 == Step::Same {
                    let (a, b) = (audible(&us.pcm, i), audible(&op25.pcm, j));
                    t.same_audio_frames[match (a, b) {
                        (true, true) => 0,
                        (true, false) => 1,
                        (false, true) => 2,
                        (false, false) => 3,
                    }] += 1;
                }
                if st.0 != Step::Same {
                    if st.1.is_some() {
                        t.ours_extra_audio[usize::from(!audible(&us.pcm, i))] += 1;
                    }
                    if st.2.is_some() {
                        t.op25_extra_audio[usize::from(!audible(&op25.pcm, j))] += 1;
                    }
                }
                i += usize::from(st.1.is_some());
                j += usize::from(st.2.is_some());
            }
        }
        if phase2 {
            let mut i = 0usize;
            for st in &steps {
                if st.0 == Step::OursOnly {
                    *t.ours_only_rewritten
                        .entry(rewritten.get(i).copied())
                        .or_default() += 1;
                }
                i += usize::from(st.1.is_some());
            }
        }
        let sources_differ = us.sources != dedup(&op25.sources);
        println!(
            "{}: same {} differ {} (encryption {}) ours-only {:?} op25-only {:?}{}",
            base.file_name().unwrap().to_string_lossy(),
            line.same,
            line.differ,
            line.differ_encryption,
            line.ours_only,
            line.op25_only,
            if sources_differ {
                format!(" sources {:?} / {:?}", dedup(&op25.sources), us.sources)
            } else {
                String::new()
            },
        );
        t.same += line.same;
        t.differ += line.differ;
        t.differ_encryption += line.differ_encryption;
        for i in 0..3 {
            t.ours_only[i] += line.ours_only[i];
            t.op25_only[i] += line.op25_only[i];
        }
        t.sources_differ += usize::from(sources_differ);
    }
    println!("{tally:#?}");
    assert!(tally.values().map(|t| t.calls).sum::<usize>() > 0);
    // Whatever the framing, the vocoders are op25's.
    for t in tally.values() {
        assert_eq!(t.same_vocoding, t.calls, "{t:?}");
    }
    // Phase 1 is op25's frame for frame: every codeword is regenerated, so
    // both framers see the same frames.
    if let Some(t) = tally.get("phase1") {
        assert_eq!(t.differ, 0, "{t:?}");
        assert_eq!(
            t.ours_only.iter().sum::<usize>() + t.op25_only.iter().sum::<usize>(),
            0,
            "{t:?}"
        );
    }
    // Phase 2 differs only at the edges of lock (LockEdges): op25 frames
    // from its first S-ISCH to ten bursts past its last, trunk-p25 while
    // its flywheel holds, and the ISCH is rewritten only then. Never in
    // what an aligned frame carries, and more of ours make sound.
    if let Some(t) = tally.get("phase2") {
        assert_eq!(t.differ_encryption, 0, "{t:?}");
        // Past the end of ours op25 frames noise for up to ten more bursts
        // (mostly muted), which is not counted here.
        let one_sided =
            t.ours_only.iter().sum::<usize>() + t.op25_only[0] + t.op25_only[2] + 2 * t.differ;
        assert!(one_sided * 100 <= 5 * t.same, "{t:?}");
        let ours = t.same_audio_frames[0] + t.same_audio_frames[1] + t.ours_extra_audio[0];
        let op25 = t.same_audio_frames[0] + t.same_audio_frames[2] + t.op25_extra_audio[0];
        assert!(ours >= op25, "audible frames: ours {ours} op25 {op25}");
    }
}

// ── The vocoders against op25's ──

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

/// The first sample where two frames differ.
fn first_difference(ours: &Pcm, op25: &Pcm) -> String {
    let i = ours.iter().zip(op25).position(|(a, b)| a != b).unwrap_or(0);
    format!("sample {i}: ours {} op25 {}", ours[i], op25[i])
}

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

#[test]
fn the_imbe_vocoder_is_op25s() {
    let (mut audible, mut muted) = (0, 0);
    for seed in 0..8 {
        let mut rng = Rng::new(100 + seed);
        let mut ours = ImbeVocoder::new();
        let mut op25 = ffi::tr_op25_vocoder_reference_new();
        for (i, (u, e0, et)) in imbe_frames(&mut rng, 2000).into_iter().enumerate() {
            let mut want = [0i16; SAMPLES_PER_FRAME];
            ffi::tr_op25_vocoder_reference_phase1(op25.pin_mut(), &u, e0, et, &mut want);
            let got = ours.decode(&u, e0, et);
            audible += usize::from(got.is_some());
            muted += usize::from(got.is_none());
            let got = got.unwrap_or([0; SAMPLES_PER_FRAME]);
            assert!(
                got == want,
                "seed {seed} frame {i}: {}",
                first_difference(&got, &want)
            );
        }
    }
    assert!(audible > 0 && muted > 0, "audible {audible} muted {muted}");
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

#[test]
fn the_ambe_vocoder_is_op25s() {
    // Frames heard (audible, muted) by what op25 decodes them as: voice,
    // silence, erasure (b0 120–123, 126, 127) and tone.
    let mut heard: BTreeMap<&str, [usize; 2]> = BTreeMap::new();
    for seed in 0..8 {
        let mut rng = Rng::new(200 + seed);
        let mut ours = AmbeVocoder::new();
        let mut op25 = ffi::tr_op25_vocoder_reference_new();
        for (i, (dibits, encrypted)) in ambe_frames(&mut rng, 2000).into_iter().enumerate() {
            let mut want = [0i16; SAMPLES_PER_FRAME];
            let op25_audible = ffi::tr_op25_vocoder_reference_phase2(
                op25.pin_mut(),
                &dibits,
                encrypted,
                &mut want,
            );
            let got = if encrypted {
                ours.encrypted(&dibits);
                None
            } else {
                ours.decode(&dibits)
            };
            // op25 counts an encrypted frame as not muted.
            assert_eq!(
                got.is_some(),
                op25_audible && !encrypted,
                "seed {seed} frame {i}"
            );
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
            let got = got.unwrap_or([0; SAMPLES_PER_FRAME]);
            assert!(
                got == want,
                "seed {seed} frame {i}: {}",
                first_difference(&got, &want)
            );
        }
    }
    // Each kind both heard and muted (an erasure is heard as a repeat).
    assert_eq!(heard.len(), 4, "{heard:?}");
    assert!(heard.values().all(|&[a, m]| a > 0 && m > 0), "{heard:?}");
}
