//! [`AnalogFm`]: an analog FM channel, demodulated and gated by trunk-fm.
//!
//! The gate (noise squelch, CTCSS, lookahead) is the channel's transmission
//! detector: its opens and closes are reported as [`RadioEvent::Squelch`]
//! and only what it passes is recorded.

use std::sync::Arc;

use num_complex::Complex32;
use trunk_channelizer::{ChannelDecoder, ConditioningSpec, Reading, WIDE};
use trunk_fm::denoise::Denoiser;
use trunk_fm::gate::{Config as GateConfig, Event as GateEvent, Gate};
use trunk_fm::{Audio, Decimation, Detector, FmDemod, Measurement, TAU_US, Tone};

use crate::fm;
use crate::kind::{Cmd, Kind, OpenError, Opening, Tuning};
use crate::p25_voice::EXCESS_BW;
use crate::recording::Tape;
use crate::types::{CallId, Decoded, RadioEvent, Squelch};

/// Conditioned rate of an analog slot.
pub const WIDE_RATE: f64 = 96_000.0;
/// The analog channel's width, for its filter (cutoff `max_dev`,
/// transition half of this).
const BANDWIDTH_HZ: f64 = 12_000.0;

/// What an analog channel is opened with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnalogParams {
    /// The CTCSS tone a transmission must carry, if any.
    pub tone: Option<Tone>,
}

impl AnalogParams {
    /// With the tone at `tone_hz` (none for 0, or for a frequency that is
    /// not a CTCSS tone).
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn with_tone_hz(tone_hz: f64) -> Self {
        Self {
            tone: (tone_hz > 0.0)
                .then(|| Tone::from_hz(tone_hz as f32))
                .flatten(),
        }
    }
}

/// Analog narrowband FM.
#[derive(Debug)]
pub enum AnalogFm {}

impl Kind for AnalogFm {
    type Params = AnalogParams;
    type Decoder = AnalogFmChannel;
    const NAME: &'static str = "analog";
    const PROFILE: trunk_channelizer::ProfileId = WIDE;
    const AUDIO_RATE: Option<f64> = Some(trunk_fm::AUDIO_RATE);

    fn tuning(at: &Opening<'_, Self>) -> Tuning {
        Tuning {
            conditioning: ConditioningSpec {
                sps: 2,
                excess_bw: EXCESS_BW,
                in_rate: at.channelizer.out_rate(WIDE),
                out_rate: WIDE_RATE,
                // trunk-fm's own gate decides; the power probe still runs.
                squelch: None,
            },
            filter: Some((f64::from(at.system.max_dev), BANDWIDTH_HZ / 2.0)),
            squelch_db: None,
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn decoder(at: &Opening<'_, Self>, tape: Tape) -> Result<AnalogFmChannel, OpenError> {
        let tone = at.params.tone;
        Ok(AnalogFmChannel {
            call: at.call.id,
            demod: FmDemod::new(WIDE_RATE, TAU_US, Decimation::AntiAliased),
            detector: Detector::new(),
            gate: Gate::new(GateConfig::for_tone(tone)),
            audio: Audio::default(),
            measurements: Vec::new(),
            gated: Vec::new(),
            gate_events: Vec::new(),
            voice_pending: Vec::new(),
            pcm: Vec::new(),
            tape,
            stats: fm::Shared::register(
                fm::AnalogChannel {
                    system: at.system.short_name.clone(),
                    talkgroup: at.call.talkgroup,
                    freq_hz: at.call.channel.freq_hz,
                    source: at.source,
                    port: u32::try_from(at.port).unwrap_or(u32::MAX),
                },
                tone,
            ),
            levels: at.system.analog_levels as f32,
            denoise: at.system.noise_suppression.map(Denoiser::new),
            cleaned: Vec::new(),
        })
    }
}

#[allow(clippy::cast_possible_truncation)]
fn level(s: f32, k: f32) -> i16 {
    (s * k * 32767.0).round().clamp(-32768.0, 32767.0) as i16
}

/// An analog channel's decoder, recording one call.
pub struct AnalogFmChannel {
    call: CallId,
    demod: FmDemod,
    detector: Detector,
    gate: Gate,
    audio: Audio,
    measurements: Vec<Measurement>,
    gated: Vec<f32>,
    gate_events: Vec<GateEvent>,
    voice_pending: Vec<f32>,
    pcm: Vec<i16>,
    tape: Tape,
    stats: Arc<fm::Shared>,
    levels: f32,
    /// Noise suppression on the gated voice, when the system asks for it.
    denoise: Option<Denoiser>,
    cleaned: Vec<f32>,
}

impl std::fmt::Debug for AnalogFmChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalogFmChannel")
            .field("call", &self.call)
            .finish_non_exhaustive()
    }
}

impl ChannelDecoder for AnalogFmChannel {
    type Output = Decoded;
    type Command = Cmd;

    fn process(&mut self, iq: &[Complex32], reading: &Reading, out: &mut Decoded) {
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
        let power_db = (!reading.power_db.is_nan()).then_some(reading.power_db);
        for e in &self.gate_events {
            let squelch = match e {
                GateEvent::Open { .. } => Squelch::Open,
                GateEvent::Close { reason, .. } => Squelch::Close((*reason).into()),
            };
            out.events.push(RadioEvent::Squelch {
                call_id: self.call,
                squelch,
                power_db,
            });
        }
        let voice = if let Some(d) = &mut self.denoise {
            self.cleaned.clear();
            d.process(&self.gated, &mut self.cleaned);
            // A closed gate ends the transmission: its tail comes out now,
            // not at the start of the next one.
            if !self.gate.is_open() {
                d.flush(&mut self.cleaned);
            }
            &self.cleaned
        } else {
            &self.gated
        };
        if !voice.is_empty() {
            self.pcm.clear();
            self.pcm
                .extend(voice.iter().map(|&v| level(v, self.levels)));
            let pcm = self.tape.deliver(&self.pcm);
            out.audio(self.call, &pcm);
        }
    }

    fn retune(&mut self) {
        self.demod.reset();
        self.detector.reset();
        self.gate.reset();
        self.voice_pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::{Call, Chz};
    use crate::radio::SystemSetup;
    use crate::recording::{Accounting, Recording};
    use crate::types::{Channel, ChannelNum, CloseReason};
    use trunk_channelizer::SharedChannelizer;

    /// Records 2 s of PL'd carrier between noise on slot `port`, checks the
    /// gate opened and closed once, and returns the samples recorded.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn record_one_transmission(
        noise_suppression: Option<crate::NoiseSuppression>,
        port: usize,
    ) -> usize {
        let cfg = trunk_channelizer::Config::standard(912_000.0).unwrap();
        let chz: Chz = SharedChannelizer::new(cfg.clone()).unwrap();
        let center = 488_661_750.0;
        let call = Call {
            id: CallId(42),
            sys_num: 0,
            talkgroup: 2,
            channel: Channel {
                freq_hz: center + 100_000.0,
                slot: None,
            },
        };
        let system = SystemSetup {
            sys_num: 0,
            short_name: "conv".into(),
            qpsk: false,
            max_dev: 5000,
            noise_suppression,
            analog_levels: 1.0,
            digital_levels: 1.0,
            nac: 0,
            sys_id: 0,
            wacn: 0,
        };
        let params = AnalogParams::with_tone_hz(162.2);
        assert!(params.tone.is_some());
        let at = Opening::<AnalogFm> {
            call: &call,
            params: &params,
            system: &system,
            scrambler: None,
            channelizer: &cfg,
            num: ChannelNum(7),
            source: 0,
            port,
            tap_dir: None,
        };
        let tape = Tape::new(Some(Recording::new(Accounting::new(
            "conv",
            2,
            call.channel.freq_hz,
            false,
            trunk_fm::AUDIO_RATE,
        ))));
        let tuning = AnalogFm::tuning(&at);
        assert_eq!(tuning.filter, Some((5000.0, 6000.0)));
        chz.set_slot_conditioning(port, tuning.conditioning);
        let (cutoff, transition) = tuning.filter.unwrap();
        chz.set_slot_filter(port, cutoff, transition);
        chz.set_slot_decoder(
            port,
            Some(Box::new(AnalogFm::decoder(&at, tape.clone()).unwrap())),
        );
        chz.set_slot(port, WIDE, 100_000.0);

        // 1 s of noise, 2 s of carrier with PL at +100 kHz, 1 s of noise.
        let rate = 912_000.0f64;
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
        for block in iq.chunks_exact(cfg.hop()) {
            chz.run(block, &mut out);
            for e in out.events.drain(..) {
                if let RadioEvent::Squelch {
                    call_id, squelch, ..
                } = e
                {
                    assert_eq!(call_id, CallId(42));
                    match squelch {
                        Squelch::Open => opened += 1,
                        Squelch::Close(r) => {
                            assert_eq!(r, CloseReason::Quieting);
                            closed += 1;
                        }
                    }
                }
            }
            for (id, pcm) in out.audio.drain(..) {
                assert_eq!(id, CallId(42));
                audio += pcm.len();
            }
        }
        let st = fm::analog_channels()
            .into_iter()
            .find(|e| e.value.channel.talkgroup == 2 && e.value.channel.port == port as u32)
            .unwrap();
        assert_eq!(st.value.stats.opens, 1, "{:?}", st.value.stats);
        assert_eq!(st.value.stats.closes_quieting, 1, "{:?}", st.value.stats);
        assert_eq!(st.value.channel.system, "conv");
        assert_eq!((opened, closed), (1, 1));
        let (acct, tail) = tape.close().unwrap();
        assert_eq!(audio, acct.sample_count as usize);
        assert!(tail.is_empty());
        audio
    }

    #[test]
    fn an_analog_channel_gates_a_pl_transmission_and_records_it() {
        let plain = record_one_transmission(None, 1);
        // about 2 s of 16 kHz audio came out under the call id
        assert!((28_000..=36_000).contains(&plain), "{plain}");
    }

    #[test]
    fn noise_suppression_keeps_every_sample_and_flushes_at_the_close() {
        let plain = record_one_transmission(None, 2);
        let suppressed = record_one_transmission(Some(crate::NoiseSuppression::Db12), 3);
        assert_eq!(suppressed, plain + trunk_fm::denoise::DELAY);
    }
}
