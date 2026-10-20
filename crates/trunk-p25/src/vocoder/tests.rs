//! The vocoders' rules. Their audio is frozen in trunk-radio's `p25_voice`
//! tests.

use super::*;
use crate::tdma::AmbeParams;
use crate::testgen::{self, Rng};

/// `u` as sent for IMBE parameters with b̂0 = `b0` (BABA §7.1).
fn imbe_with_b0(rng: &mut Rng, b0: u32) -> [u32; 8] {
    let mut u = testgen::random_imbe(rng).0.map(u32::from);
    u[0] = (u[0] & 0x3f) | ((b0 >> 2) << 6);
    u[7] = (u[7] & !0x6) | ((b0 & 3) << 1);
    u
}

/// A frame's dibits from its parameters.
fn dibits(p: AmbeParams) -> [u8; 36] {
    let cw = p.encode();
    std::array::from_fn(|d| u8::from(cw.bit(2 * d)) * 2 + u8::from(cw.bit(2 * d + 1)))
}

/// AMBE+2 parameters with quantiser index b0 = `b0`, never a tone frame
/// (u0[11:6] = 63, so b0 ≥ 120 and u0 bit 6 set).
fn ambe_with_b0(rng: &mut Rng, b0: u16) -> AmbeParams {
    let AmbeParams(mut u) = testgen::tdma::random_ambe(rng);
    u[0] = (u[0] & !(0xf << 8) & !(1 << 6)) | ((b0 >> 3) << 8);
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
            let pcm = imbe.decode(&imbe_with_b0(&mut rng, b0), 0, 0, 0);
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
        assert!(v.decode(&imbe_with_b0(&mut rng, 100), 0, 0, 0).is_some());
    }
    let heard: Vec<bool> = (0..6)
        .map(|i| {
            v.decode(
                &imbe_with_b0(&mut rng, if i % 2 == 0 { 100 } else { 220 }),
                2,
                0,
                12,
            )
            .is_some()
        })
        .collect();
    assert_eq!(heard, [true, true, true, false, false, false]);
    assert!(v.decode(&imbe_with_b0(&mut rng, 100), 0, 0, 0).is_some());
}

/// A frame is repeated only when c0 failed (Eq. 97) and it has many errors
/// (Eq. 98); op25 repeated when either held.
#[test]
fn imbe_repeats_only_on_both_tests() {
    let mut rng = Rng::new(10);
    let mut v = ImbeVocoder::new();
    v.decode(&imbe_with_b0(&mut rng, 100), 0, 0, 0);
    // c0 failed, few errors besides.
    v.decode(&imbe_with_b0(&mut rng, 100), 2, 0, 2);
    assert_eq!(v.last(), Decision::Played);
    // Many errors, c0 sound.
    v.decode(&imbe_with_b0(&mut rng, 100), 0, 0, 12);
    assert_eq!(v.last(), Decision::Played);
    v.decode(&imbe_with_b0(&mut rng, 100), 3, 0, 12);
    assert_eq!(v.last(), Decision::Repeated);
}

/// A phase 1 frame plays by the bits the decoder corrected in it.
#[test]
fn imbe_counts_the_bits_it_corrected() {
    let mut rng = Rng::new(11);
    let mut v = ImbeVocoder::new();
    let mut frame = sure_imbe(&mut rng, 100);
    v.vocode(&frame);
    (frame.errors_c0, frame.errors) = (3, 12);
    v.vocode(&frame);
    assert_eq!(v.last(), Decision::Repeated);
}

/// τ<sub>M</sub> builds on the last frame's while errors persist, and `S_E`
/// is a running average (BABA Eqs. 111, 115).
#[test]
fn imbe_thresholds_carry_from_frame_to_frame() {
    use super::synth::{Errors, Synthesizer};
    let mut s = Synthesizer::new();
    let noisy = Errors {
        rate: 0.02,
        total: 10.0,
        c4: 1.0,
    };
    let rm0 = 1e6;
    let mut se = 75000.0;
    for tm in [23480.0, 26480.0, 29480.0] {
        se = 0.95f32.mul_add(se, 0.05 * rm0);
        let t = s.thresholds(Some(rm0), noisy);
        assert_eq!(t.amplitude(), tm);
        let vm = 1.414 * f64::from(se).powf(0.375);
        assert!(
            (f64::from(t.voicing()) / vm - 1.0).abs() < 1e-5,
            "{t:?} {vm}"
        );
    }
    // A repeat keeps S_E; a clean frame resets τ_M.
    let t = s.thresholds(None, noisy);
    assert!((f64::from(t.voicing()) / (1.414 * f64::from(se).powf(0.375)) - 1.0).abs() < 1e-5);
    assert_eq!(
        s.thresholds(Some(rm0), Errors::default()).amplitude(),
        20480.0
    );
}

/// Phase 1 mutes while its running error rate is high (7.8), and recovers
/// as it decays.
#[test]
fn imbe_mutes_on_a_high_error_rate() {
    let mut rng = Rng::new(3);
    let mut v = ImbeVocoder::new();
    let mut run = |v: &mut ImbeVocoder, n: usize, et: u32| -> Vec<bool> {
        (0..n)
            .map(|_| v.decode(&imbe_with_b0(&mut rng, 100), 0, 0, et).is_some())
            .collect()
    };
    assert!(run(&mut v, 10, 0).iter().all(|&a| a));
    // Fifteen errors a frame: muted once the rate passes 0.0875, about
    // thirty frames in.
    assert!(run(&mut v, 60, 15).iter().rev().take(25).all(|&a| !a));
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

// ── Policy::Soft ──

use crate::tdma::AmbeFrame;
use crate::voice::ImbeFrame;

fn sure_imbe(rng: &mut Rng, b0: u32) -> ImbeFrame {
    ImbeFrame {
        params: crate::voice::ImbeParams(imbe_with_b0(rng, b0).map(|x| u16::try_from(x).unwrap())),
        errors: 0,
        errors_c0: 0,
        errors_c4: 0,
        reliability: f32::INFINITY,
        unprotected: f32::INFINITY,
    }
}

fn sure_ambe(rng: &mut Rng, b0: u16) -> AmbeFrame {
    AmbeFrame {
        params: ambe_with_b0(rng, b0),
        errors: 0,
        reliability: f32::INFINITY,
        unprotected: f32::INFINITY,
    }
}

/// Under `Soft`, a trusted frame plays, an untrusted one repeats the last
/// three times and then mutes, whatever op25 would have counted.
#[test]
fn soft_plays_trusted_frames_and_repeats_untrusted_ones() {
    let mut rng = Rng::new(6);
    let mut v = ImbeVocoder::with_policy(Policy::Soft);
    // Hard would repeat this (e0 = 5), Soft goes by the decoder's margin.
    let mut sure = sure_imbe(&mut rng, 100);
    (sure.errors_c0, sure.errors) = (5, 30);
    assert!(v.vocode(&sure).is_some());
    assert_eq!(v.last(), Decision::Played);
    let mut doubtful = sure_imbe(&mut rng, 100);
    doubtful.reliability = PLAY_MARGIN / 2.0;
    let decisions: Vec<Decision> = (0..5)
        .map(|_| {
            v.vocode(&doubtful);
            v.last()
        })
        .collect();
    assert_eq!(
        decisions,
        [
            Decision::Repeated,
            Decision::Repeated,
            Decision::Repeated,
            Decision::Muted,
            Decision::Muted
        ]
    );
    assert!(v.vocode(&sure_imbe(&mut rng, 100)).is_some());
    assert_eq!(v.last(), Decision::Played);

    let mut a = AmbeVocoder::with_policy(Policy::Soft);
    let frame = sure_ambe(&mut rng, 60);
    // Dibits op25 could not decode: Soft never looks at them.
    assert!(a.vocode(&[0; 36], Some(&frame)).is_some());
    assert_eq!(a.last(), Decision::Played);
    let mut doubtful = frame;
    doubtful.reliability = PLAY_MARGIN / 2.0;
    let decisions: Vec<Decision> = (0..5)
        .map(|_| {
            a.vocode(&[0; 36], Some(&doubtful));
            a.last()
        })
        .collect();
    assert_eq!(
        decisions,
        [
            Decision::Repeated,
            Decision::Repeated,
            Decision::Repeated,
            Decision::Muted,
            Decision::Muted
        ]
    );
    assert!(a.vocode(&[0; 36], Some(&frame)).is_some());
    assert_eq!(a.last(), Decision::Played);
}

/// Without the decoder's frame, `Soft` falls back to `Hard`.
#[test]
fn soft_without_a_frame_is_hard() {
    let mut rng = Rng::new(8);
    let mut a = AmbeVocoder::with_policy(Policy::Soft);
    let mut b = AmbeVocoder::new();
    for _ in 0..20 {
        let d = dibits(ambe_with_b0(&mut rng, 60));
        assert_eq!(a.vocode(&d, None), b.vocode(&d, None));
    }
}

// ── Where op25 departs from the standards ──

/// b̂0's low bits are u7's bits 2..1 as sent (BABA §7.1, Fig. 22), where
/// op25 reads bits 1..0.
#[test]
#[allow(clippy::cast_possible_truncation)]
fn the_pitch_is_read_from_u7_as_sent() {
    let mut rng = Rng::new(9);
    for b0 in [100, 101, 102, 103] {
        let mut v = ImbeVocoder::new();
        v.vocode(&sure_imbe(&mut rng, b0));
        let w0 = (4.0 * std::f64::consts::PI / (f64::from(b0) + 39.5)) as f32;
        assert_eq!(v.spectrum().map(|s| s.w0), Some(w0), "b0 {b0}");
    }
}

/// A silence frame between two voice frames leaves the second predicted
/// from the first (BABA-1 Eqs. 26, 43); op25 predicts it from the silence.
#[test]
fn past_silence_the_prediction_is_the_last_voice_frame() {
    let mut rng = Rng::new(12);
    let (a, silence, b) = (
        dibits(ambe_with_b0(&mut rng, 60)),
        dibits(ambe_with_b0(&mut rng, 124)),
        dibits(ambe_with_b0(&mut rng, 70)),
    );
    let after = |frames: &[[u8; 36]]| {
        let mut v = AmbeVocoder::new();
        for f in frames {
            v.vocode(f, None);
        }
        v.spectrum()
    };
    assert!(after(&[a, b]).is_some());
    assert_eq!(after(&[a, silence, b]), after(&[a, b]));
}

/// A tone is known by u0 alone: an error in u3[3:0], which is not
/// protected, leaves it a tone (BABA-1 §7). To op25 it was voice, b0 = 120:
/// an erasure.
#[test]
fn a_tone_is_known_by_u0() {
    let AmbeParams(mut u) = tone(129, 100);
    u[3] |= 1;
    let damaged = dibits(AmbeParams(u));
    let mut v = AmbeVocoder::new();
    let pcm: Vec<i16> = (0..10)
        .flat_map(|_| v.vocode(&damaged, None).expect("a tone"))
        .collect();
    assert!(v.spectrum().is_none(), "played as a tone");
    assert!(power(&pcm, 1209.0) > 1000.0 * power(&pcm, 950.0));
}

/// A tone's amplitude is logarithmic: AD 127 is full scale, 0.711 dB down
/// a step (BABA-1 §7.2).
#[test]
fn a_tones_amplitude_is_logarithmic() {
    let peak = |ad: u16| {
        let mut v = AmbeVocoder::new();
        // 1000 Hz (index 32), a single tone.
        let pcm: Vec<i16> = (0..5)
            .flat_map(|_| v.vocode(&dibits(tone(32, ad)), None).expect("a tone"))
            .collect();
        f64::from(pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0))
    };
    let db = |ad| 20.0 * (peak(ad) / 32767.0).log10();
    assert!(db(127).abs() < 0.1, "{}", db(127));
    assert!(63.0f64.mul_add(0.711, db(64)).abs() < 0.2, "{}", db(64));
    assert!((db(0) + 90.3).abs() < 1.5, "{}", db(0));
}
