//! DTMF keys in an analog channel's audio.
//!
//! A key is one tone from each of two groups, sounded together:
//!
//! ```text
//!          1209  1336  1477  1633 Hz
//!   697       1     2     3     A
//!   770       4     5     6     B
//!   852       7     8     9     C
//!   941       *     0     #     D
//! ```
//!
//! What tells it from speech, which has energy at these frequencies all the
//! time, is that the pair is nearly *all* there is: a 40 ms window is called
//! a key only when one tone in each group stands well clear of the rest of
//! its group, the two hold most of the window's energy, their levels are
//! within the twist the standards allow and the high tone has no second
//! harmonic; and then only when consecutive windows agree. A vowel passes
//! the first of those and almost never the second.

use crate::biquad::HighPass;
use crate::goertzel::tone_power;
use crate::signalling::{Heard, Signal};
use crate::tap::AudioTap;

const LOW: [f64; 4] = [697.0, 770.0, 852.0, 941.0];
const HIGH: [f64; 4] = [1_209.0, 1_336.0, 1_477.0, 1_633.0];
const KEYS: [[char; 4]; 4] = [
    ['1', '2', '3', 'A'],
    ['4', '5', '6', 'B'],
    ['7', '8', '9', 'C'],
    ['*', '0', '#', 'D'],
];

/// Samples a decision is made over (40 ms: 25 Hz between bins, so the
/// 73 Hz between the two lowest tones is clear).
const WINDOW: usize = 640;
/// Samples between decisions (20 ms).
const HOP: usize = 320;
/// Consecutive decisions that must agree: with the window, a key of about
/// 60 ms or more.
const MIN_STEPS: usize = 2;
/// Below the corner of this high-pass is where a channel's CTCSS tone and
/// its rumble are; the lowest key tone is at 697 Hz.
const HIGH_PASS_HZ: f64 = 500.0;
/// A tone in a group must have this many times the power of the group's
/// next (about 8 dB).
const DOMINANCE: f64 = 6.0;
/// The two tones must hold this share of the window's power. A pure key
/// holds nearly all of it; speech never does.
const CONCENTRATION: f64 = 0.6;
/// The level of the high tone against the low, in dB, that is accepted:
/// the standards' 8 dB of twist the other way and 4 dB of reverse twist,
/// with 2 dB more each way for the radio between.
const TWIST_DB: (f64, f64) = (-10.0, 6.0);
/// The second harmonic of the high tone, against the tone.
const HARMONIC: f64 = 0.1;
/// Least amplitude of each tone (of a full-deviation signal's 1.0): quiet
/// hiss has to be no key.
const FLOOR: f64 = 0.02;

/// One run of windows that agree on a key.
#[derive(Clone, Copy)]
struct Run {
    key: char,
    /// Sample at which the first window of the run ended.
    first_end: u64,
    /// Sample at which the latest window of the run ended.
    last_end: u64,
    steps: usize,
}

/// Reads DTMF keys out of 16 kHz audio, whatever the block sizes it is fed in.
pub struct Dtmf {
    high_pass: HighPass,
    /// The last [`WINDOW`] samples, or fewer at the start.
    window: Vec<f32>,
    /// Samples fed since the last reset.
    n: u64,
    run: Option<Run>,
}

impl Default for Dtmf {
    fn default() -> Self {
        Self::new()
    }
}

/// How far from its nominal frequency a key's tone may be: the standards
/// allow 1.5 %.
const TOLERANCE: f64 = 0.015;

/// Power of a key's tone at `hz`, allowing for the tolerance of the sender.
fn key_tone_power(samples: &[f32], hz: f64) -> f64 {
    [1.0 - TOLERANCE, 1.0, 1.0 + TOLERANCE]
        .into_iter()
        .map(|scale| tone_power(samples, hz * scale))
        .fold(0.0, f64::max)
}

/// The strongest of `powers` and how many times it exceeds the next.
fn standout(powers: &[f64; 4]) -> (usize, f64, f64) {
    let (best, &top) = powers
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .expect("four tones");
    let next = powers
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != best)
        .map(|(_, &p)| p)
        .fold(0.0f64, f64::max);
    (best, top, top / next.max(f64::MIN_POSITIVE))
}

/// The key a window holds, if it holds one.
fn key_in(window: &[f32]) -> Option<char> {
    let low = LOW.map(|hz| key_tone_power(window, hz));
    let high = HIGH.map(|hz| key_tone_power(window, hz));
    let (l, low_power, low_standout) = standout(&low);
    let (h, high_power, high_standout) = standout(&high);
    let floor = FLOOR * FLOOR;
    if low_power < floor || high_power < floor {
        return None;
    }
    if low_standout < DOMINANCE || high_standout < DOMINANCE {
        return None;
    }
    // a sine of amplitude a has a mean square of a^2 / 2, so two tones of
    // the same amplitude hold all of 2 * mean square
    let total = window.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / window.len() as f64;
    if low_power + high_power < CONCENTRATION * 2.0 * total {
        return None;
    }
    let twist = 10.0 * (high_power / low_power).log10();
    if !(TWIST_DB.0..=TWIST_DB.1).contains(&twist) {
        return None;
    }
    if tone_power(window, 2.0 * HIGH[h]) > HARMONIC * high_power {
        return None;
    }
    Some(KEYS[l][h])
}

impl Dtmf {
    #[must_use]
    pub fn new() -> Self {
        Self {
            high_pass: HighPass::new(HIGH_PASS_HZ),
            window: Vec::with_capacity(WINDOW + HOP),
            n: 0,
            run: None,
        }
    }

    /// Forget the audio so far, as for a retuned channel. Spans count from here.
    pub fn reset(&mut self) {
        self.high_pass.reset();
        self.window.clear();
        self.n = 0;
        self.run = None;
    }

    /// Feeds `audio`, appending each key once it has ended.
    pub fn process(&mut self, audio: &[f32], out: &mut Vec<Heard>) {
        for &x in audio {
            let x = self.high_pass.run(f64::from(x)) as f32;
            if self.window.len() == WINDOW {
                self.window.remove(0);
            }
            self.window.push(x);
            self.n += 1;
            if self.n.is_multiple_of(HOP as u64) && self.window.len() == WINDOW {
                let key = key_in(&self.window);
                self.step(key, out);
            }
        }
    }

    /// One decision, made as the window ended at sample `self.n`.
    fn step(&mut self, key: Option<char>, out: &mut Vec<Heard>) {
        match (self.run, key) {
            (Some(run), Some(key)) if run.key == key => {
                self.run = Some(Run {
                    last_end: self.n,
                    steps: run.steps + 1,
                    ..run
                });
            }
            (run, key) => {
                if let Some(run) = run.filter(|run| run.steps >= MIN_STEPS) {
                    out.push(Heard {
                        span: run.first_end.saturating_sub(WINDOW as u64)..run.last_end,
                        signal: Signal::Dtmf(run.key),
                    });
                }
                self.run = key.map(|key| Run {
                    key,
                    first_end: self.n,
                    last_end: self.n,
                    steps: 1,
                });
            }
        }
    }
}

impl AudioTap for Dtmf {
    fn process(&mut self, audio: &[f32], out: &mut Vec<Heard>) {
        Self::process(self, audio, out);
    }

    fn reset(&mut self) {
        Self::reset(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AUDIO_RATE;

    fn tones(low: f64, high: f64, amplitude: f64, seconds: f64) -> Vec<f32> {
        (0..(seconds * AUDIO_RATE) as usize)
            .map(|n| {
                let t = n as f64 / AUDIO_RATE;
                (amplitude
                    * ((std::f64::consts::TAU * low * t).sin()
                        + (std::f64::consts::TAU * high * t).sin())
                    / 2.0) as f32
            })
            .collect()
    }

    fn key_tones(key: char, amplitude: f64, seconds: f64) -> Vec<f32> {
        let (l, h) = (0..4)
            .flat_map(|l| (0..4).map(move |h| (l, h)))
            .find(|&(l, h)| KEYS[l][h] == key)
            .unwrap();
        tones(LOW[l], HIGH[h], amplitude, seconds)
    }

    /// A xorshift hiss of the given peak.
    fn hiss(n: usize, peak: f32, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 2.0 * peak
            })
            .collect()
    }

    fn run(audio: &[f32], chunk: usize) -> Vec<Heard> {
        let mut decoder = Dtmf::new();
        let mut out = Vec::new();
        for block in audio.chunks(chunk) {
            decoder.process(block, &mut out);
        }
        // a key that runs to the end of the audio is closed by what follows
        decoder.process(&vec![0.0; 2 * WINDOW], &mut out);
        out
    }

    fn keys(heard: &[Heard]) -> String {
        heard
            .iter()
            .map(|h| match h.signal {
                Signal::Dtmf(k) => k,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    /// `keys` dialled with `tone` seconds of each and `gap` between, in hiss.
    fn dial(keys: &str, tone: f64, gap: f64, amplitude: f64) -> Vec<f32> {
        let silence = |s: f64| hiss((s * AUDIO_RATE) as usize, 0.003, 7);
        let mut audio = silence(0.2);
        for key in keys.chars() {
            let mut t = key_tones(key, amplitude, tone);
            for (x, h) in t.iter_mut().zip(hiss(t_len(tone), 0.003, 9)) {
                *x += h;
            }
            audio.extend(t);
            audio.extend(silence(gap));
        }
        audio.extend(silence(0.2));
        audio
    }

    fn t_len(seconds: f64) -> usize {
        (seconds * AUDIO_RATE) as usize
    }

    #[test]
    fn every_key_is_read() {
        for row in KEYS {
            for key in row {
                let heard = run(&dial(&key.to_string(), 0.1, 0.1, 0.3), 1_600);
                assert_eq!(keys(&heard), key.to_string(), "{key}");
            }
        }
    }

    #[test]
    fn a_number_is_read_in_order_with_its_repeats() {
        let heard = run(&dial("5551212#A", 0.07, 0.06, 0.3), 1_600);
        assert_eq!(keys(&heard), "5551212#A");
    }

    #[test]
    fn the_blocks_it_is_fed_in_make_no_difference() {
        let audio = dial("0123456789*#ABCD", 0.08, 0.08, 0.25);
        let whole = run(&audio, audio.len());
        assert_eq!(keys(&whole), "0123456789*#ABCD");
        for chunk in [1, 7, 160, 320, 1_000] {
            assert_eq!(run(&audio, chunk), whole, "{chunk}");
        }
    }

    #[test]
    fn it_reads_quiet_and_loud_keys() {
        for amplitude in [0.06, 0.15, 0.9] {
            assert_eq!(
                keys(&run(&dial("159", 0.1, 0.1, amplitude), 800)),
                "159",
                "{amplitude}"
            );
        }
    }

    #[test]
    fn the_span_covers_the_key_to_within_a_window() {
        let heard = run(&dial("7", 0.1, 0.1, 0.3), 1_600);
        let [one] = &heard[..] else {
            panic!("{heard:?}")
        };
        let (start, end) = (3_200u64, 3_200 + 1_600);
        assert!(
            one.span.start <= start && start - one.span.start <= 700,
            "{:?}",
            one.span
        );
        assert!(
            one.span.end >= end && one.span.end - end <= 700,
            "{:?}",
            one.span
        );
    }

    #[test]
    fn twist_within_what_a_radio_does_is_read_and_beyond_it_is_not() {
        // high tone 6 dB under the low one (a radio's roll-off), and 4 dB over it
        for (low_gain, high_gain) in [(1.0, 0.5), (0.63, 1.0)] {
            let audio: Vec<f32> = (0..t_len(0.12))
                .map(|n| {
                    let t = n as f64 / AUDIO_RATE;
                    (0.3 * (low_gain * (std::f64::consts::TAU * 770.0 * t).sin()
                        + high_gain * (std::f64::consts::TAU * 1_477.0 * t).sin()))
                        as f32
                })
                .collect();
            assert_eq!(keys(&run(&audio, 800)), "6", "{low_gain} {high_gain}");
        }
        for (low_gain, high_gain) in [(1.0, 0.1), (0.1, 1.0)] {
            let audio: Vec<f32> = (0..t_len(0.12))
                .map(|n| {
                    let t = n as f64 / AUDIO_RATE;
                    (0.3 * (low_gain * (std::f64::consts::TAU * 770.0 * t).sin()
                        + high_gain * (std::f64::consts::TAU * 1_477.0 * t).sin()))
                        as f32
                })
                .collect();
            assert_eq!(keys(&run(&audio, 800)), "", "{low_gain} {high_gain}");
        }
    }

    #[test]
    fn tones_that_are_not_a_key_are_not_read() {
        // a single tone, two tones of one group, two tones that are no key's
        for (a, b) in [
            (1_209.0, 1_209.0),
            (697.0, 770.0),
            (1_209.0, 1_336.0),
            (800.0, 1_400.0),
        ] {
            assert_eq!(keys(&run(&tones(a, b, 0.4, 0.3), 800)), "", "{a} {b}");
        }
    }

    #[test]
    fn a_key_with_speech_under_it_in_the_same_window_is_not_a_key() {
        // the pair is there, but under a third of the power is the pair's
        let mut audio = key_tones('5', 0.3, 0.3);
        for (x, h) in audio.iter_mut().zip(hiss(t_len(0.3), 0.4, 3)) {
            *x += h;
        }
        assert_eq!(keys(&run(&audio, 800)), "");
    }

    #[test]
    fn hiss_alone_is_no_key() {
        for peak in [0.01, 0.1, 0.5] {
            assert_eq!(
                keys(&run(&hiss(16_000 * 60, peak, 99), 1_600)),
                "",
                "{peak}"
            );
        }
    }

    /// Voiced speech, as far as a decoder cares: a pulse train through three
    /// resonators whose centres wander about the vowel space, which is
    /// where the key tones are.
    fn vowels(seconds: f64, pitch: f64, seed: u64) -> Vec<f32> {
        let mut state = seed;
        let mut rnd = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f64 / (1u64 << 24) as f64
        };
        let n = t_len(seconds);
        let mut formants = [700.0, 1_200.0, 2_400.0];
        let mut targets = formants;
        let mut y = [[0.0f64; 2]; 3];
        let mut out = Vec::with_capacity(n);
        let period = (AUDIO_RATE / pitch) as usize;
        for i in 0..n {
            if i % 1_600 == 0 {
                targets = [
                    300.0 + 700.0 * rnd(),
                    900.0 + 1_100.0 * rnd(),
                    2_200.0 + 800.0 * rnd(),
                ];
            }
            let excite = if i % period == 0 { 1.0 } else { 0.0 };
            let mut sum = 0.0;
            for (k, f) in formants.iter_mut().enumerate() {
                *f += (targets[k] - *f) * 0.002;
                let r = 0.97;
                let w = std::f64::consts::TAU * *f / AUDIO_RATE;
                let v = excite + 2.0 * r * w.cos() * y[k][0] - r * r * y[k][1];
                y[k][1] = y[k][0];
                y[k][0] = v;
                sum += v;
            }
            out.push(sum as f32);
        }
        let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        out.iter().map(|v| 0.4 * v / peak).collect()
    }

    #[test]
    fn vowels_are_not_keys() {
        for (pitch, seed) in [(100.0, 1), (130.0, 2), (180.0, 3), (220.0, 4), (95.0, 5)] {
            let heard = run(&vowels(60.0, pitch, seed), 1_600);
            assert_eq!(keys(&heard), "", "pitch {pitch}");
        }
    }

    #[test]
    fn a_reset_forgets_a_key_half_heard() {
        let audio = dial("8", 0.1, 0.1, 0.3);
        let mut decoder = Dtmf::new();
        let mut out = Vec::new();
        decoder.process(&audio[..4_200], &mut out);
        decoder.reset();
        decoder.process(&audio[4_200..], &mut out);
        decoder.process(&vec![0.0; 2 * WINDOW], &mut out);
        assert_eq!(keys(&out), "");
        assert_eq!(keys(&run(&audio, 800)), "8");
    }
}
