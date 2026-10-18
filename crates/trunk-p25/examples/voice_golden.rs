//! Score Phase 1 voice decoding on a tapped replay's `v_*` taps (non-TDMA).
//!
//! Each call is framed twice: from op25's own demodulated symbols
//! (`.symbols.f32`) and from the conditioned IQ through the Rust demodulator
//! (`.out.cf32`). On the op25-symbol frames, op25's decoding algorithms (hard
//! Golay/Hamming, bounded-distance Reed–Solomon) are compared with the Rust
//! soft decoders on exactly the same bits.
//!
//! cargo run --release -p trunk-p25 --example `voice_golden` -- <tap dir>

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::comparison_chain
)]

use std::collections::BTreeMap;
use std::fs;

use num_complex::Complex32;
use trunk_p25::fec::{HAMMING_10, RS_24_12, RS_24_16};
use trunk_p25::framer::{Duid, Frame, Framer};
use trunk_p25::gf64::Gf64;
use trunk_p25::voice::signalling::pack;
use trunk_p25::voice::{self, FrameBits, LcMessage, LinkControl, VoiceDecoder, VoiceEvent, imbe};
use trunk_p25::{DemodConfig, Modulation};

#[derive(Default)]
struct Tally {
    calls: usize,
    ldu1: usize,
    ldu2: usize,
    lc_ours: usize,
    lc_op25: usize,
    lc_talkgroup_match: usize,
    ess_ours: usize,
    ess_op25: usize,
    imbe: usize,
    imbe_disagree: usize,
    errors_ours: u64,
    errors_op25: u64,
    iq_ldu: usize,
    iq_lc: usize,
    iq_errors: u64,
    iq_frames: u64,
    sources: usize,
    /// Disagreements where pitch differs: whose b0 is nearer its neighbours'.
    pitch_ours: usize,
    pitch_op25: usize,
    pitch_tie: usize,
    /// Disagreeing frames the vocoder would reject (b0 > 207).
    invalid_ours: usize,
    invalid_op25: usize,
}

/// IMBE fundamental-frequency index (op25 `software_imbe_decoder`).
const fn b0(p: &voice::ImbeParams) -> i32 {
    (((p.0[0] >> 4) & 0xfc) | (p.0[7] & 3)) as i32
}

/// For frames where the decoders disagree, compare each one's pitch with the
/// mean of the nearest agreeing neighbours on both sides.
fn pitch_check(t: &mut Tally, pairs: &[(voice::ImbeParams, voice::ImbeParams)]) {
    let agree = |i: usize| pairs[i].0.0[..7] == pairs[i].1.0[..7];
    for (k, (ours, op25)) in pairs.iter().enumerate() {
        if agree(k) {
            continue;
        }
        t.invalid_ours += usize::from(b0(ours) > 207);
        t.invalid_op25 += usize::from(b0(op25) > 207);
        if b0(ours) == b0(op25) {
            continue;
        }
        let prev = (0..k).rev().find(|&i| agree(i) && b0(&pairs[i].0) <= 207);
        let next = (k + 1..pairs.len()).find(|&i| agree(i) && b0(&pairs[i].0) <= 207);
        let (Some(a), Some(b)) = (prev, next) else {
            continue;
        };
        if b - a > 6 {
            continue; // too far apart to say what the pitch should be
        }
        let target = i32::midpoint(b0(&pairs[a].0), b0(&pairs[b].0));
        let (d_ours, d_op25) = ((b0(ours) - target).abs(), (b0(op25) - target).abs());
        if d_ours < d_op25 {
            t.pitch_ours += 1;
        } else if d_op25 < d_ours {
            t.pitch_op25 += 1;
        } else {
            t.pitch_tie += 1;
        }
    }
}

fn hard_hexbits(llr: &[f32]) -> [Gf64; 24] {
    let mut w = [Gf64::ZERO; 24];
    for (i, bits) in llr.as_chunks::<10>().0.iter().enumerate() {
        let word = bits
            .iter()
            .fold(0u32, |a, &l| (a << 1) | u32::from(l < 0.0));
        #[allow(clippy::cast_possible_truncation)]
        {
            w[i] = Gf64::new(HAMMING_10.decode_hard(word).info as u8);
        }
    }
    w
}

/// op25's per-frame error count: bits of c0–c6 its hard decode changed.
fn op25_errors(llr: &[f32; 144], params: voice::ImbeParams) -> u64 {
    let cw = params.encode();
    (0..137).filter(|&j| cw.bit(j) != (llr[j] < 0.0)).count() as u64
}

fn frames_of(symbols: &[f32]) -> Vec<Frame> {
    let mut framer = Framer::new();
    let mut frames = Vec::new();
    for &s in symbols {
        framer.push(s, &mut frames);
    }
    frames
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("tap dir");
    let (mut mf, mut df, mut bw, mut quiet) = (false, 0.0f32, None, false);
    for a in args {
        if let Some(v) = a.strip_prefix("mf=") {
            mf = v == "1";
        } else if let Some(v) = a.strip_prefix("df=") {
            df = v.parse().unwrap();
        } else if let Some(v) = a.strip_prefix("bw=") {
            bw = Some(v.parse().unwrap());
        } else if a == "quiet" {
            quiet = true;
        }
    }
    let mut t = Tally::default();
    let mut names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    names.sort();
    for json in names.iter().filter(|p| {
        let n = p.file_name().unwrap().to_string_lossy();
        n.starts_with("v_") && p.extension().is_some_and(|e| e == "json")
    }) {
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(json).unwrap()).unwrap();
        if meta["tdma"] == true {
            continue;
        }
        let tg = meta["talkgroup"].as_u64().unwrap();
        let base = json.with_extension("");
        let base = base.to_string_lossy();
        t.calls += 1;

        // op25 symbols → Rust framer; op25 vs Rust decoders on the same frames.
        let raw = fs::read(format!("{base}.symbols.f32")).unwrap();
        let symbols: Vec<f32> = raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        let mut sources = std::collections::BTreeSet::new();
        let mut pairs = Vec::new();
        for frame in frames_of(&symbols) {
            let bits = FrameBits::new(&frame);
            match frame.duid {
                Duid::Ldu1 | Duid::Ldu2 => {
                    let ldu1 = frame.duid == Duid::Ldu1;
                    if ldu1 {
                        t.ldu1 += 1
                    } else {
                        t.ldu2 += 1
                    }
                    for i in 0..9 {
                        let llr = bits.imbe(i);
                        let ours = imbe::decode(&llr);
                        let op25 = imbe::decode_hard(&llr);
                        t.imbe += 1;
                        t.imbe_disagree += usize::from(ours.params.0[..7] != op25.0[..7]);
                        t.errors_ours += u64::from(ours.errors);
                        t.errors_op25 += op25_errors(&llr, op25);
                        pairs.push((ours.params, op25));
                    }
                    let sig = bits.signalling();
                    let mut w = hard_hexbits(&sig);
                    if ldu1 {
                        let op25 = RS_24_12.decode(&mut w, &[]).is_ok();
                        t.lc_op25 += usize::from(op25);
                        if let Some(lc) = voice::signalling::ldu1_link_control(&sig) {
                            t.lc_ours += 1;
                            if let Some(LcMessage::GroupVoice(g)) = lc.value.message() {
                                t.lc_talkgroup_match += usize::from(u64::from(g.talkgroup) == tg);
                                sources.insert(g.source);
                            }
                        }
                        let _: Option<LinkControl> = op25.then(|| LinkControl(pack(&w[..12])));
                    } else {
                        t.ess_op25 += usize::from(RS_24_16.decode(&mut w, &[]).is_ok());
                        t.ess_ours +=
                            usize::from(voice::signalling::ldu2_encryption_sync(&sig).is_some());
                    }
                }
                _ => {}
            }
        }
        t.sources += sources.len();
        pitch_check(&mut t, &pairs);

        // Conditioned IQ through the whole Rust receiver.
        let raw = fs::read(format!("{base}.out.cf32")).unwrap();
        let iq: Vec<Complex32> = raw
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| {
                Complex32::new(
                    f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect();
        let modulation = Modulation::Cqpsk;
        let mut cfg = DemodConfig::new(meta["out_rate"].as_f64().unwrap(), modulation);
        cfg.matched_filter = mf;
        cfg.reference_memory = df;
        if let Some(bw) = bw {
            cfg.timing_bandwidth = bw;
        }
        let mut dec = VoiceDecoder::new(cfg);
        let mut out = Vec::new();
        for c in iq.chunks(4096) {
            dec.process(c, &mut out);
        }
        let s = dec.stats();
        t.iq_ldu += (s.ldu1 + s.ldu2) as usize;
        let ldu1_lc = out
            .iter()
            .filter(|r| {
                matches!(
                    r.message,
                    VoiceEvent::Ldu1 {
                        link_control: Some(_),
                        ..
                    }
                )
            })
            .count();
        t.iq_lc += ldu1_lc;
        t.iq_frames += s.imbe_frames;
        t.iq_errors += s.imbe_errors;
        let groups: BTreeMap<u16, usize> = out.iter().fold(BTreeMap::new(), |mut m, r| {
            if let VoiceEvent::Ldu1 {
                link_control: Some(lc),
                ..
            } = &r.message
                && let Some(LcMessage::GroupVoice(g)) = lc.value.message()
            {
                *m.entry(g.talkgroup).or_default() += 1;
            }
            m
        });
        if !quiet {
            println!(
                "{:28} tg {tg:5}  op25-sym LDUs {:3}  iq LDUs {:3}  iq LDU1 LC {:3}  iq errs/frame {:.2}  iq talkgroups {groups:?}  snr {:.1} dB",
                json.file_stem().unwrap().to_string_lossy(),
                frames_of(&symbols)
                    .iter()
                    .filter(|f| matches!(f.duid, Duid::Ldu1 | Duid::Ldu2))
                    .count(),
                s.ldu1 + s.ldu2,
                ldu1_lc,
                s.imbe_errors as f64 / (s.imbe_frames.max(1)) as f64,
                s.demod.symbol_snr_db(),
            );
        }
    }
    println!();
    println!("calls {}", t.calls);
    println!("on op25's symbols: LDU1 {}, LDU2 {}", t.ldu1, t.ldu2);
    println!(
        "  link control decoded: rust {} vs op25 {} (of {}); rust talkgroup = call talkgroup {}",
        t.lc_ours, t.lc_op25, t.ldu1, t.lc_talkgroup_match
    );
    println!(
        "  encryption sync decoded: rust {} vs op25 {} (of {})",
        t.ess_ours, t.ess_op25, t.ldu2
    );
    println!(
        "  IMBE frames {}, rust and op25 differ on u0-u6 in {} ({:.2}%); bits corrected rust {} op25 {}",
        t.imbe,
        t.imbe_disagree,
        100.0 * t.imbe_disagree as f64 / t.imbe.max(1) as f64,
        t.errors_ours,
        t.errors_op25
    );
    println!(
        "  where they differ: pitch nearer the neighbours' rust {} op25 {} tie {}; vocoder-invalid pitch rust {} op25 {}",
        t.pitch_ours, t.pitch_op25, t.pitch_tie, t.invalid_ours, t.invalid_op25
    );
    println!("  distinct sources {}", t.sources);
    println!(
        "rust receiver on IQ: LDUs {}, LDU1 link control {}, bits corrected {} ({:.2}/frame)",
        t.iq_ldu,
        t.iq_lc,
        t.iq_errors,
        t.iq_errors as f64 / t.iq_frames.max(1) as f64
    );
}
