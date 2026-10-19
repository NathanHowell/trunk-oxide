//! Holding a transmission's audio until it proves audible.
//!
//! A transmission is audible once some half-second window of its audio
//! reaches [`AUDIBLE_RMS`]. Until then its audio is held back; a
//! transmission that ends without reaching it is dropped, so a tone-only
//! key-up or an empty channel never reaches anyone downstream.

/// The RMS (full scale 1.0) a window must reach: about -46 dBFS.
pub const AUDIBLE_RMS: f64 = 0.005;
/// The window over which [`AUDIBLE_RMS`] is measured.
pub const AUDIBLE_WINDOW_S: f64 = 0.5;

/// One transmission's hold: `Holding` → `Audible` (or dropped at the end).
#[derive(Clone, Debug, PartialEq)]
pub struct Hold {
    window: usize,
    state: State,
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    /// Nothing has cleared the threshold yet: audio waits in `audio`, the
    /// window in progress is `sq` (sum of squares) over `n` samples.
    Holding { audio: Vec<i16>, sq: f64, n: usize },
    /// Audio flows.
    Audible,
    /// Finished: nothing more goes out.
    Done,
}

impl Hold {
    /// A hold for audio at `rate` samples per second.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn new(rate: f64) -> Self {
        Self {
            window: ((rate * AUDIBLE_WINDOW_S) as usize).max(1),
            state: State::Holding {
                audio: Vec::new(),
                sq: 0.0,
                n: 0,
            },
        }
    }

    /// The transmission has proved audible.
    #[must_use]
    pub const fn is_audible(&self) -> bool {
        matches!(self.state, State::Audible)
    }

    /// Samples held back so far.
    #[must_use]
    pub const fn held(&self) -> usize {
        match &self.state {
            State::Holding { audio, .. } => audio.len(),
            _ => 0,
        }
    }

    /// Take `audio` and return what of it goes out: nothing until a window
    /// clears the threshold, then everything held plus this; nothing after
    /// [`Self::finish`].
    #[allow(clippy::cast_precision_loss, clippy::suboptimal_flops)]
    pub fn push(&mut self, audio: &[i16]) -> Vec<i16> {
        match &mut self.state {
            State::Audible => audio.to_vec(),
            State::Done => Vec::new(),
            State::Holding { audio: held, sq, n } => {
                held.extend_from_slice(audio);
                for &s in audio {
                    let v = f64::from(s) / 32768.0;
                    *sq += v * v;
                    *n += 1;
                    if *n >= self.window {
                        let rms = (*sq / *n as f64).sqrt();
                        (*sq, *n) = (0.0, 0);
                        if rms >= AUDIBLE_RMS {
                            let out = std::mem::take(held);
                            self.state = State::Audible;
                            return out;
                        }
                    }
                }
                Vec::new()
            }
        }
    }

    /// The transmission is over: whatever a final partial window makes
    /// audible is returned to send; otherwise the held audio is dropped
    /// (`Err` with how much).
    ///
    /// # Errors
    /// The transmission never proved audible: the number of samples dropped.
    #[allow(clippy::cast_precision_loss)]
    pub fn finish(&mut self) -> Result<Vec<i16>, usize> {
        let out = match std::mem::replace(&mut self.state, State::Done) {
            State::Holding { audio, sq, n } => {
                let rms = if n > 0 { (sq / n as f64).sqrt() } else { 0.0 };
                if rms >= AUDIBLE_RMS {
                    Ok(audio)
                } else {
                    Err(audio.len())
                }
            }
            State::Audible | State::Done => Ok(Vec::new()),
        };
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: i16 = 40;
    const LOUD: i16 = 3000;

    #[test]
    fn quiet_audio_is_held_and_dropped_at_the_end() {
        let mut h = Hold::new(8_000.0);
        let quiet = vec![QUIET; 4000];
        assert_eq!(h.push(&quiet), Vec::<i16>::new());
        assert_eq!(h.push(&quiet), Vec::<i16>::new());
        assert_eq!(h.held(), 8000);
        assert!(!h.is_audible());
        assert_eq!(h.finish(), Err(8000));
        assert_eq!(
            h.push(&[LOUD; 100]),
            Vec::<i16>::new(),
            "done takes nothing"
        );
    }

    #[test]
    fn loud_audio_releases_what_was_held_then_flows() {
        let mut h = Hold::new(8_000.0);
        let quiet = vec![QUIET; 4000];
        let loud = vec![LOUD; 4000];
        assert_eq!(h.push(&quiet), Vec::<i16>::new());
        let out = h.push(&loud);
        assert_eq!(out.len(), 8000);
        assert_eq!(&out[..4000], &quiet[..]);
        assert_eq!(&out[4000..], &loud[..]);
        assert!(h.is_audible());
        assert_eq!(h.push(&quiet), quiet);
        assert_eq!(h.finish(), Ok(Vec::new()));
    }

    #[test]
    fn a_loud_partial_last_window_is_released_at_the_end() {
        let mut h = Hold::new(8_000.0);
        assert_eq!(h.push(&[LOUD; 1000]), Vec::<i16>::new());
        assert_eq!(h.finish().map(|v| v.len()), Ok(1000));
    }

    #[test]
    fn a_window_clears_on_its_rms_not_on_its_last_sample() {
        // 0.4 s quiet then 0.1 s loud fills the first window; its RMS
        // clears, so the whole block goes out at once.
        let mut h = Hold::new(8_000.0);
        let mut block = vec![QUIET; 3200];
        block.extend_from_slice(&[LOUD; 800]);
        assert_eq!(h.push(&block), block);
        assert!(h.is_audible());
        // 0.4 s quiet then a single loud sample does not.
        let mut h = Hold::new(8_000.0);
        let mut block = vec![QUIET; 3999];
        block.push(LOUD);
        assert_eq!(h.push(&block), Vec::<i16>::new());
        assert_eq!(h.finish(), Err(4000));
    }

    #[test]
    fn an_empty_transmission_is_nothing() {
        let mut h = Hold::new(16_000.0);
        assert_eq!(h.finish(), Err(0));
    }
}
