//! A recorder: a channelizer slot with a decoder on it, and the control side
//! the state machine drives.
//!
//! P25 recorders decode voice to audio with trunk-p25; analog recorders
//! demodulate FM and gate it with trunk-fm. Both run on the source's DSP
//! thread and append what they decode (audio under the call's id, in-band
//! events) to the block's [`Decoded`] output; the source thread routes it.
//! The control side reaches them with [`Cmd`]s through the channelizer's
//! mailbox, and reads the [`Recording`] accounting they keep.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use num_complex::Complex32;
use trunk_channelizer::{
    ChannelDecoder, ConditioningSpec, NARROW, ProfileId, Reading, SharedChannelizer, SquelchMode,
    WIDE,
};
use trunk_fm::gate::{Config as GateConfig, Event as GateEvent, Gate};
use trunk_fm::{Audio, Decimation, Detector, FmDemod, Measurement, TAU_US, Tone};

use crate::alias;
use crate::fm;
use crate::p25_calls;
use crate::p25_voice::P25VoiceHandle;
use crate::types::{
    CallCounts, Decoded, RadioEvent, RecorderState, TrCallReport, TrRecorderConfig,
    TrStopRecordingResult, TrTransmission, VoiceTagKind,
};

/// The channelizer as the recorders use it.
pub type Chz = SharedChannelizer<Decoded, Cmd>;

/// Conditioned rate of a P25 slot: 5 × 4800 (phase 1) = 4 × 6000 (phase 2).
pub const NARROW_RATE: f64 = 24_000.0;
/// Conditioned rate of an analog slot.
pub const WIDE_RATE: f64 = 96_000.0;
/// trunk-p25's vocoded audio rate.
const P25_AUDIO_RATE: f64 = 8_000.0;
/// Conditioner roll-off, as `channel_control` set it.
const EXCESS_BW: f32 = 0.2;
pub(crate) const EXCESS_BW_PUB: f32 = EXCESS_BW;
/// The analog channel's width, for its filter (cutoff `max_dev`,
/// transition half of this).
const ANALOG_BANDWIDTH_HZ: f64 = 12_000.0;
/// A sample at or above this is voice, not digital silence (~-66 dBFS).
const VOICE_THRESHOLD: i16 = 16;
/// A transmission is audible once some half-second window of its audio
/// reaches this RMS (about -46 dBFS).
///
/// Until then its audio is held back; a transmission that ends without
/// reaching it is dropped, so a tone-only key-up or an empty channel never
/// reaches anyone downstream.
pub const AUDIBLE_RMS: f64 = 0.005;
/// The window over which [`AUDIBLE_RMS`] is measured.
const AUDIBLE_WINDOW_S: f64 = 0.5;
/// What `get_pwr()` returned with no reading: trunk-recorder's `DB_UNSET`.
pub const DB_UNSET: f64 = 999.0;

/// A power reading as the state machine takes it: [`DB_UNSET`] for none.
const fn pwr_or_unset(db: f64) -> f64 {
    if db.is_nan() { DB_UNSET } else { db }
}

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
pub struct Start {
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

/// What the control side tells a recorder's decoder.
#[derive(Clone, Debug)]
pub enum Cmd {
    Start(Start),
    Stop,
    /// The unit the state machine says is talking (P25: for Harris aliases).
    Source(i64),
}

/// One recording's accounting (what `headless_sink` kept), written by the
/// decoder as it goes and read by the control side.
#[derive(Clone, Debug)]
struct Recording {
    /// The call being recorded (0: none); audio for any other is ignored.
    call_id: u64,
    state: RecorderState,
    system: String,
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
    /// Some window reached [`AUDIBLE_RMS`]: the audio flows.
    audible: bool,
    /// Audio held back until the transmission proves audible.
    held: Vec<i16>,
    /// The window in progress: sum of squares and samples.
    win_sq: f64,
    win_n: usize,
}

impl Default for Recording {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            call_id: 0,
            state: RecorderState::Available,
            system: String::new(),
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
            audible: false,
            held: Vec::new(),
            win_sq: 0.0,
            win_n: 0,
        }
    }
}

impl Recording {
    fn start(&mut self, s: &Start) {
        let now = Instant::now();
        *self = Self {
            call_id: s.rust_call_id,
            state: RecorderState::Idle,
            system: s.system.clone(),
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

    /// Account for `audio` (at `rate` samples per second) of call `call_id`
    /// and decide what of it goes out: nothing until a half-second window
    /// clears [`AUDIBLE_RMS`], then everything held plus this. Returns the
    /// samples to send. Audio of a call that is not this one (over, or not
    /// yet started) is dropped.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::suboptimal_flops
    )]
    fn deliver(&mut self, call_id: u64, audio: &[i16], rate: f64) -> Vec<i16> {
        if call_id != self.call_id || self.state == RecorderState::Available {
            return Vec::new();
        }
        let voice = audio.iter().any(|&s| s.abs() >= VOICE_THRESHOLD);
        self.wrote(audio.len(), voice);
        if self.audible {
            return audio.to_vec();
        }
        let win_len = (rate * AUDIBLE_WINDOW_S) as usize;
        self.held.extend_from_slice(audio);
        for &s in audio {
            let v = f64::from(s) / 32768.0;
            self.win_sq += v * v;
            self.win_n += 1;
            if self.win_n >= win_len {
                let rms = (self.win_sq / self.win_n as f64).sqrt();
                self.win_sq = 0.0;
                self.win_n = 0;
                if rms >= AUDIBLE_RMS {
                    self.audible = true;
                    return std::mem::take(&mut self.held);
                }
            }
        }
        Vec::new()
    }

    /// The transmission is over: whatever a final partial window makes
    /// audible is returned to send, else the held audio is dropped. The
    /// recording takes no more audio.
    #[allow(clippy::cast_precision_loss)]
    fn finish(&mut self) -> Vec<i16> {
        self.state = RecorderState::Available;
        if !self.audible && self.win_n > 0 {
            let rms = (self.win_sq / self.win_n as f64).sqrt();
            if rms >= AUDIBLE_RMS {
                self.audible = true;
            }
        }
        self.win_sq = 0.0;
        self.win_n = 0;
        if self.audible {
            std::mem::take(&mut self.held)
        } else {
            if !self.held.is_empty() {
                tracing::info!(
                    talkgroup = self.talkgroup,
                    samples = self.held.len(),
                    "dropping a transmission with no audible audio"
                );
            }
            self.held.clear();
            Vec::new()
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

    /// A P25 call's outcome, for the metrics.
    fn report(&self, recorder: i32) -> TrCallReport {
        let t = self.transmission();
        TrCallReport {
            system: self.system.clone(),
            recorder,
            talkgroup: self.talkgroup,
            freq: self.freq,
            phase2: self.phase2,
            #[allow(clippy::cast_precision_loss)]
            audio_seconds: t
                .as_ref()
                .map_or(0.0, |t| t.sample_count as f64 / P25_AUDIO_RATE),
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

/// The accounting, shared by the decoder and the control side.
type Shared = Arc<Mutex<Recording>>;

fn lock(rec: &Shared) -> std::sync::MutexGuard<'_, Recording> {
    rec.lock().unwrap_or_else(PoisonError::into_inner)
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
    rec: Shared,
    audio: Vec<i16>,
    tags: Vec<crate::types::VoiceTag>,
    call: Option<Start>,
    levels: f64,
    /// The unit talking, as last heard in band or told by the state machine.
    source_id: i64,
    /// The conditioner's power squelch, as of the last block (digital
    /// conventional channels; always open on the others).
    squelch_open: bool,
}

impl std::fmt::Debug for P25Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P25Slot")
            .field("num", &self.num)
            .finish_non_exhaustive()
    }
}

impl ChannelDecoder for P25Slot {
    type Output = Decoded;
    type Command = Cmd;

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start(s) => {
                self.voice
                    .start_call(s.qpsk, s.phase2, &s.xor_mask, &s.system, s.slot);
                let _ = self.voice.take_call_counts(); // anything before is not this call's
                self.levels = s.levels;
                self.source_id = -1;
                self.call = Some(s);
            }
            Cmd::Stop => self.call = None,
            Cmd::Source(src) => self.source_id = src,
        }
    }

    fn process(&mut self, iq: &[Complex32], reading: &Reading, out: &mut Decoded) {
        if reading.squelch_open != self.squelch_open {
            self.squelch_open = reading.squelch_open;
            out.events.push(RadioEvent::Squelch {
                recorder_num: self.num,
                muted: !reading.squelch_open,
                pwr_db: pwr_or_unset(reading.power_db),
            });
        }
        let Some(call) = &self.call else { return };
        self.audio.clear();
        self.tags.clear();
        self.voice.process(iq, &mut self.audio, &mut self.tags);
        let crypto = self.voice.take_crypto();
        if crypto != 0 {
            out.events.push(RadioEvent::Crypto {
                rust_call_id: call.rust_call_id,
                alg_id: (crypto >> 16) & 0xff,
                key_id: crypto & 0xffff,
            });
        }
        let counts = self.voice.take_call_counts();
        let mut rec = lock(&self.rec);
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
                        if t.value != self.source_id {
                            self.source_id = t.value;
                            out.events.push(RadioEvent::SourceChange {
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
                        && let Some(alias) = alias::decode(a, self.source_id)
                    {
                        tracing::info!(
                            radio_id = alias.radio_id,
                            alias = %alias.alias,
                            source = %alias.source,
                            "new unit alias"
                        );
                        out.events.push(RadioEvent::UnitAlias {
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
            let pcm = rec.deliver(call.rust_call_id, &self.audio, P25_AUDIO_RATE);
            drop(rec);
            out.audio(call.rust_call_id, &pcm);
        }
    }

    fn retune(&mut self) {}
}

// ── Analog ──

struct AnalogSlot {
    num: i32,
    demod: FmDemod,
    detector: Detector,
    gate: Gate,
    audio: Audio,
    measurements: Vec<Measurement>,
    gated: Vec<f32>,
    gate_events: Vec<GateEvent>,
    voice_pending: Vec<f32>,
    pcm: Vec<i16>,
    rec: Shared,
    stats: Arc<fm::Shared>,
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

impl ChannelDecoder for AnalogSlot {
    type Output = Decoded;
    type Command = Cmd;

    #[allow(clippy::cast_possible_truncation)]
    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start(s) => {
                self.gate = Gate::new(GateConfig::for_tone(s.tone));
                self.detector.reset();
                self.voice_pending.clear();
                self.levels = s.levels as f32;
                self.stats.call(&s.system, s.talkgroup, s.freq, s.tone);
                self.call = Some(s);
            }
            Cmd::Stop => self.call = None,
            Cmd::Source(_) => {}
        }
    }

    fn process(&mut self, iq: &[Complex32], reading: &Reading, out: &mut Decoded) {
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
        for e in &self.gate_events {
            out.events.push(RadioEvent::Squelch {
                recorder_num: self.num,
                muted: matches!(e, GateEvent::Close { .. }),
                pwr_db: pwr_or_unset(reading.power_db),
            });
        }
        if !self.gated.is_empty() {
            self.pcm.clear();
            self.pcm
                .extend(self.gated.iter().map(|&v| level_f32(v, self.levels)));
            let pcm = lock(&self.rec).deliver(call.rust_call_id, &self.pcm, trunk_fm::AUDIO_RATE);
            out.audio(call.rust_call_id, &pcm);
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
    rec: Shared,
    chz: Arc<Chz>,
    center_hz: f64,
    profile: ProfileId,
    /// Tuning of the current (or last) call, relative to the source centre.
    offset_hz: f64,
    /// The call it records, while it does.
    call: Option<u64>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("num", &self.num)
            .field("kind", &self.kind)
            .field("conventional", &self.conventional)
            .field("call", &self.call)
            .finish_non_exhaustive()
    }
}

/// What a recorder needs from its source.
pub struct Slot<'a> {
    pub source: usize,
    pub port: usize,
    pub channelizer: &'a Arc<Chz>,
    pub center_hz: f64,
}

/// What stopping a recorder leaves: the result for the state machine, and
/// the audio the audibility hold still kept, to send under the call before
/// the call ends.
pub struct Stopped {
    pub result: TrStopRecordingResult,
    pub call: u64,
    pub tail: Vec<i16>,
}

impl Recorder {
    /// Install a `kind` recorder on `slot`. Conventional ones stay disabled
    /// until [`Self::set_enabled`]; the others enable themselves on start.
    pub fn new(num: i32, slot: &Slot<'_>, kind: Kind, conventional: bool, tone_hz: f64) -> Self {
        let rec: Shared = Arc::new(Mutex::new(Recording::default()));
        let cfg = slot.channelizer.config();
        let (profile, spec, decoder): (
            ProfileId,
            ConditioningSpec,
            Box<dyn ChannelDecoder<Output = Decoded, Command = Cmd>>,
        ) = match kind {
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
                    rec: Arc::clone(&rec),
                    audio: Vec::new(),
                    tags: Vec::new(),
                    call: None,
                    levels: 1.0,
                    source_id: -1,
                    squelch_open: true,
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
                    demod: FmDemod::new(WIDE_RATE, TAU_US, Decimation::AntiAliased),
                    detector: Detector::new(),
                    gate: Gate::new(GateConfig::for_tone(None)),
                    audio: Audio::default(),
                    measurements: Vec::new(),
                    gated: Vec::new(),
                    gate_events: Vec::new(),
                    voice_pending: Vec::new(),
                    pcm: Vec::new(),
                    rec: Arc::clone(&rec),
                    stats: fm::Shared::register(fm::AnalogChannel {
                        system: String::new(),
                        talkgroup: 0,
                        freq_hz: 0.0,
                        source: i32::try_from(slot.source).unwrap_or(-1),
                        port: u32::try_from(slot.port).unwrap_or(u32::MAX),
                    }),
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
            rec,
            chz: Arc::clone(slot.channelizer),
            center_hz: slot.center_hz,
            profile,
            offset_hz: 0.0,
            call: None,
        }
    }

    /// Free for a call: created, or stopped since the last one.
    pub const fn available(&self) -> bool {
        self.call.is_none()
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
        if self.call.is_some() {
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
        // The accounting shows the call from now; the decoder starts it on
        // its next block.
        lock(&self.rec).start(&start);
        self.chz.send(self.port, Cmd::Start(start));
        self.call = Some(cfg.rust_call_id);
        if self.conventional {
            self.chz.set_slot_squelch_db(self.port, cfg.squelch_db);
        } else {
            self.chz.set_slot(self.port, self.profile, self.offset_hz);
        }
        true
    }

    /// Stop recording: the slot is disabled and the decoder told; returns
    /// the FLL error and power the call ends with, and the held audio.
    pub fn stop(&mut self) -> Stopped {
        let result = TrStopRecordingResult {
            freq_error: self.freq_error(),
            recorder_pwr: self.pwr_db(),
            has_recorder: true,
        };
        let Some(call) = self.call.take() else {
            tracing::error!(rec_num = self.num, "stopping an inactive recorder");
            return Stopped {
                result,
                call: 0,
                tail: Vec::new(),
            };
        };
        self.chz.clear_slot(self.port);
        self.chz.send(self.port, Cmd::Stop);
        let mut rec = lock(&self.rec);
        let tail = rec.finish();
        if self.kind == Kind::P25 {
            p25_calls::observe_call(&rec.report(self.num));
        }
        drop(rec);
        Stopped { result, call, tail }
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
        if self.call.is_none() {
            return RecorderState::Available;
        }
        lock(&self.rec).state
    }

    pub fn transmission_list(&self) -> Vec<TrTransmission> {
        lock(&self.rec).transmission().into_iter().collect()
    }

    pub fn since_last_write_secs(&self) -> f64 {
        lock(&self.rec).last_write.elapsed().as_secs_f64()
    }

    pub fn since_last_voice_secs(&self) -> f64 {
        lock(&self.rec).last_voice.elapsed().as_secs_f64()
    }

    /// The unit the state machine says is talking.
    pub fn set_source(&self, src: i64) {
        self.chz.send(self.port, Cmd::Source(src));
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
        pwr_or_unset(self.chz.slot_power_db(self.port))
    }

    pub fn freq_error(&self) -> i32 {
        self.chz.slot_freq_error_hz(self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start() -> Start {
        Start {
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
        }
    }

    #[test]
    fn a_transmission_is_held_until_audible_and_dropped_otherwise() {
        // Quiet audio is held, then dropped at the end.
        let mut r = Recording::default();
        r.start(&start());
        let quiet = vec![40i16; 4000];
        assert_eq!(r.deliver(1, &quiet, 8_000.0), Vec::<i16>::new());
        assert_eq!(r.deliver(1, &quiet, 8_000.0), Vec::<i16>::new());
        assert_eq!(r.sample_count, 8000, "accounting sees every sample");
        assert_eq!(r.finish(), Vec::<i16>::new());
        assert!(!r.audible);

        // Loud audio releases everything held before it, then flows live.
        let mut r = Recording::default();
        r.start(&start());
        assert_eq!(r.deliver(1, &quiet, 8_000.0), Vec::<i16>::new());
        let loud = vec![3000i16; 4000];
        let out = r.deliver(1, &loud, 8_000.0);
        assert_eq!(out.len(), 8000);
        assert_eq!(&out[..4000], &quiet[..]);
        assert_eq!(r.deliver(1, &quiet, 8_000.0), quiet);
        assert_eq!(r.finish(), Vec::<i16>::new());
        // Nothing after the finish, nor for another call.
        assert_eq!(r.deliver(1, &loud, 8_000.0), Vec::<i16>::new());
        r.start(&start());
        assert_eq!(r.deliver(2, &loud, 8_000.0), Vec::<i16>::new());
        assert_eq!(r.sample_count, 0);

        // A loud partial last window is released at the end.
        let mut r = Recording::default();
        r.start(&start());
        assert_eq!(r.deliver(1, &loud[..1000], 8_000.0), Vec::<i16>::new());
        assert_eq!(r.finish().len(), 1000);
    }

    #[test]
    fn recording_accounts_for_audio_and_voice() {
        let mut r = Recording::default();
        assert!(r.transmission().is_none());
        r.start(&start());
        assert_eq!(r.state, RecorderState::Idle);
        r.wrote(160, false);
        assert_eq!(r.state, RecorderState::Recording);
        r.wrote(160, true);
        let t = r.transmission().unwrap();
        assert_eq!((t.talkgroup, t.sample_count), (5, 320));
        assert!(r.last_voice >= r.last_write - std::time::Duration::from_millis(1));
        let report = r.report(3);
        assert_eq!((report.recorder, report.transmissions), (3, 1));
        assert!((report.audio_seconds - 0.04).abs() < 1e-9);
    }

    #[test]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn an_analog_recorder_gates_a_pl_transmission_and_reports_it() {
        let cfg = trunk_channelizer::Config::standard(912_000.0).unwrap();
        let chz: Arc<Chz> = Arc::new(SharedChannelizer::new(cfg.clone()).unwrap());
        let slot = Slot {
            source: 0,
            port: 1,
            channelizer: &chz,
            center_hz: 488_661_750.0,
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
        let mut out = Decoded::default();
        let (mut opened, mut closed, mut audio) = (0, 0, 0);
        for block in iq.chunks_exact(hop) {
            chz.run(block, &mut out);
            for e in out.events.drain(..) {
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
            for (id, pcm) in out.audio.drain(..) {
                assert_eq!(id, 42);
                audio += pcm.len();
            }
        }
        let st = fm::analog_channels()
            .into_iter()
            .find(|r| r.channel.talkgroup == 2)
            .unwrap();
        assert_eq!(st.stats.opens, 1, "{:?}", st.stats);
        assert_eq!(st.stats.closes_quieting, 1, "{:?}", st.stats);
        assert_eq!((opened, closed), (1, 1));
        let t = rec.transmission_list();
        assert_eq!(t.len(), 1);
        // about 2 s of 16 kHz audio came out under the call id
        assert!((28_000..=36_000).contains(&t[0].sample_count), "{t:?}");
        assert_eq!(audio, t[0].sample_count as usize);
        assert_eq!(rec.state(), RecorderState::Recording);
        let s = rec.stop();
        assert!(s.result.has_recorder);
        assert_eq!(s.call, 42);
        assert!(s.tail.is_empty());
        assert_eq!(rec.state(), RecorderState::Available);
        assert!(rec.available());
    }
}
