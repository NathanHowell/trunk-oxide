//! Cheap per-segment audio shape analysis, used to turn "the call sounds
//! clipped" into numbers: where speech starts and ends, how abruptly it ends,
//! and whether the decoder left digital-silence holes inside it.
//!
//! Input is 16 kHz mono f32 PCM in [-1, 1] (the transcription audio clock).

/// Frame length: 20 ms at 16 kHz.
const FRAME: usize = 320;
const FRAME_S: f64 = 0.020;

/// A frame is "voiced" when its RMS is within 20 dB of the loudest frame. The
/// threshold is relative so it doesn't depend on the system's audio level.
const VOICED_REL: f64 = 0.1;
/// Floor for the voiced threshold, so a segment of pure noise isn't "voiced".
const VOICED_FLOOR: f64 = 0.002;
/// A frame whose peak is below this is digital silence (a muted codeword, not
/// a quiet word). About -66 dBFS.
const DIGITAL_SILENCE_PEAK: f32 = 0.0005;
/// Digital-silence runs shorter than this are ignored (2 frames = 40 ms).
const MIN_GAP_FRAMES: usize = 2;
/// An ending this close to the segment's peak level is abrupt (about -9 dB).
const ABRUPT_END_DB: f64 = -9.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioShape {
    /// Seconds from the segment start to the first voiced frame.
    pub onset_s: f64,
    /// Seconds from the last voiced frame to the segment end.
    pub trailing_silence_s: f64,
    /// Level of the last voiced frame relative to the loudest frame, in dB
    /// (<= 0). Natural endings decay and land near -20; a cut mid-word stays loud.
    pub end_level_db: f64,
    /// Number of digital-silence holes (>= 40 ms) between onset and the end of
    /// speech. Muted/dropped voice frames show up here.
    pub gaps: u32,
    /// Longest such hole, in seconds.
    pub longest_gap_s: f64,
}

impl AudioShape {
    /// The speech ends while still loud, which is what a clipped tail looks like.
    pub fn abrupt_end(&self) -> bool {
        self.end_level_db > ABRUPT_END_DB
    }
}

fn frame_rms(frame: &[f32]) -> f64 {
    let sum: f64 = frame.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    (sum / frame.len() as f64).sqrt()
}

/// Analyse a segment. `None` when it is too short for a frame or has no voiced
/// frame at all (silence / noise only).
pub fn analyze(samples: &[f32]) -> Option<AudioShape> {
    let frames: Vec<&[f32]> = samples
        .as_chunks::<FRAME>()
        .0
        .iter()
        .map(<[f32; FRAME]>::as_slice)
        .collect();
    if frames.is_empty() {
        return None;
    }
    let rms: Vec<f64> = frames.iter().map(|f| frame_rms(f)).collect();
    let peak = rms.iter().copied().fold(0.0_f64, f64::max);
    let threshold = (peak * VOICED_REL).max(VOICED_FLOOR);
    let first = rms.iter().position(|r| *r >= threshold)?;
    let last = rms.iter().rposition(|r| *r >= threshold)?;

    // Digital-silence holes strictly inside the speech span.
    let (mut gaps, mut longest, mut run) = (0u32, 0usize, 0usize);
    for f in &frames[first..=last] {
        if f.iter().all(|s| s.abs() < DIGITAL_SILENCE_PEAK) {
            run += 1;
        } else {
            if run >= MIN_GAP_FRAMES {
                gaps += 1;
                longest = longest.max(run);
            }
            run = 0;
        }
    }
    // `last` is voiced, so a run can't be open at the end of the span.

    Some(AudioShape {
        onset_s: first as f64 * FRAME_S,
        trailing_silence_s: (frames.len() - 1 - last) as f64 * FRAME_S,
        end_level_db: 20.0 * (rms[last] / peak).log10(),
        gaps,
        longest_gap_s: longest as f64 * FRAME_S,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, amp: f32) -> Vec<f32> {
        (0..frames * FRAME)
            .map(|n| amp * (n as f32 * 0.3).sin())
            .collect()
    }

    fn silence(frames: usize) -> Vec<f32> {
        vec![0.0; frames * FRAME]
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

        // Speech that decays: each frame 0.7x the last, ending ~-20 dB.
        let mut decay = Vec::new();
        let mut amp = 0.3_f32;
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
    fn pure_silence_and_short_input_have_no_shape() {
        assert_eq!(analyze(&silence(20)), None);
        assert_eq!(analyze(&[0.1; 100]), None);
    }
}
