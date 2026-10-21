//! Tones that are not data: the paging tones that open a dispatch, a tone
//! held on the channel, a warble.
//!
//! A tone is told from speech and noise the way a DTMF key is: it must stand
//! well clear of the rest of the spectrum *and* hold most of the window's
//! energy. A run of windows with the same tone is a segment; segments that
//! follow one another make a chain, and a chain that ends is read as one of:
//!
//! - a **two-tone** page: a tone of half a second to two, straight into a
//!   different one of one to five (Quik-Call II, Motorola's sequential
//!   pages);
//! - a **warble**: two tones alternating, four or more changes and at least
//!   four a second;
//! - a **long tone**: any one tone held more than two seconds.
//!
//! None can be known until it ends, which is after the start of it has gone
//! into the recording; the span says where it was.

use crate::AUDIO_RATE;
use crate::biquad::HighPass;
use crate::goertzel::tone_power;
use crate::signalling::{Heard, Signal};
use crate::tap::AudioTap;

/// Samples a decision is made over (40 ms) and between decisions (20 ms).
const WINDOW: usize = 640;
const HOP: usize = 320;
/// Seconds a window adds to a tone, as the thresholds below count them.
const HOP_S: f64 = HOP as f64 / AUDIO_RATE;
/// Frequencies searched: 250 to 3500 Hz in 25 Hz steps.
const BANK_START_HZ: f64 = 250.0;
const BANK_END_HZ: f64 = 3_500.0;
const BANK_STEP_HZ: f64 = 25.0;
/// Below the corner of this high-pass is where a channel's CTCSS tone is.
const HIGH_PASS_HZ: f64 = 280.0;
/// The peak of the bank must be this many times its mean.
const PEAK_TO_MEAN: f64 = 10.0;
/// The tone must hold this share of the window's power: a pure tone holds
/// all of it, two equal tones (a DTMF key) half each, speech little.
const TONALITY: f64 = 0.6;
/// Least amplitude of a tone (of a full-deviation signal's 1.0).
const FLOOR: f64 = 0.02;
/// Two windows' tones are the same tone if they are within this many Hz.
const SAME_TONE_HZ: f64 = 50.0;
/// Windows a tone must last to be a segment.
const MIN_WINDOWS: usize = 2;
/// Samples a chain may pause for and go on: one missed window.
const MAX_GAP: u64 = 2 * HOP as u64;
/// Seconds of a two-tone page's first tone and its second.
const FIRST_TONE_S: (f64, f64) = (0.5, 2.0);
const SECOND_TONE_S: (f64, f64) = (1.0, 5.0);
/// Seconds past which one tone is a long tone.
const LONG_TONE_S: f64 = 2.0;
/// A warble's changes of tone, in all and per second.
const WARBLE_CHANGES: usize = 4;
const WARBLE_CHANGES_PER_S: f64 = 4.0;

/// A run of windows with one tone.
#[derive(Clone, Copy, Debug)]
struct Segment {
    /// Sample at which the first window ended, and the last.
    first_end: u64,
    last_end: u64,
    windows: usize,
    /// Sum of the windows' frequencies.
    sum_hz: f64,
}

impl Segment {
    fn hz(&self) -> f64 {
        self.sum_hz / self.windows as f64
    }

    fn seconds(&self) -> f64 {
        self.windows as f64 * HOP_S
    }

    fn start(&self) -> u64 {
        self.first_end.saturating_sub(WINDOW as u64)
    }
}

/// Reads dispatch tones out of 16 kHz audio, whatever the block sizes it is
/// fed in.
pub struct Tones {
    high_pass: HighPass,
    window: Vec<f32>,
    n: u64,
    /// The segment still going.
    current: Option<Segment>,
    /// Segments one after another that have ended.
    chain: Vec<Segment>,
}

impl Default for Tones {
    fn default() -> Self {
        Self::new()
    }
}

/// Tenths of a hertz, the resolution tones are specified to.
fn tenths(hz: f64) -> u32 {
    (hz * 10.0).round() as u32
}

/// The frequency within a bin's width of `hz` at which `window` holds the
/// most power: a golden-section search, good to a fraction of a hertz where
/// interpolating between the bank's bins is good to several.
fn refine(window: &[f32], hz: f64) -> f64 {
    const GOLDEN: f64 = 0.618_033_988_75;
    let (mut low, mut high) = (hz - BANK_STEP_HZ, hz + BANK_STEP_HZ);
    let mut a = high - GOLDEN * (high - low);
    let mut b = low + GOLDEN * (high - low);
    let (mut power_a, mut power_b) = (tone_power(window, a), tone_power(window, b));
    for _ in 0..20 {
        if power_a < power_b {
            low = a;
            (a, power_a) = (b, power_b);
            b = low + GOLDEN * (high - low);
            power_b = tone_power(window, b);
        } else {
            high = b;
            (b, power_b) = (a, power_a);
            a = high - GOLDEN * (high - low);
            power_a = tone_power(window, a);
        }
    }
    f64::midpoint(low, high)
}

/// The tone in a window, if it holds one.
fn tone_in(window: &[f32]) -> Option<f64> {
    let steps = ((BANK_END_HZ - BANK_START_HZ) / BANK_STEP_HZ) as usize + 1;
    let powers: Vec<f64> = (0..steps)
        .map(|i| tone_power(window, BANK_START_HZ + i as f64 * BANK_STEP_HZ))
        .collect();
    let (peak, &top) = powers
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))?;
    let mean = powers.iter().sum::<f64>() / powers.len() as f64;
    if top < FLOOR * FLOOR || top < PEAK_TO_MEAN * mean {
        return None;
    }
    let hz = refine(window, BANK_START_HZ + peak as f64 * BANK_STEP_HZ);
    let total = window.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / window.len() as f64;
    (tone_power(window, hz) >= TONALITY * 2.0 * total).then_some(hz)
}

impl Tones {
    #[must_use]
    pub fn new() -> Self {
        Self {
            high_pass: HighPass::new(HIGH_PASS_HZ),
            window: Vec::with_capacity(WINDOW + HOP),
            n: 0,
            current: None,
            chain: Vec::new(),
        }
    }

    /// Forget the audio so far, as for a retuned channel. Spans count from here.
    pub fn reset(&mut self) {
        self.high_pass.reset();
        self.window.clear();
        self.n = 0;
        self.current = None;
        self.chain.clear();
    }

    /// Feeds `audio`, appending each tone once it has ended.
    pub fn process(&mut self, audio: &[f32], out: &mut Vec<Heard>) {
        for &x in audio {
            let x = self.high_pass.run(f64::from(x)) as f32;
            if self.window.len() == WINDOW {
                self.window.remove(0);
            }
            self.window.push(x);
            self.n += 1;
            if self.n.is_multiple_of(HOP as u64) && self.window.len() == WINDOW {
                let tone = tone_in(&self.window);
                self.step(tone, out);
            }
        }
    }

    /// One decision, made as the window ended at sample `self.n`.
    fn step(&mut self, tone: Option<f64>, out: &mut Vec<Heard>) {
        match (self.current, tone) {
            (Some(seg), Some(hz)) if (hz - seg.hz()).abs() < SAME_TONE_HZ => {
                self.current = Some(Segment {
                    last_end: self.n,
                    windows: seg.windows + 1,
                    sum_hz: seg.sum_hz + hz,
                    ..seg
                });
            }
            (current, tone) => {
                if let Some(seg) = current.filter(|seg| seg.windows >= MIN_WINDOWS) {
                    self.chain.push(seg);
                }
                self.current = tone.map(|hz| Segment {
                    first_end: self.n,
                    last_end: self.n,
                    windows: 1,
                    sum_hz: hz,
                });
            }
        }
        // a chain is over when nothing has joined it for a gap
        if self.current.is_none()
            && self
                .chain
                .last()
                .is_some_and(|last| self.n - last.last_end > MAX_GAP)
        {
            self.read_chain(out);
        }
    }

    fn read_chain(&mut self, out: &mut Vec<Heard>) {
        let chain = std::mem::take(&mut self.chain);
        // break it where a gap was too long to go on across
        let mut links: Vec<&[Segment]> = Vec::new();
        let mut from = 0;
        for i in 1..chain.len() {
            if chain[i].first_end - chain[i - 1].last_end > MAX_GAP {
                links.push(&chain[from..i]);
                from = i;
            }
        }
        links.push(&chain[from..]);
        for link in links {
            if let Some(heard) = read_link(link) {
                out.extend(heard);
            }
        }
    }
}

/// What a run of adjacent segments is.
fn read_link(link: &[Segment]) -> Option<Vec<Heard>> {
    let (first, last) = (link.first()?, link.last()?);
    let span = first.start()..last.last_end;
    let heard = |signal| Heard {
        span: span.clone(),
        signal,
    };
    if let [a, b] = link
        && (a.hz() - b.hz()).abs() > SAME_TONE_HZ
        && (FIRST_TONE_S.0..=FIRST_TONE_S.1).contains(&a.seconds())
        && (SECOND_TONE_S.0..=SECOND_TONE_S.1).contains(&b.seconds())
    {
        return Some(vec![heard(Signal::TwoTone {
            first_dhz: tenths(a.hz()),
            second_dhz: tenths(b.hz()),
            first_ms: (a.seconds() * 1_000.0).round() as u32,
            second_ms: (b.seconds() * 1_000.0).round() as u32,
        })]);
    }
    if let Some(signal) = warble(link) {
        return Some(vec![heard(signal)]);
    }
    let long: Vec<Heard> = link
        .iter()
        .filter(|seg| seg.seconds() > LONG_TONE_S)
        .map(|seg| Heard {
            span: seg.start()..seg.last_end,
            signal: Signal::LongTone {
                dhz: tenths(seg.hz()),
                ms: (seg.seconds() * 1_000.0).round() as u32,
            },
        })
        .collect();
    (!long.is_empty()).then_some(long)
}

/// A link of two tones taking turns, fast.
fn warble(link: &[Segment]) -> Option<Signal> {
    let changes = link.len().checked_sub(1)?;
    let a = link.first()?.hz();
    let b = link
        .iter()
        .map(Segment::hz)
        .find(|hz| (hz - a).abs() > SAME_TONE_HZ)?;
    let turns = link.iter().enumerate().all(|(i, seg)| {
        let expected = if i % 2 == 0 { a } else { b };
        (seg.hz() - expected).abs() < SAME_TONE_HZ
    });
    let seconds = (link.last()?.last_end - link.first()?.start()) as f64 / AUDIO_RATE;
    (turns && changes >= WARBLE_CHANGES && changes as f64 / seconds >= WARBLE_CHANGES_PER_S).then(
        || Signal::Warble {
            high_dhz: tenths(a.max(b)),
            low_dhz: tenths(a.min(b)),
            ms: (seconds * 1_000.0).round() as u32,
        },
    )
}

impl AudioTap for Tones {
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

    fn sine(hz: f64, amplitude: f64, seconds: f64) -> Vec<f32> {
        (0..(seconds * AUDIO_RATE) as usize)
            .map(|n| {
                (amplitude * (std::f64::consts::TAU * hz * n as f64 / AUDIO_RATE).sin()) as f32
            })
            .collect()
    }

    fn hiss(seconds: f64, peak: f32, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..(seconds * AUDIO_RATE) as usize)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 2.0 * peak
            })
            .collect()
    }

    /// `parts` one after another, over a little hiss, with `lead` seconds of
    /// hiss before and `tail` after.
    fn play(parts: &[Vec<f32>], lead: f64, tail: f64) -> Vec<f32> {
        let mut audio = hiss(lead, 0.003, 5);
        for part in parts {
            audio.extend(part);
        }
        audio.extend(hiss(tail, 0.003, 6));
        audio
    }

    fn run(audio: &[f32], chunk: usize) -> Vec<Heard> {
        let mut decoder = Tones::new();
        let mut out = Vec::new();
        for block in audio.chunks(chunk) {
            decoder.process(block, &mut out);
        }
        out
    }

    fn signals(heard: &[Heard]) -> Vec<Signal> {
        heard.iter().map(|h| h.signal).collect()
    }

    fn near(actual: u32, expected: u32, within: u32) -> bool {
        actual.abs_diff(expected) <= within
    }

    #[test]
    fn a_two_tone_page_is_read_with_its_tones_and_lengths() {
        let audio = play(&[sine(358.9, 0.4, 1.0), sine(1_273.8, 0.4, 3.0)], 0.3, 0.3);
        let heard = run(&audio, 1_600);
        let [
            Signal::TwoTone {
                first_dhz,
                second_dhz,
                first_ms,
                second_ms,
            },
        ] = signals(&heard)[..]
        else {
            panic!("{heard:?}");
        };
        assert!(
            near(first_dhz, 3_589, 20) && near(second_dhz, 12_738, 20),
            "{first_dhz} {second_dhz}"
        );
        assert!(
            near(first_ms, 1_000, 100) && near(second_ms, 3_000, 100),
            "{first_ms} {second_ms}"
        );
        // and the span runs from the start of the first to the end of the second
        let span = &heard[0].span;
        assert!(
            span.start.abs_diff(4_800) < 1_000 && span.end.abs_diff(4_800 + 64_000) < 1_000,
            "{span:?}"
        );
    }

    #[test]
    fn a_page_whose_tones_do_not_follow_on_is_two_long_tones_not_a_page() {
        let audio = play(
            &[
                sine(600.0, 0.4, 1.0),
                hiss(0.5, 0.003, 8),
                sine(1_100.0, 0.4, 3.0),
            ],
            0.3,
            0.3,
        );
        let kinds: Vec<_> = signals(&run(&audio, 1_600))
            .iter()
            .map(|s| s.scheme())
            .collect();
        assert_eq!(kinds, ["long_tone"]);
    }

    #[test]
    fn a_tone_held_longer_than_two_seconds_is_a_long_tone_and_a_shorter_one_is_not() {
        let held = run(&play(&[sine(1_000.0, 0.4, 3.0)], 0.3, 0.3), 1_600);
        let [Signal::LongTone { dhz, ms }] = signals(&held)[..] else {
            panic!("{held:?}")
        };
        assert!(near(dhz, 10_000, 20) && near(ms, 3_000, 100), "{dhz} {ms}");
        assert!(run(&play(&[sine(1_000.0, 0.4, 1.5)], 0.3, 0.3), 1_600).is_empty());
    }

    #[test]
    fn two_tones_taking_turns_fast_are_a_warble() {
        let mut turns = Vec::new();
        for i in 0..12 {
            turns.push(sine(if i % 2 == 0 { 1_000.0 } else { 1_500.0 }, 0.4, 0.1));
        }
        let heard = run(&play(&turns, 0.3, 0.3), 1_600);
        let [
            Signal::Warble {
                high_dhz,
                low_dhz,
                ms,
            },
        ] = signals(&heard)[..]
        else {
            panic!("{heard:?}")
        };
        assert!(
            near(high_dhz, 15_000, 30) && near(low_dhz, 10_000, 30),
            "{high_dhz} {low_dhz}"
        );
        assert!(near(ms, 1_200, 150), "{ms}");
        // slowly, they are two tones, not a warble
        let slow: Vec<Vec<f32>> = (0..6)
            .map(|i| sine(if i % 2 == 0 { 1_000.0 } else { 1_500.0 }, 0.4, 0.5))
            .collect();
        let kinds: Vec<_> = signals(&run(&play(&slow, 0.3, 0.3), 1_600))
            .iter()
            .map(|s| s.scheme())
            .collect();
        assert!(!kinds.contains(&"warble"), "{kinds:?}");
    }

    #[test]
    fn the_blocks_it_is_fed_in_make_no_difference() {
        let audio = play(&[sine(358.9, 0.4, 1.0), sine(1_273.8, 0.4, 3.0)], 0.3, 0.3);
        let whole = run(&audio, audio.len());
        assert_eq!(whole.len(), 1);
        for chunk in [1, 7, 160, 320, 1_000] {
            assert_eq!(run(&audio, chunk), whole, "{chunk}");
        }
    }

    #[test]
    fn quiet_and_loud_pages_are_both_read() {
        for amplitude in [0.05, 0.9] {
            let audio = play(
                &[sine(600.0, amplitude, 1.0), sine(1_000.0, amplitude, 2.0)],
                0.3,
                0.3,
            );
            assert_eq!(run(&audio, 1_600).len(), 1, "{amplitude}");
        }
    }

    #[test]
    fn hiss_and_dtmf_keys_and_a_tone_under_hiss_are_not_dispatch_tones() {
        assert!(run(&hiss(30.0, 0.3, 1), 1_600).is_empty());
        // a DTMF key held for three seconds is two tones, not one
        let key: Vec<f32> = sine(770.0, 0.2, 3.0)
            .iter()
            .zip(sine(1_336.0, 0.2, 3.0))
            .map(|(a, b)| a + b)
            .collect();
        assert!(run(&play(&[key], 0.3, 0.3), 1_600).is_empty());
        // a tone with more hiss than tone
        let noisy: Vec<f32> = sine(1_000.0, 0.15, 3.0)
            .iter()
            .zip(hiss(3.0, 0.5, 2))
            .map(|(a, b)| a + b)
            .collect();
        assert!(run(&noisy, 1_600).is_empty());
    }

    #[test]
    fn a_ctcss_tone_under_a_page_does_not_hide_it() {
        let mut audio = play(&[sine(600.0, 0.4, 1.0), sine(1_100.0, 0.4, 2.0)], 0.3, 0.3);
        for (n, x) in audio.iter_mut().enumerate() {
            *x += 0.3 * (std::f32::consts::TAU * 162.2 * n as f32 / 16_000.0).sin();
        }
        assert_eq!(run(&audio, 1_600).len(), 1);
    }

    #[test]
    fn a_reset_forgets_a_page_half_heard() {
        let audio = play(&[sine(600.0, 0.4, 1.0), sine(1_100.0, 0.4, 2.0)], 0.3, 0.3);
        let mut decoder = Tones::new();
        let mut out = Vec::new();
        decoder.process(&audio[..30_000], &mut out);
        decoder.reset();
        decoder.process(&audio[30_000..], &mut out);
        // what is left of the second tone is under a second and a half
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(run(&audio, 800).len(), 1);
    }
}
