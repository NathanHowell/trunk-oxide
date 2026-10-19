//! The transmission gate: turns per-hop [`Measurement`]s into `Open` and
//! `Close` events and lets audio through only in between.
//!
//! A hop passes when the channel has quieted (a carrier is there) and, on a
//! PL channel, the configured tone dominates the sub-audible band. The gate
//! opens only for a run of `attack` passing hops, and closes only after
//! `release` failing hops in a row. Neither costs audio: the audio runs
//! through a delay line of `attack` hops and each hop is judged once the
//! hops after it are known, so the gate opens on the first hop of a
//! confirmed run and a short dropout inside a transmission is bridged
//! rather than cut. One hop of noise that happens to look like a tone, the
//! case that made the C++ tone squelch post one-second clicks, never opens
//! anything.

use crate::detect::{HOP, Measurement, Tone};

/// Tunables; [`Config::default`] is what production runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    /// The PL tone the channel carries, if any: without it the gate is a
    /// noise squelch alone.
    pub tone: Option<Tone>,
    /// Quieting (dB) a closed gate needs to open.
    pub open_db: f64,
    /// Quieting (dB) below which an open gate counts the hop as failed.
    pub close_db: f64,
    /// Least tone amplitude (1.0 = full deviation) and prominence over the
    /// bank's median for the tone to count.
    pub tone_min_amplitude: f32,
    pub tone_min_prominence: f32,
    /// Passing hops in a row that open the gate (also the audio delay).
    pub attack: usize,
    /// Failing hops in a row that close it.
    pub release: usize,
}

impl Config {
    /// For a channel with `tone` (or none).
    #[must_use]
    pub const fn for_tone(tone: Option<Tone>) -> Self {
        Self {
            tone,
            // A solid signal quiets 15–25 dB; 8 keeps weak but readable
            // ones, 4 holds through fades once open.
            open_db: 8.0,
            close_db: 4.0,
            // PL deviation is 10–15 % of peak; 0.02 is 100 Hz. A real tone
            // stands 5–10× over the rest of the bank, noise 2–3×.
            tone_min_amplitude: 0.02,
            tone_min_prominence: 4.0,
            attack: 3,
            release: 3,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::for_tone(None)
    }
}

/// Why the gate closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// The carrier went (the noise came back).
    Quieting,
    /// Carrier still there, the configured tone not.
    ToneLost,
    /// The audio went to zeros: something upstream muted the slot.
    Silence,
}

/// What the gate reports, with the hop it applies to (hops since the
/// gate was made; the audio emitted for that hop follows the event).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    Open {
        hop: u64,
        quieting_db: f64,
        /// The tone seen on opening (the configured one, when there is one).
        tone: Option<Tone>,
    },
    Close {
        hop: u64,
        reason: CloseReason,
    },
}

/// A hop's verdict, with why it failed when it did.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Verdict {
    pass: bool,
    fail: CloseReason,
    quieting_db: f64,
    tone: Option<Tone>,
}

/// The gate. Feed it one [`Measurement`] and that hop's [`HOP`] samples of
/// audio at a time.
#[derive(Clone, Debug)]
pub struct Gate {
    cfg: Config,
    /// Verdicts of the hops in the delay line, oldest at `pos`.
    verdicts: Vec<Verdict>,
    /// Audio of the same hops, same order.
    audio: Vec<Vec<f32>>,
    pos: usize,
    /// Hops fed so far; the hop being decided is `fed - attack`.
    fed: u64,
    /// Passing hops in a row just before the one being decided.
    run_before: usize,
    open: bool,
}

impl Gate {
    /// # Panics
    /// With a zero `attack` or `release`, or `release > attack` (the
    /// lookahead must cover the release).
    #[must_use]
    pub fn new(cfg: Config) -> Self {
        assert!(cfg.attack > 0 && cfg.release > 0 && cfg.release <= cfg.attack);
        Self {
            cfg,
            verdicts: Vec::with_capacity(cfg.attack),
            audio: Vec::with_capacity(cfg.attack),
            pos: 0,
            fed: 0,
            run_before: 0,
            open: false,
        }
    }

    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub const fn config(&self) -> &Config {
        &self.cfg
    }

    fn judge(&self, m: &Measurement) -> Verdict {
        let carrier = m.quieting_db
            >= if self.open {
                self.cfg.close_db
            } else {
                self.cfg.open_db
            };
        let tone_seen = (m.tone_amplitude >= self.cfg.tone_min_amplitude
            && m.tone_prominence >= self.cfg.tone_min_prominence)
            .then_some(m.tone);
        let tone_ok = self.cfg.tone.is_none_or(|t| tone_seen == Some(t));
        let (pass, fail) = if m.silent() {
            (false, CloseReason::Silence)
        } else if !carrier {
            (false, CloseReason::Quieting)
        } else if !tone_ok {
            (false, CloseReason::ToneLost)
        } else {
            (true, CloseReason::Quieting)
        };
        Verdict {
            pass,
            fail,
            quieting_db: m.quieting_db,
            tone: tone_seen,
        }
    }

    /// Feed one hop. `audio` is that hop's samples (any length, normally
    /// [`HOP`]). Audio of hops the gate is open for is appended to `out`,
    /// `attack` hops late; events to `events`.
    pub fn push(
        &mut self,
        m: &Measurement,
        audio: &[f32],
        out: &mut Vec<f32>,
        events: &mut Vec<Event>,
    ) {
        let v = self.judge(m);
        let a = self.cfg.attack;
        if self.verdicts.len() < a {
            // Filling the delay line: nothing to decide yet.
            self.verdicts.push(v);
            self.audio.push(audio.to_vec());
            self.fed += 1;
            return;
        }
        // The ring holds the a hops before this one; the oldest is decided
        // now, in the light of the a-1 after it and this one.
        let oldest = self.verdicts[self.pos];
        let later = |i: usize| -> Verdict {
            if i == a - 1 {
                v
            } else {
                self.verdicts[(self.pos + 1 + i) % a]
            }
        };
        let hop = self.fed - a as u64;
        let pass = if self.open {
            // Bridge a dropout shorter than the release.
            oldest.pass || (0..self.cfg.release - 1).any(|i| later(i).pass)
        } else if oldest.pass {
            let mut run = self.run_before + 1;
            for i in 0..a - 1 {
                if !later(i).pass {
                    break;
                }
                run += 1;
            }
            run >= a
        } else {
            false
        };
        self.run_before = if oldest.pass { self.run_before + 1 } else { 0 };

        if pass && !self.open {
            self.open = true;
            events.push(Event::Open {
                hop,
                quieting_db: oldest.quieting_db,
                tone: oldest.tone,
            });
        } else if !pass && self.open {
            self.open = false;
            events.push(Event::Close {
                hop,
                reason: oldest.fail,
            });
        }
        if pass {
            out.extend_from_slice(&self.audio[self.pos]);
        }

        // Replace the oldest with this hop.
        self.verdicts[self.pos] = v;
        let slot = &mut self.audio[self.pos];
        slot.clear();
        slot.extend_from_slice(audio);
        self.pos = (self.pos + 1) % a;
        self.fed += 1;
    }

    /// Hops of audio the gate delays.
    pub const fn delay_hops(&self) -> usize {
        self.cfg.attack
    }

    /// Samples the gate delays, at [`HOP`] samples per hop.
    pub const fn delay_samples(&self) -> usize {
        self.cfg.attack * HOP
    }

    /// Forget everything, as for a retuned channel. Does not emit a close.
    pub fn reset(&mut self) {
        self.verdicts.clear();
        self.audio.clear();
        self.pos = 0;
        self.fed = 0;
        self.run_before = 0;
        self.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Tone = Tone(25); // 162.2 Hz
    const OTHER: Tone = Tone(37); // 250.3 Hz

    fn noise() -> Measurement {
        Measurement {
            quieting_db: 0.3,
            tone: OTHER,
            tone_amplitude: 0.05,
            tone_prominence: 2.5,
            rms: 0.4,
        }
    }
    fn carrier(tone: Tone) -> Measurement {
        Measurement {
            quieting_db: 19.0,
            tone,
            tone_amplitude: 0.07,
            tone_prominence: 9.0,
            rms: 0.05,
        }
    }
    fn carrier_no_tone() -> Measurement {
        Measurement {
            quieting_db: 19.0,
            tone: OTHER,
            tone_amplitude: 0.004,
            tone_prominence: 1.5,
            rms: 0.05,
        }
    }
    fn silent() -> Measurement {
        Measurement {
            quieting_db: 0.0,
            tone: Tone(0),
            tone_amplitude: 0.0,
            tone_prominence: 0.0,
            rms: 0.0,
        }
    }

    /// Runs `seq` through a gate, each hop's audio being its index as a
    /// one-sample "hop", and returns the events and the audio let through.
    fn run(cfg: Config, seq: &[Measurement]) -> (Vec<Event>, Vec<f32>) {
        let mut g = Gate::new(cfg);
        let (mut out, mut events) = (Vec::new(), Vec::new());
        for (i, m) in seq.iter().enumerate() {
            g.push(m, &[i as f32], &mut out, &mut events);
        }
        // Flush: the last `attack` hops are still in the delay line; feed
        // noise so they get decided.
        for _ in 0..cfg.attack {
            g.push(&noise(), &[-1.0], &mut out, &mut events);
        }
        (events, out)
    }

    fn seq(parts: &[(usize, Measurement)]) -> Vec<Measurement> {
        parts
            .iter()
            .flat_map(|(n, m)| std::iter::repeat_n(*m, *n))
            .collect()
    }

    #[test]
    fn a_transmission_opens_once_and_closes_once_with_all_its_audio() {
        let s = seq(&[(10, noise()), (20, carrier(T)), (10, noise())]);
        let (ev, out) = run(Config::for_tone(Some(T)), &s);
        assert_eq!(
            ev,
            vec![
                Event::Open {
                    hop: 10,
                    quieting_db: 19.0,
                    tone: Some(T)
                },
                Event::Close {
                    hop: 30,
                    reason: CloseReason::Quieting
                }
            ]
        );
        let want: Vec<f32> = (10..30).map(|i| i as f32).collect();
        assert_eq!(out, want, "every hop of the transmission, nothing else");
    }

    #[test]
    fn one_or_two_hops_of_tone_in_noise_open_nothing() {
        for n in 1..3 {
            let s = seq(&[(10, noise()), (n, carrier(T)), (10, noise())]);
            let (ev, out) = run(Config::for_tone(Some(T)), &s);
            assert!(ev.is_empty(), "{n} hops: {ev:?}");
            assert_eq!(out, Vec::<f32>::new());
        }
        let s = seq(&[(10, noise()), (3, carrier(T)), (10, noise())]);
        let (ev, _) = run(Config::for_tone(Some(T)), &s);
        assert_eq!(ev.len(), 2, "three hops is a transmission");
    }

    #[test]
    fn a_short_dropout_is_bridged_and_a_long_one_closes() {
        let s = seq(&[
            (5, noise()),
            (10, carrier(T)),
            (2, noise()),
            (10, carrier(T)),
            (5, noise()),
        ]);
        let (ev, out) = run(Config::for_tone(Some(T)), &s);
        assert_eq!(ev.len(), 2, "{ev:?}");
        assert_eq!(out.len(), 22, "the gap's audio is kept too");

        let s = seq(&[
            (5, noise()),
            (10, carrier(T)),
            (3, noise()),
            (10, carrier(T)),
            (5, noise()),
        ]);
        let (ev, out) = run(Config::for_tone(Some(T)), &s);
        assert_eq!(ev.len(), 4, "{ev:?}");
        assert!(matches!(ev[1], Event::Close { hop: 15, .. }));
        assert!(matches!(ev[2], Event::Open { hop: 18, .. }));
        assert_eq!(out.len(), 20);
    }

    #[test]
    fn the_wrong_tone_never_opens_a_pl_channel_but_a_carrier_channel_opens() {
        let s = seq(&[(5, noise()), (20, carrier(OTHER)), (5, noise())]);
        let (ev, _) = run(Config::for_tone(Some(T)), &s);
        assert!(ev.is_empty(), "{ev:?}");
        let (ev, _) = run(Config::for_tone(None), &s);
        assert_eq!(ev.len(), 2);
        let s = seq(&[(5, noise()), (20, carrier_no_tone()), (5, noise())]);
        let (ev, _) = run(Config::for_tone(Some(T)), &s);
        assert!(ev.is_empty(), "{ev:?}");
        let (ev, _) = run(Config::for_tone(None), &s);
        assert!(matches!(ev[0], Event::Open { tone: None, .. }));
    }

    #[test]
    fn tone_loss_and_upstream_mute_are_named() {
        let s = seq(&[
            (5, noise()),
            (10, carrier(T)),
            (10, carrier_no_tone()),
            (5, noise()),
        ]);
        let (ev, _) = run(Config::for_tone(Some(T)), &s);
        assert!(matches!(
            ev[1],
            Event::Close {
                hop: 15,
                reason: CloseReason::ToneLost
            }
        ));
        let s = seq(&[(5, noise()), (10, carrier(T)), (10, silent())]);
        let (ev, _) = run(Config::for_tone(Some(T)), &s);
        assert!(matches!(
            ev[1],
            Event::Close {
                reason: CloseReason::Silence,
                ..
            }
        ));
    }

    #[test]
    fn hysteresis_holds_through_a_fade_once_open() {
        let mut faded = carrier(T);
        faded.quieting_db = 5.0; // under open_db, over close_db
        let s = seq(&[(5, noise()), (10, carrier(T)), (10, faded), (5, noise())]);
        let (ev, out) = run(Config::for_tone(Some(T)), &s);
        assert_eq!(ev.len(), 2, "{ev:?}");
        assert_eq!(out.len(), 20);
        let s = seq(&[(5, noise()), (10, faded), (5, noise())]);
        let (ev, _) = run(Config::for_tone(Some(T)), &s);
        assert!(ev.is_empty(), "a faded signal does not open: {ev:?}");
    }
}
