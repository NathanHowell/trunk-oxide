//! What geometry a source rate gets and what its FFTs cost.
//!
//!   cargo run --release -p trunk-channelizer --example geometry -- 912000 2496000 2472000
//!
//! Per rate: the chosen forward size, hop and per-profile inverse size, the
//! measured time of each FFT, and the resulting cost per second of input
//! (forward once per block, one inverse per open channel of each profile).

use std::hint::black_box;
use std::time::Instant;

use num_complex::Complex32;
use rustfft::FftPlanner;
use trunk_channelizer::Config;

fn factors(mut n: usize) -> String {
    let mut out = Vec::new();
    let mut p = 2;
    while n > 1 {
        let mut e = 0;
        while n.is_multiple_of(p) {
            n /= p;
            e += 1;
        }
        if e > 0 {
            out.push(if e > 1 {
                format!("{p}^{e}")
            } else {
                p.to_string()
            });
        }
        p += 1;
    }
    out.join("*")
}

/// Median microseconds for one forward (or inverse) FFT of size `n`.
fn time_fft(n: usize, inverse: bool) -> f64 {
    let mut planner = FftPlanner::<f32>::new();
    let fft = if inverse {
        planner.plan_fft_inverse(n)
    } else {
        planner.plan_fft_forward(n)
    };
    let mut buf: Vec<Complex32> = (0..n)
        .map(|i| Complex32::new((i % 17) as f32, (i % 5) as f32))
        .collect();
    let mut scratch = vec![Complex32::ZERO; fft.get_inplace_scratch_len()];
    for _ in 0..20 {
        fft.process_with_scratch(black_box(&mut buf), &mut scratch);
    }
    let mut v: Vec<f64> = (0..200)
        .map(|_| {
            let t = Instant::now();
            fft.process_with_scratch(black_box(&mut buf), &mut scratch);
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let rates: Vec<f64> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    for fs in rates {
        let c = Config::standard(fs).expect("geometry");
        let blocks_per_s = fs / c.hop() as f64;
        let fwd = time_fft(c.n, false);
        println!(
            "\nfs={fs}: n={} ({}) overlap_in={} hop={} blocks/s={blocks_per_s:.1}",
            c.n,
            factors(c.n),
            c.overlap_in,
            c.hop()
        );
        println!(
            "  forward {:>9.1} us/block = {:.4} cores",
            fwd,
            fwd * blocks_per_s / 1e6
        );
        for (p, prof) in c.profiles.iter().enumerate() {
            let k = c.k(p);
            let inv = time_fft(k, true);
            println!(
                "  profile {p}: decim={} k={k} ({}) rate={:.0} out/block={} inverse {:>7.1} us = {:.5} cores per channel",
                prof.decim,
                factors(k),
                c.out_rate(p),
                c.out_per_block(p),
                inv,
                inv * blocks_per_s / 1e6
            );
        }
    }
}
