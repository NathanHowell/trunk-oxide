//! Compare the Rust conditioner with a baseline tap pair from `trunk-replay
//! run --tap`: feeds `<base>.in.cf32` through [`Conditioner`] and measures it
//! against `<base>.out.cf32` (the GNU Radio chain's output).
//!
//!   cargo run --release -p trunk-channelizer --example condcheck -- <base> [sps] [excess_bw]

use std::fs;

use num_complex::Complex32;
use trunk_channelizer::{Conditioner, ConditionerParams, SquelchMode};

/// Crude lookup of a numeric field in the tap's JSON sidecar.
fn field(json: &str, key: &str) -> Option<f64> {
    let at = json.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = &json[at..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

fn load(path: &str) -> Vec<Complex32> {
    let b = fs::read(path).expect(path);
    let (pairs, _) = b.as_chunks::<8>();
    pairs
        .iter()
        .map(|c| {
            let (re, im) = c.split_at(4);
            Complex32::new(
                f32::from_le_bytes(re.try_into().unwrap()),
                f32::from_le_bytes(im.try_into().unwrap()),
            )
        })
        .collect()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let base = &a[1];
    let meta = fs::read_to_string(format!("{base}.json")).unwrap_or_default();
    let sps: u32 = a.get(2).map_or_else(
        || field(&meta, "samples_per_symbol").map_or(5, |v| v as u32),
        |s| s.parse().unwrap(),
    );
    let ebw: f32 = a.get(3).map_or(0.2, |s| s.parse().unwrap());
    let analog = meta.contains("\"squelch\":true");
    let input = load(&format!("{base}.in.cf32"));
    let golden = load(&format!("{base}.out.cf32"));
    let n = input.len().min(golden.len());
    println!("in {} out {} samples", input.len(), golden.len());

    let mut c = Conditioner::new(sps, ebw, ConditionerParams::default());
    let mut mine = vec![num_complex::Complex32::ZERO; input.len()];
    let m = if analog {
        let (fin, fout) = (
            field(&meta, "in_rate").unwrap(),
            field(&meta, "out_rate").unwrap(),
        );
        // the sidecar rounds the rate; the channelizer's is sample_rate / decim
        let fin = std::env::var("IN_RATE").map_or(fin, |v| v.parse().unwrap());
        let sq = field(&meta, "squelch_db").unwrap_or(-50.0);
        println!("analog: {fin} -> {fout} S/s, squelch {sq} dB");
        c = c
            .with_resampler((fout / fin) as f32)
            .with_squelch(SquelchMode::ANALOG, sq);
        let mut m = 0;
        for chunk in input.chunks(1024) {
            m += c.process_into(chunk, &mut mine[m..]);
        }
        let ev: Vec<_> = c.take_events().collect();
        println!("squelch events: {} (first {:?})", ev.len(), ev.first());
        m
    } else {
        mine[..n].copy_from_slice(&input[..n]);
        c.process(&mut mine[..n]);
        n
    };
    mine.truncate(m);
    if let Ok(path) = std::env::var("DUMP") {
        let bytes: Vec<u8> = mine
            .iter()
            .flat_map(|c| c.re.to_le_bytes().into_iter().chain(c.im.to_le_bytes()))
            .collect();
        fs::write(path, bytes).unwrap();
    }
    let n = m.min(golden.len());
    println!("mine {m} samples");

    // The in/out sinks start independently: find the lag that aligns them.
    let win = 4000usize.min(n / 4);
    // compare where the golden output carries signal (not squelched to zero)
    let mut at = n / 2;
    while at + win < n && golden[at..at + win].iter().any(|v| v.norm_sqr() == 0.0) {
        at += win;
    }
    let max_lag: isize = std::env::var("LAG").map_or(2000, |v| v.parse().unwrap());
    let mut best = (f64::MAX, 0isize);
    for lag in -max_lag..=max_lag {
        let (s, g) = (at as isize, at as isize + lag);
        if g < 0 || (g as usize) + win > n {
            continue;
        }
        let e: f64 = (0..win)
            .map(|i| f64::from((mine[s as usize + i] - golden[g as usize + i]).norm_sqr()))
            .sum();
        if e < best.0 {
            best = (e, lag);
        }
    }
    let sig: f64 = golden[at..at + win]
        .iter()
        .map(|v| f64::from(v.norm_sqr()))
        .sum();
    println!(
        "best lag {} samples, rel error {:.4} ({:.1} dB)",
        best.1,
        (best.0 / sig).sqrt(),
        10.0 * (best.0 / sig).log10()
    );
    println!(
        "fll {} Hz, power {:?} dB",
        c.freq_error_hz(24000.0),
        c.power_db()
    );
}
