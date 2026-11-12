use std::f64::consts::TAU;

use num_complex::{Complex32, Complex64};

use super::*;
use crate::config::{NARROW, WIDE};

use rand::{Rng as _, SeedableRng as _};
use rand_chacha::ChaCha8Rng;

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
                acc += Complex64::from_polar(
                    tone.amp,
                    (TAU * (centre + tone.off_hz)).mul_add(t, tone.phase),
                );
            }
            Complex32::new(acc.re as f32, acc.im as f32)
        })
        .collect()
}

/// Direct f64 reference with the channelizer's semantics: mix by the nearest
/// bin centre, decimate, causal FIR at the output rate, then rotate by the
/// residual offset (so the filter is centred on the bin, not on `centre`;
/// at most half a bin off, ~47 Hz of a 6.25 kHz cutoff).
fn reference(
    cfg: &Config,
    profile: ProfileId,
    centre: f64,
    input: &[Complex32],
    taps: &[f32],
) -> Vec<Complex64> {
    let df = cfg.sample_rate / cfg.fft_size() as f64;
    let c1 = (centre / df).round() * df;
    let resid = centre - c1;
    let z: Vec<Complex64> = input
        .iter()
        .enumerate()
        .map(|(n, x)| {
            Complex64::new(f64::from(x.re), f64::from(x.im))
                * Complex64::from_polar(1.0, -TAU * c1 * n as f64 / cfg.sample_rate)
        })
        .collect();
    let dec: Vec<Complex64> = z
        .iter()
        .step_by(cfg.profile(profile).decim)
        .copied()
        .collect();
    (0..dec.len())
        .map(|g| {
            let y: Complex64 = taps
                .iter()
                .enumerate()
                .filter(|(l, _)| *l <= g)
                .map(|(l, t)| dec[g - l] * f64::from(*t))
                .sum();
            y * Complex64::from_polar(1.0, -TAU * resid * g as f64 / cfg.out_rate(profile))
        })
        .collect()
}

fn run(
    cfg: &Config,
    profile: ProfileId,
    centre: f64,
    tones: &[Tone],
    blocks: usize,
) -> (Vec<Complex32>, Vec<Complex32>) {
    let mut ch = Channelizer::new(cfg.clone()).unwrap();
    let id = ch.add_channel(profile, centre).unwrap();
    let hop = cfg.hop();
    let input = synth(cfg.sample_rate, centre, tones, 0, hop * blocks);
    let mut out = Vec::new();
    for b in 0..blocks {
        ch.process_block(&input[b * hop..(b + 1) * hop]);
        out.extend_from_slice(ch.output(id).unwrap());
    }
    (input, out)
}

fn max_err(cfg: &Config, profile: ProfileId, centre: f64, tones: &[Tone], blocks: usize) -> f64 {
    let (input, out) = run(cfg, profile, centre, tones, blocks);
    let p = cfg.profile(profile);
    let taps = lowpass_taps(
        cfg.out_rate(profile),
        p.cutoff_hz,
        p.transition_hz,
        p.atten_db,
    );
    let want = reference(cfg, profile, centre, &input, &taps);
    assert!(out.len() <= want.len());
    // skip the filter start-up transient (first block's zero history)
    let skip = cfg.out_per_block(profile) + taps.len();
    out.iter()
        .zip(&want)
        .skip(skip)
        .map(|(a, b)| (Complex64::new(f64::from(a.re), f64::from(a.im)) - b).norm())
        .fold(0.0, f64::max)
}

fn in_slice_tones() -> Vec<Tone> {
    vec![
        Tone {
            off_hz: 1_000.0,
            amp: 1.0,
            phase: 0.3,
        },
        Tone {
            off_hz: -2_345.6,
            amp: 0.5,
            phase: 1.1,
        },
        Tone {
            off_hz: 9_000.0,
            amp: 1.0,
            phase: 2.0,
        }, // in the filter stop band
        Tone {
            off_hz: -10_500.0,
            amp: 1.0,
            phase: 0.7,
        },
    ]
}

#[test]
fn matches_direct_reference_unaligned_centres() {
    for &(fs, centre) in &[
        (2_496_000.0, 123_456.789),
        (2_496_000.0, -310_001.3),
        (1_872_000.0, 7.7),
        (912_000.0, -401_000.4),
        (2_496_000.0, 0.0),
    ] {
        let cfg = Config::p25(fs).unwrap();
        let err = max_err(&cfg, NARROW, centre, &in_slice_tones(), 12);
        assert!(err < 2e-4, "fs={fs} centre={centre}: max error {err}");
    }
}

#[test]
fn out_of_slice_tone_is_rejected() {
    let cfg = Config::p25(2_496_000.0).unwrap();
    let centre = 200_000.0;
    // tone 20 kHz away: outside the +/-12 kHz slice, must not alias in
    let (_, out) = run(
        &cfg,
        NARROW,
        centre,
        &[Tone {
            off_hz: 20_000.0,
            amp: 1.0,
            phase: 0.0,
        }],
        8,
    );
    let p = out
        .iter()
        .skip(cfg.out_per_block(NARROW) * 2)
        .map(|c| f64::from(c.norm_sqr()))
        .sum::<f64>()
        / (out.len() - cfg.out_per_block(NARROW) * 2) as f64;
    assert!(
        10.0 * p.log10() < -90.0,
        "leakage {:.1} dB",
        10.0 * p.log10()
    );
}

#[test]
fn passband_gain_is_unity() {
    let cfg = Config::p25(2_496_000.0).unwrap();
    let (_, out) = run(
        &cfg,
        NARROW,
        -50_123.4,
        &[Tone {
            off_hz: 500.0,
            amp: 1.0,
            phase: 0.0,
        }],
        8,
    );
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
    let tones = [Tone {
        off_hz: 700.0,
        amp: 1.0,
        phase: 0.0,
    }];
    // signal at each of the three centres + 700 Hz
    let mut input = synth(cfg.sample_rate, fa, &tones, 0, hop * 10);
    for (i, f) in [fb, fc].iter().enumerate() {
        let _ = i;
        for (x, y) in input
            .iter_mut()
            .zip(synth(cfg.sample_rate, *f, &tones, 0, hop * 10))
        {
            *x += y;
        }
    }
    let mut ch = Channelizer::new(cfg).unwrap();
    let a = ch.add_channel(NARROW, fa).unwrap();
    let b = ch.add_channel(NARROW, fb).unwrap();
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
    cfg.overlap_in = 16 * cfg.profile(NARROW).decim; // overlap_out 16: far shorter than the ~29-tap filter
    assert!(Channelizer::new(cfg).is_err());
}

#[test]
fn rejects_channel_outside_passband() {
    let mut ch = Channelizer::new(Config::p25(912_000.0).unwrap()).unwrap();
    assert!(ch.add_channel(NARROW, 450_000.0).is_err());
    assert!(ch.add_channel(NARROW, 440_000.0).is_ok());
    assert!(
        ch.add_channel(WIDE, 440_000.0).is_err(),
        "no wide profile in a p25-only config"
    );
}

#[test]
fn parallel_matches_serial() {
    let serial = Config::p25(1_872_000.0).unwrap();
    let mut par = serial.clone();
    par.threads = 3;
    let hop = serial.hop();
    let mut rng = ChaCha8Rng::seed_from_u64(7);
    let input: Vec<Complex32> = (0..hop * 4)
        .map(|_| {
            Complex32::new(
                rng.random_range(-1.0f32..1.0),
                rng.random_range(-1.0f32..1.0),
            )
        })
        .collect();
    let mut a = Channelizer::new(serial).unwrap();
    let mut b = Channelizer::new(par).unwrap();
    let freqs = [-300_000.0, -100_000.5, 5_000.0, 250_000.0, 400_000.0];
    let ids: Vec<_> = freqs
        .iter()
        .map(|f| {
            (
                a.add_channel(NARROW, *f).unwrap(),
                b.add_channel(NARROW, *f).unwrap(),
            )
        })
        .collect();
    for blk in 0..4 {
        a.process_block(&input[blk * hop..(blk + 1) * hop]);
        b.process_block(&input[blk * hop..(blk + 1) * hop]);
        for (ia, ib) in &ids {
            assert_eq!(a.output(*ia), b.output(*ib));
        }
    }
}

/// Wide (analog) channel against the direct f64 reference, on every source
/// rate we run, including ones where the rate is not a multiple of 96 kHz.
#[test]
fn wide_profile_matches_direct_reference() {
    for &(fs, centre) in &[
        (2_496_000.0, 123_456.789),
        (1_872_000.0, -7.7),
        (912_000.0, 311_000.4),
        (2_400_000.0, -400_000.0),
    ] {
        let cfg = Config::standard(fs).unwrap();
        // tones inside the 12 kHz channel, in the stop band, and just outside
        let tones = [
            Tone {
                off_hz: 1_000.0,
                amp: 1.0,
                phase: 0.3,
            },
            Tone {
                off_hz: -4_321.0,
                amp: 0.5,
                phase: 1.1,
            },
            Tone {
                off_hz: 20_000.0,
                amp: 1.0,
                phase: 2.0,
            },
            Tone {
                off_hz: -31_000.0,
                amp: 1.0,
                phase: 0.7,
            },
        ];
        let err = max_err(&cfg, WIDE, centre, &tones, 10);
        assert!(err < 2e-4, "fs={fs} centre={centre}: max error {err}");
    }
}

#[test]
fn wide_profile_rejects_out_of_slice_and_has_unity_gain() {
    let cfg = Config::standard(2_496_000.0).unwrap();
    let (_, out) = run(
        &cfg,
        WIDE,
        200_000.0,
        &[
            Tone {
                off_hz: 500.0,
                amp: 1.0,
                phase: 0.0,
            },
            Tone {
                off_hz: 30_000.0,
                amp: 1.0,
                phase: 0.0,
            },
        ],
        8,
    );
    // 500 Hz passband tone at unity; the 30 kHz tone (>= 60 dB down) barely ripples it
    for c in &out[out.len() - 64..] {
        assert!((c.norm() - 1.0).abs() < 2e-3, "|y|={}", c.norm());
    }
}

/// A channel's own filter changes its response and nothing else: narrowing a
/// wide channel's cutoff kills a tone that the default filter passes.
#[test]
fn per_channel_filter_applies_to_that_channel_only() {
    let cfg = Config::standard(2_496_000.0).unwrap();
    let hop = cfg.hop();
    let centre = 100_000.0;
    let tones = [Tone {
        off_hz: 3_500.0,
        amp: 1.0,
        phase: 0.0,
    }];
    let input = synth(cfg.sample_rate, centre, &tones, 0, hop * 8);
    let mut ch = Channelizer::new(cfg).unwrap();
    let a = ch.add_channel(WIDE, centre).unwrap();
    let b = ch.add_channel(WIDE, centre).unwrap();
    let c = ch.add_channel(NARROW, centre).unwrap();
    ch.set_filter(b, 2_000.0, 1_500.0).unwrap();
    for blk in 0..8 {
        ch.process_block(&input[blk * hop..(blk + 1) * hop]);
    }
    let level = |id: ChannelId| -> f64 {
        let o = ch.output(id).unwrap();
        o.iter().map(|x| f64::from(x.norm())).sum::<f64>() / o.len() as f64
    };
    let (la, lb, lc) = (level(a), level(b), level(c));
    assert!(
        (la - 1.0).abs() < 2e-3,
        "default wide filter passes 3.5 kHz: {la}"
    );
    assert!(lb < 1e-3, "2 kHz filter rejects 3.5 kHz: {lb}");
    assert!(
        (lc - 1.0).abs() < 2e-3,
        "narrow channel passes it too: {lc}"
    );
}

#[test]
fn rejects_filters_that_do_not_fit() {
    let cfg = Config::standard(2_496_000.0).unwrap();
    let mut ch = Channelizer::new(cfg).unwrap();
    let id = ch.add_channel(NARROW, 0.0).unwrap();
    assert!(
        ch.set_filter(id, 3_000.0, 100.0).is_err(),
        "too many taps for the overlap"
    );
    assert!(
        ch.set_filter(id, 20_000.0, 3_000.0).is_err(),
        "cutoff beyond the channel Nyquist"
    );
}

/// Mixed analog + digital on one source: each channel equals what it would
/// produce alone, so sharing the FFT does not couple them.
#[test]
fn mixed_profiles_do_not_interact() {
    let cfg = Config::standard(1_872_000.0).unwrap();
    let hop = cfg.hop();
    let mut rng = ChaCha8Rng::seed_from_u64(11);
    let input: Vec<Complex32> = (0..hop * 5)
        .map(|_| {
            Complex32::new(
                rng.random_range(-1.0f32..1.0),
                rng.random_range(-1.0f32..1.0),
            )
        })
        .collect();
    let chans = [
        (NARROW, -300_000.0),
        (WIDE, -300_000.0),
        (WIDE, 120_500.0),
        (NARROW, 450_000.0),
    ];
    let mut together = Channelizer::new(cfg.clone()).unwrap();
    let ids: Vec<_> = chans
        .iter()
        .map(|(p, f)| together.add_channel(*p, *f).unwrap())
        .collect();
    let mut alone: Vec<_> = chans
        .iter()
        .map(|(p, f)| {
            let mut c = Channelizer::new(cfg.clone()).unwrap();
            let id = c.add_channel(*p, *f).unwrap();
            (c, id)
        })
        .collect();
    for blk in 0..5 {
        let block = &input[blk * hop..(blk + 1) * hop];
        together.process_block(block);
        for ((c, id), tid) in alone.iter_mut().zip(&ids) {
            c.process_block(block);
            assert_eq!(c.output(*id), together.output(*tid));
        }
    }
}
