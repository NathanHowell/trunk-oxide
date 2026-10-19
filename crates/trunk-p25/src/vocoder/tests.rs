//! The vocoders' rules. That their audio is op25's sample for sample is
//! checked against op25 itself in trunk-recorder-sys (`p25_voice` tests).

use super::*;
use crate::tdma::AmbeParams;
use crate::testgen::{self, Rng};

/// op25's `u` for IMBE parameters with b̂0 = `b0`.
fn imbe_with_b0(rng: &mut Rng, b0: u32) -> [u32; 8] {
    let mut u = testgen::random_imbe(rng).to_op25();
    u[0] = (u[0] & 0x3f) | ((b0 >> 2) << 6);
    u[7] = (u[7] & !0x6) | ((b0 & 3) << 1);
    u
}

/// A frame's dibits from its parameters.
fn dibits(p: AmbeParams) -> [u8; 36] {
    let cw = p.encode();
    std::array::from_fn(|d| u8::from(cw.bit(2 * d)) * 2 + u8::from(cw.bit(2 * d + 1)))
}

/// AMBE+2 parameters with quantiser index b0 = `b0`.
fn ambe_with_b0(rng: &mut Rng, b0: u16) -> AmbeParams {
    let AmbeParams(mut u) = testgen::tdma::random_ambe(rng);
    u[0] = (u[0] & !(0xf << 8)) | ((b0 >> 3) << 8);
    u[3] = (u[3] & !(0x7 << 9)) | ((b0 & 7) << 9);
    AmbeParams(u)
}

/// A tone frame: index `id`, amplitude `amplitude`.
fn tone(id: u16, amplitude: u16) -> AmbeParams {
    AmbeParams([
        0xfc0 | (amplitude >> 1),
        (id << 4) | (id >> 4),
        ((id & 0xf) << 7) | (id >> 1),
        ((id & 1) << 13) | (id << 5) | ((amplitude & 1) << 4),
    ])
}

/// Power at `hz` (Goertzel), per sample.
#[allow(clippy::suboptimal_flops, clippy::cast_precision_loss)]
fn power(pcm: &[i16], hz: f64) -> f64 {
    let k = 2.0 * (2.0 * std::f64::consts::PI * hz / 8000.0).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in pcm {
        let s = f64::from(x) + k * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    (s1 * s1 + s2 * s2 - k * s1 * s2) / pcm.len() as f64
}

/// Every pitch either phase sends synthesises within the spectrum (the band
/// edges are debug-asserted), and makes sound.
#[test]
fn every_pitch_synthesises() {
    let mut rng = Rng::new(1);
    let mut imbe = ImbeVocoder::new();
    for b0 in 0..=207 {
        for _ in 0..3 {
            let pcm = imbe.decode(&imbe_with_b0(&mut rng, b0), 0, 0);
            assert!(pcm.is_some_and(|p| p.iter().any(|&s| s != 0)), "b0 {b0}");
        }
    }
    let mut ambe = AmbeVocoder::new();
    for b0 in (0..120).chain([124, 125]) {
        for _ in 0..3 {
            let pcm = ambe.decode(&dibits(ambe_with_b0(&mut rng, b0)));
            assert!(pcm.is_some_and(|p| p.iter().any(|&s| s != 0)), "b0 {b0}");
        }
    }
}

/// Phase 1 repeats a frame it cannot use at most three times, then mutes
/// until one decodes (TIA-102.BABA 7.7).
#[test]
fn imbe_repeats_three_frames_then_mutes() {
    let mut rng = Rng::new(2);
    let mut v = ImbeVocoder::new();
    for _ in 0..5 {
        assert!(v.decode(&imbe_with_b0(&mut rng, 100), 0, 0).is_some());
    }
    let heard: Vec<bool> = (0..6)
        .map(|i| {
            v.decode(
                &imbe_with_b0(&mut rng, if i % 2 == 0 { 100 } else { 220 }),
                2,
                2,
            )
            .is_some()
        })
        .collect();
    assert_eq!(heard, [true, true, true, false, false, false]);
    assert!(v.decode(&imbe_with_b0(&mut rng, 100), 0, 0).is_some());
}

/// Phase 1 mutes while its running error rate is high (7.8), and recovers
/// as it decays.
#[test]
fn imbe_mutes_on_a_high_error_rate() {
    let mut rng = Rng::new(3);
    let mut v = ImbeVocoder::new();
    let mut run = |v: &mut ImbeVocoder, n: usize, et: u32| -> Vec<bool> {
        (0..n)
            .map(|_| v.decode(&imbe_with_b0(&mut rng, 100), 0, et).is_some())
            .collect()
    };
    assert!(run(&mut v, 10, 0).iter().all(|&a| a));
    // Fifteen errors a frame (repeated, then muted for the rate).
    assert!(run(&mut v, 60, 15).iter().rev().take(40).all(|&a| !a));
    let recovery = run(&mut v, 60, 0);
    assert!(!recovery[0] && recovery[59], "{recovery:?}");
}

/// Phase 2 repeats a frame too damaged to use at most three times, then
/// mutes until one decodes.
#[test]
fn ambe_repeats_three_frames_then_mutes() {
    let mut rng = Rng::new(4);
    let mut v = AmbeVocoder::new();
    for _ in 0..5 {
        assert!(v.decode(&dibits(ambe_with_b0(&mut rng, 60))).is_some());
    }
    // b0 120–123 is an erasure.
    let heard: Vec<bool> = (0..6)
        .map(|_| v.decode(&dibits(ambe_with_b0(&mut rng, 121))).is_some())
        .collect();
    assert_eq!(heard, [true, true, true, false, false, false]);
    assert!(v.decode(&dibits(ambe_with_b0(&mut rng, 60))).is_some());
}

/// Encrypted frames make no sound, but their errors raise the rate that
/// mutes the clear ones after them.
#[test]
fn encrypted_frames_count_toward_the_error_rate() {
    let mut rng = Rng::new(5);
    let mut v = AmbeVocoder::new();
    for _ in 0..60 {
        let mut d = dibits(ambe_with_b0(&mut rng, 60));
        // Sent bits 0, 4 and 8: c0's first three, which op25 counts as
        // five errors, enough to hold its rate above the limit.
        d[0] ^= 2;
        d[2] ^= 2;
        d[4] ^= 2;
        v.encrypted(&d);
    }
    assert!(v.decode(&dibits(ambe_with_b0(&mut rng, 60))).is_none());
    let recovered = (0..100).any(|_| v.decode(&dibits(ambe_with_b0(&mut rng, 60))).is_some());
    assert!(recovered);
}

/// A DTMF tone frame plays both of its tones, continuing across frames.
#[test]
fn tones_are_their_frequencies() {
    let mut v = AmbeVocoder::new();
    // '1': 1209 and 697 Hz.
    let pcm: Vec<i16> = (0..10)
        .flat_map(|_| v.decode(&dibits(tone(129, 100))).expect("a tone"))
        .collect();
    let (high, low, between) = (power(&pcm, 1209.0), power(&pcm, 697.0), power(&pcm, 950.0));
    assert!(
        high > 1000.0 * between && low > 1000.0 * between,
        "{high} {low} {between}"
    );
    // Index 255 is a silent tone; an index no tone has is an erasure.
    assert_eq!(
        v.decode(&dibits(tone(255, 100))),
        Some([0; SAMPLES_PER_FRAME])
    );
    assert!(
        v.decode(&dibits(tone(200, 100))).is_some(),
        "repeats the last tone"
    );
}
