//! The radio as the call state machine drives it: sources with their
//! recorder pools, systems with their control-channel decoders, and the
//! calls that bind a recorder for a while.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Result, anyhow, bail};
use trunk_channelizer::ConditioningSettings;
use trunk_p25::Modulation;
use trunk_sdr::{Running, Stream};

use crate::recorder::{DB_UNSET, Kind, Recorder, Slot, SystemParams};
use crate::source::Source;
use crate::types::{
    AudioSink, EventSender, TrChannelConfig, TrRecorderConfig, TrRecorderStatus,
    TrStopRecordingResult, TrTransmission, TrTrunkMessage,
};

#[derive(Clone, Debug, Default)]
pub struct RadioConfig {
    /// Where baseline taps go (`trunk-replay --tap`); none in production.
    pub tap_dir: Option<String>,
    pub conditioning: ConditioningSettings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemKind {
    /// Trunked P25: control channels decoded, voice on pool recorders.
    P25,
    /// Conventional analog channels, each with its own recorder.
    Conventional,
    /// Conventional P25 channels, each with its own recorder.
    ConventionalP25,
}

pub struct SystemSetup {
    pub sys_num: i32,
    pub short_name: String,
    pub kind: SystemKind,
    /// CQPSK (else C4FM) on the control and voice channels.
    pub qpsk: bool,
    pub control_channels: Vec<f64>,
    pub max_dev: i32,
    pub analog_levels: f64,
    pub digital_levels: f64,
}

pub struct SourceSetup {
    pub device: String,
    pub center_hz: f64,
    pub rate_hz: f64,
    pub stream: Stream,
    pub running: Box<dyn Running>,
    pub digital_recorders: i32,
    pub analog_recorders: i32,
    pub signal_detector_threshold: Option<f32>,
}

struct System {
    setup: SystemSetup,
    /// (source, slot, frequency) of each control channel decoder.
    decoders: Vec<(usize, usize, f64)>,
    current_control_channel: usize,
    sysid_received: bool,
    status_received: bool,
}

impl System {
    fn params(&self) -> SystemParams {
        SystemParams {
            sys_num: self.setup.sys_num,
            short_name: self.setup.short_name.clone(),
            qpsk: self.setup.qpsk,
            max_dev: self.setup.max_dev,
            analog_levels: self.setup.analog_levels,
            digital_levels: self.setup.digital_levels,
        }
    }
}

struct Call {
    sys_num: i32,
    /// Index into `recorders`.
    recorder: Option<usize>,
}

struct Inner {
    sources: Vec<Source>,
    systems: Vec<System>,
    recorders: Vec<Recorder>,
    calls: HashMap<u64, Call>,
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
            .field("recorders", &i.recorders.len())
            .field("calls", &i.calls.len())
            .finish()
    }
}

impl Radio {
    pub fn new(cfg: RadioConfig, sink: Arc<dyn AudioSink>, events: EventSender) -> Self {
        Self {
            inner: Mutex::new(Inner {
                sources: Vec::new(),
                systems: Vec::new(),
                recorders: Vec::new(),
                calls: HashMap::new(),
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

    /// Start a source and its recorder pool; returns its index.
    pub fn add_source(&self, setup: SourceSetup) -> Result<i32> {
        let mut i = self.lock();
        let idx = i32::try_from(i.sources.len()).unwrap_or(i32::MAX);
        let mut source = Source::start(
            idx,
            crate::source::SourceSpec {
                device: setup.device.clone(),
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
        if setup.analog_recorders > 0 && !source.has_wide() {
            bail!(
                "source '{}' at {} S/s cannot carry analog channels; raise its rate",
                setup.device,
                setup.rate_hz
            );
        }
        let source_idx = i.sources.len();
        i.sources.push(source);
        for (count, kind) in [
            (setup.digital_recorders, Kind::P25),
            (setup.analog_recorders, Kind::Analog),
        ] {
            for _ in 0..count.max(0) {
                Self::add_recorder(&mut i, source_idx, kind, false, 0.0);
            }
        }
        Ok(idx)
    }

    fn add_recorder(
        i: &mut Inner,
        source_idx: usize,
        kind: Kind,
        conventional: bool,
        tone_hz: f64,
    ) -> usize {
        let num = i32::try_from(i.recorders.len()).unwrap_or(i32::MAX);
        let source = &mut i.sources[source_idx];
        let port = source.alloc_port();
        let rec = Recorder::new(
            num,
            &Slot {
                source: source_idx,
                port,
                channelizer: &source.channelizer,
                center_hz: source.center_hz,
            },
            kind,
            conventional,
            tone_hz,
        );
        i.recorders.push(rec);
        i.recorders.len() - 1
    }

    /// Add a system; a trunked one gets a decoder on every control channel
    /// some source covers. Returns its number (its position).
    pub fn add_system(&self, setup: SystemSetup) -> Result<i32> {
        let mut i = self.lock();
        let sys_num = i32::try_from(i.systems.len()).unwrap_or(i32::MAX);
        if sys_num != setup.sys_num {
            bail!(
                "system {} added out of order (expected {sys_num})",
                setup.sys_num
            );
        }
        let mut decoders = Vec::new();
        if setup.kind == SystemKind::P25 {
            let modulation = if setup.qpsk {
                Modulation::Cqpsk
            } else {
                Modulation::C4fm
            };
            for &freq in &setup.control_channels {
                let Some(src_idx) = i.sources.iter().position(|s| s.covers(freq)) else {
                    tracing::warn!(
                        system = %setup.short_name, freq_hz = freq,
                        "no source covers the control channel; skipping"
                    );
                    continue;
                };
                #[allow(clippy::cast_possible_truncation)]
                let tap_base = self.cfg.tap_dir.as_ref().map_or_else(String::new, |d| {
                    format!("{d}/cc_{}_{}", setup.short_name, freq as i64)
                });
                let port = i.sources[src_idx].add_control_channel(
                    freq,
                    modulation,
                    sys_num,
                    &setup.short_name,
                    &tap_base,
                );
                tracing::info!(system = %setup.short_name, freq_hz = freq, source = src_idx, port, "control channel decoder started");
                decoders.push((src_idx, port, freq));
            }
            if decoders.len() > 1 {
                tracing::warn!(system = %setup.short_name, decoders = decoders.len(), "decoders active");
            }
        }
        i.systems.push(System {
            setup,
            decoders,
            current_control_channel: 0,
            sysid_received: false,
            status_received: false,
        });
        Ok(sys_num)
    }

    // ── Systems ──

    pub fn add_control_channel(&self, sys_num: i32, freq: f64) {
        let mut i = self.lock();
        if let Some(s) = usize::try_from(sys_num)
            .ok()
            .and_then(|n| i.systems.get_mut(n))
            && !s.setup.control_channels.contains(&freq)
        {
            s.setup.control_channels.push(freq);
        }
    }

    /// Note a system's site (first time only); true if this was it.
    pub fn update_sysid(&self, msg: &TrTrunkMessage) -> bool {
        let mut i = self.lock();
        let Some(s) = usize::try_from(msg.sys_num)
            .ok()
            .and_then(|n| i.systems.get_mut(n))
        else {
            return false;
        };
        if s.setup.kind == SystemKind::Conventional || s.sysid_received {
            return false;
        }
        s.sysid_received = true;
        tracing::info!(
            system = %s.setup.short_name, rfss = msg.sys_rfss, site_id = msg.sys_site_id,
            "decoding system site"
        );
        true
    }

    /// Note a system's id and WACN (first time only); true if this was it.
    pub fn update_status(&self, msg: &TrTrunkMessage) -> bool {
        let mut i = self.lock();
        let Some(s) = usize::try_from(msg.sys_num)
            .ok()
            .and_then(|n| i.systems.get_mut(n))
        else {
            return false;
        };
        if s.status_received {
            return false;
        }
        s.status_received = true;
        tracing::info!(
            system = %s.setup.short_name,
            sys_id = format_args!("{:X}", msg.sys_id),
            wacn = format_args!("{:X}", msg.wacn),
            nac = format_args!("{:X}", msg.nac),
            "decoding system id"
        );
        true
    }

    pub fn current_control_channel(&self, sys_num: i32) -> f64 {
        let i = self.lock();
        usize::try_from(sys_num)
            .ok()
            .and_then(|n| i.systems.get(n))
            .and_then(|s| {
                s.setup
                    .control_channels
                    .get(s.current_control_channel)
                    .copied()
            })
            .unwrap_or(0.0)
    }

    /// The strongest control channel decoder's power (dB), NaN with none.
    pub fn control_channel_pwr(&self, sys_num: i32) -> f64 {
        let i = self.lock();
        let Some(s) = usize::try_from(sys_num).ok().and_then(|n| i.systems.get(n)) else {
            return f64::NAN;
        };
        s.decoders
            .iter()
            .map(|&(src, port, _)| i.sources[src].channelizer.slot_power_db(port))
            .filter(|p| !p.is_nan())
            .fold(
                f64::NAN,
                |best, p| if best.is_nan() || p > best { p } else { best },
            )
    }

    // ── Calls ──

    /// A trunked call, with no recorder yet.
    pub fn create_call(&self, call_id: u64, sys_num: i32) {
        self.lock().calls.insert(
            call_id,
            Call {
                sys_num,
                recorder: None,
            },
        );
    }

    /// A recorder of its own for a conventional channel on `source_idx`,
    /// for a system of that kind; returns the recorder's number. The call
    /// is bound to it with [`Self::bind_conventional_call`] once the state
    /// machine has numbered it.
    pub fn add_conventional_recorder(
        &self,
        sys_num: i32,
        source_idx: i32,
        channel: &TrChannelConfig,
    ) -> Result<i32> {
        let mut i = self.lock();
        let kind = match usize::try_from(sys_num)
            .ok()
            .and_then(|n| i.systems.get(n))
            .map(|s| s.setup.kind)
        {
            Some(SystemKind::Conventional) => Kind::Analog,
            Some(SystemKind::ConventionalP25) => Kind::P25,
            Some(SystemKind::P25) => bail!("system {sys_num} is trunked"),
            None => bail!("no system {sys_num}"),
        };
        let src = usize::try_from(source_idx)
            .ok()
            .filter(|&s| s < i.sources.len())
            .ok_or_else(|| anyhow!("no source {source_idx}"))?;
        if kind == Kind::Analog && !i.sources[src].has_wide() {
            bail!(
                "source {source_idx} at {} S/s cannot carry an analog channel; raise its rate",
                i.sources[src].rate_hz
            );
        }
        i.sources[src].attach_detector();
        let rec = Self::add_recorder(&mut i, src, kind, true, channel.tone);
        Ok(i.recorders[rec].num)
    }

    /// Bind a conventional call to the recorder made for its channel.
    pub fn bind_conventional_call(&self, call_id: u64, sys_num: i32, rec_num: i32) {
        let mut i = self.lock();
        let recorder = i.recorders.iter().position(|r| r.num == rec_num);
        i.calls.insert(call_id, Call { sys_num, recorder });
    }

    pub fn remove_call(&self, call_id: u64) {
        self.lock().calls.remove(&call_id);
    }

    /// Pool recorders of a kind free on a source.
    pub fn available_recorders(&self, source_idx: i32, analog: bool) -> i32 {
        let i = self.lock();
        let kind = if analog { Kind::Analog } else { Kind::P25 };
        let n = i
            .recorders
            .iter()
            .filter(|r| {
                !r.conventional
                    && r.kind == kind
                    && i32::try_from(r.source).is_ok_and(|s| s == source_idx)
                    && r.available()
            })
            .count();
        i32::try_from(n).unwrap_or(i32::MAX)
    }

    /// Take a free pool recorder on `source_idx` for `call_id` and start
    /// it; returns its number.
    pub fn start_recording(
        &self,
        source_idx: i32,
        call_id: u64,
        analog: bool,
        cfg: &TrRecorderConfig,
    ) -> Option<i32> {
        let mut i = self.lock();
        let sys_num = i.calls.get(&call_id)?.sys_num;
        let params = usize::try_from(sys_num)
            .ok()
            .and_then(|n| i.systems.get(n))?
            .params();
        let kind = if analog { Kind::Analog } else { Kind::P25 };
        let idx = i.recorders.iter().position(|r| {
            !r.conventional
                && r.kind == kind
                && i32::try_from(r.source).is_ok_and(|s| s == source_idx)
                && r.available()
        })?;
        if !i.recorders[idx].start(cfg, &params) {
            return None;
        }
        let num = i.recorders[idx].num;
        if let Some(c) = i.calls.get_mut(&call_id) {
            c.recorder = Some(idx);
        }
        Some(num)
    }

    /// Stop `rec`, sending the audio its audibility hold still kept under
    /// the call before the state machine ends it.
    fn stop_recorder(&self, rec: &mut Recorder) -> TrStopRecordingResult {
        let s = rec.stop();
        if !s.tail.is_empty() {
            self.sink.audio(s.call, &s.tail);
        }
        s.result
    }

    /// The recorder recording `call_id`, if one is bound to it.
    fn bound(i: &Inner, call_id: u64) -> Option<usize> {
        i.calls.get(&call_id)?.recorder
    }

    /// The call is known to the radio (created, not yet removed).
    pub fn has_call(&self, call_id: u64) -> bool {
        self.lock().calls.contains_key(&call_id)
    }

    pub fn has_recorder(&self, call_id: u64) -> bool {
        Self::bound(&self.lock(), call_id).is_some()
    }

    pub fn recorder_num(&self, call_id: u64) -> i32 {
        let i = self.lock();
        Self::bound(&i, call_id).map_or(0, |r| i.recorders[r].num)
    }

    /// Stop the call's recorder (the binding stays until the call is removed
    /// so its transmission list can still be read).
    pub fn stop_recording(&self, call_id: u64) -> TrStopRecordingResult {
        let mut i = self.lock();
        let Some(r) = Self::bound(&i, call_id) else {
            return TrStopRecordingResult::default();
        };
        let rec = &mut i.recorders[r];
        if rec.call() == Some(call_id) {
            self.stop_recorder(rec)
        } else {
            // Stopped already (and possibly recording another call now).
            TrStopRecordingResult {
                freq_error: rec.freq_error(),
                recorder_pwr: rec.pwr_db(),
                has_recorder: true,
            }
        }
    }

    pub fn transmission_list(&self, call_id: u64) -> Vec<TrTransmission> {
        let i = self.lock();
        Self::bound(&i, call_id).map_or_else(Vec::new, |r| {
            let rec = &i.recorders[r];
            if rec.call().is_some_and(|c| c != call_id) {
                Vec::new()
            } else {
                rec.transmission_list()
            }
        })
    }

    /// Start a conventional call's recorder (the first time, and again
    /// after each idle timeout).
    pub fn restart(&self, call_id: u64, cfg: &TrRecorderConfig) {
        let mut i = self.lock();
        let Some(r) = Self::bound(&i, call_id) else {
            return;
        };
        let Some(params) = i
            .calls
            .get(&call_id)
            .and_then(|c| usize::try_from(c.sys_num).ok())
            .and_then(|n| i.systems.get(n))
            .map(System::params)
        else {
            return;
        };
        let rec = &mut i.recorders[r];
        if rec.call().is_some() {
            self.stop_recorder(rec);
        }
        rec.start(cfg, &params);
    }

    pub fn set_recorder_enabled(&self, call_id: u64, enabled: bool) {
        let i = self.lock();
        if let Some(r) = Self::bound(&i, call_id) {
            i.recorders[r].set_enabled(enabled);
        }
    }

    pub fn set_source(&self, call_id: u64, src: i64) {
        let i = self.lock();
        if let Some(r) = Self::bound(&i, call_id) {
            i.recorders[r].set_source(src);
        }
    }

    pub fn set_squelch_db(&self, call_id: u64, db: f64) {
        let i = self.lock();
        if let Some(r) = Self::bound(&i, call_id) {
            i.recorders[r].set_squelch_db(db);
        }
    }

    /// The recorder's channel power (dB; [`DB_UNSET`] without one).
    pub fn pwr_db(&self, call_id: u64) -> f64 {
        let i = self.lock();
        Self::bound(&i, call_id).map_or(DB_UNSET, |r| i.recorders[r].pwr_db())
    }

    pub fn since_last_write_secs(&self, call_id: u64) -> f64 {
        let i = self.lock();
        Self::bound(&i, call_id).map_or(1e9, |r| i.recorders[r].since_last_write_secs())
    }

    pub fn since_last_voice_secs(&self, call_id: u64) -> f64 {
        let i = self.lock();
        Self::bound(&i, call_id).map_or(1e9, |r| i.recorders[r].since_last_voice_secs())
    }

    // ── Status ──

    pub fn recorder_status(&self) -> Vec<TrRecorderStatus> {
        let i = self.lock();
        let mut out: Vec<TrRecorderStatus> = i
            .recorders
            .iter()
            .map(|r| {
                let s = &i.sources[r.source];
                TrRecorderStatus {
                    source_num: s.idx,
                    source_center: s.center_hz,
                    source_device: s.device.clone(),
                    rec_num: r.num,
                    rec_type: r.type_string().into(),
                    rec_state: r.state() as i32,
                }
            })
            .collect();
        out.sort_by_key(|r| (r.source_num, r.rec_num));
        out
    }

    /// Every source delivered samples since the last check.
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
    use crate::types::RadioEvent;
    use num_complex::Complex32;

    struct NoAudio;
    impl AudioSink for NoAudio {
        fn audio(&self, _: u64, _: &[i16]) {}
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
    fn silent_source(rate_hz: f64) -> SourceSetup {
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
            center_hz: 771_496_500.0,
            rate_hz,
            stream,
            running,
            digital_recorders: 2,
            analog_recorders: 1,
            signal_detector_threshold: None,
        }
    }

    #[test]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn pool_recorders_are_taken_started_stopped_and_freed() {
        let (radio, _rx) = radio();
        let src = radio.add_source(silent_source(2_496_000.0)).unwrap();
        assert_eq!(src, 0);
        radio
            .add_system(SystemSetup {
                sys_num: 0,
                short_name: "smco".into(),
                kind: SystemKind::P25,
                qpsk: true,
                control_channels: vec![772_031_250.0, 900e6],
                max_dev: 2500,
                analog_levels: 1.0,
                digital_levels: 1.0,
            })
            .unwrap();
        assert_eq!(radio.available_recorders(0, false), 2);
        assert_eq!(radio.available_recorders(0, true), 1);
        radio.create_call(11, 0);
        let cfg = TrRecorderConfig {
            talkgroup: 929,
            call_num: 1,
            freq: 771_881_250.0,
            short_name: "smco".into(),
            temp_dir: String::new(),
            phase2_tdma: false,
            tdma_slot: 0,
            xor_mask: vec![],
            squelch_db: 0.0,
            digital_levels: 1.0,
            rust_call_id: 11,
        };
        let num = radio.start_recording(0, 11, false, &cfg).unwrap();
        assert_eq!(radio.recorder_num(11), num);
        assert!(radio.has_recorder(11));
        assert_eq!(radio.available_recorders(0, false), 1);
        // TDMA without a mask is refused
        radio.create_call(12, 0);
        assert!(
            radio
                .start_recording(
                    0,
                    12,
                    false,
                    &TrRecorderConfig {
                        phase2_tdma: true,
                        rust_call_id: 12,
                        ..cfg
                    }
                )
                .is_none()
        );
        assert_eq!(radio.available_recorders(0, false), 1);
        let r = radio.stop_recording(11);
        assert!(r.has_recorder);
        assert_eq!(radio.available_recorders(0, false), 2);
        assert!(
            radio.transmission_list(11).is_empty(),
            "no audio was decoded"
        );
        radio.remove_call(11);
        assert!(!radio.has_recorder(11));
        assert_eq!(radio.current_control_channel(0), 772_031_250.0);
        let status = radio.recorder_status();
        assert_eq!(status.len(), 3);
        assert!(status.iter().all(|s| s.rec_state == 7));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(radio.check_sources().is_ok());
    }

    #[test]
    fn conventional_calls_get_their_own_recorder_and_the_detector() {
        let (radio, _rx) = radio();
        radio.add_source(silent_source(912_000.0)).unwrap();
        radio
            .add_system(SystemSetup {
                sys_num: 0,
                short_name: "conv".into(),
                kind: SystemKind::Conventional,
                qpsk: false,
                control_channels: vec![],
                max_dev: 5000,
                analog_levels: 1.0,
                digital_levels: 1.0,
            })
            .unwrap();
        let ch = TrChannelConfig {
            id: 2,
            freq: 771_600_000.0,
            tone: 162.2,
            squelch_db: -50.0,
            signal_detection: false,
        };
        let num = radio.add_conventional_recorder(0, 0, &ch).unwrap();
        radio.bind_conventional_call(5, 0, num);
        assert_eq!(radio.recorder_num(5), num);
        // the pool is untouched
        assert_eq!(radio.available_recorders(0, true), 1);
        let cfg = TrRecorderConfig {
            talkgroup: 2,
            call_num: 1,
            freq: ch.freq,
            short_name: "conv".into(),
            temp_dir: String::new(),
            phase2_tdma: false,
            tdma_slot: 0,
            xor_mask: vec![],
            squelch_db: -50.0,
            digital_levels: 1.0,
            rust_call_id: 5,
        };
        radio.restart(5, &cfg);
        radio.set_recorder_enabled(5, true);
        radio.restart(5, &cfg); // the idle timeout's restart stops and starts
        assert!(radio.has_recorder(5));
        assert!(radio.pwr_db(5) >= DB_UNSET || radio.pwr_db(5) < 0.0);
        assert!(radio.since_last_write_secs(5) < 10.0);
        assert!(radio.add_conventional_recorder(9, 0, &ch).is_err());
    }
}
