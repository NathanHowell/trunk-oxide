//! Score the Rust control-channel decoder against op25 on a tapped replay.
//!
//! For each `cc_*.json` in the tap directory, decodes
//! - `<base>.dibits` (op25's own slicer output) — decoder gains only, and
//! - `<base>.out.cf32` (conditioned IQ) — the full Rust receiver,
//!
//! and compares the TSBKs with op25's `<base>.msgs.jsonl` as multisets.
//! A Rust-only TSBK that also appears elsewhere in the Rust output is counted
//! as corroborated (control channels repeat most messages).
//!
//! cargo run --release -p trunk-p25 --example golden -- <tap dir> [mf=0|1] [df=<memory>] [list=<n>]

#![allow(
    clippy::cast_precision_loss,
    clippy::format_collect,
    clippy::too_many_lines
)]

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use num_complex::Complex32;
use trunk_p25::{
    ControlChannelDecoder, DecoderConfig, DecoderStats, DemodConfig, Message, Modulation, Received,
};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn multiset(items: impl IntoIterator<Item = String>) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for i in items {
        *m.entry(i).or_insert(0) += 1;
    }
    m
}

struct Score {
    total: usize,
    common: usize,
    cpp_only: usize,
    rust_only: usize,
    corroborated: usize,
}

fn score(cpp: &HashMap<String, usize>, rust: &[Received]) -> Score {
    let r = multiset(
        rust.iter()
            .filter(|m| matches!(m.message, Message::Tsbk { .. }))
            .map(|m| hex(&m.message.op25_payload())),
    );
    let common: usize = cpp
        .iter()
        .map(|(k, &n)| n.min(*r.get(k).unwrap_or(&0)))
        .sum();
    let cpp_total: usize = cpp.values().sum();
    let rust_total: usize = r.values().sum();
    let corroborated: usize = r
        .iter()
        .map(|(k, &n)| {
            n.saturating_sub(*cpp.get(k).unwrap_or(&0))
                .min(n.saturating_sub(1).max(usize::from(cpp.contains_key(k))))
        })
        .sum();
    Score {
        total: rust_total,
        common,
        cpp_only: cpp_total - common,
        rust_only: rust_total - common,
        corroborated,
    }
}

fn print(label: &str, s: &Score, cpp_total: usize, st: &DecoderStats) {
    let ranks: Vec<String> = st
        .tsbk_list_rank
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, n)| **n > 0)
        .map(|(r, n)| format!("r{r}:{n}"))
        .collect();
    println!(
        "  {label:5} tsbk {:6} ({:+6} vs op25 {cpp_total})  common {:6}  op25-only {:5}  rust-only {:5} (corroborated {:5})  failed {:5}  corrected {:6}  list [{}]  nid hard/ml/fail {}/{}/{}  syncs {} (flywheel {})  snr {:.1} dB  bias {:.1} Hz",
        s.total,
        i64::try_from(s.total).unwrap() - i64::try_from(cpp_total).unwrap(),
        s.common,
        s.cpp_only,
        s.rust_only,
        s.corroborated,
        st.tsbk_failed,
        st.tsbk_corrected,
        ranks.join(" "),
        st.framer.nid_ok_hard,
        st.framer.nid_ok_ml,
        st.framer.nid_failed,
        st.framer.syncs,
        st.framer.flywheel_syncs,
        st.demod.symbol_snr_db(),
        st.demod.bias_hz,
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("tap dir");
    let mut mf = None;
    let mut df = 0.0f32;
    let mut list = 8usize;
    let mut bw = None;
    // Write each channel's IQ-decoded TSBKs, in order, to <dump>/<base>.hex.
    let mut dump: Option<String> = None;
    for a in args {
        if let Some(v) = a.strip_prefix("mf=") {
            mf = Some(v == "1");
        } else if let Some(v) = a.strip_prefix("df=") {
            df = v.parse().unwrap();
        } else if let Some(v) = a.strip_prefix("list=") {
            list = v.parse().unwrap();
        } else if let Some(v) = a.strip_prefix("dump=") {
            dump = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("bw=") {
            bw = Some(v.parse().unwrap());
        }
    }

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for json in entries.iter().filter(|p| {
        p.extension().is_some_and(|e| e == "json")
            && p.file_name().unwrap().to_string_lossy().starts_with("cc_")
    }) {
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(json).unwrap()).unwrap();
        if meta["kind"] != "control_channel" {
            continue;
        }
        let base = json.with_extension("");
        let base = base.to_string_lossy();
        let msgs = fs::read_to_string(format!("{base}.msgs.jsonl")).unwrap_or_default();
        let cpp = multiset(msgs.lines().filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            (v["type"] == 7).then(|| v["hex"].as_str().unwrap().to_string())
        }));
        let cpp_total: usize = cpp.values().sum();
        let modulation = if meta["modulation"] == "qpsk" {
            Modulation::Cqpsk
        } else {
            Modulation::C4fm
        };
        let rate = meta["out_rate"].as_f64().unwrap();
        let mut demod = DemodConfig::new(rate, modulation);
        if let Some(mf) = mf {
            demod.matched_filter = mf;
        }
        if let Some(bw) = bw {
            demod.timing_bandwidth = bw;
        }
        demod.reference_memory = df;
        let cfg = DecoderConfig {
            list_size: list,
            ..DecoderConfig::new(demod)
        };
        println!(
            "{} ({modulation:?}, op25 {cpp_total} TSBKs)",
            Path::new(&*base).file_name().unwrap().to_string_lossy()
        );
        if cpp_total < 1000 {
            println!("  idle channel, skipped");
            continue;
        }

        if let Ok(dibits) = fs::read(format!("{base}.dibits")) {
            let mut dec = ControlChannelDecoder::new(cfg);
            let mut out = Vec::new();
            dec.process_dibits(&dibits, &mut out);
            print("dibit", &score(&cpp, &out), cpp_total, &dec.stats());
        }

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
        let mut dec = ControlChannelDecoder::new(cfg);
        let mut out = Vec::new();
        let t0 = std::time::Instant::now();
        for chunk in iq.chunks(4096) {
            dec.process(chunk, &mut out);
        }
        let secs = t0.elapsed().as_secs_f64();
        print("iq", &score(&cpp, &out), cpp_total, &dec.stats());
        if let Some(dir) = &dump {
            let lines: String = out
                .iter()
                .filter(|m| matches!(m.message, Message::Tsbk { .. }))
                .map(|m| hex(&m.message.op25_payload()) + "\n")
                .collect();
            let name = Path::new(&*base)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            fs::write(format!("{dir}/{name}.hex"), lines).unwrap();
        }
        #[allow(clippy::cast_precision_loss)]
        let audio_secs = iq.len() as f64 / rate;
        println!("  ({:.0}× real time)", audio_secs / secs);
    }
}
