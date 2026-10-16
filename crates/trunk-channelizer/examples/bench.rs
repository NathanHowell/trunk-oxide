//! wxt.1 benchmark: shared-FFT channelizer cost vs channel count / threads,
//! plus FFTW-vs-rustfft raw forward FFT time.
//!
//!   cargo run --release -p trunk-channelizer --example bench
//!   cargo run --release -p trunk-channelizer --example bench -- --quick

use std::hint::black_box;
use std::time::{Duration, Instant};

use num_complex::Complex32;
use rustfft::FftPlanner;
use trunk_channelizer::{Channelizer, Config, NARROW, WIDE};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// rustfft vs FFTW raw forward FFT time. FFTW columns need `--features fftw`.
fn raw_fft_compare(sizes: &[usize]) {
    println!("\n== raw forward FFT, complex f32, in-place (median of 200) ==");
    println!(
        "{:>8} {:>12} {:>12} {:>9}",
        "N", "fftw_meas us", "rustfft us", "rf/fftw"
    );
    for &n in sizes {
        let mut rng = Rng(1);
        let time = |f: &mut dyn FnMut()| {
            for _ in 0..20 {
                f();
            }
            let mut v: Vec<f64> = (0..200)
                .map(|_| {
                    let t = Instant::now();
                    f();
                    us(t.elapsed())
                })
                .collect();
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        #[cfg(feature = "fftw")]
        let m = {
            use trunk_channelizer::fftw::{AlignedBuf, Direction, Effort, Plan};
            let plan = Plan::new(n, Direction::Forward, Effort::Measure).unwrap();
            let mut buf = AlignedBuf::new(n);
            buf.iter_mut()
                .for_each(|c| *c = Complex32::new(rng.next(), rng.next()));
            time(&mut || plan.execute(&mut buf))
        };
        #[cfg(not(feature = "fftw"))]
        let m = f64::NAN;
        let rf = FftPlanner::<f32>::new().plan_fft_forward(n);
        let mut buf: Vec<Complex32> = (0..n)
            .map(|_| Complex32::new(rng.next(), rng.next()))
            .collect();
        let mut scratch = vec![Complex32::ZERO; rf.get_inplace_scratch_len()];
        let r = time(&mut || rf.process_with_scratch(black_box(&mut buf), &mut scratch));
        println!("{n:>8} {m:>12.1} {r:>12.1} {:>8.2}x", r / m);
    }
}

fn run(
    fs: f64,
    k: usize,
    ko: usize,
    threads: usize,
    nch: usize,
    blocks: usize,
) -> (Config, Duration, Duration, Duration) {
    let mut cfg = Config::p25(fs).unwrap();
    let d = cfg.profile(NARROW).decim;
    cfg.n = d * k;
    cfg.overlap_in = d * ko;
    cfg.threads = threads;
    let hop = cfg.hop();
    let mut ch = Channelizer::new(cfg.clone()).unwrap();
    // spread channels across the passband, avoiding the DC bin
    let span = fs * 0.9 - cfg.out_rate(NARROW);
    for i in 0..nch {
        let f = -span / 2.0 + span * (i as f64 + 0.37) / nch as f64;
        ch.add_channel(NARROW, f).unwrap();
    }
    let mut rng = Rng(42);
    let input: Vec<Complex32> = (0..hop)
        .map(|_| Complex32::new(rng.next(), rng.next()))
        .collect();
    for _ in 0..20 {
        ch.process_block(&input);
    }
    let mut f = Vec::with_capacity(blocks);
    let mut c = Vec::with_capacity(blocks);
    let mut tot = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        let t = Instant::now();
        let s = ch.process_block(&input);
        tot.push(t.elapsed());
        f.push(s.forward);
        c.push(s.channels);
        black_box(ch.outputs().count());
    }
    let med = |mut v: Vec<Duration>| {
        v.sort();
        v[v.len() / 2]
    };
    (cfg, med(f), med(c), med(tot))
}

fn main() {
    let quick = std::env::args().any(|a| a == "--quick");
    println!(
        "cores available: {}",
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    );

    raw_fft_compare(&[
        128, 256, 512, 19_968, 26_624, 8_192, 16_384, 32_768, 65_536, 262_144, 1_048_576,
    ]);
    if std::env::args().any(|a| a == "--fft-only") {
        return;
    }

    let rates = [912_000.0, 1_872_000.0, 2_496_000.0];
    let geoms: &[(usize, usize)] = if quick {
        &[(256, 64)]
    } else {
        &[(128, 32), (256, 64), (512, 128)]
    };
    let chans: &[usize] = if quick {
        &[1, 6, 16]
    } else {
        &[1, 2, 4, 6, 8, 16, 32, 64]
    };
    let threads: &[usize] = if quick { &[0] } else { &[0, 2, 4] };

    for &fs in &rates {
        for &(k, ko) in geoms {
            let probe = Config::p25(fs).unwrap();
            println!(
                "\n== fs={:.3} MS/s decim={} K={k} Ko={ko} N={} hop={} ({:.2} ms/block) bin={:.1} Hz ==",
                fs / 1e6,
                probe.profile(NARROW).decim,
                probe.profile(NARROW).decim * k,
                probe.profile(NARROW).decim * (k - ko),
                (probe.profile(NARROW).decim * (k - ko)) as f64 / fs * 1e3,
                fs / (probe.profile(NARROW).decim * k) as f64
            );
            println!(
                "{:>4} {:>3} {:>9} {:>10} {:>10} {:>9} {:>8}",
                "thr", "ch", "fwd us", "chans us", "us/chan", "load %", "x_rt"
            );
            for &t in threads {
                for &n in chans {
                    let (cfg, fwd, chs, tot) = run(fs, k, ko, t, n, if quick { 100 } else { 300 });
                    let budget = cfg.block_duration();
                    println!(
                        "{:>4} {:>3} {:>9.0} {:>10.0} {:>10.1} {:>8.1}% {:>7.1}x",
                        if t == 0 {
                            "ser".to_string()
                        } else {
                            t.to_string()
                        },
                        n,
                        us(fwd),
                        us(chs),
                        us(chs) / n as f64,
                        100.0 * tot.as_secs_f64() / budget.as_secs_f64(),
                        budget.as_secs_f64() / tot.as_secs_f64()
                    );
                }
            }
        }
    }

    mixed(quick);
}

/// One source carrying analog and digital channels at once (`Config::standard`):
/// the forward FFT is shared, so adding analog channels costs only their
/// (larger) inverse FFTs.
fn mixed(quick: bool) {
    println!("\n== mixed analog + digital, Config::standard, serial ==");
    println!(
        "{:>9} {:>6} {:>6} {:>8} {:>9} {:>10} {:>8}",
        "fs MS/s", "narrow", "wide", "fwd us", "chans us", "block ms", "load %"
    );
    let blocks = if quick { 100 } else { 300 };
    for fs in [912_000.0, 1_872_000.0, 2_496_000.0] {
        for &(nn, nw) in &[(6, 0), (0, 4), (6, 4), (6, 16)] {
            let cfg = Config::standard(fs).unwrap();
            let hop = cfg.hop();
            let mut ch = Channelizer::new(cfg.clone()).unwrap();
            let span = fs * 0.85 - cfg.out_rate(WIDE);
            let total = nn + nw;
            for i in 0..total {
                let f = -span / 2.0 + span * (i as f64 + 0.37) / total as f64;
                ch.add_channel(if i < nn { NARROW } else { WIDE }, f)
                    .unwrap();
            }
            let mut rng = Rng(42);
            let input: Vec<Complex32> = (0..hop)
                .map(|_| Complex32::new(rng.next(), rng.next()))
                .collect();
            for _ in 0..20 {
                ch.process_block(&input);
            }
            let mut f = Vec::new();
            let mut c = Vec::new();
            for _ in 0..blocks {
                let s = ch.process_block(&input);
                f.push(s.forward);
                c.push(s.channels);
                black_box(ch.outputs().count());
            }
            f.sort();
            c.sort();
            let (f, c) = (f[f.len() / 2], c[c.len() / 2]);
            let budget = cfg.block_duration();
            println!(
                "{:>9.3} {:>6} {:>6} {:>8.0} {:>9.0} {:>10.2} {:>7.1}%",
                fs / 1e6,
                nn,
                nw,
                us(f),
                us(c),
                budget.as_secs_f64() * 1e3,
                100.0 * (f + c).as_secs_f64() / budget.as_secs_f64()
            );
        }
    }
}
