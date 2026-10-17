//! Compare the Rust conditioner with a baseline tap pair from `trunk-replay
//! run --tap`: feeds `<base>.in.cf32` through [`Conditioner`] and measures it
//! against `<base>.out.cf32` (the GNU Radio chain's output).
//!
//!   cargo run --release -p trunk-channelizer --example condcheck -- <base> [sps] [excess_bw]

use std::fs;

use num_complex::Complex32;
use trunk_channelizer::{Conditioner, ConditionerParams};

fn load(path: &str) -> Vec<Complex32> {
    let b = fs::read(path).expect(path);
    b.chunks_exact(8)
        .map(|c| {
            Complex32::new(
                f32::from_le_bytes(c[..4].try_into().unwrap()),
                f32::from_le_bytes(c[4..].try_into().unwrap()),
            )
        })
        .collect()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let base = &a[1];
    let sps: u32 = a.get(2).map_or(5, |s| s.parse().unwrap());
    let ebw: f32 = a.get(3).map_or(0.2, |s| s.parse().unwrap());
    let input = load(&format!("{base}.in.cf32"));
    let golden = load(&format!("{base}.out.cf32"));
    let n = input.len().min(golden.len());
    println!("in {} out {} samples", input.len(), golden.len());

    let mut c = Conditioner::new(sps, ebw, ConditionerParams::default());
    let mut mine = input[..n].to_vec();
    c.process(&mut mine);

    // The in/out sinks start independently: find the lag that aligns them.
    let win = 4000usize.min(n / 4);
    let at = n / 2;
    let mut best = (f64::MAX, 0isize);
    for lag in -2000isize..=2000 {
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
