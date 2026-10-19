//! FIR tap design, as GNU Radio's `firdes` does it (windowed sinc, tap count
//! from the window's attenuation and the transition width), so the audio
//! filters here are the ones the C++ analog recorder built.

use std::f64::consts::PI;

/// A window function, with the attenuation `firdes` sizes filters by.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Window {
    Hann,
    /// With its beta.
    Kaiser(f64),
}

impl Window {
    /// `window::max_attenuation`.
    fn max_attenuation(self) -> f64 {
        match self {
            Self::Hann => 44.0,
            Self::Kaiser(beta) => beta / 0.1102 + 8.7,
        }
    }

    /// The window's `ntaps` coefficients.
    fn taps(self, ntaps: usize) -> Vec<f32> {
        match self {
            Self::Hann => {
                let m = (ntaps - 1) as f32;
                (0..ntaps)
                    .map(|n| {
                        0.5 - 0.5 * ((2.0 * PI * f64::from(n as f32) / f64::from(m)) as f32).cos()
                    })
                    .collect()
            }
            Self::Kaiser(beta) => {
                let ibeta = 1.0 / izero(beta);
                let inm1 = 1.0 / (ntaps as f64 - 1.0);
                (0..ntaps)
                    .map(|i| {
                        if i == 0 || i == ntaps - 1 {
                            ibeta as f32
                        } else {
                            let temp = 2.0 * i as f64 * inm1 - 1.0;
                            (izero(beta * (1.0 - temp * temp).sqrt()) * ibeta) as f32
                        }
                    })
                    .collect()
            }
        }
    }
}

/// Zeroth-order modified Bessel function of the first kind, by the series
/// `window.cc` sums.
fn izero(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut u = 1.0;
    let mut n = 1.0;
    let halfx = x / 2.0;
    loop {
        let temp = halfx / n;
        n += 1.0;
        u *= temp * temp;
        sum += u;
        if u < 1e-21 * sum {
            return sum;
        }
    }
}

/// `firdes::compute_ntaps`: odd, from the window's attenuation.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn compute_ntaps(sampling_freq: f64, transition_width: f64, window: Window) -> usize {
    let a = window.max_attenuation();
    let mut ntaps = (a * sampling_freq / (22.0 * transition_width)) as usize;
    if ntaps.is_multiple_of(2) {
        ntaps += 1;
    }
    ntaps
}

/// `firdes::low_pass`: unity gain at DC.
#[must_use]
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
pub fn low_pass(
    gain: f64,
    sampling_freq: f64,
    cutoff_freq: f64,
    transition_width: f64,
    window: Window,
) -> Vec<f32> {
    let ntaps = compute_ntaps(sampling_freq, transition_width, window);
    let w = window.taps(ntaps);
    let m = ((ntaps - 1) / 2) as i64;
    let fw_t0 = 2.0 * PI * cutoff_freq / sampling_freq;
    let mut taps: Vec<f32> = (-m..=m)
        .map(|n| {
            let wn = f64::from(w[(n + m) as usize]);
            let t = if n == 0 {
                fw_t0 / PI * wn
            } else {
                (n as f64 * fw_t0).sin() / (n as f64 * PI) * wn
            };
            t as f32
        })
        .collect();
    let mut fmax = f64::from(taps[m as usize]);
    for n in 1..=m {
        fmax += 2.0 * f64::from(taps[(n + m) as usize]);
    }
    let gain = gain / fmax;
    for t in &mut taps {
        *t = (f64::from(*t) * gain) as f32;
    }
    taps
}

/// `firdes::high_pass`: unity gain at the Nyquist frequency.
#[must_use]
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
pub fn high_pass(
    gain: f64,
    sampling_freq: f64,
    cutoff_freq: f64,
    transition_width: f64,
    window: Window,
) -> Vec<f32> {
    let ntaps = compute_ntaps(sampling_freq, transition_width, window);
    let w = window.taps(ntaps);
    let m = ((ntaps - 1) / 2) as i64;
    let fw_t0 = 2.0 * PI * cutoff_freq / sampling_freq;
    let mut taps: Vec<f32> = (-m..=m)
        .map(|n| {
            let wn = f64::from(w[(n + m) as usize]);
            let t = if n == 0 {
                (1.0 - fw_t0 / PI) * wn
            } else {
                -(n as f64 * fw_t0).sin() / (n as f64 * PI) * wn
            };
            t as f32
        })
        .collect();
    let mut fmax = f64::from(taps[m as usize]);
    for n in 1..=m {
        fmax += 2.0 * f64::from(taps[(n + m) as usize]) * (n as f64 * PI).cos();
    }
    let gain = gain / fmax;
    for t in &mut taps {
        *t = (f64::from(*t) * gain) as f32;
    }
    taps
}

/// `firdes::band_pass`: unity gain at the centre of the band.
#[must_use]
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
pub fn band_pass(
    gain: f64,
    sampling_freq: f64,
    low_cutoff_freq: f64,
    high_cutoff_freq: f64,
    transition_width: f64,
    window: Window,
) -> Vec<f32> {
    let ntaps = compute_ntaps(sampling_freq, transition_width, window);
    let w = window.taps(ntaps);
    let m = ((ntaps - 1) / 2) as i64;
    let fw_t0 = 2.0 * PI * low_cutoff_freq / sampling_freq;
    let fw_t1 = 2.0 * PI * high_cutoff_freq / sampling_freq;
    let mut taps: Vec<f32> = (-m..=m)
        .map(|n| {
            let wn = f64::from(w[(n + m) as usize]);
            let t = if n == 0 {
                (fw_t1 - fw_t0) / PI * wn
            } else {
                ((n as f64 * fw_t1).sin() - (n as f64 * fw_t0).sin()) / (n as f64 * PI) * wn
            };
            t as f32
        })
        .collect();
    let mut fmax = f64::from(taps[m as usize]);
    for n in 1..=m {
        fmax += 2.0 * f64::from(taps[(n + m) as usize]) * (n as f64 * (fw_t0 + fw_t1) * 0.5).cos();
    }
    let gain = gain / fmax;
    for t in &mut taps {
        *t = (f64::from(*t) * gain) as f32;
    }
    taps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_pass_passes_its_centre_and_rejects_dc() {
        let taps = band_pass(1.0, 16_000.0, 4_000.0, 6_000.0, 500.0, Window::Hann);
        let gain_at = |f: f64| {
            let (re, im) = taps
                .iter()
                .enumerate()
                .fold((0.0f64, 0.0f64), |(re, im), (n, t)| {
                    let w = 2.0 * PI * f * n as f64 / 16_000.0;
                    (re + f64::from(*t) * w.cos(), im - f64::from(*t) * w.sin())
                });
            re.hypot(im)
        };
        assert!((gain_at(5_000.0) - 1.0).abs() < 1e-3);
        assert!(gain_at(0.0) < 1e-3);
        assert!(gain_at(2_000.0) < 1e-2);
        assert!(gain_at(7_500.0) < 1e-2);
    }

    #[test]
    fn tap_counts_match_firdes() {
        assert_eq!(compute_ntaps(1.0, 0.1, Window::Kaiser(5.0)), 25);
        assert_eq!(compute_ntaps(16_000.0, 50.0, Window::Hann), 641);
        assert_eq!(compute_ntaps(16_000.0, 500.0, Window::Hann), 65);
    }

    #[test]
    fn low_pass_has_unity_dc_gain_and_is_symmetric() {
        let taps = low_pass(1.0, 16_000.0, 3_250.0, 500.0, Window::Hann);
        let dc: f32 = taps.iter().sum();
        assert!((dc - 1.0).abs() < 1e-6, "dc gain {dc}");
        // Symmetric up to the window's float rounding (cos(2πn/M) and
        // cos(2π(M-n)/M) are not the same bits).
        for (a, b) in taps.iter().zip(taps.iter().rev()) {
            assert!((a - b).abs() < 1e-7, "{a} vs {b}");
        }
    }

    #[test]
    fn high_pass_has_unity_nyquist_gain() {
        let taps = high_pass(1.0, 16_000.0, 300.0, 50.0, Window::Hann);
        let nyq: f32 = taps
            .iter()
            .enumerate()
            .map(|(i, t)| if i % 2 == 0 { *t } else { -*t })
            .sum();
        assert!((nyq - 1.0).abs() < 1e-5, "nyquist gain {nyq}");
        let dc: f32 = taps.iter().sum();
        assert!(dc.abs() < 1e-3, "dc leak {dc}");
    }

    #[test]
    fn kaiser_ends_at_one_over_izero_beta() {
        let w = Window::Kaiser(5.0).taps(25);
        assert_eq!(w[0], w[24]);
        assert!((w[12] - 1.0).abs() < 1e-6);
        assert!((izero(0.0) - 1.0).abs() < 1e-15);
    }
}
