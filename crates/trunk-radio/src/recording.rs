//! What a channel records: the audio its decoder writes, held until it is
//! audible ([`Hold`]), and the accounting the state machine reads.
//!
//! A decoder writes through its [`Tape`]; the control side reads the
//! accounting off the same tape, rotates it when a conventional
//! transmission ends and closes it with the channel.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::audible::Hold;
use crate::shape::{AudioShape, ShapeMeter};
use crate::types::{CallCounts, CallReport, Transmission};

/// A sample at or above this is voice, not digital silence (~-66 dBFS).
const VOICE_THRESHOLD: i16 = 16;

/// What was kept of one recording: when it started, what was written,
/// what the decoder counted.
#[derive(Clone, Debug, PartialEq)]
pub struct Accounting {
    pub system: String,
    pub talkgroup: u32,
    /// Where the channel was tuned.
    pub freq: f64,
    pub phase2: bool,
    /// The rate of the audio written.
    pub rate: f64,
    pub start: SystemTime,
    /// When audio was last written, and audio with voice in it (the start,
    /// before any).
    pub last_write: Instant,
    pub last_voice: Instant,
    pub sample_count: i64,
    pub term_events: i64,
    /// `term_events` by cause ([`Transmission::ended_by`]).
    pub ended_by: [i64; 4],
    pub after_term_samples: i64,
    /// The decoder's counts so far (P25).
    pub counts: CallCounts,
    /// Units heard in band, in order of first appearance (P25).
    pub sources: Vec<u32>,
    /// The shape of the audio sent downstream so far (see [`ShapeMeter`]).
    pub shape: Option<AudioShape>,
}

impl Accounting {
    /// A recording starting now.
    #[must_use]
    pub fn new(system: &str, talkgroup: u32, freq: f64, phase2: bool, rate: f64) -> Self {
        let now = crate::pace::now();
        Self {
            system: system.to_owned(),
            talkgroup,
            freq,
            phase2,
            rate,
            start: crate::pace::wall_now(),
            last_write: now,
            last_voice: now,
            sample_count: 0,
            term_events: 0,
            ended_by: [0; 4],
            after_term_samples: 0,
            counts: CallCounts::default(),
            sources: Vec::new(),
            shape: None,
        }
    }

    /// The next recording on the same channel, starting now.
    #[must_use]
    pub fn successor(&self) -> Self {
        Self::new(
            &self.system,
            self.talkgroup,
            self.freq,
            self.phase2,
            self.rate,
        )
    }

    /// Audio was written: `n` samples, voice among them or not.
    pub fn wrote(&mut self, n: usize, voice: bool) {
        let now = crate::pace::now();
        self.sample_count += i64::try_from(n).unwrap_or(0);
        self.last_write = now;
        if voice {
            self.last_voice = now;
        }
    }

    /// Seconds since audio was last written (since the start, before any).
    #[must_use]
    pub fn since_write_s(&self) -> f64 {
        crate::pace::now()
            .saturating_duration_since(self.last_write)
            .as_secs_f64()
    }

    /// Seconds since voice was last written (since the start, before any).
    #[must_use]
    pub fn since_voice_s(&self) -> f64 {
        crate::pace::now()
            .saturating_duration_since(self.last_voice)
            .as_secs_f64()
    }

    /// What was recorded, if anything was.
    #[must_use]
    pub fn transmission(&self) -> Option<Transmission> {
        if self.sample_count == 0 {
            return None;
        }
        let secs = |t: SystemTime| {
            i64::try_from(t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())).unwrap_or(0)
        };
        Some(Transmission {
            talkgroup: self.talkgroup,
            start_time: secs(self.start),
            stop_time: secs(crate::pace::wall_now()),
            sample_count: self.sample_count,
            spike_count: i64::try_from(self.counts.spikes).unwrap_or(i64::MAX),
            error_count: i64::try_from(self.counts.errors).unwrap_or(i64::MAX),
            term_events: self.term_events,
            ended_by: self.ended_by,
            after_term_samples: self.after_term_samples,
            freq: self.freq,
            shape: self.shape,
        })
    }

    /// A P25 recording's outcome, for the metrics.
    #[must_use]
    pub fn report(&self) -> CallReport {
        let t = self.transmission();
        CallReport {
            system: self.system.clone(),
            talkgroup: self.talkgroup,
            freq: self.freq,
            phase2: self.phase2,
            #[allow(clippy::cast_precision_loss)]
            audio_seconds: t
                .as_ref()
                .map_or(0.0, |t| t.sample_count as f64 / self.rate),
            transmissions: i64::from(t.is_some()),
            errors: t.as_ref().map_or(0, |t| t.error_count),
            spikes: t.as_ref().map_or(0, |t| t.spike_count),
            sources: self.sources.clone(),
            algid: self.counts.algid,
            repeated: i64::try_from(self.counts.repeated).unwrap_or(0),
            muted: i64::try_from(self.counts.muted).unwrap_or(0),
        }
    }
}

/// One recording: the decoder writes its audio through here (the
/// accounting and the audibility hold see it).
#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    pub acct: Accounting,
    hold: Hold,
    /// Measures what leaves the hold.
    meter: ShapeMeter,
}

impl Recording {
    #[must_use]
    pub fn new(acct: Accounting) -> Self {
        let hold = Hold::new(acct.rate);
        let meter = ShapeMeter::new(acct.rate);
        Self { acct, hold, meter }
    }

    /// The accounting so far, with the shape of what has been sent.
    #[must_use]
    pub fn accounting(&self) -> Accounting {
        Accounting {
            shape: self.meter.shape(),
            ..self.acct.clone()
        }
    }

    /// Account for `audio` and return what of it goes out now (see
    /// [`Hold::push`]).
    pub fn deliver(&mut self, audio: &[i16]) -> Vec<i16> {
        let voice = audio.iter().any(|&s| s.abs() >= VOICE_THRESHOLD);
        self.acct.wrote(audio.len(), voice);
        let out = self.hold.push(audio);
        self.meter.push(&out);
        out
    }

    /// The recording is over: its accounting, and the audio still to send
    /// (see [`Hold::finish`]).
    #[must_use]
    pub fn finish(mut self) -> (Accounting, Vec<i16>) {
        let tail = match self.hold.finish() {
            Ok(tail) => tail,
            Err(0) => Vec::new(),
            Err(samples) => {
                tracing::info!(
                    talkgroup = self.acct.talkgroup,
                    samples,
                    "dropping a transmission with no audible audio"
                );
                Vec::new()
            }
        };
        self.meter.push(&tail);
        self.acct.shape = self.meter.shape();
        (self.acct, tail)
    }
}

/// A channel's recording, shared by its decoder and the control side.
/// Empty once the channel is closed (and always, on a channel that records
/// nothing): what a decoder writes then goes nowhere.
#[derive(Clone, Debug)]
pub struct Tape(Arc<Mutex<Option<Recording>>>);

impl Tape {
    #[must_use]
    pub fn new(recording: Option<Recording>) -> Self {
        Self(Arc::new(Mutex::new(recording)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Recording>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `f` on the recording, while there is one.
    pub fn with<R>(&self, f: impl FnOnce(&mut Recording) -> R) -> Option<R> {
        self.lock().as_mut().map(f)
    }

    /// Account for `audio`; what of it goes out now.
    pub fn deliver(&self, audio: &[i16]) -> Vec<i16> {
        self.with(|r| r.deliver(audio)).unwrap_or_default()
    }

    #[must_use]
    pub fn accounting(&self) -> Option<Accounting> {
        self.lock().as_ref().map(Recording::accounting)
    }

    /// End the recording and start the next: the ended one's accounting
    /// and the audio still to send.
    pub fn rotate(&self) -> Option<(Accounting, Vec<i16>)> {
        let mut guard = self.lock();
        let next = guard.as_ref().map(|r| Recording::new(r.acct.successor()))?;
        let done = guard.replace(next)?;
        drop(guard);
        Some(done.finish())
    }

    /// End the recording; nothing written after is kept.
    pub fn close(&self) -> Option<(Accounting, Vec<i16>)> {
        let done = self.lock().take()?;
        Some(done.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct() -> Accounting {
        Accounting::new("s", 5, 851e6, false, 8_000.0)
    }

    #[test]
    fn a_tape_holds_until_audible_rotates_and_closes() {
        let quiet = vec![40i16; 4000];
        let loud = vec![3000i16; 4000];
        let tape = Tape::new(Some(Recording::new(acct())));
        assert_eq!(tape.deliver(&quiet), Vec::<i16>::new(), "held");
        assert_eq!(tape.accounting().unwrap().sample_count, 4000);
        assert_eq!(tape.deliver(&loud).len(), 8000, "released");
        let (done, tail) = tape.rotate().unwrap();
        assert_eq!((done.sample_count, tail.len()), (8000, 0));
        let next = tape.accounting().unwrap();
        assert_eq!((next.sample_count, next.talkgroup), (0, 5), "the next one");
        assert_eq!(tape.deliver(&quiet), Vec::<i16>::new(), "held again");
        let (done, tail) = tape.close().unwrap();
        assert_eq!(
            (done.sample_count, tail.len()),
            (4000, 0),
            "inaudible: dropped"
        );
        assert_eq!(tape.deliver(&loud), Vec::<i16>::new(), "closed");
        assert!(tape.accounting().is_none() && tape.close().is_none());
    }

    #[test]
    fn a_recording_reports_the_shape_of_what_it_sent_and_nothing_for_what_it_held() {
        // 8 kHz: 0.6 s of quiet (held, never audible), then 0.2 s of nothing
        // and 1 s of speech-level tone (released), then 0.1 s of nothing
        let tone = |n: usize| -> Vec<i16> {
            (0..n)
                .map(|i| (6_000.0 * (i as f64 * 0.5).sin()) as i16)
                .collect()
        };
        let tape = Tape::new(Some(Recording::new(acct())));
        assert_eq!(tape.deliver(&vec![40i16; 4_800]), Vec::<i16>::new());
        assert_eq!(tape.accounting().unwrap().shape, None, "nothing sent yet");
        let mut speech = vec![0i16; 1_600];
        speech.extend(tone(8_000));
        speech.extend(vec![0i16; 800]);
        assert!(!tape.deliver(&speech).is_empty());
        let (done, tail) = tape.close().unwrap();
        assert!(tail.is_empty());
        let shape = done.shape.expect("it sent audio");
        // what was sent includes the held quiet: 0.6 s of it, then the 0.2 s
        // gap, so speech begins 0.8 s in (to the frame) and ends 0.1 s early
        assert!((shape.onset_s - 0.8).abs() < 0.03, "{shape:?}");
        assert!((shape.trailing_silence_s - 0.1).abs() < 0.03, "{shape:?}");
        assert_eq!(done.transmission().unwrap().shape, Some(shape));

        // a recording that never became audible sent nothing, so has no shape
        let quiet = Tape::new(Some(Recording::new(acct())));
        quiet.deliver(&vec![40i16; 4_000]);
        assert_eq!(quiet.close().unwrap().0.shape, None);
    }

    #[test]
    fn accounting_follows_audio_and_voice() {
        let mut a = acct();
        assert!(a.transmission().is_none());
        assert!(a.since_write_s() < 1.0, "counted from the start");
        a.wrote(160, false);
        assert!(a.last_voice <= a.last_write);
        a.wrote(160, true);
        let t = a.transmission().unwrap();
        assert_eq!((t.talkgroup, t.sample_count), (5, 320));
        assert!(a.since_voice_s() < 1.0);
        let report = a.report();
        assert_eq!(report.transmissions, 1);
        assert!((report.audio_seconds - 0.04).abs() < 1e-9);
    }
}
