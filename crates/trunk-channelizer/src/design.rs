//! Channel filter design (Kaiser-windowed sinc low-pass).

use std::f64::consts::PI;

/// Zeroth-order modified Bessel function of the first kind.
fn i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..64 {
        term *= q / f64::from(k * k);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Kaiser-window low-pass with unity DC gain.
///
/// `cutoff_hz` is the centre of the transition band, `transition_hz` its
/// width, `atten_db` the stop-band attenuation (same convention as GNU
/// Radio's `firdes::low_pass_2`). Length is odd.
pub fn lowpass_taps(rate: f64, cutoff_hz: f64, transition_hz: f64, atten_db: f64) -> Vec<f32> {
    let beta = if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db >= 21.0 {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0)
    } else {
        0.0
    };
    let dw = 2.0 * PI * transition_hz / rate;
    let mut n = ((atten_db - 7.95) / (2.285 * dw)).ceil() as usize + 1;
    n |= 1; // odd
    let mid = (n - 1) as f64 / 2.0;
    let wc = 2.0 * PI * cutoff_hz / rate;
    let denom = i0(beta);
    let mut taps: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64 - mid;
            let sinc = if x == 0.0 { wc / PI } else { (wc * x).sin() / (PI * x) };
            let r = x / mid.max(1.0);
            sinc * i0(beta * (1.0 - r * r).max(0.0).sqrt()) / denom
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    for t in &mut taps {
        *t /= sum;
    }
    taps.into_iter().map(|t| t as f32).collect()
}
