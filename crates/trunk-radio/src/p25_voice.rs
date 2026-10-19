//! P25 voice end to end in trunk-p25: demodulate, decode, assemble the call
//! ([`trunk_p25::assemble`]), and vocode each frame
//! ([`trunk_p25::vocoder`]).

use std::sync::Arc;

use num_complex::Complex32;
use trunk_p25::assemble::{CallCounter, CallEvent, Phase1Assembler, Phase2Assembler, Vocode};
use trunk_p25::control::Received;
use trunk_p25::tdma::{BurstContent, Scrambler, TdmaDecoder, op25_view};
use trunk_p25::vocoder::{AmbeVocoder, Decision, ImbeVocoder, Pcm, SAMPLES_PER_FRAME};
use trunk_p25::voice::signalling::ALGID_CLEAR;
use trunk_p25::voice::{Encryption, EncryptionTracker, VoiceDecoder, VoiceEvent};
use trunk_p25::{DemodConfig, Modulation};

use crate::p25::{VoiceShared, packed_encryption, track_encryption};
use crate::types::{CallCounts, VoiceTag, VoiceTagKind};

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

/// One recorder's voice receiver.
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
        Self::for_recorder(rate, -1)
    }

    /// For recorder `recorder`, whose decoder counters it reports
    /// ([`crate::p25::voice_channels`]).
    pub fn for_recorder(rate: f64, recorder: i32) -> Self {
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
            shared: VoiceShared::register(recorder),
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
            self.p2 = TdmaDecoder::new(DemodConfig::phase2(self.rate), scrambler);
            self.a2.start_call(slot);
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

// ── A recorder's voice decoder, as the recorder drives it ──

/// A recorder's voice decoder: audio out, with the in-band tags at their
/// sample offsets.
pub struct P25VoiceHandle {
    pipeline: VoicePipeline,
    out: Vec<CallOutput>,
    /// A terminator has been tagged since the last audio (op25 tags one per
    /// transmission's end, however many TDUs follow).
    terminated: bool,
}

impl P25VoiceHandle {
    /// For recorder `recorder` at the conditioned channel `rate`.
    pub fn new(rate: f64, recorder: i32) -> Self {
        Self {
            pipeline: VoicePipeline::for_recorder(rate, recorder),
            out: Vec::new(),
            terminated: false,
        }
    }

    /// A new call: CQPSK (else C4FM), phase 2 on `slot` with the system's
    /// 2160-dibit scrambling mask.
    pub fn start_call(&mut self, qpsk: bool, tdma: bool, xor_mask: &[u8], system: &str, slot: i32) {
        let modulation = if qpsk {
            Modulation::Cqpsk
        } else {
            Modulation::C4fm
        };
        self.pipeline.start_call(
            modulation,
            tdma,
            Scrambler::from_dibits(xor_mask),
            u8::try_from(slot).unwrap_or(0),
        );
        self.pipeline.shared.call(system);
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
            let mut tag = |kind, value: i64, alias| {
                tags.push(VoiceTag {
                    offset,
                    kind,
                    value,
                    alias,
                });
            };
            match o {
                CallOutput::Audio { pcm, .. } => {
                    audio.extend(pcm);
                    self.terminated = false;
                }
                CallOutput::Event(CallEvent::Source(s)) => {
                    tag(VoiceTagKind::Source, i64::from(s), None);
                }
                CallOutput::Event(CallEvent::Group(g)) => {
                    tag(VoiceTagKind::Group, i64::from(g), None);
                }
                CallOutput::Event(CallEvent::Terminator) => {
                    if !self.terminated {
                        self.terminated = true;
                        tag(VoiceTagKind::Terminate, 1, None);
                    }
                }
                CallOutput::Event(CallEvent::Alias(a)) => {
                    tag(VoiceTagKind::Alias, 0, Some(a));
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
    pub const fn take_crypto(&mut self) -> u32 {
        packed_encryption(self.pipeline.crypto_pending.take())
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

#[cfg(test)]
mod tests;
