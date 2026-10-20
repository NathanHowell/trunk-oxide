//! P25 voice end to end in trunk-p25: demodulate, decode, assemble the call
//! ([`trunk_p25::assemble`]), and vocode each frame
//! ([`trunk_p25::vocoder`]); and [`P25Voice`], the kind of channel it
//! decodes.

use std::sync::Arc;

use num_complex::Complex32;
use trunk_channelizer::{ChannelDecoder, ConditioningSpec, NARROW, Reading, SquelchMode};
use trunk_p25::assemble::{CallCounter, CallEvent, Phase1Assembler, Phase2Assembler, Vocode};
use trunk_p25::control::Received;
use trunk_p25::tdma::{BurstContent, Scrambler, TdmaDecoder, op25_view};
use trunk_p25::vocoder::{AmbeVocoder, Decision, ImbeVocoder, Pcm, SAMPLES_PER_FRAME};
use trunk_p25::voice::signalling::ALGID_CLEAR;
use trunk_p25::voice::{Encryption, EncryptionTracker, VoiceDecoder, VoiceEvent};
use trunk_p25::{DemodConfig, Modulation};

use crate::alias;
use crate::carrier::Carrier;
use crate::kind::{Cmd, Kind, OpenError, Opening, Tuning};
use crate::p25::{VoiceShared, track_encryption};
use crate::p25_calls;
use crate::recording::{Accounting, Tape};
use crate::types::{CallCounts, CallId, ChannelNum, Decoded, RadioEvent, VoiceTag, VoiceTagKind};

/// Conditioned rate of a P25 slot: 5 × 4800 (phase 1) = 4 × 6000 (phase 2).
pub const NARROW_RATE: f64 = 24_000.0;
/// trunk-p25's vocoded audio rate.
const AUDIO_RATE: f64 = 8_000.0;
/// Conditioner roll-off, as `channel_control` set it.
pub(crate) const EXCESS_BW: f32 = 0.2;

/// How a P25 slot of `cfg`'s channelizer is conditioned: 5 samples per
/// symbol for phase 1 (4800 baud), 4 for phase 2 (6000), and the power
/// squelch only on a conventional channel (a trunked call has the control
/// channel to say when it ends).
pub(crate) fn p25_conditioning(
    cfg: &trunk_channelizer::Config,
    phase2: bool,
    squelch: bool,
) -> ConditioningSpec {
    ConditioningSpec {
        sps: if phase2 { 4 } else { 5 },
        excess_bw: EXCESS_BW,
        in_rate: cfg.out_rate(NARROW),
        out_rate: NARROW_RATE,
        squelch: squelch.then_some(SquelchMode::DIGITAL),
    }
}

/// What a call produced, in order.
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "nearly every output is audio, into a buffer that is reused"
)]
pub enum CallOutput {
    /// One frame's audio and what it was made from.
    Audio {
        pcm: Pcm,
        frame: Vocode,
    },
    Event(CallEvent),
}

/// One channel's voice receiver.
pub struct VoicePipeline {
    rate: f64,
    phase2: bool,
    p1: VoiceDecoder,
    a1: Phase1Assembler,
    p2: TdmaDecoder,
    a2: Phase2Assembler,
    imbe: ImbeVocoder,
    ambe: AmbeVocoder,
    events: Vec<Received<VoiceEvent>>,
    calls: Vec<CallEvent>,
    counter: CallCounter,
    /// The call's encryption, from the ESS on its own slot.
    crypto: EncryptionTracker,
    crypto_pending: Option<Encryption>,
    shared: Arc<VoiceShared>,
    /// Frames the vocoders repeated and muted this call.
    repeated: u64,
    muted: u64,
}

impl VoicePipeline {
    /// `rate`: the conditioned channel rate.
    pub fn new(rate: f64) -> Self {
        Self::for_channel(rate, None)
    }

    /// For channel `channel`, whose decoder counters it reports
    /// ([`crate::p25::voice_channels`]).
    pub fn for_channel(rate: f64, channel: Option<ChannelNum>) -> Self {
        Self {
            rate,
            phase2: false,
            p1: VoiceDecoder::new(DemodConfig::new(rate, Modulation::Cqpsk)),
            a1: Phase1Assembler::new(),
            p2: TdmaDecoder::new(DemodConfig::phase2(rate), None),
            a2: Phase2Assembler::new(0),
            imbe: ImbeVocoder::new(),
            ambe: AmbeVocoder::new(),
            events: Vec::new(),
            calls: Vec::new(),
            counter: CallCounter::default(),
            crypto: EncryptionTracker::default(),
            crypto_pending: None,
            shared: VoiceShared::register(channel),
            repeated: 0,
            muted: 0,
        }
    }

    /// A new call: phase 2 on logical channel `slot`, descrambled with
    /// `scrambler`; else phase 1 in `modulation`.
    pub fn start_call(
        &mut self,
        modulation: Modulation,
        phase2: bool,
        scrambler: Option<Scrambler>,
        slot: u8,
    ) {
        self.phase2 = phase2;
        self.counter = CallCounter::default();
        self.crypto.reset();
        self.crypto_pending = None;
        self.repeated = 0;
        self.muted = 0;
        if phase2 {
            self.a2
                .start_call(slot, scrambler.as_ref().map(Scrambler::color_code));
            self.p2 = TdmaDecoder::new(DemodConfig::phase2(self.rate), scrambler);
        } else {
            self.p1 = VoiceDecoder::new(DemodConfig::new(self.rate, modulation));
        }
    }

    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<CallOutput>) {
        self.calls.clear();
        if self.phase2 {
            let mut state = Phase2State {
                // Voice frames are descrambled with the decoder's own scrambler.
                scrambler: self.p2.scrambler().cloned(),
                assembler: &mut self.a2,
                calls: &mut self.calls,
                counter: &mut self.counter,
                crypto: &mut self.crypto,
                crypto_pending: &mut self.crypto_pending,
            };
            self.p2.process_with(iq, &mut |burst, event, symbol| {
                state.burst(burst, event, symbol);
            });
            self.shared.phase2(self.p2.stats());
        } else {
            self.events.clear();
            self.p1.process(iq, &mut self.events);
            self.shared.phase1(self.p1.stats());
        }
        self.finish(out);
    }

    /// [`Self::process`] from demodulated symbols.
    pub fn process_symbols(&mut self, soft: &[f32], out: &mut Vec<CallOutput>) {
        self.calls.clear();
        if self.phase2 {
            let mut state = Phase2State {
                // Voice frames are descrambled with the decoder's own scrambler.
                scrambler: self.p2.scrambler().cloned(),
                assembler: &mut self.a2,
                calls: &mut self.calls,
                counter: &mut self.counter,
                crypto: &mut self.crypto,
                crypto_pending: &mut self.crypto_pending,
            };
            self.p2
                .process_symbols_with(soft, &mut |burst, event, symbol| {
                    state.burst(burst, event, symbol);
                });
        } else {
            self.events.clear();
            self.p1.process_symbols(soft, &mut self.events);
        }
        self.finish(out);
    }

    fn finish(&mut self, out: &mut Vec<CallOutput>) {
        if !self.phase2 {
            for e in &self.events {
                self.counter.phase1(&e.message);
                if let VoiceEvent::Ldu2 { encryption, .. } = &e.message {
                    track_encryption(
                        &mut self.crypto,
                        &mut self.crypto_pending,
                        encryption.as_ref().map(|c| &c.value),
                    );
                }
                self.a1.push(e, &mut self.calls);
            }
        }
        let calls = std::mem::take(&mut self.calls);
        for c in calls {
            match c {
                CallEvent::Frame(v) => out.push(CallOutput::Audio {
                    pcm: self.vocode(v),
                    frame: v,
                }),
                e => out.push(CallOutput::Event(e)),
            }
        }
    }

    /// A frame's audio; silence where the frame is muted or encrypted.
    fn vocode(&mut self, v: Vocode) -> Pcm {
        let (pcm, decision) = match v {
            Vocode::Imbe { frame, e0, et } => {
                (self.imbe.vocode(&frame, e0, et), Some(self.imbe.last()))
            }
            Vocode::Ambe {
                dibits,
                frame,
                encrypted: false,
            } => (
                self.ambe.vocode(&dibits, frame.as_ref()),
                Some(self.ambe.last()),
            ),
            Vocode::Ambe {
                dibits,
                encrypted: true,
                ..
            } => {
                self.ambe.encrypted(&dibits);
                (None, None)
            }
            Vocode::Silence => (None, None),
        };
        match decision {
            Some(Decision::Repeated) => self.repeated += 1,
            Some(Decision::Muted) => self.muted += 1,
            Some(Decision::Played) | None => {}
        }
        pcm.unwrap_or([0; SAMPLES_PER_FRAME])
    }
}

/// What a phase 2 burst updates.
struct Phase2State<'a> {
    scrambler: Option<Scrambler>,
    assembler: &'a mut Phase2Assembler,
    calls: &'a mut Vec<CallEvent>,
    counter: &'a mut CallCounter,
    crypto: &'a mut EncryptionTracker,
    crypto_pending: &'a mut Option<Encryption>,
}

impl Phase2State<'_> {
    fn burst(
        &mut self,
        burst: &trunk_p25::tdma::Burst,
        event: trunk_p25::tdma::TdmaEvent,
        symbol: u64,
    ) {
        let view = op25_view(burst, self.scrambler.as_ref(), &event);
        self.counter.phase2(&event);
        if event.channel == self.assembler.channel
            && let BurstContent::Voice2 { encryption, .. } = &event.content
        {
            track_encryption(
                self.crypto,
                self.crypto_pending,
                encryption.as_ref().map(|c| &c.value),
            );
        }
        self.assembler.push(
            &Received {
                symbol,
                message: event,
            },
            &view,
            self.calls,
        );
    }
}

// ── A channel's voice decoder ──

/// A voice channel's decoder: audio out, with the in-band tags at their
/// sample offsets.
pub struct P25VoiceHandle {
    pipeline: VoicePipeline,
    out: Vec<CallOutput>,
    /// A terminator has been tagged since the last audio (op25 tags one per
    /// transmission's end, however many TDUs follow).
    terminated: bool,
}

impl P25VoiceHandle {
    /// For channel `channel` at the conditioned channel `rate`.
    pub fn new(rate: f64, channel: Option<ChannelNum>) -> Self {
        Self {
            pipeline: VoicePipeline::for_channel(rate, channel),
            out: Vec::new(),
            terminated: false,
        }
    }

    /// A new call: CQPSK (else C4FM), phase 2 on `slot` with the system's
    /// scrambling sequence.
    pub fn start_call(
        &mut self,
        qpsk: bool,
        tdma: bool,
        scrambler: Option<&Scrambler>,
        system: &str,
        freq_hz: f64,
        slot: i32,
    ) {
        let modulation = if qpsk {
            Modulation::Cqpsk
        } else {
            Modulation::C4fm
        };
        self.pipeline.start_call(
            modulation,
            tdma,
            scrambler.cloned(),
            u8::try_from(slot).unwrap_or(0),
        );
        self.pipeline.shared.call(system, freq_hz);
        self.terminated = false;
    }

    /// Decode `iq`; append its audio to `audio` and its tags (at offsets
    /// into what `audio` gained) to `tags`.
    pub fn process(&mut self, iq: &[Complex32], audio: &mut Vec<i16>, tags: &mut Vec<VoiceTag>) {
        let start = audio.len();
        self.out.clear();
        self.pipeline.process(iq, &mut self.out);
        for o in self.out.drain(..) {
            #[allow(clippy::cast_possible_truncation)]
            let offset = (audio.len() - start) as u32;
            let mut tag = |kind| tags.push(VoiceTag { offset, kind });
            match o {
                CallOutput::Audio { pcm, .. } => {
                    audio.extend(pcm);
                    self.terminated = false;
                }
                CallOutput::Event(CallEvent::Source(s)) => {
                    tag(VoiceTagKind::Source(s));
                }
                CallOutput::Event(CallEvent::Group(g)) => {
                    tag(VoiceTagKind::Group(g));
                }
                CallOutput::Event(CallEvent::Terminator) => {
                    if !self.terminated {
                        self.terminated = true;
                        tag(VoiceTagKind::Terminate);
                    }
                }
                CallOutput::Event(CallEvent::Alias(a)) => {
                    tag(VoiceTagKind::Alias(a));
                }
                CallOutput::Event(CallEvent::Frame(_) | CallEvent::Encryption { .. }) => {}
            }
        }
    }

    /// The phase 2 logical channel of the call in progress.
    pub const fn slot(&self) -> u8 {
        self.pipeline.a2.channel
    }

    /// Encryption confirmed on the call's slot since the last take.
    pub const fn take_crypto(&mut self) -> Option<Encryption> {
        self.pipeline.crypto_pending.take()
    }

    pub fn take_call_counts(&mut self) -> CallCounts {
        let c = self.pipeline.counter.take();
        CallCounts {
            errors: c.errors,
            spikes: c.spikes,
            algid: self
                .pipeline
                .crypto
                .confirmed()
                .map_or(ALGID_CLEAR, |e| e.algid),
            repeated: std::mem::take(&mut self.pipeline.repeated),
            muted: std::mem::take(&mut self.pipeline.muted),
        }
    }
}

// ── The kind ──

/// What says when a P25 voice channel's transmission is on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Keying {
    /// The control channel: a trunked call.
    Trunked,
    /// The carrier, by power squelch at `squelch_db`: a conventional
    /// channel. Its transitions are reported as [`RadioEvent::Squelch`].
    Carrier { squelch_db: f64 },
}

/// P25 voice, phase 1 or (on a slot) phase 2, trunked or conventional.
#[derive(Debug)]
pub enum P25Voice {}

impl Kind for P25Voice {
    type Params = Keying;
    type Decoder = P25VoiceChannel;
    const NAME: &'static str = "p25";
    const PROFILE: trunk_channelizer::ProfileId = NARROW;
    const AUDIO_RATE: Option<f64> = Some(AUDIO_RATE);

    fn tuning(at: &Opening<'_, Self>) -> Tuning {
        let squelch_db = match *at.params {
            Keying::Trunked => None,
            Keying::Carrier { squelch_db } => Some(squelch_db),
        };
        Tuning {
            conditioning: p25_conditioning(
                at.channelizer,
                at.call.channel.slot.is_some(),
                squelch_db.is_some(),
            ),
            filter: None,
            squelch_db,
        }
    }

    fn decoder(at: &Opening<'_, Self>, tape: Tape) -> Result<P25VoiceChannel, OpenError> {
        let slot = at.call.channel.slot;
        if slot.is_some() && at.scrambler.is_none() {
            return Err(OpenError::NoScramblingMask);
        }
        let mut voice = P25VoiceHandle::new(NARROW_RATE, Some(at.num));
        voice.start_call(
            at.system.qpsk,
            slot.is_some(),
            at.scrambler,
            &at.system.short_name,
            at.call.channel.freq_hz,
            slot.map_or(0, i32::from),
        );
        Ok(P25VoiceChannel {
            call: at.call.id,
            sys_num: at.system.sys_num,
            num: at.num,
            voice,
            tape,
            audio: Vec::new(),
            tags: Vec::new(),
            levels: at.system.digital_levels,
            source: None,
            carrier: matches!(at.params, Keying::Carrier { .. }).then(|| Carrier::new(NARROW_RATE)),
        })
    }

    fn finished(recording: &Accounting) {
        p25_calls::observe_call(&recording.report());
    }
}

/// Level, then PCM: `k` is the system's `digital_levels`.
#[allow(clippy::cast_possible_truncation)]
fn level(s: i16, k: f64) -> i16 {
    (f64::from(s) * k).round().clamp(-32768.0, 32767.0) as i16
}

/// A P25 voice channel's decoder, recording one call.
pub struct P25VoiceChannel {
    call: CallId,
    sys_num: i32,
    num: ChannelNum,
    voice: P25VoiceHandle,
    tape: Tape,
    audio: Vec<i16>,
    tags: Vec<VoiceTag>,
    levels: f64,
    /// The unit talking, as last heard in band or told by the state machine.
    source: Option<u32>,
    /// The transmission detector of a conventional channel.
    carrier: Option<Carrier>,
}

impl std::fmt::Debug for P25VoiceChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P25VoiceChannel")
            .field("call", &self.call)
            .field("num", &self.num)
            .finish_non_exhaustive()
    }
}

impl ChannelDecoder for P25VoiceChannel {
    type Output = Decoded;
    type Command = Cmd;

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Source(unit) => self.source = Some(unit),
        }
    }

    fn process(&mut self, iq: &[Complex32], reading: &Reading, out: &mut Decoded) {
        if let Some(squelch) = self
            .carrier
            .as_mut()
            .and_then(|c| c.push(reading.squelch_open, iq.len()))
        {
            out.events.push(RadioEvent::Squelch {
                call_id: self.call,
                squelch,
                power_db: (!reading.power_db.is_nan()).then_some(reading.power_db),
            });
        }
        self.audio.clear();
        self.tags.clear();
        self.voice.process(iq, &mut self.audio, &mut self.tags);
        if let Some(encryption) = self.voice.take_crypto() {
            out.events.push(RadioEvent::Crypto {
                call_id: self.call,
                encryption,
            });
        }
        let counts = self.voice.take_call_counts();
        let Self {
            call,
            sys_num,
            tape,
            audio,
            tags,
            levels,
            source,
            ..
        } = self;
        let pcm = tape.with(|rec| {
            let acct = &mut rec.acct;
            acct.counts.errors += counts.errors;
            acct.counts.spikes += counts.spikes;
            acct.counts.repeated += counts.repeated;
            acct.counts.muted += counts.muted;
            acct.counts.algid = counts.algid;
            for t in tags.iter() {
                match &t.kind {
                    &VoiceTagKind::Source(unit) => {
                        if unit != 0 {
                            if !acct.sources.contains(&unit) {
                                acct.sources.push(unit);
                            }
                            if *source != Some(unit) {
                                *source = Some(unit);
                                out.events.push(RadioEvent::SourceChange {
                                    call_id: *call,
                                    unit,
                                });
                            }
                        }
                    }
                    VoiceTagKind::Terminate => {
                        acct.term_events += 1;
                        acct.after_term_samples +=
                            i64::try_from(audio.len()).unwrap_or(0) - i64::from(t.offset);
                    }
                    VoiceTagKind::Alias(a) => {
                        if let Some(alias) = alias::decode(a, *source) {
                            tracing::info!(
                                radio_id = alias.radio_id,
                                alias = %alias.alias,
                                source = %alias.source,
                                "new unit alias"
                            );
                            out.events.push(RadioEvent::UnitAlias {
                                sys_num: *sys_num,
                                alias,
                            });
                        }
                    }
                    VoiceTagKind::Group(_) => {}
                }
            }
            if audio.is_empty() {
                return Vec::new();
            }
            if (*levels - 1.0).abs() > f64::EPSILON {
                for s in audio.iter_mut() {
                    *s = level(*s, *levels);
                }
            }
            rec.deliver(audio)
        });
        out.audio(*call, &pcm.unwrap_or_default());
    }

    fn retune(&mut self) {}
}

#[cfg(test)]
mod tests;
