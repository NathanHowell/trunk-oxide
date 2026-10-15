use std::f64::consts::TAU;

use num_complex::{Complex32, Complex64};

use super::*;

/// Deterministic LCG in [-1, 1).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

struct Tone {
    off_hz: f64, // relative to the channel centre
    amp: f64,
    phase: f64,
}

fn synth(fs: f64, centre: f64, tones: &[Tone], start: usize, len: usize) -> Vec<Complex32> {
    (start..start + len)
        .map(|n| {
            let t = n as f64 / fs;
            let mut acc = Complex64::new(0.0, 0.0);
            for tone in tones {
                acc += Complex64::from_polar(tone.amp, TAU * (centre + tone.off_hz) * t + tone.phase);
            }
            Complex32::new(acc.re as f32, acc.im as f32)
        })
        .collect()
}

/// Direct f64 reference with the channelizer's semantics: mix by the nearest
/// bin centre, decimate, causal FIR at the output rate, then rotate by the
/// residual offset (so the filter is centred on the bin, not on `centre`;
/// at most half a bin off, ~47 Hz of a 6.25 kHz cutoff).
fn reference(cfg: &Config, centre: f64, input: &[Complex32], taps: &[f32]) -> Vec<Complex64> {
    let df = cfg.sample_rate / cfg.fft_size() as f64;
    let c1 = (centre / df).round() * df;
    let resid = centre - c1;
    let z: Vec<Complex64> = input
        .iter()
        .enumerate()
        .map(|(n, x)| Complex64::new(f64::from(x.re), f64::from(x.im)) * Complex64::from_polar(1.0, -TAU * c1 * n as f64 / cfg.sample_rate))
        .collect();
    let dec: Vec<Complex64> = z.iter().step_by(cfg.decim).copied().collect();
    (0..dec.len())
        .map(|g| {
            let y: Complex64 = taps.iter().enumerate().filter(|(l, _)| *l <= g).map(|(l, t)| dec[g - l] * f64::from(*t)).sum();
            y * Complex64::from_polar(1.0, -TAU * resid * g as f64 / cfg.out_rate())
        })
        .collect()
}

fn run(cfg: &Config, centre: f64, tones: &[Tone], blocks: usize) -> (Vec<Complex32>, Vec<Complex32>) {
    let mut ch = Channelizer::new(cfg.clone()).unwrap();
    let id = ch.add_channel(centre).unwrap();
    let hop = cfg.hop();
    let input = synth(cfg.sample_rate, centre, tones, 0, hop * blocks);
    let mut out = Vec::new();
    for b in 0..blocks {
        ch.process_block(&input[b * hop..(b + 1) * hop]);
        out.extend_from_slice(ch.output(id).unwrap());
    }
    (input, out)
}

fn max_err(cfg: &Config, centre: f64, tones: &[Tone], blocks: usize) -> f64 {
    let (input, out) = run(cfg, centre, tones, blocks);
    let taps = lowpass_taps(cfg.out_rate(), cfg.cutoff_hz, cfg.transition_hz, cfg.atten_db);
    let want = reference(cfg, centre, &input, &taps);
    assert!(out.len() <= want.len());
    // skip the filter start-up transient (first block's zero history)
    let skip = cfg.out_per_block() + taps.len();
    out.iter().zip(&want).skip(skip).map(|(a, b)| (Complex64::new(f64::from(a.re), f64::from(a.im)) - b).norm()).fold(0.0, f64::max)
}

fn in_slice_tones() -> Vec<Tone> {
    vec![
        Tone { off_hz: 1_000.0, amp: 1.0, phase: 0.3 },
        Tone { off_hz: -2_345.6, amp: 0.5, phase: 1.1 },
        Tone { off_hz: 9_000.0, amp: 1.0, phase: 2.0 }, // in the filter stop band
        Tone { off_hz: -10_500.0, amp: 1.0, phase: 0.7 },
    ]
}

#[test]
fn matches_direct_reference_unaligned_centres() {
    for &(fs, centre) in &[(2_496_000.0, 123_456.789), (2_496_000.0, -310_001.3), (1_872_000.0, 7.7), (912_000.0, -401_000.4), (2_496_000.0, 0.0)] {
        let cfg = Config::p25(fs).unwrap();
        let err = max_err(&cfg, centre, &in_slice_tones(), 12);
        assert!(err < 2e-4, "fs={fs} centre={centre}: max error {err}");
    }
}

#[test]
fn out_of_slice_tone_is_rejected() {
    let cfg = Config::p25(2_496_000.0).unwrap();
    let centre = 200_000.0;
    // tone 20 kHz away: outside the +/-12 kHz slice, must not alias in
    let (_, out) = run(&cfg, centre, &[Tone { off_hz: 20_000.0, amp: 1.0, phase: 0.0 }], 8);
    let p = out.iter().skip(cfg.out_per_block() * 2).map(|c| f64::from(c.norm_sqr())).sum::<f64>() / (out.len() - cfg.out_per_block() * 2) as f64;
    assert!(10.0 * p.log10() < -90.0, "leakage {:.1} dB", 10.0 * p.log10());
}

#[test]
fn passband_gain_is_unity() {
    let cfg = Config::p25(2_496_000.0).unwrap();
    let (_, out) = run(&cfg, -50_123.4, &[Tone { off_hz: 500.0, amp: 1.0, phase: 0.0 }], 8);
    let tail = &out[out.len() - 64..];
    for c in tail {
        assert!((c.norm() - 1.0).abs() < 1e-3, "|y|={}", c.norm());
    }
}

#[test]
fn retune_is_glitch_free_for_other_channels_and_immediately_valid() {
    let cfg = Config::p25(2_496_000.0).unwrap();
    let hop = cfg.hop();
    let (fa, fb, fc) = (100_000.0, -250_000.0, 300_500.5);
    let tones = [Tone { off_hz: 700.0, amp: 1.0, phase: 0.0 }];
    // signal at each of the three centres + 700 Hz
    let mut input = synth(cfg.sample_rate, fa, &tones, 0, hop * 10);
    for (i, f) in [fb, fc].iter().enumerate() {
        let _ = i;
        for (x, y) in input.iter_mut().zip(synth(cfg.sample_rate, *f, &tones, 0, hop * 10)) {
            *x += y;
        }
    }
    let mut ch = Channelizer::new(cfg).unwrap();
    let a = ch.add_channel(fa).unwrap();
    let b = ch.add_channel(fb).unwrap();
    let mut a_out = Vec::new();
    for blk in 0..10 {
        if blk == 5 {
            ch.retune(b, fc).unwrap(); // retune B mid-stream
        }
        ch.process_block(&input[blk * hop..(blk + 1) * hop]);
        a_out.extend_from_slice(ch.output(a).unwrap());
        if blk >= 6 {
            // one block after the retune B must already show the 700 Hz tone at unity gain
            for c in ch.output(b).unwrap() {
                assert!((c.norm() - 1.0).abs() < 2e-3);
            }
        }
    }
    for c in &a_out[a_out.len() / 2..] {
        assert!((c.norm() - 1.0).abs() < 2e-3);
    }
}

#[test]
fn rejects_filter_longer_than_overlap() {
    let mut cfg = Config::p25(2_496_000.0).unwrap();
    cfg.overlap_out = 16; // far shorter than the ~29-tap filter
    assert!(Channelizer::new(cfg).is_err());
}

#[test]
fn rejects_channel_outside_passband() {
    let mut ch = Channelizer::new(Config::p25(912_000.0).unwrap()).unwrap();
    assert!(ch.add_channel(450_000.0).is_err());
    assert!(ch.add_channel(440_000.0).is_ok());
}

#[test]
fn parallel_matches_serial() {
    let serial = Config::p25(1_872_000.0).unwrap();
    let mut par = serial.clone();
    par.threads = 3;
    let hop = serial.hop();
    let mut rng = Rng(7);
    let input: Vec<Complex32> = (0..hop * 4).map(|_| Complex32::new(rng.next() as f32, rng.next() as f32)).collect();
    let mut a = Channelizer::new(serial).unwrap();
    let mut b = Channelizer::new(par).unwrap();
    let freqs = [-300_000.0, -100_000.5, 5_000.0, 250_000.0, 400_000.0];
    let ids: Vec<_> = freqs.iter().map(|f| (a.add_channel(*f).unwrap(), b.add_channel(*f).unwrap())).collect();
    for blk in 0..4 {
        a.process_block(&input[blk * hop..(blk + 1) * hop]);
        b.process_block(&input[blk * hop..(blk + 1) * hop]);
        for (ia, ib) in &ids {
            assert_eq!(a.output(*ia), b.output(*ib));
        }
    }
}

