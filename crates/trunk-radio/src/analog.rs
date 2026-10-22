//! [`AnalogFm`]: an analog FM channel, demodulated and gated by trunk-fm.
//!
//! The gate (noise squelch, CTCSS, lookahead) is the channel's transmission
//! detector: its opens and closes are reported as [`RadioEvent::Squelch`]
//! and only what it passes is recorded.

use std::sync::Arc;

use num_complex::Complex32;
use trunk_channelizer::{ChannelDecoder, ConditioningSpec, Reading, WIDE};
use trunk_fm::denoise::Denoiser;
use trunk_fm::dtmf::Dtmf;
use trunk_fm::fleetsync::FleetSync;
use trunk_fm::gate::{Config as GateConfig, Event as GateEvent, Gate};
use trunk_fm::tones::Tones;
use trunk_fm::{
    Audio, AudioTap, Decimation, Detector, FmDemod, Heard, Measurement, Mute, TAU_US, Tone,
};

use crate::fm;
use crate::kind::{Cmd, Kind, OpenError, Opening, Tuning};
use crate::p25_voice::EXCESS_BW;
use crate::recording::Tape;
use crate::types::{CallId, Decoded, Muted, RadioEvent, Squelch};

/// Conditioned rate of an analog slot.
pub const WIDE_RATE: f64 = 96_000.0;
/// The analog channel's width, for its filter (cutoff `max_dev`,
/// transition half of this).
const BANDWIDTH_HZ: f64 = 12_000.0;

/// Samples blanked ahead of a signalling burst's first symbol and after its
/// last: the span the decoder reports is good to a symbol at the front and
/// the burst's tail (the filters', and a codec's) runs a little past the end.
const MUTE_BEFORE: u64 = 14;
const MUTE_AFTER: u64 = 40;
/// Samples faded to meet a blanked burst: 2 ms, below what is heard as a
/// click and a fraction of a syllable.
const MUTE_FADE: usize = 32;

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
                // The discriminator reads the carrier offset (see
                // `AnalogFmChannel::freq_error_hz`) and ignores amplitude.
                carrier_lock: false,
            },
            filter: Some((f64::from(at.system.max_dev), BANDWIDTH_HZ / 2.0)),
            squelch_db: None,
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn decoder(at: &Opening<'_, Self>, tape: Tape) -> Result<AnalogFmChannel, OpenError> {
        let tone = at.params.tone;
        let demod = FmDemod::new(WIDE_RATE, TAU_US, Decimation::AntiAliased);
        Ok(AnalogFmChannel {
            call: at.call.id,
            voice_delay: demod.voice_delay() as u64,
            demod,
            detector: Detector::new(),
            taps: vec![
                Box::new(FleetSync::new()),
                Box::new(Dtmf::new()),
                Box::new(Tones::new()),
            ],
            raw_samples: 0,
            gate_pos: 0,
            heard: Vec::new(),
            mutes: Vec::new(),
            held: Vec::new(),
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
            carrier: Carrier::default(),
        })
    }
}

#[allow(clippy::cast_possible_truncation)]
fn level(s: f32, k: f32) -> i16 {
    (s * k * 32767.0).round().clamp(-32768.0, 32767.0) as i16
}

/// The carrier's offset from the tuning, read off the discriminator over the
/// hops that have a carrier in them: speech and tones average to nothing, so
/// what is left is the carrier. It replaces the FLL, which at 2 samples per symbol looks at
/// frequencies well outside a 12.5 kHz channel; against the same calls its
/// readings spread four times as far.
#[derive(Debug, Default)]
struct Carrier {
    /// The discriminator output since the last hop was judged, 1.0 being
    /// the demodulator's full deviation.
    pending: (f64, u64),
    /// Its sum and length over the carrier hops of this transmission.
    sum: f64,
    len: u64,
    /// The last transmission's reading, kept until the next one has enough
    /// samples to replace it.
    held: Option<i32>,
}

impl Carrier {
    /// Samples (0.25 s) before a mean is worth reading.
    const MIN_LEN: u64 = 24_000;

    /// A block's discriminator output.
    const fn block(&mut self, sum: f64, len: u64) {
        self.pending.0 += sum;
        self.pending.1 += len;
    }

    /// A hop was measured: what came since the last one counts if it had a
    /// carrier. Without one, and with no transmission going on (`keep`
    /// false), what was gathered was noise and starts over; the gate lags
    /// the carrier, so inside a transmission a quiet hop only adds nothing.
    const fn hop(&mut self, carrier: bool, keep: bool) {
        if carrier {
            self.sum += self.pending.0;
            self.len += self.pending.1;
        } else if !keep {
            self.sum = 0.0;
            self.len = 0;
        }
        self.pending = (0.0, 0);
    }

    /// The reading so far, if enough: the FLL's sign, positive when the
    /// carrier is below the tuning.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn now(&self) -> Option<i32> {
        (self.len >= Self::MIN_LEN)
            .then(|| (-self.sum / self.len as f64 * trunk_fm::MAX_DEV_HZ).round() as i32)
    }

    fn error_hz(&self) -> Option<i32> {
        self.now().or(self.held)
    }

    /// The transmission is over.
    fn end(&mut self) {
        self.held = self.error_hz();
        self.sum = 0.0;
        self.len = 0;
    }
}

/// An analog channel's decoder, recording one call.
pub struct AnalogFmChannel {
    call: CallId,
    demod: FmDemod,
    detector: Detector,
    /// In-band signalling is read from the same raw audio the detector sees.
    taps: Vec<Box<dyn AudioTap>>,
    /// Samples the recording lags the raw audio by.
    voice_delay: u64,
    /// Samples of raw audio the signalling decoders have been fed.
    raw_samples: u64,
    /// Samples of the recording given to the gate. The recording's samples
    /// are counted as the raw audio's are, so a burst's span is a span of
    /// the recording `voice_delay` later.
    gate_pos: u64,
    heard: Vec<Heard>,
    /// Bursts blanked so far from the audio that had come out of the
    /// recording's filters, and still to be from what comes next.
    mutes: Vec<Mute>,
    /// What was heard before the gate opened for it: the gate holds a
    /// transmission's first moments back, so a burst at key-up is decoded
    /// ahead of the open it belongs to.
    held: Vec<(Heard, Muted)>,
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
    carrier: Carrier,
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
        self.carrier
            .block(self.audio.discriminator_sum, self.audio.discriminator_len);
        self.measurements.clear();
        self.detector
            .process(&self.audio.raw, &mut self.measurements);
        for m in &self.measurements {
            let carrier = m.quieting_db >= self.gate.config().close_db;
            self.carrier.hop(carrier, self.gate.is_open());
        }
        let fresh = self.voice_pending.len();
        self.voice_pending.extend_from_slice(&self.audio.voice);
        // Bursts already read reach on into audio that has only now come
        // out of the recording's filters.
        let first_fresh = self.gate_pos + fresh as u64;
        for mute in &self.mutes {
            mute.apply(first_fresh, &mut self.voice_pending[fresh..]);
        }
        let total = first_fresh + self.audio.voice.len() as u64;
        self.mutes.retain(|mute| mute.window().end > total);
        for tap in &mut self.taps {
            tap.process(&self.audio.raw, &mut self.heard);
        }
        // in the order they happened, not the taps'
        self.heard.sort_by_key(|heard| heard.span.start);
        self.raw_samples += self.audio.raw.len() as u64;
        self.blank_bursts(total);
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
            self.gate_pos += voice.len() as u64;
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
                GateEvent::Close { reason, .. } => {
                    self.carrier.end();
                    Squelch::Close((*reason).into())
                }
            };
            out.events.push(RadioEvent::Squelch {
                call_id: self.call,
                squelch,
                power_db,
            });
        }
        // Signalling goes out behind the open it belongs to, and only while
        // a transmission is on; what is heard outside one, and not about to
        // be inside it, is dropped rather than credited to the next.
        let opened = self
            .gate_events
            .iter()
            .any(|e| matches!(e, GateEvent::Open { .. }));
        if opened || self.gate.is_open() {
            out.events.extend(
                self.held
                    .drain(..)
                    .map(|(heard, muted)| RadioEvent::Signalling {
                        call_id: self.call,
                        heard,
                        muted,
                    }),
            );
        } else {
            let oldest = self
                .raw_samples
                .saturating_sub((self.gate.delay_samples() + 2 * trunk_fm::detect::HOP) as u64);
            self.held.retain(|(h, _)| h.span.end >= oldest);
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

    fn freq_error_hz(&self) -> Option<i32> {
        self.carrier.error_hz()
    }

    fn retune(&mut self) {
        self.carrier = Carrier::default();
        self.demod.reset();
        self.detector.reset();
        for tap in &mut self.taps {
            tap.reset();
        }
        self.raw_samples = 0;
        self.gate_pos = 0;
        self.mutes.clear();
        self.held.clear();
        self.gate.reset();
        self.voice_pending.clear();
    }
}

impl AnalogFmChannel {
    /// Takes each burst just read out of what is recorded. A burst is read
    /// as its last bit ends, well ahead of its audio leaving the gate's delay
    /// line, so it can be, whether or not the gate goes on to open for it.
    /// `total` is the samples of the recording that exist.
    fn blank_bursts(&mut self, total: u64) {
        for heard in self.heard.drain(..) {
            let mute = Mute {
                span: (heard.span.start + self.voice_delay).saturating_sub(MUTE_BEFORE)
                    ..heard.span.end + self.voice_delay + MUTE_AFTER,
                fade: MUTE_FADE,
            };
            let late = self.gate.mute(&mute);
            mute.apply(self.gate_pos, &mut self.voice_pending);
            let length = mute.span.end - mute.span.start;
            let muted = Muted {
                blanked: length - late.min(length),
                late: late.min(length),
            };
            self.held.push((heard, muted));
            if mute.window().end > total {
                self.mutes.push(mute);
            }
        }
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

    /// What a transmission came to.
    struct Transmitted {
        /// Samples recorded.
        audio: usize,
        /// The open, the in-band signalling and the close, in the order the
        /// monitor loop would see them.
        events: Vec<String>,
        /// What was recorded.
        pcm: Vec<i16>,
        /// What blanking the signalling came to.
        muted: Vec<Muted>,
        /// The channel's reading of the carrier's offset when it was over.
        freq_error_hz: i32,
    }

    fn record_one_transmission(
        noise_suppression: Option<crate::NoiseSuppression>,
        port: usize,
    ) -> usize {
        transmit(noise_suppression, port, None).audio
    }

    /// Records 2 s of PL'd carrier between noise on slot `port`, checks the
    /// gate opened and closed once, and reports what came of it. `burst`
    /// (16 kHz audio) is sent as the first thing on the carrier, before
    /// the talking.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    fn transmit(
        noise_suppression: Option<crate::NoiseSuppression>,
        port: usize,
        burst: Option<&[f32]>,
    ) -> Transmitted {
        transmit_off(noise_suppression, port, burst, 0.0)
    }

    /// [`transmit`] with the carrier `offset_hz` above the slot's tuning.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    fn transmit_off(
        noise_suppression: Option<crate::NoiseSuppression>,
        port: usize,
        burst: Option<&[f32]>,
        offset_hz: f64,
    ) -> Transmitted {
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
                let at = ((t - 1.0) * 16_000.0) as usize;
                let m = 700.0 * (std::f64::consts::TAU * 162.2 * t).sin()
                    + match burst {
                        Some(b) if at < b.len() => 2_000.0 * f64::from(b[at]),
                        Some(b) => {
                            let _ = b;
                            2_000.0 * (std::f64::consts::TAU * 800.0 * t).sin()
                        }
                        None => 2_000.0 * (std::f64::consts::TAU * 800.0 * t).sin(),
                    };
                phase += std::f64::consts::TAU * (100_000.0 + offset_hz + m) / rate;
            } else {
                phase += std::f64::consts::TAU * f64::from(rnd() - 0.5);
            }
            iq.push(Complex32::from_polar(0.3, phase as f32));
        }
        let mut out = Decoded::default();
        let (mut opened, mut closed, mut audio) = (0, 0, 0);
        let mut events = Vec::new();
        let (mut recorded, mut muted) = (Vec::new(), Vec::new());
        for block in iq.chunks_exact(cfg.hop()) {
            chz.run(block, &mut out);
            for e in out.events.drain(..) {
                match e {
                    RadioEvent::Squelch {
                        call_id, squelch, ..
                    } => {
                        assert_eq!(call_id, CallId(42));
                        match squelch {
                            Squelch::Open => {
                                opened += 1;
                                events.push("open".to_owned());
                            }
                            Squelch::Close(r) => {
                                assert_eq!(r, CloseReason::Quieting);
                                closed += 1;
                                events.push("close".to_owned());
                            }
                        }
                    }
                    RadioEvent::Signalling {
                        call_id,
                        heard,
                        muted: m,
                    } => {
                        assert_eq!(call_id, CallId(42));
                        events.push(heard.signal.to_string());
                        muted.push(m);
                    }
                    _ => {}
                }
            }
            for (id, pcm) in out.audio.drain(..) {
                assert_eq!(id, CallId(42));
                audio += pcm.len();
                recorded.extend_from_slice(&pcm);
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
        Transmitted {
            audio,
            events,
            pcm: recorded,
            muted,
            freq_error_hz: chz.slot_freq_error_hz(port),
        }
    }

    #[test]
    fn an_analog_channel_gates_a_pl_transmission_and_records_it() {
        let plain = record_one_transmission(None, 1);
        // about 2 s of 16 kHz audio came out under the call id
        assert!((28_000..=36_000).contains(&plain), "{plain}");
    }

    #[test]
    fn an_analog_channel_reads_its_carriers_offset_from_the_discriminator() {
        // a carrier 120 Hz above the tuning reads as the FLL would: -120
        let above = transmit_off(None, 10, None, 120.0).freq_error_hz;
        assert!((-130..=-110).contains(&above), "{above}");
        let below = transmit_off(None, 11, None, -75.0).freq_error_hz;
        assert!((65..=85).contains(&below), "{below}");
    }

    #[test]
    fn a_carrier_reads_only_once_it_has_enough_samples_and_is_held_after() {
        let mut c = Carrier::default();
        assert_eq!(c.error_hz(), None);
        // 0.1 s at +40 Hz (0.008 of the deviation): too short
        c.block(0.008 * 9_600.0, 9_600);
        c.hop(true, false);
        assert_eq!(c.error_hz(), None);
        // 0.3 s in, it reads, and the sign is the FLL's
        c.block(0.008 * 19_200.0, 19_200);
        c.hop(true, false);
        assert_eq!(c.error_hz(), Some(-40));
        // noise inside the transmission adds nothing, whatever it averages to
        c.block(4_000.0, 96_000);
        c.hop(false, true);
        assert_eq!(c.error_hz(), Some(-40));
        c.end();
        // the next transmission's first moments do not wipe the reading,
        // and noise before it never counts
        c.block(0.5, 100);
        c.hop(false, false);
        c.block(0.5, 100);
        c.hop(true, false);
        assert_eq!(c.error_hz(), Some(-40));
    }

    #[test]
    fn a_fleetsync_burst_at_key_up_is_reported_behind_the_open() {
        let burst: Vec<f32> =
            include_bytes!("../../trunk-fm/tests/data/fleetsync_atherton_burst.s16")
                .chunks_exact(2)
                .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
                .collect();
        let sent = transmit(None, 4, Some(&burst));
        assert_eq!(
            sent.events,
            ["open", "FleetSync 120-2182 (type 0x80, code 0xfc)", "close"]
        );
        // and a transmission without one reports none
        assert_eq!(transmit(None, 5, None).events, ["open", "close"]);
    }

    /// Energy at `hz`, summed over 10 ms windows so that a tone that comes
    /// and goes (a burst of FSK) is counted wherever it falls.
    fn tone_energy(pcm: &[i16], hz: f64) -> f64 {
        pcm.as_chunks::<160>()
            .0
            .iter()
            .map(|w| {
                let (mut re, mut im) = (0.0, 0.0);
                for (n, &x) in w.iter().enumerate() {
                    let phase = std::f64::consts::TAU * hz * n as f64 / 16_000.0;
                    re += f64::from(x) * phase.cos();
                    im -= f64::from(x) * phase.sin();
                }
                re * re + im * im
            })
            .sum()
    }

    #[test]
    fn a_decoded_burst_is_blanked_from_the_recording_and_the_speech_after_it_is_not() {
        let burst: Vec<f32> =
            include_bytes!("../../trunk-fm/tests/data/fleetsync_atherton_burst.s16")
                .chunks_exact(2)
                .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
                .collect();
        // the same squawk played backwards: as much 1200/1800 Hz as before,
        // and nothing a decoder can read
        let reversed: Vec<f32> = burst.iter().rev().copied().collect();

        let heard = transmit(None, 6, Some(&burst));
        let unread = transmit(None, 7, Some(&reversed));
        assert_eq!(heard.events.len(), 3, "{:?}", heard.events);
        assert_eq!(unread.events, ["open", "close"]);
        assert_eq!(
            heard.muted.len(),
            1,
            "blanked before it was recorded: {:?}",
            heard.muted
        );
        assert_eq!(heard.muted[0].late, 0);
        assert!(heard.muted[0].blanked > 1_400, "{:?}", heard.muted);

        // from the start-of-transmission thump (the first 50 ms, the same
        // either way) to where the stand-in for speech comes in
        let squawk = |t: &Transmitted| {
            let pcm = &t.pcm[800..6_400];
            tone_energy(pcm, 1_200.0) + tone_energy(pcm, 1_800.0)
        };
        let (left, kept) = (squawk(&heard), squawk(&unread));
        assert!(kept > 0.0 && left < 0.005 * kept, "{left} of {kept} left");
        // and the 800 Hz that follows it, the stand-in for speech, is whole
        let (speech, original) = (
            tone_energy(&heard.pcm, 800.0),
            tone_energy(&unread.pcm, 800.0),
        );
        assert!(original > 0.0 && (speech / original - 1.0).abs() < 0.01);
        assert_eq!(heard.pcm.len(), unread.pcm.len());
    }

    #[test]
    fn dtmf_keys_at_key_up_are_reported_behind_the_open_and_blanked_from_the_recording() {
        // 0.2 s of nothing, then 5, 1 and 9 as 100 ms of tone and 100 ms
        // of nothing each, as an autodialler sends them
        let mut keys = vec![0.0f32; 3_200];
        for (low, high) in [(770.0, 1_336.0), (697.0, 1_209.0), (852.0, 1_477.0)] {
            keys.extend((0..1_600).map(|n| {
                let t = f64::from(n) / 16_000.0;
                (0.4 * ((std::f64::consts::TAU * low * t).sin()
                    + (std::f64::consts::TAU * high * t).sin())
                    / 2.0) as f32
            }));
            keys.extend(vec![0.0f32; 1_600]);
        }
        let sent = transmit(None, 9, Some(&keys));
        assert_eq!(
            sent.events,
            ["open", "DTMF 5", "DTMF 1", "DTMF 9", "close"],
            "{:?}",
            sent.muted
        );
        assert!(sent.muted.iter().all(|m| m.late == 0 && m.blanked > 1_500));

        // what is recorded across the keys (after the start-of-transmission
        // thump) is nothing, beside the 800 Hz that stands in for speech
        let rms = |pcm: &[i16]| {
            (pcm.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt()
        };
        let keyed = rms(&sent.pcm[800..keys.len()]);
        let speech = rms(&sent.pcm[keys.len() + 3_200..keys.len() + 11_200]);
        assert!(
            speech > 1_000.0 && keyed < 0.05 * speech,
            "{keyed} vs {speech}"
        );
    }

    #[test]
    fn noise_suppression_keeps_every_sample_and_flushes_at_the_close() {
        let plain = record_one_transmission(None, 2);
        let suppressed = record_one_transmission(Some(crate::NoiseSuppression::Db12), 3);
        assert_eq!(suppressed, plain + trunk_fm::denoise::DELAY);
    }
}
