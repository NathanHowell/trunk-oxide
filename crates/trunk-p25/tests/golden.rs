//! Regression against a tapped replay: on every active control channel the
//! Rust decoder must find every TSBK op25 found, and more.
//!
//! Needs the tap directory (4 GB), so it is ignored by default:
//! `TRUNK_P25_GOLDEN=/build/trunk-oxide-runs/baseline-tap/taps cargo test --release -p trunk-p25 -- --ignored`

use std::collections::HashMap;
use std::fs;

use num_complex::Complex32;
use trunk_p25::{ControlChannelDecoder, DecoderConfig, DemodConfig, Message, Modulation};

fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

#[test]
#[ignore = "needs a tapped replay (TRUNK_P25_GOLDEN)"]
fn rust_finds_every_op25_tsbk_and_more() {
    let dir = std::env::var("TRUNK_P25_GOLDEN").expect("set TRUNK_P25_GOLDEN to a taps directory");
    let mut checked = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !(name.starts_with("cc_") && path.extension().is_some_and(|e| e == "json")) {
            continue;
        }
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let base = path.with_extension("");
        let base = base.to_string_lossy();
        let mut op25: HashMap<String, usize> = HashMap::new();
        for line in fs::read_to_string(format!("{base}.msgs.jsonl"))
            .unwrap_or_default()
            .lines()
        {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if v["type"] == 7 {
                *op25
                    .entry(v["hex"].as_str().unwrap().to_owned())
                    .or_default() += 1;
            }
        }
        let op25_total: usize = op25.values().sum();
        if op25_total < 1000 {
            continue; // idle channel
        }

        let modulation = if meta["modulation"] == "qpsk" {
            Modulation::Cqpsk
        } else {
            Modulation::C4fm
        };
        let rate = meta["out_rate"].as_f64().unwrap();
        let mut dec =
            ControlChannelDecoder::new(DecoderConfig::new(DemodConfig::new(rate, modulation)));
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
        let mut out = Vec::new();
        for chunk in iq.chunks(4096) {
            dec.process(chunk, &mut out);
        }
        let mut rust: HashMap<String, usize> = HashMap::new();
        for r in &out {
            if matches!(r.message, Message::Tsbk { .. }) {
                *rust.entry(hex(&r.message.op25_payload())).or_default() += 1;
            }
        }
        let rust_total: usize = rust.values().sum();
        let missed: usize = op25
            .iter()
            .map(|(k, &n)| n.saturating_sub(*rust.get(k).unwrap_or(&0)))
            .sum();
        println!(
            "{name}: op25 {op25_total}, rust {rust_total}, op25-only {missed}, stats {:?}",
            dec.stats()
        );
        assert_eq!(missed, 0, "{name}: TSBKs op25 decoded that we did not");
        assert!(rust_total > op25_total, "{name}: no gain over op25");
        checked += 1;
    }
    assert!(checked > 0, "no active control channel in {dir}");
}
