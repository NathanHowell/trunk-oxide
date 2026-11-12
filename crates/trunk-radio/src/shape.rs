//! Cheap per-transmission audio shape analysis, used to turn "the call
//! sounds clipped" into numbers: where speech starts and ends, how abruptly
//! it ends, and whether the decoder left digital-silence holes inside it.
//!
//! It measures what a recording sends downstream, as the recording sends it
//! ([`ShapeMeter`] is fed what leaves the audibility hold), so a lead-in the
//! hold trimmed or a tail the decoder cut shows here and not after the
//! wire, a resampler and a transcription chunking have each had a go.

/// Frames per second: 20 ms frames, at any rate.
const FRAMES_PER_SECOND: f64 = 50.0;
const FRAME_S: f64 = 0.020;

/// A frame is "voiced" when its RMS is within 20 dB of the loudest frame. The
/// threshold is relative so it doesn't depend on the system's audio level.
const VOICED_REL: f64 = 0.1;
/// Floor for the voiced threshold, so a segment of pure noise isn't "voiced".
const VOICED_FLOOR: f64 = 0.002;
/// A frame whose peak is below this is digital silence (a muted codeword, not
/// a quiet word). About -66 dBFS.
const DIGITAL_SILENCE_PEAK: f64 = 0.0005;
/// Digital-silence runs shorter than this are ignored (2 frames = 40 ms).
const MIN_GAP_FRAMES: usize = 2;
/// Longer runs are pauses between phrases, not dropouts (20 frames = 400 ms).
const MAX_GAP_FRAMES: usize = 20;
/// A dropout interrupts speech that is still going: the frames on both sides
/// are within about 12 dB of the segment's loudest. Words end by decaying, so
/// the natural pause after one starts from a quiet frame and doesn't count.
const LOUD_NEIGHBOR_REL: f64 = 0.25;
/// An ending this close to the segment's peak level is abrupt (about -9 dB).
const ABRUPT_END_DB: f64 = -9.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioShape {
    /// Seconds from the segment start to the first voiced frame.
    pub onset_s: f64,
    /// Seconds from the last voiced frame to the segment end.
    pub trailing_silence_s: f64,
    /// Level of the last voiced frame relative to the loudest frame, in dB
    /// (<= 0). Natural endings decay and land near -20; a cut mid-word stays loud.
    pub end_level_db: f64,
    /// Number of dropouts: digital-silence holes of 40-400 ms with loud speech
    /// on both sides. Muted/dropped voice frames show up here; pauses between
    /// words and phrases don't.
    pub gaps: u32,
    /// Longest such hole, in seconds.
    pub longest_gap_s: f64,
}

impl AudioShape {
    /// The speech ends while still loud, which is what a clipped tail looks like.
    #[must_use]
    pub fn abrupt_end(&self) -> bool {
        self.end_level_db > ABRUPT_END_DB
    }
}

/// Measures a transmission's shape as its audio goes by, in blocks of any
/// size. All it keeps is a level and a silence flag per 20 ms.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeMeter {
    /// Samples in a frame.
    frame: usize,
    /// The frame in progress: samples so far, their sum of squares and peak
    /// (as fractions of full scale).
    filled: usize,
    squares: f64,
    peak: f64,
    /// Each finished frame's RMS, and whether it was digital silence.
    rms: Vec<f64>,
    silent: Vec<bool>,
}

impl ShapeMeter {
    /// For audio at `rate` samples per second.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn new(rate: f64) -> Self {
        Self {
            frame: ((rate / FRAMES_PER_SECOND).round() as usize).max(1),
            filled: 0,
            squares: 0.0,
            peak: 0.0,
            rms: Vec::new(),
            silent: Vec::new(),
        }
    }

    /// Takes in `audio`, 16-bit samples.
    pub fn push(&mut self, audio: &[i16]) {
        for &s in audio {
            let x = f64::from(s) / 32_768.0;
            self.squares += x * x;
            self.peak = self.peak.max(x.abs());
            self.filled += 1;
            if self.filled == self.frame {
                #[allow(clippy::cast_precision_loss)]
                self.rms.push((self.squares / self.frame as f64).sqrt());
                self.silent.push(self.peak < DIGITAL_SILENCE_PEAK);
                self.filled = 0;
                self.squares = 0.0;
                self.peak = 0.0;
            }
        }
    }

    /// The shape of what has been pushed: `None` when it is too short for a
    /// frame or has no voiced frame at all (silence, or noise only).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn shape(&self) -> Option<AudioShape> {
        let rms = &self.rms;
        let peak = rms.iter().copied().fold(0.0_f64, f64::max);
        let threshold = (peak * VOICED_REL).max(VOICED_FLOOR);
        let first = rms.iter().position(|r| *r >= threshold)?;
        let last = rms.iter().rposition(|r| *r >= threshold)?;

        // Dropouts strictly inside the speech span.
        let loud = |i: usize| rms[i] >= peak * LOUD_NEIGHBOR_REL;
        let (mut gaps, mut longest, mut run) = (0u32, 0usize, 0usize);
        for (i, &silent) in self.silent.iter().enumerate().take(last + 1).skip(first) {
            if silent {
                run += 1;
            } else {
                // `first` is voiced, so a run always has a frame before it.
                if (MIN_GAP_FRAMES..=MAX_GAP_FRAMES).contains(&run) && loud(i - run - 1) && loud(i)
                {
                    gaps += 1;
                    longest = longest.max(run);
                }
                run = 0;
            }
        }
        // `last` is voiced, so a run can't be open at the end of the span.

        Some(AudioShape {
            onset_s: first as f64 * FRAME_S,
            trailing_silence_s: (rms.len() - 1 - last) as f64 * FRAME_S,
            end_level_db: 20.0 * (rms[last] / peak).log10(),
            gaps,
            longest_gap_s: longest as f64 * FRAME_S,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 16_000.0;
    const FRAME: usize = 320;

    fn tone(frames: usize, amp: f64) -> Vec<i16> {
        (0..frames * FRAME)
            .map(|n| (amp * 32_768.0 * (n as f64 * 0.3).sin()) as i16)
            .collect()
    }

    fn silence(frames: usize) -> Vec<i16> {
        vec![0; frames * FRAME]
    }

    fn analyze(audio: &[i16]) -> Option<AudioShape> {
        let mut meter = ShapeMeter::new(RATE);
        meter.push(audio);
        meter.shape()
    }

    #[test]
    fn leading_and_trailing_silence_are_measured() {
        let mut s = silence(10);
        s.extend(tone(20, 0.3));
        s.extend(silence(5));
        let a = analyze(&s).unwrap();
        assert!((a.onset_s - 0.2).abs() < 1e-9);
        assert!((a.trailing_silence_s - 0.1).abs() < 1e-9);
        assert_eq!(a.gaps, 0);
    }

    #[test]
    fn abrupt_end_is_loud_and_decayed_end_is_not() {
        let mut cut = tone(20, 0.3);
        cut.extend(silence(3));
        assert!(analyze(&cut).unwrap().abrupt_end());

        // Speech that decays: each frame 0.8x the last, ending ~-20 dB.
        let mut decay = Vec::new();
        let mut amp = 0.3;
        for _ in 0..20 {
            decay.extend(tone(1, amp));
            amp *= 0.8;
        }
        decay.extend(silence(3));
        let a = analyze(&decay).unwrap();
        assert!(!a.abrupt_end(), "end_level_db = {}", a.end_level_db);
    }

    #[test]
    fn digital_silence_holes_inside_speech_are_counted() {
        let mut s = tone(10, 0.3);
        s.extend(silence(3)); // 60 ms hole
        s.extend(tone(10, 0.3));
        s.extend(silence(1)); // 20 ms: too short to count
        s.extend(tone(5, 0.3));
        let a = analyze(&s).unwrap();
        assert_eq!(a.gaps, 1);
        assert!((a.longest_gap_s - 0.06).abs() < 1e-9);
    }

    #[test]
    fn pauses_are_not_dropouts() {
        // Hole after a word that decayed: the frame before it is quiet.
        let mut s = tone(10, 0.3);
        for amp in [0.1, 0.05, 0.02] {
            s.extend(tone(1, amp));
        }
        s.extend(silence(5));
        s.extend(tone(10, 0.3));
        assert_eq!(analyze(&s).unwrap().gaps, 0);

        // A 600 ms silence between two loud phrases is a pause, not a mute.
        let mut s = tone(10, 0.3);
        s.extend(silence(30));
        s.extend(tone(10, 0.3));
        assert_eq!(analyze(&s).unwrap().gaps, 0);
    }

    #[test]
    fn pure_silence_and_short_input_have_no_shape() {
        assert_eq!(analyze(&silence(20)), None);
        assert_eq!(analyze(&[3000; 100]), None);
        assert_eq!(ShapeMeter::new(RATE).shape(), None);
    }

    #[test]
    fn the_blocks_it_is_fed_in_make_no_difference() {
        let mut s = silence(7);
        s.extend(tone(12, 0.3));
        s.extend(silence(3));
        s.extend(tone(9, 0.25));
        s.extend(silence(4));
        let whole = analyze(&s).unwrap();
        for chunk in [1, 7, 160, 320, 333, 5_000] {
            let mut meter = ShapeMeter::new(RATE);
            for block in s.chunks(chunk) {
                meter.push(block);
            }
            assert_eq!(meter.shape().unwrap(), whole, "{chunk}");
        }
    }

    #[test]
    fn a_frame_is_twenty_milliseconds_at_any_rate() {
        // 8 kHz audio (P25): 160-sample frames, the same seconds out
        let tone8 = |frames: usize| -> Vec<i16> {
            (0..frames * 160)
                .map(|n| (0.3 * 32_768.0 * (n as f64 * 0.6).sin()) as i16)
                .collect()
        };
        let mut s = vec![0i16; 10 * 160];
        s.extend(tone8(20));
        s.extend(vec![0i16; 5 * 160]);
        let mut meter = ShapeMeter::new(8_000.0);
        meter.push(&s);
        let a = meter.shape().unwrap();
        assert!((a.onset_s - 0.2).abs() < 1e-9 && (a.trailing_silence_s - 0.1).abs() < 1e-9);
    }

    #[test]
    fn a_partial_frame_is_not_counted_until_it_is_whole() {
        let mut meter = ShapeMeter::new(RATE);
        meter.push(&tone(5, 0.3));
        meter.push(&[1000; 100]);
        let a = meter.shape().unwrap();
        // the 100 samples are not yet a frame, so no trailing silence either
        assert!(a.trailing_silence_s.abs() < 1e-9);
    }
}
