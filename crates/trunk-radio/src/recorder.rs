//! A recorder: a channelizer slot with a decoder on it, and the control side
//! the state machine drives.
//!
//! P25 recorders decode voice to audio with trunk-p25; analog recorders
//! demodulate FM and gate it with trunk-fm. Both hand their audio to the
//! [`AudioSink`] and report in-band events through the [`EventSender`],
//! from the source's DSP thread.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use num_complex::Complex32;
use trunk_channelizer::{
    ChannelDecoder, ConditioningSpec, NARROW, ProfileId, SharedChannelizer, SquelchMode, WIDE,
};
use trunk_fm::gate::{Config as GateConfig, Event as GateEvent, Gate};
use trunk_fm::{Audio, Decimation, Detector, FmDemod, Measurement, TAU_US, Tone};

use crate::alias;
use crate::fm;
use crate::p25_calls;
use crate::p25_voice::P25VoiceHandle;
use crate::types::{
    AudioSink, CallCounts, EventSender, RadioEvent, RecorderState, TrCallReport, TrRecorderConfig,
    TrStopRecordingResult, TrTransmission, VoiceTagKind,
};

/// Conditioned rate of a P25 slot: 5 × 4800 (phase 1) = 4 × 6000 (phase 2).
pub const NARROW_RATE: f64 = 24_000.0;
/// Conditioned rate of an analog slot.
pub const WIDE_RATE: f64 = 96_000.0;
/// Conditioner roll-off, as `channel_control` set it.
const EXCESS_BW: f32 = 0.2;
pub(crate) const EXCESS_BW_PUB: f32 = EXCESS_BW;
/// The analog channel's width, for its filter (cutoff `max_dev`,
/// transition half of this).
const ANALOG_BANDWIDTH_HZ: f64 = 12_000.0;
/// A sample at or above this is voice, not digital silence (~-66 dBFS).
const VOICE_THRESHOLD: i16 = 16;
/// What `get_pwr()` returned with no reading: trunk-recorder's `DB_UNSET`.
pub const DB_UNSET: f64 = 999.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    P25,
    Analog,
}

/// What a system contributes to a recorder's start.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemParams {
    pub sys_num: i32,
    pub short_name: String,
    pub qpsk: bool,
    pub max_dev: i32,
    pub analog_levels: f64,
    pub digital_levels: f64,
}

/// A call's start, as the decoder receives it.
#[derive(Clone, Debug)]
struct Start {
    rust_call_id: u64,
    sys_num: i32,
    system: String,
    talkgroup: i64,
    freq: f64,
    qpsk: bool,
    phase2: bool,
    slot: i32,
    xor_mask: Vec<u8>,
    levels: f64,
    tone: Option<Tone>,
}

enum Cmd {
    Start(Start),
    Stop,
}

/// One recording's accounting (what `headless_sink` kept).
#[derive(Clone, Debug)]
struct Recording {
    state: RecorderState,
    start: SystemTime,
    last_write: Instant,
    last_voice: Instant,
    sample_count: i64,
    term_events: i64,
    after_term_samples: i64,
    talkgroup: i64,
    freq: f64,
    /// The decoder's counts so far this call (P25).
    counts: CallCounts,
    /// Units heard in band, in order of first appearance (P25).
    sources: Vec<i64>,
    phase2: bool,
}

impl Default for Recording {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            state: RecorderState::Available,
            start: SystemTime::now(),
            last_write: now,
            last_voice: now,
            sample_count: 0,
            term_events: 0,
            after_term_samples: 0,
            talkgroup: 0,
            freq: 0.0,
            counts: CallCounts::default(),
            sources: Vec::new(),
            phase2: false,
        }
    }
}

impl Recording {
    fn start(&mut self, s: &Start) {
        let now = Instant::now();
        *self = Self {
            state: RecorderState::Idle,
            start: SystemTime::now(),
            last_write: now,
            last_voice: now,
            talkgroup: s.talkgroup,
            freq: s.freq,
            phase2: s.phase2,
            ..Self::default()
        };
    }

    /// Audio was written: `n` samples, voice among them or not.
    fn wrote(&mut self, n: usize, voice: bool) {
        let now = Instant::now();
        if self.state == RecorderState::Idle {
            self.state = RecorderState::Recording;
        }
        self.sample_count += i64::try_from(n).unwrap_or(0);
        self.last_write = now;
        if voice {
            self.last_voice = now;
        }
    }

    fn transmission(&self) -> Option<TrTransmission> {
        if self.sample_count == 0 {
            return None;
        }
        let secs = |t: SystemTime| {
            i64::try_from(t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())).unwrap_or(0)
        };
        Some(TrTransmission {
            source: 0,
            talkgroup: self.talkgroup,
            slot: 0,
            color_code: 0,
            start_time: secs(self.start),
            stop_time: secs(SystemTime::now()),
            sample_count: self.sample_count,
            spike_count: i64::try_from(self.counts.spikes).unwrap_or(i64::MAX),
            error_count: i64::try_from(self.counts.errors).unwrap_or(i64::MAX),
            term_events: self.term_events,
            after_term_samples: self.after_term_samples,
            freq: self.freq,
        })
    }
}

/// What the DSP thread and the control side share.
struct Shared {
    cmd: Mutex<Option<Cmd>>,
    rec: Mutex<Recording>,
    /// The decoder is running a call.
    active: AtomicBool,
    rust_call_id: AtomicU64,
    /// The unit talking, as last told (P25: for Harris aliases).
    source_id: AtomicI64,
}

impl Shared {
    fn rec(&self) -> std::sync::MutexGuard<'_, Recording> {
        self.rec.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take_cmd(&self) -> Option<Cmd> {
        self.cmd
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// Level, then PCM: `k` is `digital_levels`/`analog_levels`.
#[allow(clippy::cast_possible_truncation)]
fn level_i16(s: i16, k: f64) -> i16 {
    (f64::from(s) * k).round().clamp(-32768.0, 32767.0) as i16
}

#[allow(clippy::cast_possible_truncation)]
fn level_f32(s: f32, k: f32) -> i16 {
    (s * k * 32767.0).round().clamp(-32768.0, 32767.0) as i16
}

// ── P25 ──

struct P25Slot {
    num: i32,
    voice: P25VoiceHandle,
    shared: Arc<Shared>,
    sink: Arc<dyn AudioSink>,
    events: EventSender,
    audio: Vec<i16>,
    tags: Vec<crate::types::VoiceTag>,
    call: Option<Start>,
    levels: f64,
    last_src: i64,
}

impl std::fmt::Debug for P25Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P25Slot")
            .field("num", &self.num)
            .finish_non_exhaustive()
    }
}

impl P25Slot {
    fn apply(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start(s) => {
                self.voice
                    .start_call(s.qpsk, s.phase2, &s.xor_mask, &s.system, s.slot);
                let _ = self.voice.take_call_counts(); // anything before is not this call's
                self.levels = s.levels;
                self.last_src = -1;
                self.shared.rec().start(&s);
                self.shared
                    .rust_call_id
                    .store(s.rust_call_id, Ordering::Relaxed);
                self.shared.source_id.store(-1, Ordering::Relaxed);
                self.shared.active.store(true, Ordering::Relaxed);
                self.call = Some(s);
            }
            Cmd::Stop => {
                self.shared.active.store(false, Ordering::Relaxed);
                self.call = None;
            }
        }
    }
}

impl ChannelDecoder for P25Slot {
    fn process(&mut self, iq: &[Complex32]) {
        if let Some(cmd) = self.shared.take_cmd() {
            self.apply(cmd);
        }
        let Some(call) = &self.call else { return };
        self.audio.clear();
        self.tags.clear();
        self.voice.process(iq, &mut self.audio, &mut self.tags);
        let crypto = self.voice.take_crypto();
        if crypto != 0 {
            self.events.send(RadioEvent::Crypto {
                rust_call_id: call.rust_call_id,
                alg_id: (crypto >> 16) & 0xff,
                key_id: crypto & 0xffff,
            });
        }
        let counts = self.voice.take_call_counts();
        let mut rec = self.shared.rec();
        rec.counts.errors += counts.errors;
        rec.counts.spikes += counts.spikes;
        rec.counts.repeated += counts.repeated;
        rec.counts.muted += counts.muted;
        rec.counts.algid = counts.algid;
        for t in &self.tags {
            match t.kind {
                VoiceTagKind::Source => {
                    if t.value > 0 {
                        if !rec.sources.contains(&t.value) {
                            rec.sources.push(t.value);
                        }
                        if t.value != self.last_src {
                            self.last_src = t.value;
                            self.shared.source_id.store(t.value, Ordering::Relaxed);
                            self.events.send(RadioEvent::SourceChange {
                                rust_call_id: call.rust_call_id,
                                src_id: t.value,
                            });
                        }
                    }
                }
                VoiceTagKind::Terminate => {
                    rec.term_events += 1;
                    rec.after_term_samples +=
                        i64::try_from(self.audio.len()).unwrap_or(0) - i64::from(t.offset);
                }
                VoiceTagKind::Alias => {
                    if let Some(a) = &t.alias
                        && let Some(alias) =
                            alias::decode(a, self.shared.source_id.load(Ordering::Relaxed))
                    {
                        tracing::info!(
                            radio_id = alias.radio_id,
                            alias = %alias.alias,
                            source = %alias.source,
                            "new unit alias"
                        );
                        self.events.send(RadioEvent::UnitAlias {
                            sys_num: call.sys_num,
                            alias,
                        });
                    }
                }
                VoiceTagKind::Group => {}
            }
        }
        if !self.audio.is_empty() {
            if (self.levels - 1.0).abs() > f64::EPSILON {
                for s in &mut self.audio {
                    *s = level_i16(*s, self.levels);
                }
            }
            let voice = self.audio.iter().any(|&s| s.abs() >= VOICE_THRESHOLD);
            rec.wrote(self.audio.len(), voice);
            drop(rec);
            self.sink.audio(call.rust_call_id, &self.audio);
        }
    }

    fn retune(&mut self) {}
}

// ── Analog ──

struct AnalogSlot {
    num: i32,
    port: usize,
    demod: FmDemod,
    detector: Detector,
    gate: Gate,
    audio: Audio,
    measurements: Vec<Measurement>,
    gated: Vec<f32>,
    gate_events: Vec<GateEvent>,
    voice_pending: Vec<f32>,
    pcm: Vec<i16>,
    shared: Arc<Shared>,
    stats: Arc<fm::Shared>,
    chz: Weak<SharedChannelizer>,
    sink: Arc<dyn AudioSink>,
    events: EventSender,
    call: Option<Start>,
    levels: f32,
}

impl std::fmt::Debug for AnalogSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalogSlot")
            .field("num", &self.num)
            .finish_non_exhaustive()
    }
}

impl AnalogSlot {
    fn power_db(&self) -> f64 {
        let db = self
            .chz
            .upgrade()
            .map_or(f64::NAN, |c| c.slot_power_db(self.port));
        if db.is_nan() { DB_UNSET } else { db }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn apply(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start(s) => {
                self.gate = Gate::new(GateConfig::for_tone(s.tone));
                self.detector.reset();
                self.voice_pending.clear();
                self.levels = s.levels as f32;
                self.stats.call(&s.system, s.talkgroup, s.freq, s.tone);
                self.shared.rec().start(&s);
                self.shared
                    .rust_call_id
                    .store(s.rust_call_id, Ordering::Relaxed);
                self.shared.active.store(true, Ordering::Relaxed);
                self.call = Some(s);
            }
            Cmd::Stop => {
                self.shared.active.store(false, Ordering::Relaxed);
                self.call = None;
            }
        }
    }
}

impl ChannelDecoder for AnalogSlot {
    fn process(&mut self, iq: &[Complex32]) {
        if let Some(cmd) = self.shared.take_cmd() {
            self.apply(cmd);
        }
        let Some(call) = &self.call else { return };
        self.audio.clear();
        self.demod.process(iq, &mut self.audio);
        self.measurements.clear();
        self.detector
            .process(&self.audio.raw, &mut self.measurements);
        self.voice_pending.extend_from_slice(&self.audio.voice);
        if self.measurements.is_empty() {
            return;
        }
        self.gated.clear();
        self.gate_events.clear();
        let hop = trunk_fm::detect::HOP;
        let mut consumed = 0;
        for m in &self.measurements {
            let end = (consumed + hop).min(self.voice_pending.len());
            let voice = &self.voice_pending[consumed..end];
            consumed = end;
            self.gate
                .push(m, voice, &mut self.gated, &mut self.gate_events);
        }
        self.voice_pending.drain(..consumed);
        self.stats.block(
            &self.measurements,
            self.gate.is_open(),
            &self.gate_events,
            self.gate.config(),
        );
        if !self.gate_events.is_empty() {
            let pwr_db = self.power_db();
            for e in &self.gate_events {
                self.events.send(RadioEvent::Squelch {
                    recorder_num: self.num,
                    muted: matches!(e, GateEvent::Close { .. }),
                    pwr_db,
                });
            }
        }
        if !self.gated.is_empty() {
            self.pcm.clear();
            self.pcm
                .extend(self.gated.iter().map(|&v| level_f32(v, self.levels)));
            let voice = self.pcm.iter().any(|&s| s.abs() >= VOICE_THRESHOLD);
            self.shared.rec().wrote(self.pcm.len(), voice);
            self.sink.audio(call.rust_call_id, &self.pcm);
        }
    }

    fn retune(&mut self) {
        self.demod.reset();
        self.detector.reset();
        self.gate.reset();
        self.voice_pending.clear();
    }
}

// ── Control side ──

/// A recorder as the state machine sees it.
pub struct Recorder {
    pub num: i32,
    pub source: usize,
    pub port: usize,
    pub kind: Kind,
    pub conventional: bool,
    /// The CTCSS tone of a conventional analog channel, 0 for none.
    tone_hz: f64,
    shared: Arc<Shared>,
    chz: Arc<SharedChannelizer>,
    center_hz: f64,
    profile: ProfileId,
    /// Tuning of the current (or last) call, relative to the source centre.
    offset_hz: f64,
    /// Started and not stopped.
    active: bool,
    /// The call it records, while it does.
    call: Option<u64>,
    /// What the current call is, for the report at its end.
    last_start: Option<(String, i64, f64, bool)>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("num", &self.num)
            .field("kind", &self.kind)
            .field("conventional", &self.conventional)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

/// What a recorder needs from its source.
pub struct Slot<'a> {
    pub source: usize,
    pub port: usize,
    pub channelizer: &'a Arc<SharedChannelizer>,
    pub center_hz: f64,
    pub events: &'a EventSender,
    pub sink: &'a Arc<dyn AudioSink>,
}

impl Recorder {
    /// Install a `kind` recorder on `slot`. Conventional ones stay disabled
    /// until [`Self::set_enabled`]; the others enable themselves on start.
    pub fn new(num: i32, slot: &Slot<'_>, kind: Kind, conventional: bool, tone_hz: f64) -> Self {
        let shared = Arc::new(Shared {
            cmd: Mutex::new(None),
            rec: Mutex::new(Recording::default()),
            active: AtomicBool::new(false),
            rust_call_id: AtomicU64::new(0),
            source_id: AtomicI64::new(-1),
        });
        let cfg = slot.channelizer.config();
        let (profile, spec, decoder): (ProfileId, ConditioningSpec, Box<dyn ChannelDecoder>) =
            match kind {
                Kind::P25 => (
                    NARROW,
                    ConditioningSpec {
                        sps: 5,
                        excess_bw: EXCESS_BW,
                        in_rate: cfg.out_rate(NARROW),
                        out_rate: NARROW_RATE,
                        squelch: conventional.then_some(SquelchMode::DIGITAL),
                    },
                    Box::new(P25Slot {
                        num,
                        voice: P25VoiceHandle::new(NARROW_RATE, num),
                        shared: Arc::clone(&shared),
                        sink: Arc::clone(slot.sink),
                        events: slot.events.clone(),
                        audio: Vec::new(),
                        tags: Vec::new(),
                        call: None,
                        levels: 1.0,
                        last_src: -1,
                    }),
                ),
                Kind::Analog => (
                    WIDE,
                    ConditioningSpec {
                        sps: 2,
                        excess_bw: EXCESS_BW,
                        in_rate: cfg.out_rate(WIDE),
                        out_rate: WIDE_RATE,
                        // trunk-fm's own gate decides; the power probe still runs.
                        squelch: None,
                    },
                    Box::new(AnalogSlot {
                        num,
                        port: slot.port,
                        demod: FmDemod::new(WIDE_RATE, TAU_US, Decimation::AntiAliased),
                        detector: Detector::new(),
                        gate: Gate::new(GateConfig::for_tone(None)),
                        audio: Audio::default(),
                        measurements: Vec::new(),
                        gated: Vec::new(),
                        gate_events: Vec::new(),
                        voice_pending: Vec::new(),
                        pcm: Vec::new(),
                        shared: Arc::clone(&shared),
                        stats: fm::Shared::register(fm::AnalogChannel {
                            system: String::new(),
                            talkgroup: 0,
                            freq_hz: 0.0,
                            source: i32::try_from(slot.source).unwrap_or(-1),
                            port: u32::try_from(slot.port).unwrap_or(u32::MAX),
                        }),
                        chz: Arc::downgrade(slot.channelizer),
                        sink: Arc::clone(slot.sink),
                        events: slot.events.clone(),
                        call: None,
                        levels: 1.0,
                    }),
                ),
            };
        slot.channelizer.set_slot_conditioning(slot.port, spec);
        slot.channelizer.set_slot_decoder(slot.port, Some(decoder));
        Self {
            num,
            source: slot.source,
            port: slot.port,
            kind,
            conventional,
            tone_hz,
            shared,
            chz: Arc::clone(slot.channelizer),
            center_hz: slot.center_hz,
            profile,
            offset_hz: 0.0,
            active: false,
            call: None,
            last_start: None,
        }
    }

    /// Free for a call: created, or stopped since the last one.
    pub const fn available(&self) -> bool {
        !self.active
    }

    pub const fn call(&self) -> Option<u64> {
        self.call
    }

    pub const fn type_string(&self) -> &'static str {
        match (self.kind, self.conventional) {
            (Kind::P25, false) => "P25",
            (Kind::P25, true) => "P25C",
            (Kind::Analog, false) => "Analog",
            (Kind::Analog, true) => "AnalogC",
        }
    }

    /// Record `cfg`'s call. A pool recorder enables its slot; a conventional
    /// one waits for [`Self::set_enabled`].
    pub fn start(&mut self, cfg: &TrRecorderConfig, sys: &SystemParams) -> bool {
        if self.active {
            tracing::error!(
                rec_num = self.num,
                "starting a recorder that is already active"
            );
            return false;
        }
        if cfg.phase2_tdma && cfg.xor_mask.is_empty() {
            tracing::warn!(
                rec_num = self.num,
                "cannot record TDMA without the system's scrambling mask"
            );
            return false;
        }
        self.offset_hz = cfg.freq - self.center_hz;
        if self.kind == Kind::P25 {
            self.chz.set_slot_conditioning(
                self.port,
                ConditioningSpec {
                    sps: if cfg.phase2_tdma { 4 } else { 5 },
                    excess_bw: EXCESS_BW,
                    in_rate: self.chz.config().out_rate(NARROW),
                    out_rate: NARROW_RATE,
                    squelch: self.conventional.then_some(SquelchMode::DIGITAL),
                },
            );
        } else {
            self.chz
                .set_slot_filter(self.port, f64::from(sys.max_dev), ANALOG_BANDWIDTH_HZ / 2.0);
        }
        #[allow(clippy::cast_possible_truncation)]
        let tone = (self.tone_hz > 0.0)
            .then(|| Tone::from_hz(self.tone_hz as f32))
            .flatten();
        let start = Start {
            rust_call_id: cfg.rust_call_id,
            sys_num: sys.sys_num,
            system: cfg.short_name.clone(),
            talkgroup: cfg.talkgroup,
            freq: cfg.freq,
            qpsk: sys.qpsk,
            phase2: cfg.phase2_tdma,
            slot: cfg.tdma_slot,
            xor_mask: cfg.xor_mask.clone(),
            levels: match self.kind {
                Kind::P25 => cfg.digital_levels,
                Kind::Analog => sys.analog_levels,
            },
            tone,
        };
        self.last_start = Some((
            cfg.short_name.clone(),
            cfg.talkgroup,
            cfg.freq,
            cfg.phase2_tdma,
        ));
        *self
            .shared
            .cmd
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Cmd::Start(start));
        // The decoder applies the start on its next block; until then the
        // accounting shows the call as pending.
        {
            let mut rec = self.shared.rec();
            rec.state = RecorderState::Idle;
            rec.start = SystemTime::now();
            rec.last_write = Instant::now();
            rec.last_voice = rec.last_write;
            rec.sample_count = 0;
            rec.term_events = 0;
            rec.after_term_samples = 0;
            rec.talkgroup = cfg.talkgroup;
            rec.freq = cfg.freq;
            rec.counts = CallCounts::default();
            rec.sources.clear();
            rec.phase2 = cfg.phase2_tdma;
        }
        self.active = true;
        self.call = Some(cfg.rust_call_id);
        if self.conventional {
            self.chz.set_slot_squelch_db(self.port, cfg.squelch_db);
        } else {
            self.chz.set_slot(self.port, self.profile, self.offset_hz);
        }
        true
    }

    /// Stop recording: the slot is disabled and the decoder told; returns
    /// the FLL error and power the call ends with.
    pub fn stop(&mut self) -> TrStopRecordingResult {
        let result = TrStopRecordingResult {
            freq_error: self.freq_error(),
            recorder_pwr: self.pwr_db(),
            has_recorder: true,
        };
        if !self.active {
            tracing::error!(rec_num = self.num, "stopping an inactive recorder");
            return result;
        }
        self.active = false;
        self.call = None;
        self.chz.clear_slot(self.port);
        *self
            .shared
            .cmd
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Cmd::Stop);
        self.shared.active.store(false, Ordering::Relaxed);
        let rec = {
            let mut rec = self.shared.rec();
            rec.state = RecorderState::Available;
            rec.clone()
        };
        if self.kind == Kind::P25
            && let Some((system, talkgroup, freq, phase2)) = self.last_start.take()
        {
            let t = rec.transmission();
            p25_calls::observe_call(&TrCallReport {
                system,
                recorder: self.num,
                talkgroup,
                freq,
                phase2,
                #[allow(clippy::cast_precision_loss)]
                audio_seconds: t.as_ref().map_or(0.0, |t| t.sample_count as f64 / 8000.0),
                transmissions: i64::from(t.is_some()),
                errors: t.as_ref().map_or(0, |t| t.error_count),
                spikes: t.as_ref().map_or(0, |t| t.spike_count),
                sources: rec.sources.clone(),
                algid: rec.counts.algid,
                repeated: i64::try_from(rec.counts.repeated).unwrap_or(0),
                muted: i64::try_from(rec.counts.muted).unwrap_or(0),
            });
        }
        result
    }

    /// Enable (tune) or disable the slot; conventional recorders are driven
    /// this way by signal detection and their own idle timeouts.
    pub fn set_enabled(&self, enabled: bool) {
        if enabled {
            self.chz.set_slot(self.port, self.profile, self.offset_hz);
        } else {
            self.chz.clear_slot(self.port);
        }
    }

    pub fn state(&self) -> RecorderState {
        if !self.active {
            return RecorderState::Available;
        }
        self.shared.rec().state
    }

    pub fn transmission_list(&self) -> Vec<TrTransmission> {
        self.shared.rec().transmission().into_iter().collect()
    }

    pub fn since_last_write_secs(&self) -> f64 {
        self.shared.rec().last_write.elapsed().as_secs_f64()
    }

    pub fn since_last_voice_secs(&self) -> f64 {
        self.shared.rec().last_voice.elapsed().as_secs_f64()
    }

    /// The unit the state machine says is talking.
    pub fn set_source(&self, src: i64) {
        self.shared.source_id.store(src, Ordering::Relaxed);
    }

    /// Power squelch threshold of a digital conventional slot (analog slots
    /// gate on trunk-fm's own detectors).
    pub fn set_squelch_db(&self, db: f64) {
        if self.kind == Kind::P25 {
            self.chz.set_slot_squelch_db(self.port, db);
        }
    }

    /// The slot's smoothed power in dB, [`DB_UNSET`] without a reading.
    pub fn pwr_db(&self) -> f64 {
        let db = self.chz.slot_power_db(self.port);
        if db.is_nan() { DB_UNSET } else { db }
    }

    pub fn freq_error(&self) -> i32 {
        self.chz.slot_freq_error_hz(self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct Captured(StdMutex<Vec<(u64, usize)>>);

    impl AudioSink for Captured {
        fn audio(&self, rust_call_id: u64, samples: &[i16]) {
            self.0.lock().unwrap().push((rust_call_id, samples.len()));
        }
    }

    fn events() -> (EventSender, tokio::sync::mpsc::Receiver<RadioEvent>) {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        (
            EventSender::new(tx, Arc::new(std::sync::atomic::AtomicUsize::new(0))),
            rx,
        )
    }

    #[test]
    fn recording_accounts_for_audio_and_voice() {
        let mut r = Recording::default();
        assert!(r.transmission().is_none());
        r.start(&Start {
            rust_call_id: 1,
            sys_num: 0,
            system: "s".into(),
            talkgroup: 5,
            freq: 851e6,
            qpsk: true,
            phase2: false,
            slot: 0,
            xor_mask: vec![],
            levels: 1.0,
            tone: None,
        });
        assert_eq!(r.state, RecorderState::Idle);
        r.wrote(160, false);
        assert_eq!(r.state, RecorderState::Recording);
        r.wrote(160, true);
        let t = r.transmission().unwrap();
        assert_eq!((t.talkgroup, t.sample_count), (5, 320));
        assert!(r.last_voice >= r.last_write - std::time::Duration::from_millis(1));
    }

    #[test]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn an_analog_recorder_gates_a_pl_transmission_and_reports_it() {
        let cfg = trunk_channelizer::Config::standard(912_000.0).unwrap();
        let chz = Arc::new(SharedChannelizer::new(cfg.clone()).unwrap());
        let sink: Arc<dyn AudioSink> = Arc::new(Captured::default());
        let (events, mut rx) = events();
        let slot = Slot {
            source: 0,
            port: 1,
            channelizer: &chz,
            center_hz: 488_661_750.0,
            events: &events,
            sink: &sink,
        };
        let mut rec = Recorder::new(7, &slot, Kind::Analog, true, 162.2);
        let sys = SystemParams {
            sys_num: 0,
            short_name: "conv".into(),
            qpsk: false,
            max_dev: 5000,
            analog_levels: 1.0,
            digital_levels: 1.0,
        };
        assert!(rec.start(
            &TrRecorderConfig {
                talkgroup: 2,
                call_num: 1,
                freq: 488_761_750.0,
                short_name: "conv".into(),
                temp_dir: String::new(),
                phase2_tdma: false,
                tdma_slot: 0,
                xor_mask: vec![],
                squelch_db: -50.0,
                digital_levels: 1.0,
                rust_call_id: 42,
            },
            &sys
        ));
        rec.set_enabled(true);
        assert_eq!(rec.state(), RecorderState::Idle);

        // 1 s of noise, 2 s of carrier with PL at +100 kHz, 1 s of noise.
        let rate = 912_000.0f64;
        let hop = cfg.hop();
        let mut s = 99u64;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f32 / (1u64 << 53) as f32
        };
        let mut phase = 0.0f64;
        let mut iq = Vec::new();
        for n in 0..(4.0 * rate) as usize {
            let t = n as f64 / rate;
            if (1.0..3.0).contains(&t) {
                let m = 700.0 * (std::f64::consts::TAU * 162.2 * t).sin()
                    + 2_000.0 * (std::f64::consts::TAU * 800.0 * t).sin();
                phase += std::f64::consts::TAU * (100_000.0 + m) / rate;
            } else {
                phase += std::f64::consts::TAU * f64::from(rnd() - 0.5);
            }
            iq.push(Complex32::from_polar(0.3, phase as f32));
        }
        for block in iq.chunks_exact(hop) {
            chz.run(block);
        }
        let st = fm::analog_channels()
            .into_iter()
            .find(|r| r.channel.talkgroup == 2)
            .unwrap();
        assert_eq!(st.stats.opens, 1, "{:?}", st.stats);
        assert_eq!(st.stats.closes_quieting, 1, "{:?}", st.stats);
        let mut opened = 0;
        let mut closed = 0;
        while let Ok(e) = rx.try_recv() {
            if let RadioEvent::Squelch {
                recorder_num,
                muted,
                ..
            } = e
            {
                assert_eq!(recorder_num, 7);
                if muted { closed += 1 } else { opened += 1 }
            }
        }
        assert_eq!((opened, closed), (1, 1));
        let t = rec.transmission_list();
        assert_eq!(t.len(), 1);
        // about 2 s of 16 kHz audio reached the sink under the call id
        assert!((28_000..=36_000).contains(&t[0].sample_count), "{t:?}");
        assert_eq!(rec.state(), RecorderState::Recording);
        let r = rec.stop();
        assert!(r.has_recorder);
        assert_eq!(rec.state(), RecorderState::Available);
        assert!(rec.available());
    }
}
