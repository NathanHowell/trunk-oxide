//! The power of one tone in a block of audio.

use crate::AUDIO_RATE;

/// Power of the tone at `hz` in `samples`, as the square of the amplitude of
/// a sine of that frequency (a Goertzel filter at exactly `hz`, not at the
/// nearest of the window's 25 Hz bins, which would lose half of a tone
/// that falls between two).
pub fn tone_power(samples: &[f32], hz: f64) -> f64 {
    let n = samples.len() as f64;
    let w = std::f64::consts::TAU * hz / AUDIO_RATE;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s0 = f64::from(x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power / (n / 2.0).powi(2)
}
