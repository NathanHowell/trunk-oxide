//! P25 voice end to end in trunk-p25: demodulate, decode, assemble the call
//! ([`trunk_p25::assemble`]), and vocode each frame with op25's vocoders
//! (C++, one frame per call).

use std::fmt::Write as _;
use std::sync::Arc;

use cxx::UniquePtr;
use num_complex::Complex32;
use trunk_p25::assemble::{
    Alias, CallCounter, CallEvent, Phase1Assembler, Phase2Assembler, SAMPLES_PER_FRAME, Vocode,
};
use trunk_p25::control::Received;
use trunk_p25::tdma::{BurstContent, Scrambler, TdmaDecoder, op25_view};
use trunk_p25::voice::signalling::ALGID_CLEAR;
use trunk_p25::voice::{Encryption, EncryptionTracker, VoiceDecoder, VoiceEvent};
use trunk_p25::{DemodConfig, Modulation};

use crate::ffi;
use crate::p25::{VoiceShared, packed_encryption, track_encryption};

/// What a call produced, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallOutput {
    /// One frame's audio (160 samples at 8 kHz) and what it was made from.
    Audio {
        pcm: Vec<i16>,
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
    synth: UniquePtr<ffi::P25VoiceSynth>,
    events: Vec<Received<VoiceEvent>>,
    calls: Vec<CallEvent>,
    counter: CallCounter,
    /// The call's encryption, from the ESS on its own slot.
    crypto: EncryptionTracker,
    crypto_pending: Option<Encryption>,
    shared: Arc<VoiceShared>,
    /// Frames the phase 2 vocoder muted.
    pub muted: u64,
}

impl VoicePipeline {
    /// `rate`: the conditioned channel rate; `soft_vocoder` as op25's.
    pub fn new(rate: f64, soft_vocoder: bool) -> Self {
        Self::for_recorder(rate, -1, soft_vocoder)
    }

    /// For recorder `recorder`, whose decoder counters it reports
    /// ([`crate::p25::voice_channels`]).
    pub fn for_recorder(rate: f64, recorder: i32, soft_vocoder: bool) -> Self {
        Self {
            rate,
            phase2: false,
            p1: VoiceDecoder::new(DemodConfig::new(rate, Modulation::Cqpsk)),
            a1: Phase1Assembler::new(),
            p2: TdmaDecoder::new(DemodConfig::phase2(rate), None),
            a2: Phase2Assembler::new(0),
            synth: ffi::tr_p25_voice_synth_new(soft_vocoder),
            events: Vec::new(),
            calls: Vec::new(),
            counter: CallCounter::default(),
            crypto: EncryptionTracker::default(),
            crypto_pending: None,
            shared: VoiceShared::register(recorder, crate::p25::Impl::Rust),
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

    fn vocode(&mut self, v: Vocode) -> Vec<i16> {
        let mut pcm = vec![0i16; SAMPLES_PER_FRAME];
        match v {
            Vocode::Imbe { u, e0, et } => {
                ffi::tr_p25_voice_synth_phase1(self.synth.pin_mut(), &u, e0, et, &mut pcm);
            }
            Vocode::Ambe { dibits, encrypted } => {
                if !ffi::tr_p25_voice_synth_phase2(
                    self.synth.pin_mut(),
                    &dibits,
                    encrypted,
                    &mut pcm,
                ) {
                    self.muted += 1;
                }
            }
            Vocode::Silence => {}
        }
        pcm
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

// ── The C++ side (`p25_voice_backend`, bridge.cpp) ──

/// A recorder's voice decoder as the GR block drives it.
pub struct P25VoiceHandle {
    pipeline: VoicePipeline,
    out: Vec<CallOutput>,
    /// A terminator has been tagged since the last audio (op25 tags one per
    /// transmission's end, however many TDUs follow).
    terminated: bool,
}

pub fn p25_voice_new(rate: f64, recorder: i32, soft_vocoder: bool) -> Box<P25VoiceHandle> {
    Box::new(P25VoiceHandle {
        pipeline: VoicePipeline::for_recorder(rate, recorder, soft_vocoder),
        out: Vec::new(),
        terminated: false,
    })
}

impl P25VoiceHandle {
    /// A new call: CQPSK (else C4FM), phase 2 on `slot` with the system's
    /// 2160-dibit scrambling mask.
    pub fn start_call(
        &mut self,
        qpsk: bool,
        tdma: bool,
        xor_mask: &[u8],
        system: &str,
        primary: bool,
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
            Scrambler::from_dibits(xor_mask),
            u8::try_from(slot).unwrap_or(0),
        );
        self.pipeline.shared.call(system, primary);
        self.terminated = false;
    }

    /// Decode `iq` (interleaved); append its audio to `audio` and its tags
    /// (at offsets into what `audio` gained) to `tags`.
    pub fn process(&mut self, iq: &[f32], audio: &mut Vec<i16>, tags: &mut Vec<ffi::TrVoiceTag>) {
        let start = audio.len();
        self.out.clear();
        self.pipeline.process(crate::as_complex(iq), &mut self.out);
        let slot = self.pipeline.a2.channel;
        for o in self.out.drain(..) {
            #[allow(clippy::cast_possible_truncation)]
            let offset = (audio.len() - start) as u32;
            let mut tag = |kind, value: i64, json: String| {
                tags.push(ffi::TrVoiceTag {
                    offset,
                    kind,
                    value,
                    json,
                });
            };
            match o {
                CallOutput::Audio { pcm, .. } => {
                    audio.extend(pcm);
                    self.terminated = false;
                }
                CallOutput::Event(CallEvent::Source(s)) => {
                    tag(ffi::TrVoiceTagKind::Source, i64::from(s), String::new());
                }
                CallOutput::Event(CallEvent::Group(g)) => {
                    tag(ffi::TrVoiceTagKind::Group, i64::from(g), String::new());
                }
                CallOutput::Event(CallEvent::Terminator) => {
                    if !self.terminated {
                        self.terminated = true;
                        tag(ffi::TrVoiceTagKind::Terminate, 1, String::new());
                    }
                }
                CallOutput::Event(CallEvent::Alias(a)) => {
                    tag(ffi::TrVoiceTagKind::Json, 0, alias_json(&a, slot));
                }
                CallOutput::Event(CallEvent::Frame(_) | CallEvent::Encryption { .. }) => {}
            }
        }
    }

    /// Encryption confirmed on the call's slot since the last take.
    pub const fn take_crypto(&mut self) -> u32 {
        packed_encryption(self.pipeline.crypto_pending.take())
    }

    pub fn take_call_counts(&mut self) -> ffi::TrCallCounts {
        let c = self.pipeline.counter.take();
        ffi::TrCallCounts {
            errors: c.errors,
            spikes: c.spikes,
            algid: self
                .pipeline
                .crypto
                .confirmed()
                .map_or(ALGID_CLEAR, |e| e.algid),
        }
    }
}

/// An alias as op25's JSON message, which the recorder decodes.
fn alias_json(a: &Alias, slot: u8) -> String {
    let hex = |b: &[u8]| {
        b.iter().fold(String::new(), |mut h, x| {
            let _ = write!(h, "{x:02X}");
            h
        })
    };
    match a {
        Alias::Motorola {
            phase2,
            messages,
            blocks,
        } => {
            let blocks: Vec<String> = blocks
                .iter()
                .map(|(i, b)| format!("\"{i}\": \"{}\"", hex(b)))
                .collect();
            format!(
                "{{\"type\": \"motorola_alias_p{}\", \"messages\": {messages}, \"blocks\": {{{}}}}}",
                if *phase2 { 2 } else { 1 },
                blocks.join(", ")
            )
        }
        Alias::Harris {
            phase2,
            data,
            source,
            group,
            cache_age_ms,
        } => format!(
            "{{\"type\": \"harris_alias_p{}\", {}\"op25_src_id\": {source}, \"op25_grp_id\": {group}, \"cache_age_ms\": {cache_age_ms}, \"blocks\": {{\"0\": \"{}\"}}}}",
            if *phase2 { 2 } else { 1 },
            if *phase2 {
                format!("\"slot\": {slot}, ")
            } else {
                String::new()
            },
            hex(data)
        ),
    }
}

#[cfg(test)]
mod tests;
