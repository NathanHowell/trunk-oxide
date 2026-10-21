//! Blanking a stretch of audio that is on its way into a recording.
//!
//! It is for the in-band signalling burst a decoder has just read: a squawk
//! the listener should not have to hear, taken out before it is recorded.
//!
//! The stretch is silent, and the audio either side of it is faded to meet
//! it, so a blanked burst neither clicks nor takes more than `fade` samples
//! of the speech beside it.

use std::ops::Range;

use crate::gate::ramp;

/// A stretch to blank, counted in samples of the stream it is applied to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mute {
    /// The samples that come out silent.
    pub span: Range<u64>,
    /// Samples before and after `span` faded to meet it.
    pub fade: usize,
}

impl Mute {
    /// The factor sample `at` is multiplied by: 0 inside the span, a raised
    /// cosine up to 1 over the `fade` samples either side, 1 beyond.
    #[must_use]
    pub fn gain(&self, at: u64) -> f32 {
        if self.span.is_empty() || self.span.contains(&at) {
            return if self.span.is_empty() { 1.0 } else { 0.0 };
        }
        let distance = if at < self.span.start {
            self.span.start - at
        } else {
            at - self.span.end + 1
        };
        if distance > self.fade as u64 {
            1.0
        } else {
            ramp((distance - 1) as usize, self.fade)
        }
    }

    /// The samples it changes: the span and its fades.
    #[must_use]
    pub const fn window(&self) -> Range<u64> {
        self.span.start.saturating_sub(self.fade as u64)..self.span.end + self.fade as u64
    }

    /// Applies it to `audio`, whose first sample is sample `base` of the
    /// stream.
    pub fn apply(&self, base: u64, audio: &mut [f32]) {
        if self.span.is_empty() {
            return;
        }
        let window = self.window();
        let from = window.start.max(base);
        let to = window.end.min(base + audio.len() as u64);
        for at in from..to.max(from) {
            audio[(at - base) as usize] *= self.gain(at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mute() -> Mute {
        Mute {
            span: 100..200,
            fade: 10,
        }
    }

    #[test]
    fn the_span_is_silent_and_the_fades_meet_it() {
        let m = mute();
        assert!((100..200).all(|at| m.gain(at) == 0.0));
        assert!(m.gain(99) < 0.01 && m.gain(200) < 0.01);
        assert!(m.gain(90) > 0.98 && m.gain(209) > 0.98);
        assert!((m.gain(94) - 0.5).abs() < 0.1 && (m.gain(205) - 0.5).abs() < 0.1);
        assert!(m.gain(89) == 1.0 && m.gain(210) == 1.0 && m.gain(0) == 1.0);
        // monotone into the span
        assert!((90..100).map(|at| m.gain(at)).is_sorted_by(|a, b| a >= b));
        assert!((200..210).map(|at| m.gain(at)).is_sorted_by(|a, b| a <= b));
    }

    #[test]
    fn the_blocks_it_is_applied_in_make_no_difference() {
        let m = mute();
        let mut whole = vec![1.0f32; 400];
        m.apply(0, &mut whole);
        for block in [1, 7, 64, 150, 399] {
            let mut parts = vec![1.0f32; 400];
            for (i, chunk) in parts.chunks_mut(block).enumerate() {
                m.apply((i * block) as u64, chunk);
            }
            assert_eq!(parts, whole, "{block}");
        }
        assert_eq!(whole[150], 0.0);
        assert_eq!(whole[50], 1.0);
        assert_eq!(whole[300], 1.0);
    }

    #[test]
    fn a_stretch_at_the_start_of_the_stream_and_an_empty_one_are_safe() {
        let mut audio = vec![1.0f32; 50];
        Mute {
            span: 0..20,
            fade: 10,
        }
        .apply(0, &mut audio);
        assert!(audio[..20].iter().all(|&s| s == 0.0));
        assert_eq!(audio[40], 1.0);
        let mut untouched = vec![1.0f32; 50];
        Mute {
            span: 30..30,
            fade: 10,
        }
        .apply(0, &mut untouched);
        assert_eq!(untouched, vec![1.0f32; 50]);
        // and audio wholly outside the window
        let mut far = vec![1.0f32; 10];
        mute().apply(1_000, &mut far);
        assert_eq!(far, vec![1.0f32; 10]);
    }
}
