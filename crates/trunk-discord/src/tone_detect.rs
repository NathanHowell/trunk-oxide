use std::f32::consts::PI;
use std::time::Duration;

// Window/hop sizes
const WINDOW_MS: usize = 40;
const HOP_MS: usize = 20;
const HOP_S: f32 = 0.020;
const MILLIS_PER_SECOND: usize = 1000;
const NANOS_PER_SECOND: u128 = Duration::from_secs(1).as_nanos();

// Broadband frequency bank
const BANK_START_HZ: u16 = 250;
const BANK_END_HZ: u16 = 3500;
const BANK_STEP_HZ: u16 = 25;

// DTMF frequencies
const DTMF_LOW: [f32; 4] = [697.0, 770.0, 852.0, 941.0];
const DTMF_HIGH: [f32; 4] = [1209.0, 1336.0, 1477.0, 1633.0];
const DTMF_MAP: [[char; 4]; 4] = [
    ['1', '2', '3', 'A'],
    ['4', '5', '6', 'B'],
    ['7', '8', '9', 'C'],
    ['*', '0', '#', 'D'],
];

// Standard EIA CTCSS/PL tones (67.0–254.1 Hz)
const CTCSS_FREQS: [f32; 38] = [
    67.0, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5, 107.2, 110.9,
    114.8, 118.8, 123.0, 127.3, 131.8, 136.5, 141.3, 146.2, 151.4, 156.7, 162.2, 167.9, 173.8,
    179.9, 186.2, 192.8, 203.5, 210.7, 218.1, 225.7, 233.6, 241.8, 250.3,
];

// CTCSS analysis uses longer windows for adequate frequency resolution
const CTCSS_WINDOW_MS: usize = 200; // 200ms → 5 Hz bin spacing at 16 kHz
const CTCSS_HOP_MS: usize = 100; // 100ms hop

// CTCSS detection thresholds
const CTCSS_PEAK_TO_NOISE_RATIO: f32 = 8.0;
const CTCSS_DOMINANCE_NUMERATOR: usize = 1;
const CTCSS_DOMINANCE_DENOMINATOR: usize = 2; // > 1/2 means "dominant"
// Detection thresholds
const FREQ_TOLERANCE_HZ: f32 = 50.0;
const PEAK_TO_MEAN_RATIO: f32 = 10.0;
const DTMF_THRESHOLD_RATIO: f32 = 5.0;
const DTMF_MIN_WINDOWS: usize = 2;
const WARBLE_MIN_TRANSITIONS: usize = 4;
const WARBLE_MIN_TRANSITIONS_PER_SEC: usize = 4;

// Streaming detection constants
const EARLY_EXIT_WINDOWS: usize = 25; // 500ms at 20ms/hop — no tones seen → done
const SILENCE_GAP_WINDOWS: usize = 10; // 200ms consecutive silence after tones → done
const CTCSS_EARLY_FIRE_WINDOWS: usize = 5; // 5 consecutive agreeing CTCSS windows → fire

const fn inclusive_steps(start: u16, end: u16, step: u16) -> u16 {
    ((end - start) / step) + 1
}

const BANK_BINS: u16 = inclusive_steps(BANK_START_HZ, BANK_END_HZ, BANK_STEP_HZ);

fn mean(values: &[f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    let (sum, count) = values.iter().fold((0.0_f32, 0.0_f32), |(sum, count), &v| {
        (sum + v, count + 1.0)
    });
    Some(sum / count)
}

fn windows_to_seconds(window_count: usize, hop_s: f32) -> f32 {
    let mut duration_s = 0.0_f32;
    for _ in 0..window_count {
        duration_s += hop_s;
    }
    duration_s
}

fn max_samples_for_duration(sample_rate: u32, max_duration_s: f32) -> usize {
    if !max_duration_s.is_finite() || max_duration_s <= 0.0 {
        return 0;
    }
    let duration = Duration::from_secs_f32(max_duration_s);
    let samples_u128 =
        u128::from(sample_rate).saturating_mul(duration.as_nanos()) / NANOS_PER_SECOND;
    usize::try_from(samples_u128).unwrap_or(usize::MAX)
}

// --- Public types ---

#[derive(Debug, Clone, PartialEq)]
pub enum DetectedTone {
    TwoTone {
        tone_a: f32,
        tone_b: f32,
        duration_a: f32,
        duration_b: f32,
    },
    LongTone {
        freq: f32,
        duration: f32,
    },
    HiLowWarble {
        freq_hi: f32,
        freq_lo: f32,
        duration: f32,
    },
    Dtmf {
        digits: Vec<char>,
    },
    Ctcss {
        freq: f32,
    },
}

pub struct ToneDetectorResult {
    pub tones: Vec<DetectedTone>,
    pub tone_end_sample: usize,
}

// --- Goertzel algorithm ---

#[allow(clippy::cast_precision_loss)]
fn goertzel_mag_sq(samples: &[f32], target_freq: f32, sample_rate: f32) -> f32 {
    let n = samples.len() as f32;
    let k = (target_freq * n / sample_rate).round();
    let omega = 2.0 * PI * k / n;
    let coeff = 2.0 * omega.cos();

    let mut s1 = 0.0_f32;
    let mut s2 = 0.0_f32;

    for &sample in samples {
        let s0 = sample + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }

    s1.mul_add(s1, s2.mul_add(s2, -(coeff * s1 * s2)))
}

// --- Window analysis ---

fn extract_dominant_freq(magnitudes: &[f32], bank_freqs: &[f32]) -> Option<f32> {
    let (peak_idx, &peak_mag) = magnitudes
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
        .unwrap();

    let mean_mag = mean(magnitudes)?;

    if mean_mag <= 0.0 || peak_mag < PEAK_TO_MEAN_RATIO * mean_mag {
        return None;
    }

    // Parabolic interpolation for sub-bin accuracy
    if peak_idx > 0 && peak_idx < magnitudes.len() - 1 {
        let m1 = magnitudes[peak_idx - 1];
        let m0 = magnitudes[peak_idx];
        let m2 = magnitudes[peak_idx + 1];
        let denom = 2.0f32.mul_add(-m0, m1) + m2;
        if denom.abs() > f32::EPSILON {
            let delta = 0.5 * (m1 - m2) / denom;
            return Some(bank_freqs[peak_idx] + delta * f32::from(BANK_STEP_HZ));
        }
    }

    Some(bank_freqs[peak_idx])
}

fn detect_dtmf_window(samples: &[f32], sample_rate: f32) -> Option<char> {
    let low_mags: [f32; 4] =
        std::array::from_fn(|i| goertzel_mag_sq(samples, DTMF_LOW[i], sample_rate));
    let high_mags: [f32; 4] =
        std::array::from_fn(|i| goertzel_mag_sq(samples, DTMF_HIGH[i], sample_rate));

    let (low_idx, low_max) = low_mags
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
        .map(|(i, &v)| (i, v))
        .unwrap();
    let (high_idx, high_max) = high_mags
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
        .map(|(i, &v)| (i, v))
        .unwrap();

    let low_others: f32 = low_mags
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != low_idx)
        .map(|(_, &m)| m)
        .sum::<f32>()
        / 3.0;
    let high_others: f32 = high_mags
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != high_idx)
        .map(|(_, &m)| m)
        .sum::<f32>()
        / 3.0;

    if low_others > 0.0
        && high_others > 0.0
        && low_max > DTMF_THRESHOLD_RATIO * low_others
        && high_max > DTMF_THRESHOLD_RATIO * high_others
    {
        let ratio = low_max / high_max;
        if ratio > 0.1 && ratio < 10.0 {
            return Some(DTMF_MAP[low_idx][high_idx]);
        }
    }

    None
}

fn detect_ctcss_window(samples: &[f32], sample_rate: f32) -> Option<usize> {
    let mags: Vec<f32> = CTCSS_FREQS
        .iter()
        .map(|&f| goertzel_mag_sq(samples, f, sample_rate))
        .collect();

    let (peak_idx, &peak_mag) = mags
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
        .unwrap();

    let mean_mag = mean(&mags)?;

    if mean_mag > 0.0 && peak_mag > CTCSS_PEAK_TO_NOISE_RATIO * mean_mag {
        Some(peak_idx)
    } else {
        None
    }
}

// --- Segment extraction ---

struct ToneSegment {
    freq: f32,
    start_window: usize,
    end_window: usize, // exclusive
}

impl ToneSegment {
    fn duration_s(&self, hop_s: f32) -> f32 {
        windows_to_seconds(self.end_window - self.start_window, hop_s)
    }
}

fn extract_segments(window_freqs: &[Option<f32>]) -> Vec<ToneSegment> {
    let mut segments = Vec::new();
    let mut i = 0;

    while i < window_freqs.len() {
        if let Some(freq) = window_freqs[i] {
            let start = i;
            let mut sum = freq;
            let mut count_f32 = 1.0_f32;
            i += 1;

            while i < window_freqs.len() {
                if let Some(f) = window_freqs[i] {
                    if (f - sum / count_f32).abs() < FREQ_TOLERANCE_HZ {
                        sum += f;
                        count_f32 += 1.0;
                        i += 1;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }

            segments.push(ToneSegment {
                freq: sum / count_f32,
                start_window: start,
                end_window: i,
            });
        } else {
            i += 1;
        }
    }

    segments
}

// --- Detection logic ---

fn detect_two_tone(segments: &[ToneSegment], hop_s: f32) -> Option<(DetectedTone, usize)> {
    for pair in segments.windows(2) {
        let a = &pair[0];
        let b = &pair[1];

        if b.start_window != a.end_window {
            continue;
        }

        let dur_a = a.duration_s(hop_s);
        let dur_b = b.duration_s(hop_s);

        if (a.freq - b.freq).abs() > FREQ_TOLERANCE_HZ
            && (0.5..=2.0).contains(&dur_a)
            && (1.0..=5.0).contains(&dur_b)
        {
            return Some((
                DetectedTone::TwoTone {
                    tone_a: a.freq,
                    tone_b: b.freq,
                    duration_a: dur_a,
                    duration_b: dur_b,
                },
                b.end_window,
            ));
        }
    }
    None
}

fn detect_long_tone(segments: &[ToneSegment], hop_s: f32) -> Option<(DetectedTone, usize)> {
    for seg in segments {
        let dur = seg.duration_s(hop_s);
        if dur > 2.0 {
            return Some((
                DetectedTone::LongTone {
                    freq: seg.freq,
                    duration: dur,
                },
                seg.end_window,
            ));
        }
    }
    None
}

fn detect_warble(window_freqs: &[Option<f32>], hop_s: f32) -> Option<(DetectedTone, usize)> {
    let tone_windows: Vec<(usize, f32)> = window_freqs
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.map(|freq| (i, freq)))
        .collect();

    if tone_windows.len() < 4 {
        return None;
    }

    let mut transitions = 0usize;
    let freq_a = tone_windows[0].1;
    let mut freq_b: Option<f32> = None;
    let start = tone_windows[0].0;
    let mut end = tone_windows[0].0;
    let mut prev_is_a = true;

    for &(idx, freq) in &tone_windows[1..] {
        if (freq - freq_a).abs() < FREQ_TOLERANCE_HZ {
            if !prev_is_a {
                transitions += 1;
                prev_is_a = true;
            }
            end = idx;
        } else if let Some(fb) = freq_b {
            if (freq - fb).abs() < FREQ_TOLERANCE_HZ {
                if prev_is_a {
                    transitions += 1;
                    prev_is_a = false;
                }
                end = idx;
            } else {
                break; // third frequency
            }
        } else {
            freq_b = Some(freq);
            transitions += 1;
            prev_is_a = false;
            end = idx;
        }
    }

    let fb = freq_b?;
    let duration_windows = end - start + 1;
    let duration_s = windows_to_seconds(duration_windows, hop_s);
    if duration_s <= 0.0 {
        return None;
    }

    // Compare transitions/window against threshold without float casts.
    let transitions_fast_enough = transitions.saturating_mul(MILLIS_PER_SECOND)
        > duration_windows.saturating_mul(HOP_MS.saturating_mul(WARBLE_MIN_TRANSITIONS_PER_SEC));
    if transitions_fast_enough && transitions >= WARBLE_MIN_TRANSITIONS {
        let (hi, lo) = if freq_a > fb {
            (freq_a, fb)
        } else {
            (fb, freq_a)
        };
        Some((
            DetectedTone::HiLowWarble {
                freq_hi: hi,
                freq_lo: lo,
                duration: duration_s,
            },
            end + 1,
        ))
    } else {
        None
    }
}

fn detect_dtmf_digits(window_digits: &[Option<char>]) -> Option<(DetectedTone, usize)> {
    let mut digits = Vec::new();
    let mut i = 0;
    let mut last_end = 0;

    while i < window_digits.len() {
        if let Some(digit) = window_digits[i] {
            let start = i;
            i += 1;
            while i < window_digits.len() && window_digits[i] == Some(digit) {
                i += 1;
            }
            if i - start >= DTMF_MIN_WINDOWS {
                digits.push(digit);
                last_end = i;
            }
        } else {
            i += 1;
        }
    }

    if digits.is_empty() {
        None
    } else {
        Some((DetectedTone::Dtmf { digits }, last_end))
    }
}

// --- Streaming state machine ---

enum StreamingState {
    /// Watching for first evidence of any tone (first 500ms).
    EarlyWatch,
    /// At least one tone window seen; watching for a silence gap.
    ToneActive { consecutive_silent: usize },
    /// Detection finished (early-exit, silence gap, or `max_samples`).
    Done,
}

// --- ToneDetector ---

pub struct ToneDetector {
    sample_rate: u32,
    window_size: usize,
    hop_size: usize,
    max_samples: usize,
    samples_fed: usize,
    pending: Vec<f32>,
    bank_freqs: Vec<f32>,
    window_freqs: Vec<Option<f32>>,
    window_dtmf: Vec<Option<char>>,
    // CTCSS uses longer windows (200ms) for adequate frequency resolution
    ctcss_pending: Vec<f32>,
    ctcss_window_size: usize,
    ctcss_hop_size: usize,
    window_ctcss: Vec<Option<usize>>, // index into CTCSS_FREQS
    detect_ctcss: bool,
    complete: bool,
    // Streaming state
    streaming_state: StreamingState,
    pending_ctcss: Option<f32>,
    ctcss_fired: bool,
    ctcss_consecutive_agreeing: usize,
    ctcss_last_index: Option<usize>,
}

impl ToneDetector {
    pub fn new(sample_rate: u32, max_duration_s: f32) -> Self {
        Self::with_ctcss(sample_rate, max_duration_s, false)
    }

    pub fn with_ctcss(sample_rate: u32, max_duration_s: f32, detect_ctcss: bool) -> Self {
        let sample_rate_usize = usize::try_from(sample_rate).unwrap_or(usize::MAX);
        let window_size = sample_rate_usize.saturating_mul(WINDOW_MS) / MILLIS_PER_SECOND;
        let hop_size = sample_rate_usize.saturating_mul(HOP_MS) / MILLIS_PER_SECOND;
        let max_samples = max_samples_for_duration(sample_rate, max_duration_s);

        let mut bank_freqs = Vec::with_capacity(usize::from(BANK_BINS));
        let mut freq = f32::from(BANK_START_HZ);
        for _ in 0..BANK_BINS {
            bank_freqs.push(freq);
            freq += f32::from(BANK_STEP_HZ);
        }

        let ctcss_window_size =
            sample_rate_usize.saturating_mul(CTCSS_WINDOW_MS) / MILLIS_PER_SECOND;
        let ctcss_hop_size = sample_rate_usize.saturating_mul(CTCSS_HOP_MS) / MILLIS_PER_SECOND;

        Self {
            sample_rate,
            window_size,
            hop_size,
            max_samples,
            samples_fed: 0,
            pending: Vec::new(),
            bank_freqs,
            window_freqs: Vec::new(),
            window_dtmf: Vec::new(),
            ctcss_pending: Vec::new(),
            ctcss_window_size,
            ctcss_hop_size,
            window_ctcss: Vec::new(),
            detect_ctcss,
            complete: false,
            streaming_state: StreamingState::EarlyWatch,
            pending_ctcss: None,
            ctcss_fired: false,
            ctcss_consecutive_agreeing: 0,
            ctcss_last_index: None,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn process(&mut self, samples: &[f32]) {
        if self.complete {
            return;
        }

        self.pending.extend_from_slice(samples);
        self.samples_fed += samples.len();

        let sr = self.sample_rate as f32;
        while self.pending.len() >= self.window_size {
            let window = &self.pending[..self.window_size];

            let magnitudes: Vec<f32> = self
                .bank_freqs
                .iter()
                .map(|&f| goertzel_mag_sq(window, f, sr))
                .collect();
            self.window_freqs
                .push(extract_dominant_freq(&magnitudes, &self.bank_freqs));
            self.window_dtmf.push(detect_dtmf_window(window, sr));

            let hop = self.hop_size;
            self.pending.drain(..hop);

            self.update_streaming_state();
            if self.complete {
                return;
            }
        }

        // CTCSS analysis with longer (200ms) windows
        if self.detect_ctcss {
            self.ctcss_pending.extend_from_slice(samples);
            while self.ctcss_pending.len() >= self.ctcss_window_size {
                let window = &self.ctcss_pending[..self.ctcss_window_size];
                self.window_ctcss.push(detect_ctcss_window(window, sr));
                let hop = self.ctcss_hop_size;
                self.ctcss_pending.drain(..hop);

                self.update_ctcss_early_fire();
            }
        }

        if self.samples_fed >= self.max_samples {
            self.complete = true;
        }
    }

    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    /// Poll for a CTCSS early-fire event. Returns `Some(freq)` once, then `None` forever.
    pub const fn take_early_ctcss(&mut self) -> Option<f32> {
        self.pending_ctcss.take()
    }

    pub fn finish(self) -> ToneDetectorResult {
        detect_all_internal(
            &self.window_freqs,
            &self.window_dtmf,
            &self.window_ctcss,
            self.detect_ctcss,
            self.hop_size,
        )
    }

    fn update_streaming_state(&mut self) {
        if self.complete {
            return;
        }

        let last_has_tone = self.window_freqs.last().copied().flatten().is_some()
            || self.window_dtmf.last().copied().flatten().is_some();

        match &mut self.streaming_state {
            StreamingState::EarlyWatch => {
                if last_has_tone {
                    self.streaming_state = StreamingState::ToneActive {
                        consecutive_silent: 0,
                    };
                } else if self.window_freqs.len() >= EARLY_EXIT_WINDOWS {
                    self.complete = true;
                    self.streaming_state = StreamingState::Done;
                }
            }
            StreamingState::ToneActive {
                consecutive_silent,
            } => {
                if last_has_tone {
                    *consecutive_silent = 0;
                } else {
                    *consecutive_silent += 1;
                    if *consecutive_silent >= SILENCE_GAP_WINDOWS {
                        self.complete = true;
                        self.streaming_state = StreamingState::Done;
                    }
                }
            }
            StreamingState::Done => {}
        }
    }

    fn update_ctcss_early_fire(&mut self) {
        if !self.detect_ctcss || self.ctcss_fired {
            return;
        }
        if let Some(&Some(idx)) = self.window_ctcss.last() {
            if self.ctcss_last_index == Some(idx) {
                self.ctcss_consecutive_agreeing += 1;
            } else {
                self.ctcss_consecutive_agreeing = 1;
                self.ctcss_last_index = Some(idx);
            }
            if self.ctcss_consecutive_agreeing >= CTCSS_EARLY_FIRE_WINDOWS {
                self.ctcss_fired = true;
                self.pending_ctcss = Some(CTCSS_FREQS[idx]);
            }
        } else {
            self.ctcss_consecutive_agreeing = 0;
            self.ctcss_last_index = None;
        }
    }
}

fn detect_all_internal(
    window_freqs: &[Option<f32>],
    window_dtmf: &[Option<char>],
    window_ctcss: &[Option<usize>],
    detect_ctcss: bool,
    hop_size: usize,
) -> ToneDetectorResult {
    let hop_s = HOP_S;
    let segments = extract_segments(window_freqs);

    let mut tones = Vec::new();
    let mut tone_end_window = 0usize;

    if let Some((dtmf, end)) = detect_dtmf_digits(window_dtmf) {
        tones.push(dtmf);
        tone_end_window = tone_end_window.max(end);
    }

    if let Some((tt, end)) = detect_two_tone(&segments, hop_s) {
        tones.push(tt);
        tone_end_window = tone_end_window.max(end);
    }

    if let Some((lt, end)) = detect_long_tone(&segments, hop_s)
        && !tones
            .iter()
            .any(|t| matches!(t, DetectedTone::TwoTone { .. }))
    {
        tones.push(lt);
        tone_end_window = tone_end_window.max(end);
    }

    if let Some((w, end)) = detect_warble(window_freqs, hop_s) {
        tones.push(w);
        tone_end_window = tone_end_window.max(end);
    }

    // CTCSS aggregation: find the dominant CTCSS frequency across all windows
    if detect_ctcss && !window_ctcss.is_empty() {
        let mut counts = [0usize; 38];
        let mut present = 0usize;
        for w in window_ctcss.iter().flatten() {
            counts[*w] += 1;
            present += 1;
        }
        if present > 0 {
            let (best_idx, &best_count) =
                counts.iter().enumerate().max_by_key(|&(_, c)| *c).unwrap();
            if best_count.saturating_mul(CTCSS_DOMINANCE_DENOMINATOR)
                > present.saturating_mul(CTCSS_DOMINANCE_NUMERATOR)
            {
                tones.push(DetectedTone::Ctcss {
                    freq: CTCSS_FREQS[best_idx],
                });
            }
        }
    }

    let tone_end_sample = tone_end_window * hop_size;

    ToneDetectorResult {
        tones,
        tone_end_sample,
    }
}

impl std::fmt::Display for DetectedTone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TwoTone {
                tone_a,
                tone_b,
                duration_a,
                duration_b,
            } => write!(
                f,
                "Two-Tone: {tone_a:.0} Hz ({duration_a:.2}s) / {tone_b:.0} Hz ({duration_b:.2}s)",
            ),
            Self::LongTone { freq, duration } => {
                write!(f, "Long Tone: {freq:.0} Hz ({duration:.2}s)")
            }
            Self::HiLowWarble {
                freq_hi,
                freq_lo,
                duration,
            } => write!(
                f,
                "Hi-Low Warble: {freq_hi:.0}/{freq_lo:.0} Hz ({duration:.2}s)",
            ),
            Self::Dtmf { digits } => {
                write!(f, "DTMF: {}", digits.iter().collect::<String>())
            }
            Self::Ctcss { freq } => {
                write!(f, "CTCSS: {freq:.1} Hz")
            }
        }
    }
}

// --- Notch (band-reject) biquad filter ---

pub struct NotchFilter {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl NotchFilter {
    #[allow(clippy::cast_precision_loss)]
    pub fn new(sample_rate: u32, center_freq: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * center_freq / sample_rate as f32;
        let alpha = w0.sin() / (2.0 * q);

        let b0 = 1.0;
        let b1 = -2.0 * w0.cos();
        let b2 = 1.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * w0.cos();
        let a2 = 1.0 - alpha;

        // Normalize by a0
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for sample in samples.iter_mut() {
            let x0 = *sample;
            let y0 = self.b0.mul_add(
                x0,
                self.b1.mul_add(
                    self.x1,
                    self.b2
                        .mul_add(self.x2, (-self.a1).mul_add(self.y1, -self.a2 * self.y2)),
                ),
            );
            self.x2 = self.x1;
            self.x1 = x0;
            self.y2 = self.y1;
            self.y1 = y0;
            *sample = y0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_wave(freq: f32, sample_rate: u32, duration_s: f32, amplitude: f32) -> Vec<f32> {
        let n = (sample_rate as f32 * duration_s) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                amplitude * (2.0 * PI * freq * t).sin()
            })
            .collect()
    }

    fn dtmf_freqs(digit: char) -> (f32, f32) {
        let low = match digit {
            '1' | '2' | '3' | 'A' => 697.0,
            '4' | '5' | '6' | 'B' => 770.0,
            '7' | '8' | '9' | 'C' => 852.0,
            '*' | '0' | '#' | 'D' => 941.0,
            _ => panic!("invalid DTMF digit"),
        };
        let high = match digit {
            '1' | '4' | '7' | '*' => 1209.0,
            '2' | '5' | '8' | '0' => 1336.0,
            '3' | '6' | '9' | '#' => 1477.0,
            'A' | 'B' | 'C' | 'D' => 1633.0,
            _ => panic!("invalid DTMF digit"),
        };
        (low, high)
    }

    fn dtmf_tone(digit: char, sample_rate: u32, duration_s: f32) -> Vec<f32> {
        let (low, high) = dtmf_freqs(digit);
        let n = (sample_rate as f32 * duration_s) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                0.5 * (2.0 * PI * low * t).sin() + 0.5 * (2.0 * PI * high * t).sin()
            })
            .collect()
    }

    fn warble_signal(
        freq_a: f32,
        freq_b: f32,
        switch_interval_s: f32,
        sample_rate: u32,
        duration_s: f32,
    ) -> Vec<f32> {
        let n = (sample_rate as f32 * duration_s) as usize;
        let switch_samples = (sample_rate as f32 * switch_interval_s) as usize;
        let mut phase = 0.0_f32;
        (0..n)
            .map(|i| {
                let freq = if (i / switch_samples).is_multiple_of(2) {
                    freq_a
                } else {
                    freq_b
                };
                let sample = phase.sin();
                phase += 2.0 * PI * freq / sample_rate as f32;
                sample
            })
            .collect()
    }

    #[test]
    fn goertzel_accuracy() {
        let sr = 16000;
        let samples = sine_wave(1000.0, sr, 0.040, 1.0);

        let mag_on = goertzel_mag_sq(&samples, 1000.0, sr as f32);
        let mag_off = goertzel_mag_sq(&samples, 500.0, sr as f32);

        assert!(
            mag_on > mag_off * 100.0,
            "on-frequency magnitude ({}) should vastly exceed off-frequency ({})",
            mag_on,
            mag_off
        );
    }

    #[test]
    fn two_tone_detection() {
        let sr = 16000;
        let mut signal = sine_wave(1000.0, sr, 1.0, 0.8);
        signal.extend(sine_wave(850.0, sr, 3.0, 0.8));

        let mut det = ToneDetector::new(sr, 5.0);
        det.process(&signal);
        let result = det.finish();

        let tt = result
            .tones
            .iter()
            .find(|t| matches!(t, DetectedTone::TwoTone { .. }));
        assert!(
            tt.is_some(),
            "should detect two-tone, got: {:?}",
            result.tones
        );

        if let Some(DetectedTone::TwoTone { tone_a, tone_b, .. }) = tt {
            assert!(
                (*tone_a - 1000.0).abs() < 30.0,
                "tone A should be ~1000 Hz, got {}",
                tone_a
            );
            assert!(
                (*tone_b - 850.0).abs() < 30.0,
                "tone B should be ~850 Hz, got {}",
                tone_b
            );
        }
        assert!(result.tone_end_sample > 0);
    }

    #[test]
    fn long_tone_detection() {
        let sr = 16000;
        let signal = sine_wave(1050.0, sr, 4.0, 0.8);

        let mut det = ToneDetector::new(sr, 5.0);
        det.process(&signal);
        let result = det.finish();

        let lt = result
            .tones
            .iter()
            .find(|t| matches!(t, DetectedTone::LongTone { .. }));
        assert!(
            lt.is_some(),
            "should detect long tone, got: {:?}",
            result.tones
        );

        if let Some(DetectedTone::LongTone { freq, duration }) = lt {
            assert!(
                (*freq - 1050.0).abs() < 30.0,
                "freq should be ~1050 Hz, got {}",
                freq
            );
            assert!(*duration > 2.0, "duration should be >2s, got {}", duration);
        }
    }

    #[test]
    fn hi_low_warble_detection() {
        let sr = 16000;
        // 100ms per tone = 10 transitions/s
        let signal = warble_signal(1000.0, 800.0, 0.1, sr, 2.0);

        let mut det = ToneDetector::new(sr, 5.0);
        det.process(&signal);
        let result = det.finish();

        let w = result
            .tones
            .iter()
            .find(|t| matches!(t, DetectedTone::HiLowWarble { .. }));
        assert!(w.is_some(), "should detect warble, got: {:?}", result.tones);

        if let Some(DetectedTone::HiLowWarble {
            freq_hi, freq_lo, ..
        }) = w
        {
            assert!(
                (*freq_hi - 1000.0).abs() < 50.0,
                "hi freq should be ~1000, got {}",
                freq_hi
            );
            assert!(
                (*freq_lo - 800.0).abs() < 50.0,
                "lo freq should be ~800, got {}",
                freq_lo
            );
        }
    }

    #[test]
    fn dtmf_single_digits() {
        let sr = 16000;
        for digit in "1234567890*#ABCD".chars() {
            let signal = dtmf_tone(digit, sr, 0.2);

            let mut det = ToneDetector::new(sr, 1.0);
            det.process(&signal);
            let result = det.finish();

            let dtmf = result
                .tones
                .iter()
                .find(|t| matches!(t, DetectedTone::Dtmf { .. }));
            assert!(
                dtmf.is_some(),
                "should detect DTMF for '{}', got: {:?}",
                digit,
                result.tones
            );

            if let Some(DetectedTone::Dtmf { digits }) = dtmf {
                assert!(
                    digits.contains(&digit),
                    "DTMF for '{}' should decode correctly, got {:?}",
                    digit,
                    digits
                );
            }
        }
    }

    #[test]
    fn dtmf_sequence() {
        let sr = 16000;
        let mut signal = Vec::new();
        let gap = vec![0.0_f32; (sr as f32 * 0.1) as usize];

        for digit in ['5', '2', '3', '#'] {
            signal.extend(dtmf_tone(digit, sr, 0.15));
            signal.extend(&gap);
        }

        let mut det = ToneDetector::new(sr, 5.0);
        det.process(&signal);
        let result = det.finish();

        let dtmf = result
            .tones
            .iter()
            .find(|t| matches!(t, DetectedTone::Dtmf { .. }));
        assert!(dtmf.is_some(), "should detect DTMF sequence");

        if let Some(DetectedTone::Dtmf { digits }) = dtmf {
            assert_eq!(digits, &['5', '2', '3', '#'], "got {:?}", digits);
        }
    }

    #[test]
    fn silence_no_false_positives() {
        let sr = 16000;
        let signal = vec![0.0_f32; sr as usize * 5];

        let mut det = ToneDetector::new(sr, 5.0);
        det.process(&signal);
        let result = det.finish();

        assert!(
            result.tones.is_empty(),
            "silence should produce no tones, got: {:?}",
            result.tones
        );
        assert_eq!(result.tone_end_sample, 0);
    }

    #[test]
    fn short_call_graceful() {
        let sr = 16000;
        // 200ms of tone - way under the 5s detection window
        let signal = sine_wave(500.0, sr, 0.2, 0.8);

        let mut det = ToneDetector::new(sr, 5.0);
        det.process(&signal);
        assert!(!det.is_complete());

        // Force finish on short call
        let result = det.finish();
        // Should not panic; may or may not detect anything
        let _ = result;
    }

    #[test]
    fn ctcss_detection() {
        let sr = 16000;
        // 5s of 118.8 Hz CTCSS (low amplitude, ~15% of voice level)
        // mixed with voice-band content (broadband)
        let n = (sr as f32 * 5.0) as usize;
        let signal: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let ctcss = 0.15 * (2.0 * PI * 118.8 * t).sin();
                // Simulate voice with multiple frequencies above 250 Hz
                let voice = 0.3 * (2.0 * PI * 350.0 * t).sin()
                    + 0.2 * (2.0 * PI * 800.0 * t).sin()
                    + 0.1 * (2.0 * PI * 1200.0 * t).sin();
                ctcss + voice
            })
            .collect();

        let mut det = ToneDetector::with_ctcss(sr, 5.0, true);
        det.process(&signal);
        let result = det.finish();

        let ctcss = result
            .tones
            .iter()
            .find(|t| matches!(t, DetectedTone::Ctcss { .. }));
        assert!(
            ctcss.is_some(),
            "should detect CTCSS, got: {:?}",
            result.tones
        );

        if let Some(DetectedTone::Ctcss { freq }) = ctcss {
            assert_eq!(*freq, 118.8, "should detect 118.8 Hz CTCSS, got {}", freq);
        }
    }

    #[test]
    fn ctcss_with_dispatch_tones() {
        let sr = 16000;
        // 1s tone A + 3s tone B (two-tone dispatch) with CTCSS underneath
        let n = (sr as f32 * 5.0) as usize;
        let signal: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let ctcss = 0.15 * (2.0 * PI * 162.2 * t).sin();
                let dispatch = if t < 1.0 {
                    0.8 * (2.0 * PI * 1000.0 * t).sin()
                } else if t < 4.0 {
                    0.8 * (2.0 * PI * 850.0 * t).sin()
                } else {
                    0.0
                };
                ctcss + dispatch
            })
            .collect();

        let mut det = ToneDetector::with_ctcss(sr, 5.0, true);
        det.process(&signal);
        let result = det.finish();

        let has_two_tone = result
            .tones
            .iter()
            .any(|t| matches!(t, DetectedTone::TwoTone { .. }));
        let has_ctcss = result
            .tones
            .iter()
            .any(|t| matches!(t, DetectedTone::Ctcss { .. }));

        assert!(
            has_two_tone,
            "should detect two-tone, got: {:?}",
            result.tones
        );
        assert!(has_ctcss, "should detect CTCSS, got: {:?}", result.tones);

        if let Some(DetectedTone::Ctcss { freq }) = result
            .tones
            .iter()
            .find(|t| matches!(t, DetectedTone::Ctcss { .. }))
        {
            assert_eq!(*freq, 162.2, "should detect 162.2 Hz CTCSS, got {}", freq);
        }
    }

    #[test]
    fn notch_filter_attenuation() {
        let sr = 16000;
        let ctcss_freq = 118.8_f32;
        let n = (sr as f32 * 2.0) as usize;
        // Skip filter startup transient when measuring
        let skip = (sr as f32 * 0.2) as usize;

        // Pure CTCSS tone
        let mut signal: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (2.0 * PI * ctcss_freq * t).sin()
            })
            .collect();

        // Measure power before filtering (steady-state region only)
        let power_before: f32 =
            signal[skip..].iter().map(|s| s * s).sum::<f32>() / (n - skip) as f32;

        let mut filter = NotchFilter::new(sr, ctcss_freq, 30.0);
        filter.process(&mut signal);

        // Measure power after filtering (skip transient)
        let power_after: f32 =
            signal[skip..].iter().map(|s| s * s).sum::<f32>() / (n - skip) as f32;

        let attenuation_db = 10.0 * (power_after / power_before).log10();
        assert!(
            attenuation_db < -20.0,
            "notch should attenuate CTCSS by >20 dB, got {:.1} dB",
            attenuation_db
        );

        // Nearby frequencies should pass through
        for offset in [50.0_f32, -50.0] {
            let nearby_freq = ctcss_freq + offset;
            if nearby_freq <= 0.0 {
                continue;
            }
            let mut nearby: Vec<f32> = (0..n)
                .map(|i| {
                    let t = i as f32 / sr as f32;
                    (2.0 * PI * nearby_freq * t).sin()
                })
                .collect();
            let power_before: f32 = nearby.iter().map(|s| s * s).sum::<f32>() / n as f32;

            let mut filter = NotchFilter::new(sr, ctcss_freq, 30.0);
            filter.process(&mut nearby);

            let power_after: f32 = nearby.iter().map(|s| s * s).sum::<f32>() / n as f32;
            let attenuation_db = 10.0 * (power_after / power_before).log10();
            assert!(
                attenuation_db > -3.0,
                "nearby freq {:.1} Hz should pass (<3 dB loss), got {:.1} dB",
                nearby_freq,
                attenuation_db
            );
        }
    }

    #[test]
    fn tone_then_voice() {
        let sr = 16000;
        // 3s of long tone, then 2s of pseudo-noise
        let mut signal = sine_wave(1050.0, sr, 3.0, 0.8);

        // Generate deterministic noise-like signal (sum of many inharmonic frequencies)
        let noise_len = (sr as f32 * 2.0) as usize;
        let noise: Vec<f32> = (0..noise_len)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let mut s = 0.0_f32;
                for &f in &[
                    173.0, 311.0, 479.0, 613.0, 757.0, 919.0, 1061.0, 1223.0, 1399.0, 1567.0,
                    1741.0, 1933.0, 2111.0, 2309.0, 2503.0,
                ] {
                    s += (2.0 * PI * f * t).sin();
                }
                s / 15.0
            })
            .collect();
        signal.extend(noise);

        let mut det = ToneDetector::new(sr, 5.0);
        det.process(&signal);
        let result = det.finish();

        assert!(
            result.tone_end_sample > 0,
            "should identify tone region end"
        );
        // tone_end_sample should be roughly at the 3s mark (48000 samples)
        assert!(
            result.tone_end_sample > 40000 && result.tone_end_sample < 56000,
            "tone_end_sample ({}) should be near the 3s boundary",
            result.tone_end_sample
        );
    }

    // --- Streaming detection tests ---

    #[test]
    fn streaming_early_exit_no_tones() {
        let sr = 16000;
        // Feed 500ms of silence in small chunks
        let chunk_size = 320; // 20ms at 16kHz
        let total_samples = sr as usize / 2; // 500ms = 8000 samples
        let silence = vec![0.0_f32; chunk_size];

        let mut det = ToneDetector::new(sr, 5.0);
        let mut fed = 0;
        // Feed enough for 25 windows: first window needs 640 samples, then 320 per hop
        // 640 + 24*320 = 8320 samples minimum, round up to allow for buffering
        while fed < total_samples + chunk_size && !det.is_complete() {
            det.process(&silence);
            fed += chunk_size;
        }

        assert!(
            det.is_complete(),
            "should complete after ~500ms of silence (fed {} samples)",
            fed
        );
        // Should complete well before the 5s max (80000 samples)
        assert!(
            fed <= 12000,
            "should early-exit, not wait for max_samples (fed {} samples)",
            fed
        );

        let result = det.finish();
        assert!(result.tones.is_empty(), "no tones in silence");
        assert_eq!(result.tone_end_sample, 0);
    }

    #[test]
    fn streaming_completes_after_tone_gap() {
        let sr = 16000;
        // 1s tone A + 3s tone B (two-tone) + 200ms+ silence
        let mut signal = sine_wave(1000.0, sr, 1.0, 0.8);
        signal.extend(sine_wave(850.0, sr, 3.0, 0.8));
        signal.extend(vec![0.0_f32; sr as usize / 2]); // 500ms silence

        let chunk_size = 320;
        let mut det = ToneDetector::new(sr, 10.0); // long max to ensure streaming fires first
        let mut fed = 0;
        let mut completed_at = None;

        for chunk in signal.chunks(chunk_size) {
            det.process(chunk);
            fed += chunk.len();
            if det.is_complete() && completed_at.is_none() {
                completed_at = Some(fed);
                break;
            }
        }

        assert!(
            det.is_complete(),
            "should complete after silence gap"
        );
        // Should complete around 4s + 200ms gap = ~4.2s = ~67,200 samples
        let completed_at = completed_at.unwrap();
        assert!(
            completed_at < 75000,
            "should complete well before max_samples (completed at {} samples)",
            completed_at
        );

        let result = det.finish();
        let has_two_tone = result
            .tones
            .iter()
            .any(|t| matches!(t, DetectedTone::TwoTone { .. }));
        assert!(has_two_tone, "should detect two-tone, got: {:?}", result.tones);
    }

    #[test]
    fn streaming_no_complete_during_active_tone() {
        let sr = 16000;
        // 2s of continuous tone — should NOT complete (tone still active)
        let signal = sine_wave(1000.0, sr, 2.0, 0.8);

        let chunk_size = 320;
        let mut det = ToneDetector::new(sr, 10.0);

        for chunk in signal.chunks(chunk_size) {
            det.process(chunk);
        }

        assert!(
            !det.is_complete(),
            "should NOT complete while tone is still active"
        );
    }

    #[test]
    fn streaming_max_samples_fallback() {
        let sr = 16000;
        // Continuous tone for 6s with max_duration 5s
        let signal = sine_wave(1000.0, sr, 6.0, 0.8);

        let chunk_size = 320;
        let mut det = ToneDetector::new(sr, 5.0);

        for chunk in signal.chunks(chunk_size) {
            det.process(chunk);
        }

        assert!(
            det.is_complete(),
            "max_samples should still act as hard upper bound"
        );
    }

    #[test]
    fn streaming_ctcss_early_fire() {
        let sr = 16000;
        // 2s of CTCSS at 118.8 Hz mixed with voice-like content
        let n = sr as usize * 2;
        let signal: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let ctcss = 0.15 * (2.0 * PI * 118.8 * t).sin();
                let voice = 0.3 * (2.0 * PI * 350.0 * t).sin()
                    + 0.2 * (2.0 * PI * 800.0 * t).sin();
                ctcss + voice
            })
            .collect();

        let chunk_size = 320;
        let mut det = ToneDetector::with_ctcss(sr, 5.0, true);
        let mut early_ctcss = None;
        let mut early_at = 0;

        for chunk in signal.chunks(chunk_size) {
            det.process(chunk);
            early_at += chunk.len();
            if let Some(freq) = det.take_early_ctcss() {
                early_ctcss = Some(freq);
                break;
            }
        }

        assert!(
            early_ctcss.is_some(),
            "should early-fire CTCSS"
        );
        assert_eq!(
            early_ctcss.unwrap(),
            118.8,
            "should detect 118.8 Hz CTCSS"
        );
        // Should fire within ~1s (CTCSS needs 200ms windows, 5 consecutive = ~600ms minimum)
        assert!(
            early_at < sr as usize * 2,
            "should fire early, not wait for full duration (fired at {} samples)",
            early_at
        );
        // Subsequent calls should return None
        assert!(det.take_early_ctcss().is_none(), "should only fire once");
    }
}
