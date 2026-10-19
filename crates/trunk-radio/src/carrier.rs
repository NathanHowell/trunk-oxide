//! The transmission detector of a digital conventional channel.
//!
//! The conditioner's power squelch, held closed for a while before the
//! transmission is called over, so a dip near the threshold does not cut
//! one key-up into several. (Analog channels use trunk-fm's gate, which
//! has its own lookahead.)

use crate::types::{CloseReason, Squelch};

/// How long the squelch must stay closed before the transmission is over.
pub const HOLD_S: f64 = 0.25;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Carrier {
    hold: usize,
    open: bool,
    /// Samples the squelch has been closed for, while `open`.
    closed_for: usize,
}

impl Carrier {
    /// A detector over blocks at `rate` samples per second.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn new(rate: f64) -> Self {
        Self {
            hold: (rate * HOLD_S) as usize,
            open: false,
            closed_for: 0,
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// One block of `samples` on which the squelch was `squelch_open`.
    pub const fn push(&mut self, squelch_open: bool, samples: usize) -> Option<Squelch> {
        if squelch_open {
            self.closed_for = 0;
            if self.open {
                None
            } else {
                self.open = true;
                Some(Squelch::Open)
            }
        } else if self.open {
            self.closed_for += samples;
            if self.closed_for >= self.hold {
                self.open = false;
                self.closed_for = 0;
                Some(Squelch::Close(CloseReason::Power))
            } else {
                None
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_at_once_and_closes_after_the_hold() {
        let mut c = Carrier::new(24_000.0);
        assert_eq!(c.push(false, 600), None, "starts closed");
        assert_eq!(c.push(true, 600), Some(Squelch::Open));
        assert_eq!(c.push(true, 600), None);
        // 0.2 s closed, reopened: still one transmission.
        for _ in 0..8 {
            assert_eq!(c.push(false, 600), None);
        }
        assert_eq!(c.push(true, 600), None);
        assert!(c.is_open());
        // 0.25 s closed: over.
        for _ in 0..9 {
            assert_eq!(c.push(false, 600), None);
        }
        assert_eq!(c.push(false, 600), Some(Squelch::Close(CloseReason::Power)));
        assert_eq!(c.push(false, 600), None);
        assert!(!c.is_open());
    }
}
