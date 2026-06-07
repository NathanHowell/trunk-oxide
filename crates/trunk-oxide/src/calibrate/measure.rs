//! Pure measurement of a captured IQ block: noise floor, saturation, peak.
//!
//! This is the hardware-independent half of gain calibration. It takes a block
//! of complex baseband samples (already captured at a fixed gain by some
//! [`IqProbe`](super::probe::IqProbe)) and reduces it to a small
//! [`Measurement`]. Everything here is deterministic and unit-testable with
//! synthetic signals — no device, no FFI.

use num_complex::Complex32;
use rustfft::{FftPlanner, num_complex::Complex};

/// Reduced statistics for one IQ capture at a fixed gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// Estimated noise floor: a low percentile of the per-bin PSD, in dB.
    ///
    /// The scale is relative (FFT magnitude, not calibrated to dBm). Only
    /// *changes across gain steps* are meaningful — which is exactly what knee
    /// detection needs. Do not treat this as an absolute power.
    pub noise_floor_db: f64,
    /// Strongest PSD bin, in dB (same relative scale as `noise_floor_db`).
    pub peak_db: f64,
    /// Fraction of samples at/near ADC full-scale — the overload / clipping
    /// indicator. `0.0` = nothing clipping, `1.0` = everything pinned.
    pub sat_frac: f64,
}

/// Tunables for the measurement stage. Defaults live in
/// [`CalibrationPolicy`](super::CalibrationPolicy).
#[derive(Debug, Clone, Copy)]
pub struct MeasureParams {
    /// FFT length for the Welch periodogram (power of two recommended).
    pub fft_size: usize,
    /// Percentile of the PSD taken as the noise floor, in `0.0..=1.0`.
    ///
    /// Noise occupies the quiet bins, so a low percentile (e.g. 0.25) tracks
    /// the floor robustly even when several carriers are present in-band.
    pub noise_floor_percentile: f64,
    /// |I| or |Q| magnitude (in normalized full-scale units, ~`1.0`) at or
    /// above which a sample counts as saturated. RTL output is ~`[-1, 1]`.
    pub sat_threshold: f32,
}

/// Compute an averaged power-spectral-density estimate (Welch's method,
/// Hann window, 50% overlap). Returns linear power per bin, length `fft_size`.
/// Returns an empty vec if there aren't enough samples for one segment.
fn welch_psd(iq: &[Complex32], fft_size: usize) -> Vec<f64> {
    if fft_size == 0 || iq.len() < fft_size {
        return Vec::new();
    }

    let window = hann(fft_size);
    // Window power, for normalization (so the floor scale is stable across
    // captures regardless of segment count).
    let win_power: f64 = window.iter().map(|w| f64::from(*w) * f64::from(*w)).sum();

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);
    let mut scratch = vec![Complex::<f32>::new(0.0, 0.0); fft_size];

    let mut acc = vec![0.0f64; fft_size];
    let hop = (fft_size / 2).max(1);
    let mut segments = 0u64;
    let mut start = 0;
    while start + fft_size <= iq.len() {
        for i in 0..fft_size {
            let s = iq[start + i];
            scratch[i] = Complex::new(s.re * window[i], s.im * window[i]);
        }
        fft.process(&mut scratch);
        for (a, c) in acc.iter_mut().zip(scratch.iter()) {
            *a += f64::from(c.re) * f64::from(c.re) + f64::from(c.im) * f64::from(c.im);
        }
        segments += 1;
        start += hop;
    }

    let norm = 1.0 / (segments as f64 * win_power);
    for a in &mut acc {
        *a *= norm;
    }
    acc
}

/// Periodic Hann window of length `n`.
fn hann(n: usize) -> Vec<f32> {
    if n == 1 {
        return vec![1.0];
    }
    (0..n)
        .map(|i| {
            let x = std::f64::consts::PI * i as f64 / n as f64;
            (x.sin() * x.sin()) as f32
        })
        .collect()
}

/// dB of a linear power value, floored to avoid `-inf` on exact zeros.
fn to_db(power: f64) -> f64 {
    const FLOOR: f64 = 1e-30;
    10.0 * power.max(FLOOR).log10()
}

/// Value at `percentile` (`0.0..=1.0`) of `values` via nearest-rank on a sorted
/// copy. Returns `NaN` for an empty slice.
fn percentile(values: &[f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p = percentile.clamp(0.0, 1.0);
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

/// Fraction of samples whose I or Q component is at/above `threshold` of
/// full-scale — the ADC-clipping (overload) indicator.
#[must_use]
pub fn saturation_fraction(iq: &[Complex32], threshold: f32) -> f64 {
    if iq.is_empty() {
        return 0.0;
    }
    let clipped = iq
        .iter()
        .filter(|s| s.re.abs() >= threshold || s.im.abs() >= threshold)
        .count();
    clipped as f64 / iq.len() as f64
}

/// Reduce an IQ capture to a [`Measurement`].
#[must_use]
pub fn measure(iq: &[Complex32], params: &MeasureParams) -> Measurement {
    let psd = welch_psd(iq, params.fft_size);
    let (noise_floor_db, peak_db) = if psd.is_empty() {
        (f64::NAN, f64::NAN)
    } else {
        let db: Vec<f64> = psd.iter().map(|p| to_db(*p)).collect();
        let floor = percentile(&db, params.noise_floor_percentile);
        let peak = db.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        (floor, peak)
    };
    Measurement {
        noise_floor_db,
        peak_db,
        sat_frac: saturation_fraction(iq, params.sat_threshold),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn params() -> MeasureParams {
        MeasureParams {
            fft_size: 1024,
            noise_floor_percentile: 0.25,
            sat_threshold: 0.95,
        }
    }

    /// Deterministic pseudo-random noise in [-amp, amp] without rand/Date.
    fn noise(n: usize, amp: f32, seed: u64) -> Vec<Complex32> {
        let mut s = seed | 1;
        let mut next = || {
            // xorshift64
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            // map to [-1, 1)
            (s >> 11) as f32 / (1u64 << 52) as f32 * 2.0 - 1.0
        };
        (0..n)
            .map(|_| Complex32::new(next() * amp, next() * amp))
            .collect()
    }

    /// A complex tone at normalized frequency `f` (cycles/sample), amplitude `a`.
    fn tone(n: usize, f: f32, a: f32) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let ph = TAU * f * i as f32;
                Complex32::new(a * ph.cos(), a * ph.sin())
            })
            .collect()
    }

    #[test]
    fn saturation_counts_clipped_samples() {
        let mut iq = noise(1000, 0.1, 7);
        // Pin 50 samples to full scale.
        for s in iq.iter_mut().take(50) {
            s.re = 1.0;
        }
        let f = saturation_fraction(&iq, 0.95);
        assert!((f - 0.05).abs() < 1e-9, "expected ~0.05, got {f}");
    }

    #[test]
    fn saturation_zero_for_clean_signal() {
        let iq = noise(4096, 0.2, 11);
        assert_eq!(saturation_fraction(&iq, 0.95), 0.0);
    }

    #[test]
    fn higher_amplitude_noise_raises_floor() {
        let p = params();
        let quiet = measure(&noise(8192, 0.05, 1), &p);
        let loud = measure(&noise(8192, 0.5, 1), &p);
        assert!(
            loud.noise_floor_db > quiet.noise_floor_db + 6.0,
            "louder noise should raise the floor markedly: quiet={} loud={}",
            quiet.noise_floor_db,
            loud.noise_floor_db
        );
    }

    #[test]
    fn tone_shows_up_as_peak_above_floor() {
        let p = params();
        let mut iq = noise(8192, 0.02, 3);
        for (dst, t) in iq.iter_mut().zip(tone(8192, 0.1, 0.5)) {
            dst.re += t.re;
            dst.im += t.im;
        }
        let m = measure(&iq, &p);
        assert!(
            m.peak_db > m.noise_floor_db + 20.0,
            "tone peak should tower over the floor: peak={} floor={}",
            m.peak_db,
            m.noise_floor_db
        );
    }

    #[test]
    fn too_few_samples_yields_nan_floor() {
        let p = params();
        let m = measure(&noise(100, 0.2, 5), &p);
        assert!(m.noise_floor_db.is_nan());
    }
}
