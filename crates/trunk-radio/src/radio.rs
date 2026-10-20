//! The radio as the call state machine drives it: sources, the systems
//! their channels decode for, and the channels themselves.
//!
//! A channel is a channelizer slot with a decoder on it, opened for one
//! call (or one control channel) with [`Radio::open`] and freed with
//! [`Radio::close`]. Slots are reused: a free slot of the covering source
//! that last held the same kind first (its conditioner carries on, as a
//! recorder's did from one call to the next), then any free slot, then a
//! new one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Result, bail};
use trunk_channelizer::{ConditioningSettings, ProfileId};
use trunk_p25::Nac;
use trunk_p25::tdma::Scrambler;
use trunk_sdr::{Running, Stream};

use crate::correction::Correction;
use crate::kind::{Call, Cmd, Kind, OpenError, Opening};
use crate::recording::{Accounting, Recording, Tape};
use crate::source::Source;
use crate::types::{AudioSink, CallId, ChannelNum, ChannelStatus, EventSender, Transmission};

#[derive(Clone, Debug, Default)]
pub struct RadioConfig {
    /// Where baseline taps go (`trunk-replay --tap`); none in production.
    pub tap_dir: Option<String>,
    pub conditioning: ConditioningSettings,
    /// Each source's clock correction follows the FLL errors its channels
    /// close with (see [`crate::correction`]).
    pub adaptive_correction: bool,
}

/// What a system's channels are decoded with.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemSetup {
    pub sys_num: i32,
    pub short_name: String,
    /// CQPSK (else C4FM) on the control and voice channels.
    pub qpsk: bool,
    pub max_dev: i32,
    /// Noise suppression on analog voice, if any.
    pub noise_suppression: Option<crate::NoiseSuppression>,
    pub analog_levels: f64,
    pub digital_levels: f64,
    /// The P25 identity as configured (zeros if not): it gives the phase 2
    /// scrambling mask until the control channel names the system.
    pub nac: u64,
    pub sys_id: u64,
    pub wacn: u64,
}

pub struct SourceSetup {
    pub device: String,
    pub center_hz: f64,
    pub rate_hz: f64,
    pub stream: Stream,
    pub running: Box<dyn Running>,
    pub signal_detector_threshold: Option<f32>,
    /// The hardware's whole-ppm correction, as tuned.
    pub ppm: f64,
    /// The software correction (Hz) added to every channel it tunes.
    pub correction_hz: f64,
}

/// A system's phase 2 scrambling sequence, once its identity is known (a
/// NAC of 0 names no system).
fn scrambler(nac: u64, sys_id: u64, wacn: u64) -> Option<Scrambler> {
    let nac = Nac::new(u16::try_from(nac).ok()?)?;
    Some(Scrambler::new(
        u32::try_from(wacn).ok()?,
        u16::try_from(sys_id).ok()?,
        nac,
    ))
}

struct System {
    setup: SystemSetup,
    /// The phase 2 scrambling sequence (none while the identity is unknown).
    scrambler: Option<Scrambler>,
    sysid_received: bool,
    status_received: bool,
}

/// A channelizer slot the radio has used.
struct Slot {
    source: usize,
    port: usize,
    /// The kind it was last opened as.
    kind: &'static str,
    holder: Option<CallId>,
}

/// An open channel.
struct Open {
    call: CallId,
    num: ChannelNum,
    kind: &'static str,
    profile: ProfileId,
    /// The channel's frequency (before the source's correction).
    freq_hz: f64,
    tape: Tape,
    finished: fn(&Accounting),
}

/// A channel's state: while it is open, or as it was when it was closed
/// (or its recording rotated).
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelReport {
    pub num: ChannelNum,
    pub kind: &'static str,
    /// The source it is on.
    pub source: i32,
    /// What it recorded; `None` on a channel that records nothing.
    pub recording: Option<Accounting>,
    /// The slot's smoothed power, dB, once it has a reading.
    pub power_db: Option<f64>,
    /// The FLL's frequency error, Hz.
    pub freq_error: i32,
    /// The source's hardware ppm correction that would bring that error
    /// to zero, to first order (raising it lowers a positive error).
    pub preferred_ppm: f64,
}

impl ChannelReport {
    /// What it recorded, as the wire lists it.
    #[must_use]
    pub fn transmissions(&self) -> Vec<Transmission> {
        self.recording
            .as_ref()
            .and_then(Accounting::transmission)
            .into_iter()
            .collect()
    }
}

struct Inner {
    sources: Vec<Source>,
    systems: Vec<System>,
    /// By [`ChannelNum`].
    slots: Vec<Slot>,
    open: HashMap<CallId, Open>,
}

impl Inner {
    fn system_mut(&mut self, sys_num: i32) -> Option<&mut System> {
        usize::try_from(sys_num)
            .ok()
            .and_then(|n| self.systems.get_mut(n))
    }

    /// Of the sources that cover `freq_hz` and pass `fit`, the one it sits
    /// deepest in (furthest from the roll-off at the band's edges).
    fn deepest(&self, freq_hz: f64, fit: impl Fn(&Source) -> bool) -> Option<usize> {
        self.sources
            .iter()
            .enumerate()
            .filter(|(_, s)| s.covers(freq_hz) && fit(s))
            .min_by(|(_, a), (_, b)| a.depth(freq_hz).total_cmp(&b.depth(freq_hz)))
            .map(|(n, _)| n)
    }

    /// A free slot of source `src` for a `kind` channel.
    fn take_slot(&mut self, src: usize, kind: &'static str) -> ChannelNum {
        let free = |s: &Slot| s.source == src && s.holder.is_none();
        let found = self
            .slots
            .iter()
            .position(|s| free(s) && s.kind == kind)
            .or_else(|| self.slots.iter().position(free));
        let idx = found.unwrap_or_else(|| {
            let port = self.sources[src].alloc_port();
            self.slots.push(Slot {
                source: src,
                port,
                kind,
                holder: None,
            });
            self.slots.len() - 1
        });
        ChannelNum(u32::try_from(idx).unwrap_or(u32::MAX))
    }

    fn slot(&self, num: ChannelNum) -> &Slot {
        &self.slots[num.0 as usize]
    }

    /// The channel's readings, with `recording` for what it recorded.
    fn report(&self, open: &Open, recording: Option<Accounting>) -> ChannelReport {
        let slot = self.slot(open.num);
        let source = &self.sources[slot.source];
        let chz = &source.channelizer;
        let power = chz.slot_power_db(slot.port);
        let freq_error = chz.slot_freq_error_hz(slot.port);
        ChannelReport {
            num: open.num,
            kind: open.kind,
            source: source.idx,
            recording,
            power_db: (!power.is_nan()).then_some(power),
            freq_error,
            preferred_ppm: source.ppm + f64::from(freq_error) / open.freq_hz * 1e6,
        }
    }

    /// A channel of source `src` ended with `report`'s FLL error: let the
    /// source's correction follow it, and retune what is still open on the
    /// source when it moves (a step this small keeps every decoder locked).
    fn track(&mut self, src: usize, report: &ChannelReport) {
        let source = &mut self.sources[src];
        if !source.correction.observe(f64::from(report.freq_error)) {
            return;
        }
        let offset = source.correction.hz() - source.center_hz;
        tracing::info!(
            source = source.idx,
            correction_hz = source.correction.hz(),
            "source clock correction moved"
        );
        for open in self.open.values() {
            let slot = &self.slots[open.num.0 as usize];
            if slot.source == src {
                self.sources[src].channelizer.set_slot(
                    slot.port,
                    open.profile,
                    open.freq_hz + offset,
                );
            }
        }
    }
}

pub struct Radio {
    inner: Mutex<Inner>,
    events: EventSender,
    sink: Arc<dyn AudioSink>,
    cfg: RadioConfig,
}

impl std::fmt::Debug for Radio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let i = self.lock();
        f.debug_struct("Radio")
            .field("sources", &i.sources.len())
            .field("systems", &i.systems.len())
            .field("slots", &i.slots.len())
            .field("open", &i.open.len())
            .finish()
    }
}

impl Radio {
    pub fn new(cfg: RadioConfig, sink: Arc<dyn AudioSink>, events: EventSender) -> Self {
        Self {
            inner: Mutex::new(Inner {
                sources: Vec::new(),
                systems: Vec::new(),
                slots: Vec::new(),
                open: HashMap::new(),
            }),
            events,
            sink,
            cfg,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ── Setup ──

    /// Start a source; returns its index.
    ///
    /// # Errors
    /// If its channelizer or thread cannot be started.
    pub fn add_source(&self, setup: SourceSetup) -> Result<i32> {
        let mut i = self.lock();
        let idx = i32::try_from(i.sources.len()).unwrap_or(i32::MAX);
        let mut source = Source::start(
            idx,
            crate::source::SourceSpec {
                device: setup.device,
                center_hz: setup.center_hz,
                rate_hz: setup.rate_hz,
            },
            setup.stream,
            setup.running,
            self.cfg.conditioning,
            self.events.clone(),
            Arc::clone(&self.sink),
        )?;
        source.set_detector_threshold(setup.signal_detector_threshold);
        source.ppm = setup.ppm;
        source.correction = Correction::new(setup.correction_hz, self.cfg.adaptive_correction);
        i.sources.push(source);
        Ok(idx)
    }

    /// Add a system (numbered by its position).
    ///
    /// # Errors
    /// If it is not the next number.
    pub fn add_system(&self, setup: SystemSetup) -> Result<i32> {
        let mut i = self.lock();
        let sys_num = i32::try_from(i.systems.len()).unwrap_or(i32::MAX);
        if sys_num != setup.sys_num {
            bail!(
                "system {} added out of order (expected {sys_num})",
                setup.sys_num
            );
        }
        let scrambler = scrambler(setup.nac, setup.sys_id, setup.wacn);
        i.systems.push(System {
            setup,
            scrambler,
            sysid_received: false,
            status_received: false,
        });
        Ok(sys_num)
    }

    /// The source a channel at `freq_hz` would open on and how deep in its
    /// band it sits ([`Source::depth`]); `None` if no source covers it.
    pub fn placement(&self, freq_hz: f64) -> Option<(i32, f64)> {
        let i = self.lock();
        i.deepest(freq_hz, |_| true).map(|n| {
            let src = &i.sources[n];
            (src.idx, src.depth(freq_hz))
        })
    }

    /// Run the wideband signal detector on the source covering `freq_hz`.
    ///
    /// # Errors
    /// If no source covers it.
    pub fn detect_signals(&self, freq_hz: f64) -> Result<()> {
        let mut i = self.lock();
        let Some(src) = i.deepest(freq_hz, |_| true) else {
            bail!("no source covers {freq_hz} Hz");
        };
        i.sources[src].attach_detector();
        Ok(())
    }

    // ── Systems ──

    /// Note a system's site (first time only); true if this was it.
    pub fn update_sysid(&self, sys_num: i32, rfss: u8, site: u8) -> bool {
        let mut i = self.lock();
        let Some(s) = i.system_mut(sys_num) else {
            return false;
        };
        if s.sysid_received {
            return false;
        }
        s.sysid_received = true;
        tracing::info!(
            system = %s.setup.short_name, rfss, site_id = site,
            "decoding system site"
        );
        true
    }

    /// Note a system's id and WACN, and with them its scrambling mask
    /// (first time only); true if this was it.
    pub fn update_status(&self, sys_num: i32, sys_id: u16, wacn: u32, nac: u16) -> bool {
        let mut i = self.lock();
        let Some(s) = i.system_mut(sys_num) else {
            return false;
        };
        if s.status_received {
            return false;
        }
        s.status_received = true;
        s.scrambler = scrambler(u64::from(nac), u64::from(sys_id), u64::from(wacn));
        tracing::info!(
            system = %s.setup.short_name,
            sys_id = format_args!("{sys_id:X}"),
            wacn = format_args!("{wacn:X}"),
            nac = format_args!("{nac:X}"),
            "decoding system id"
        );
        true
    }

    // ── Channels ──

    /// Open a `K` channel for `call` on the first source that covers it
    /// with `K`'s profile. It decodes from the next block on.
    ///
    /// # Errors
    /// If the call has a channel already, its system is unknown, no source
    /// carries the channel, or the kind cannot decode it (see
    /// [`OpenError`]).
    pub fn open<K: Kind>(&self, call: &Call, params: &K::Params) -> Result<ChannelNum, OpenError> {
        let mut i = self.lock();
        if i.open.contains_key(&call.id) {
            return Err(OpenError::AlreadyOpen(call.id));
        }
        let sys = usize::try_from(call.sys_num)
            .ok()
            .filter(|&n| n < i.systems.len())
            .ok_or(OpenError::NoSystem(call.sys_num))?;
        let freq_hz = call.channel.freq_hz;
        let src = i
            .deepest(freq_hz, |s| s.offers(K::PROFILE))
            .ok_or(OpenError::NoSource {
                freq_hz,
                kind: K::NAME,
            })?;
        let num = i.take_slot(src, K::NAME);
        let port = i.slot(num).port;
        let (source, system) = (&i.sources[src], &i.systems[sys]);
        let at = Opening::<K> {
            call,
            params,
            system: &system.setup,
            scrambler: system.scrambler.as_ref(),
            channelizer: source.channelizer.config(),
            num,
            source: source.idx,
            port,
            tap_dir: self.cfg.tap_dir.as_deref(),
        };
        let tape = Tape::new(K::AUDIO_RATE.map(|rate| {
            Recording::new(Accounting::new(
                &system.setup.short_name,
                call.talkgroup,
                freq_hz,
                call.channel.slot.is_some(),
                rate,
            ))
        }));
        let decoder = K::decoder(&at, tape.clone())?;
        let tuning = K::tuning(&at);
        let chz = &source.channelizer;
        chz.set_slot_conditioning(port, tuning.conditioning);
        if let Some((cutoff, transition)) = tuning.filter {
            chz.set_slot_filter(port, cutoff, transition);
        }
        if let Some(db) = tuning.squelch_db {
            chz.set_slot_squelch_db(port, db);
        }
        chz.set_slot_decoder(port, Some(Box::new(decoder)));
        chz.set_slot(
            port,
            K::PROFILE,
            freq_hz + source.correction.hz() - source.center_hz,
        );
        let slot = &mut i.slots[num.0 as usize];
        slot.holder = Some(call.id);
        slot.kind = K::NAME;
        i.open.insert(
            call.id,
            Open {
                call: call.id,
                num,
                kind: K::NAME,
                profile: K::PROFILE,
                freq_hz,
                tape,
                finished: K::finished,
            },
        );
        Ok(num)
    }

    /// Close the call's channel: what it was, with the audio its
    /// audibility hold still kept already sent under the call (so it
    /// precedes the call's end). `None` if it had none.
    pub fn close(&self, id: CallId) -> Option<ChannelReport> {
        let mut i = self.lock();
        let open = i.open.remove(&id)?;
        let recording = self.finish(&open, open.tape.close());
        let report = i.report(&open, recording);
        let slot = &mut i.slots[open.num.0 as usize];
        slot.holder = None;
        let (src, port) = (slot.source, slot.port);
        i.sources[src].channelizer.release_slot(port);
        i.track(src, &report);
        Some(report)
    }

    /// End the recording on the call's channel and start the next, the
    /// channel staying open (a conventional channel's transmission is
    /// over): what was recorded, its held audio already sent.
    pub fn rotate(&self, id: CallId) -> Option<ChannelReport> {
        let mut i = self.lock();
        let open = i.open.get(&id)?;
        let recording = self.finish(open, open.tape.rotate());
        let report = i.report(open, recording);
        let src = i.slot(open.num).source;
        i.track(src, &report);
        Some(report)
    }

    /// A recording ended: send its held audio, tell its kind.
    fn finish(&self, open: &Open, done: Option<(Accounting, Vec<i16>)>) -> Option<Accounting> {
        let (acct, tail) = done?;
        if !tail.is_empty() {
            self.sink.audio(open.call, &tail);
        }
        (open.finished)(&acct);
        Some(acct)
    }

    /// The call's channel as it is now.
    pub fn reading(&self, id: CallId) -> Option<ChannelReport> {
        let i = self.lock();
        let open = i.open.get(&id)?;
        Some(i.report(open, open.tape.accounting()))
    }

    /// The unit the state machine says is talking on the call.
    pub fn set_source(&self, id: CallId, unit: u32) {
        let i = self.lock();
        if let Some(open) = i.open.get(&id) {
            let slot = i.slot(open.num);
            i.sources[slot.source]
                .channelizer
                .send(slot.port, Cmd::Source(unit));
        }
    }

    // ── Status ──

    /// Each source's clock correction now, Hz, by source index.
    pub fn corrections(&self) -> Vec<(i32, f64)> {
        let i = self.lock();
        i.sources
            .iter()
            .map(|s| (s.idx, s.correction.hz()))
            .collect()
    }

    /// Every slot the radio has used, by number.
    pub fn channels(&self) -> Vec<ChannelStatus> {
        let i = self.lock();
        i.slots
            .iter()
            .enumerate()
            .map(|(n, slot)| {
                let s = &i.sources[slot.source];
                ChannelStatus {
                    num: ChannelNum(u32::try_from(n).unwrap_or(u32::MAX)),
                    source_num: s.idx,
                    source_center: s.center_hz,
                    source_device: s.device.clone(),
                    kind: slot.kind,
                    call: slot.holder,
                }
            })
            .collect()
    }

    /// Every source delivered samples since the last check.
    ///
    /// # Errors
    /// Naming the first that did not.
    pub fn check_sources(&self) -> Result<()> {
        let i = self.lock();
        for s in &i.sources {
            if !s.got_samples() {
                bail!("Source {} ({}) stopped receiving samples", s.idx, s.device);
            }
        }
        Ok(())
    }

    /// Stop every source thread and device.
    pub fn stop(&self) {
        let mut i = self.lock();
        for s in &mut i.sources {
            s.stop();
        }
    }
}

impl Drop for Radio {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analog::{AnalogFm, AnalogParams};
    use crate::p25::P25Control;
    use crate::p25_voice::{Keying, P25Voice};
    use crate::types::{Channel, RadioEvent};
    use num_complex::Complex32;

    struct NoAudio;
    impl AudioSink for NoAudio {
        fn audio(&self, _: CallId, _: &[i16]) {}
    }

    fn radio() -> (Radio, tokio::sync::mpsc::Receiver<RadioEvent>) {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let events = EventSender::new(tx, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        (
            Radio::new(RadioConfig::default(), Arc::new(NoAudio), events),
            rx,
        )
    }

    /// A silent 2 s cf32 file as a source.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn silent_source(rate_hz: f64, center_hz: f64) -> SourceSetup {
        let path =
            std::env::temp_dir().join(format!("trunk-radio-{}-{rate_hz}.cf32", std::process::id()));
        let n = (rate_hz * 2.0) as usize;
        let bytes: Vec<u8> = std::iter::repeat_n(Complex32::new(0.001, 0.0), n)
            .flat_map(|c| [c.re.to_le_bytes(), c.im.to_le_bytes()].concat())
            .collect();
        std::fs::write(&path, bytes).unwrap();
        let (stream, running) = trunk_sdr::file::play(
            &trunk_sdr::FileSpec {
                path,
                format: trunk_sdr::Format::Cf32,
                rate_hz,
                repeat: true,
            },
            8192,
            rate_hz as usize,
        )
        .unwrap();
        SourceSetup {
            device: "file".into(),
            center_hz,
            rate_hz,
            stream,
            running,
            signal_detector_threshold: None,
            ppm: 0.0,
            correction_hz: 25.0,
        }
    }

    fn system(sys_num: i32, wacn: u64) -> SystemSetup {
        SystemSetup {
            sys_num,
            short_name: format!("s{sys_num}"),
            qpsk: true,
            max_dev: 5000,
            noise_suppression: None,
            analog_levels: 1.0,
            digital_levels: 1.0,
            nac: 0x38d * u64::from(wacn != 0),
            sys_id: 0x3a2 * u64::from(wacn != 0),
            wacn,
        }
    }

    fn call(id: u64, sys_num: i32, freq_hz: f64, slot: Option<u8>) -> Call {
        Call {
            id: CallId(id),
            sys_num,
            talkgroup: 929,
            channel: Channel { freq_hz, slot },
        }
    }

    #[test]
    fn channels_open_on_a_covering_slot_and_slots_are_reused_by_kind() {
        let (radio, _rx) = radio();
        radio
            .add_source(silent_source(2_496_000.0, 771_496_500.0))
            .unwrap();
        radio.add_system(system(0, 0)).unwrap();
        radio.add_system(system(1, 0xbee00)).unwrap();
        let open_p25 = |id, sys, freq, slot| {
            radio.open::<P25Voice>(&call(id, sys, freq, slot), &Keying::Trunked)
        };
        assert_eq!(
            radio.open::<P25Control>(&call(1, 0, 772_031_250.0, None), &()),
            Ok(ChannelNum(0))
        );
        assert_eq!(open_p25(2, 0, 771_881_250.0, None), Ok(ChannelNum(1)));
        assert_eq!(
            open_p25(2, 0, 771_881_250.0, None),
            Err(OpenError::AlreadyOpen(CallId(2)))
        );
        assert_eq!(
            open_p25(3, 0, 771_431_250.0, Some(1)),
            Err(OpenError::NoScramblingMask),
            "phase 2 without the system's identity"
        );
        assert_eq!(
            open_p25(3, 1, 771_431_250.0, Some(1)),
            Ok(ChannelNum(2)),
            "a failed open gave its slot back"
        );
        assert_eq!(
            open_p25(4, 0, 900e6, None),
            Err(OpenError::NoSource {
                freq_hz: 900e6,
                kind: "p25"
            })
        );
        assert_eq!(
            open_p25(5, 7, 771_431_250.0, None),
            Err(OpenError::NoSystem(7))
        );

        let report = radio.close(CallId(2)).unwrap();
        assert_eq!(
            (report.num, report.kind, report.source),
            (ChannelNum(1), "p25", 0)
        );
        assert_eq!(radio.corrections(), [(0, 25.0)], "not adaptive: fixed");
        assert!(report.transmissions().is_empty(), "no audio was decoded");
        assert!(radio.close(CallId(2)).is_none());
        assert!(
            radio.reading(CallId(1)).unwrap().recording.is_none(),
            "control: none"
        );
        assert!(radio.reading(CallId(3)).unwrap().recording.is_some());
        // a P25 call takes the slot P25 last had, not a fresh one
        assert_eq!(open_p25(6, 0, 770_306_250.0, None), Ok(ChannelNum(1)));
        let rotated = radio.rotate(CallId(6)).unwrap();
        assert_eq!(rotated.num, ChannelNum(1));
        assert!(radio.reading(CallId(6)).is_some(), "still open");
        radio.set_source(CallId(6), 1234);

        let status = radio.channels();
        assert_eq!(
            status
                .iter()
                .map(|c| (c.num.0, c.kind, c.call.map(|c| c.0)))
                .collect::<Vec<_>>(),
            vec![
                (0, "p25-control", Some(1)),
                (1, "p25", Some(6)),
                (2, "p25", Some(3))
            ]
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(radio.check_sources().is_ok());
    }

    #[test]
    fn a_kind_needs_a_source_with_its_profile() {
        let (radio, _rx) = radio();
        // too narrow for a wide (analog) channel, wide enough for P25
        radio
            .add_source(silent_source(120_000.0, 488_661_750.0))
            .unwrap();
        radio.add_system(system(0, 0)).unwrap();
        let freq = 488_670_000.0;
        assert_eq!(
            radio.open::<AnalogFm>(&call(1, 0, freq, None), &AnalogParams::default()),
            Err(OpenError::NoSource {
                freq_hz: freq,
                kind: "analog"
            })
        );
        assert!(
            radio
                .open::<P25Voice>(
                    &call(2, 0, freq, None),
                    &Keying::Carrier { squelch_db: -50.0 }
                )
                .is_ok()
        );
        radio.detect_signals(freq).unwrap();
        assert!(radio.detect_signals(900e6).is_err());
    }
}
