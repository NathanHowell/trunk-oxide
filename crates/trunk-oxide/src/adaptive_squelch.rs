use std::collections::VecDeque;

/// Per-channel adaptive squelch tracker using Otsu's method.
///
/// Accumulates FFT power readings into a circular buffer, builds a histogram,
/// and finds the optimal threshold between the noise and signal modes by
/// maximizing between-class variance (Otsu's method).
pub(crate) struct AdaptiveSquelch {
    /// Circular buffer of recent power readings in dB.
    buffer: VecDeque<f32>,
    /// Maximum buffer size.
    capacity: usize,
    /// Current computed optimal threshold in dB.
    current_threshold_db: f64,
    /// The configured (static) squelch threshold, used as fallback.
    configured_threshold_db: f64,
    /// Minimum number of samples before adaptive mode activates.
    min_samples: usize,
    /// Minimum change in dB to trigger an update.
    hysteresis_db: f64,
}

/// Histogram bin width in dB.
const BIN_WIDTH: f64 = 0.5;

/// Margin added above the Otsu split point. Biases the squelch toward
/// rejecting noise rather than catching every signal.
const MARGIN_DB: f64 = 3.0;

/// Minimum bimodality ratio (between-class variance / total variance).
/// Below this the distribution is considered unimodal and we keep the
/// configured threshold.
///
/// A unimodal normal distribution yields ~0.64; a uniform ~0.75.
/// Well-separated bimodal distributions (>5 dB apart) yield >0.85.
const BIMODALITY_MIN: f64 = 0.75;

/// Maximum deviation from the configured threshold in dB.
const CLAMP_DB: f64 = 10.0;

/// Default circular buffer capacity (60 seconds at ~10 readings/s).
const DEFAULT_CAPACITY: usize = 600;

/// Default minimum samples before adaptive mode activates (~5 seconds).
const DEFAULT_MIN_SAMPLES: usize = 50;

/// Default hysteresis in dB.
const DEFAULT_HYSTERESIS_DB: f64 = 2.0;

impl AdaptiveSquelch {
    pub(crate) fn new(configured_threshold_db: f64) -> Self {
        Self {
            buffer: VecDeque::with_capacity(DEFAULT_CAPACITY),
            capacity: DEFAULT_CAPACITY,
            current_threshold_db: configured_threshold_db,
            configured_threshold_db,
            min_samples: DEFAULT_MIN_SAMPLES,
            hysteresis_db: DEFAULT_HYSTERESIS_DB,
        }
    }

    /// Feed a power reading in dB. Returns `Some(new_threshold)` if the
    /// optimal threshold changed by more than `hysteresis_db`.
    pub(crate) fn feed(&mut self, power_db: f32) -> Option<f64> {
        // Push into circular buffer.
        if self.buffer.len() >= self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back(power_db);

        if self.buffer.len() < self.min_samples {
            return None;
        }

        let (threshold, bimodality) = otsu_threshold(self.buffer.make_contiguous())?;

        if bimodality < BIMODALITY_MIN {
            // Unimodal — keep configured threshold.
            return None;
        }

        // Apply margin: bias toward rejecting noise.
        let candidate = threshold + MARGIN_DB;

        // Never go below (more open than) the configured threshold — adaptive
        // squelch should only tighten, not loosen. Also clamp above.
        let clamped = candidate.clamp(
            self.configured_threshold_db,
            self.configured_threshold_db + CLAMP_DB,
        );

        if (clamped - self.current_threshold_db).abs() > self.hysteresis_db {
            self.current_threshold_db = clamped;
            Some(clamped)
        } else {
            None
        }
    }

    /// Current threshold (adaptive or configured if not yet active).
    pub(crate) fn current_threshold(&self) -> f64 {
        self.current_threshold_db
    }
}

/// Compute the optimal threshold using Otsu's method on a histogram of
/// power readings.
///
/// Returns `(threshold_db, bimodality_ratio)` where `bimodality_ratio` is
/// `between_class_variance / total_variance`. Returns `None` if the data
/// range is too narrow to form a meaningful histogram.
fn otsu_threshold(samples: &[f32]) -> Option<(f64, f64)> {
    if samples.len() < 2 {
        return None;
    }

    // Find data range.
    let mut min_val = f32::INFINITY;
    let mut max_val = f32::NEG_INFINITY;
    for &v in samples {
        if v < min_val {
            min_val = v;
        }
        if v > max_val {
            max_val = v;
        }
    }

    let range = f64::from(max_val - min_val);
    if range < BIN_WIDTH {
        return None;
    }

    let min_db = f64::from(min_val);
    let num_bins = ((range / BIN_WIDTH).ceil() as usize).max(2);

    // Build histogram.
    let mut histogram = vec![0u32; num_bins];
    for &v in samples {
        let bin = (((f64::from(v) - min_db) / BIN_WIDTH) as usize).min(num_bins - 1);
        histogram[bin] += 1;
    }

    let total = samples.len() as f64;

    // Total mean (weighted by bin center).
    let mut total_mean = 0.0;
    for (i, &count) in histogram.iter().enumerate() {
        total_mean += bin_center(i, min_db) * f64::from(count);
    }
    total_mean /= total;

    // Total variance.
    let mut total_var = 0.0;
    for (i, &count) in histogram.iter().enumerate() {
        let diff = bin_center(i, min_db) - total_mean;
        total_var += diff * diff * f64::from(count);
    }
    total_var /= total;

    if total_var < f64::EPSILON {
        return None;
    }

    // Sweep all candidate thresholds, find the one that maximizes
    // between-class variance.
    let mut best_between_var = 0.0;
    let mut best_threshold = min_db;
    let mut w0 = 0.0; // weight of class 0 (below threshold)
    let mut sum0 = 0.0; // cumulative weighted sum for class 0

    for (i, &count) in histogram.iter().enumerate() {
        let center = bin_center(i, min_db);
        w0 += f64::from(count);
        sum0 += center * f64::from(count);

        let w1 = total - w0;
        if w0 < 1.0 || w1 < 1.0 {
            continue;
        }

        let mu0 = sum0 / w0;
        let mu1 = (total_mean * total - sum0) / w1;
        let between_var = (w0 / total) * (w1 / total) * (mu0 - mu1) * (mu0 - mu1);

        if between_var > best_between_var {
            best_between_var = between_var;
            // Threshold is the upper edge of this bin.
            best_threshold = min_db + (i as f64 + 1.0) * BIN_WIDTH;
        }
    }

    let bimodality = best_between_var / total_var;
    Some((best_threshold, bimodality))
}

/// Center of histogram bin `i` given the minimum value.
fn bin_center(i: usize, min_db: f64) -> f64 {
    min_db + (i as f64 + 0.5) * BIN_WIDTH
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: generate `n` samples from a normal-ish distribution using a
    /// simple deterministic CLT approximation (no rand crate needed).
    ///
    /// Sum of 4 uniform values → approximately normal via Central Limit
    /// Theorem. `spread` is used as the standard deviation.
    fn pseudo_normal(center: f32, stddev: f32, n: usize, seed: u32) -> Vec<f32> {
        let mut samples = Vec::with_capacity(n);
        let mut state = seed;
        for _ in 0..n {
            // Sum 4 xorshift32 uniform [0, 1) values.
            // Mean of sum = 2.0, variance = 4/12 = 1/3, stddev ≈ 0.5774.
            let mut sum = 0.0f32;
            for _ in 0..4 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                sum += state as f32 / u32::MAX as f32;
            }
            let z = (sum - 2.0) / 0.5774;
            samples.push(center + z * stddev);
        }
        samples
    }

    fn make_bimodal(
        noise_center: f32,
        noise_spread: f32,
        noise_count: usize,
        signal_center: f32,
        signal_spread: f32,
        signal_count: usize,
    ) -> Vec<f32> {
        let mut samples = pseudo_normal(noise_center, noise_spread, noise_count, 42);
        samples.extend(pseudo_normal(signal_center, signal_spread, signal_count, 123));
        samples
    }

    #[test]
    fn clear_bimodal_finds_threshold() {
        let samples = make_bimodal(-55.0, 2.0, 400, -35.0, 2.0, 200);
        let (thresh, bimodality) = otsu_threshold(&samples).unwrap();

        // Otsu split should be between the two modes.
        assert!(thresh > -52.0, "threshold {thresh} should be above noise mode");
        assert!(thresh < -38.0, "threshold {thresh} should be below signal mode");
        assert!(bimodality > BIMODALITY_MIN, "should be bimodal: {bimodality}");
    }

    #[test]
    fn close_modes_finds_threshold() {
        let samples = make_bimodal(-50.0, 1.0, 300, -44.0, 1.0, 300);
        let (thresh, bimodality) = otsu_threshold(&samples).unwrap();

        assert!(thresh > -50.0, "threshold {thresh} should be above noise");
        assert!(thresh < -44.0, "threshold {thresh} should be below signal");
        assert!(bimodality > BIMODALITY_MIN, "should be bimodal: {bimodality}");
    }

    #[test]
    fn unimodal_noise_only() {
        let samples = pseudo_normal(-55.0, 1.5, 200, 42);
        let result = otsu_threshold(&samples);

        // Either returns None (range too narrow) or has low bimodality.
        if let Some((_, bimodality)) = result {
            assert!(
                bimodality < BIMODALITY_MIN,
                "unimodal noise should have low bimodality: {bimodality}"
            );
        }
    }

    #[test]
    fn unimodal_signal_only() {
        let samples = pseudo_normal(-35.0, 1.5, 200, 99);
        let result = otsu_threshold(&samples);

        if let Some((_, bimodality)) = result {
            assert!(
                bimodality < BIMODALITY_MIN,
                "unimodal signal should have low bimodality: {bimodality}"
            );
        }
    }

    #[test]
    fn cold_start_returns_none() {
        let mut tracker = AdaptiveSquelch::new(-50.0);
        // Feed fewer than min_samples.
        for i in 0..DEFAULT_MIN_SAMPLES - 1 {
            assert!(
                tracker.feed(-55.0 + (i as f32) * 0.01).is_none(),
                "should return None during cold start"
            );
        }
    }

    #[test]
    fn hysteresis_suppresses_small_changes() {
        let mut tracker = AdaptiveSquelch::new(-50.0);

        // Feed a clear bimodal distribution to establish a threshold.
        let samples = make_bimodal(-55.0, 1.5, 400, -35.0, 1.5, 200);
        let mut last_update = None;
        for &s in &samples {
            if let Some(t) = tracker.feed(s) {
                last_update = Some(t);
            }
        }
        assert!(last_update.is_some(), "should have established a threshold");
        let established = tracker.current_threshold();

        // Feed the same distribution — no change expected.
        for &s in &samples[..100] {
            assert!(
                tracker.feed(s).is_none(),
                "stable distribution should not trigger update"
            );
        }
        assert!(
            (tracker.current_threshold() - established).abs() < f64::EPSILON,
            "threshold should not have changed"
        );
    }

    #[test]
    fn safety_clamp_limits_deviation() {
        let mut tracker = AdaptiveSquelch::new(-50.0);

        // Feed extreme values — adaptive should never go below configured.
        let samples = make_bimodal(-90.0, 1.0, 400, -30.0, 1.0, 200);
        let mut last_update = None;
        for &s in &samples {
            if let Some(t) = tracker.feed(s) {
                last_update = Some(t);
            }
        }

        if let Some(t) = last_update {
            assert!(
                t >= -50.0,
                "threshold {t} should be >= configured (never loosen)"
            );
            assert!(
                t <= -50.0 + CLAMP_DB,
                "threshold {t} should be <= configured + clamp"
            );
        }
    }

    #[test]
    fn asymmetric_classes_70_30() {
        // 70% noise, 30% signal — Otsu should still find the split.
        let samples = make_bimodal(-55.0, 1.5, 350, -35.0, 1.5, 150);
        let (thresh, bimodality) = otsu_threshold(&samples).unwrap();

        assert!(thresh > -52.0, "threshold {thresh} should be above noise");
        assert!(thresh < -38.0, "threshold {thresh} should be below signal");
        assert!(bimodality > BIMODALITY_MIN, "should be bimodal: {bimodality}");
    }

    #[test]
    fn long_transmission_50pct_signal() {
        // 50% signal — window dominated by transmission.
        let samples = make_bimodal(-55.0, 1.5, 300, -35.0, 1.5, 300);
        let (thresh, bimodality) = otsu_threshold(&samples).unwrap();

        assert!(thresh > -52.0, "threshold {thresh} should be above noise");
        assert!(thresh < -38.0, "threshold {thresh} should be below signal");
        assert!(bimodality > BIMODALITY_MIN, "should be bimodal: {bimodality}");
    }

    #[test]
    fn drift_tracking() {
        let mut tracker = AdaptiveSquelch::new(-50.0);

        // Phase 1: noise at -55, signal at -35.
        let phase1 = make_bimodal(-55.0, 1.5, 400, -35.0, 1.5, 200);
        for &s in &phase1 {
            tracker.feed(s);
        }
        let thresh1 = tracker.current_threshold();

        // Phase 2: noise floor drifts up to -48, signal stays at -35.
        // Fill the buffer with new data to age out old readings.
        let phase2 = make_bimodal(-48.0, 1.5, 500, -35.0, 1.5, 200);
        for &s in &phase2 {
            tracker.feed(s);
        }
        let thresh2 = tracker.current_threshold();

        assert!(
            thresh2 > thresh1,
            "threshold should track upward with noise drift: {thresh1} → {thresh2}"
        );
    }

    #[test]
    fn too_few_samples_returns_none() {
        assert!(otsu_threshold(&[]).is_none());
        assert!(otsu_threshold(&[-50.0]).is_none());
    }

    #[test]
    fn narrow_range_returns_none() {
        // All samples within one bin width.
        let samples: Vec<f32> = (0..100).map(|i| -50.0 + (i as f32) * 0.001).collect();
        assert!(otsu_threshold(&samples).is_none());
    }

    #[test]
    fn tracker_returns_threshold_after_min_samples() {
        let mut tracker = AdaptiveSquelch::new(-50.0);
        let samples = make_bimodal(-55.0, 1.5, 400, -35.0, 1.5, 200);
        let mut first_update = None;
        for (i, &s) in samples.iter().enumerate() {
            if let Some(t) = tracker.feed(s) {
                first_update = Some((i, t));
                break;
            }
        }
        let (idx, _) = first_update.expect("should produce an update");
        assert!(
            idx >= DEFAULT_MIN_SAMPLES - 1,
            "first update at {idx} should be >= min_samples"
        );
    }
}
