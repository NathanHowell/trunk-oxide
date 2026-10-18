//! Regeneration: re-send a voice channel with every decodable field
//! corrected, as a P25 repeater does.
//!
//! The output is the input symbol stream, one symbol per symbol, delayed only
//! as long as a frame in flight may still claim it. Each completed frame is
//! re-encoded from its decoded content (sync, NID, IMBE codewords, and link
//! control, encryption sync or header when those decoded); fields that did
//! not decode, the status symbols and the low-speed data pass through as
//! received. A downstream op25 frame assembler then sees error-free frames
//! and does the rest (vocoder, decryption, aliases, call tags) unchanged.

use std::collections::VecDeque;

use num_complex::Complex32;

use super::layout::{IMBE_BASE, IMBE_BITS, PAYLOAD, imbe_bit, raw_bit, signalling_bit};
use super::signalling::encode;
use super::{Terminator, VoiceEvent, VoiceStats, decode_frame};
use crate::control::Received;
use crate::demod::{DemodConfig, Demodulator};
use crate::framer::{Frame, Framer, nid_word};
use crate::symbols::{LEVELS, slice, sync_dibits};

/// A frame's dibits, written in data-bit coordinates.
struct FrameDibits(Vec<u8>);

impl FrameDibits {
    fn set(&mut self, p: usize, bit: bool) {
        let r = raw_bit(p);
        if let Some(d) = self.0.get_mut(r / 2) {
            let mask = if r.is_multiple_of(2) { 2 } else { 1 };
            *d = if bit { *d | mask } else { *d & !mask };
        }
    }

    fn write(&mut self, from: usize, bits: &[bool]) {
        for (i, &b) in bits.iter().enumerate() {
            self.set(from + i, b);
        }
    }
}

/// `frame` re-encoded from what decoded out of it.
pub fn regenerate(frame: &Frame, event: &VoiceEvent) -> Vec<u8> {
    let mut d = FrameDibits(frame.raw.iter().map(|&s| slice(s)).collect());
    let sync: Vec<bool> = sync_dibits()
        .iter()
        .flat_map(|&x| [x & 2 != 0, x & 1 != 0])
        .collect();
    d.write(0, &sync);
    let nid = nid_word(frame.nac, frame.duid);
    d.write(
        48,
        &(0..64)
            .rev()
            .map(|b| (nid >> b) & 1 == 1)
            .collect::<Vec<_>>(),
    );
    if let Some(frames) = event.frames() {
        for (i, f) in frames.iter().enumerate() {
            let cw = f.params.encode();
            for j in 0..IMBE_BITS {
                d.set(imbe_bit(IMBE_BASE[i], j), cw.bit(j));
            }
        }
    }
    let signalling = match event {
        VoiceEvent::Ldu1 {
            link_control: Some(lc),
            ..
        } => Some(encode::link_control_ldu1(&lc.value.0)),
        VoiceEvent::Ldu2 {
            encryption: Some(es),
            ..
        } => Some(encode::encryption_sync(
            &es.value.mi,
            es.value.algid,
            es.value.keyid,
        )),
        _ => None,
    };
    if let Some(bits) = signalling {
        for (j, &b) in bits.iter().enumerate() {
            d.set(signalling_bit(j), b);
        }
    }
    match event {
        VoiceEvent::Header {
            header: Some(h), ..
        } => {
            let h = h.value;
            d.write(
                PAYLOAD,
                &encode::header(&h.mi, h.mfid, h.algid, h.keyid, h.talkgroup),
            );
        }
        VoiceEvent::Terminator {
            terminator: Terminator::LinkControl(Some(lc)),
            ..
        } => {
            d.write(PAYLOAD, &encode::link_control_tdulc(&lc.value.0));
        }
        _ => {}
    }
    d.0
}

/// op25's per-call "spike" count: frames whose corrected-bit count exceeds
/// the standard deviation of the last 20 frames' counts (`p25p1_fdma.cc`).
#[derive(Clone, Debug, Default)]
struct SpikeDetector {
    history: VecDeque<u32>,
}

impl SpikeDetector {
    #[allow(clippy::cast_precision_loss)]
    fn is_spike(&mut self, errors: u32) -> bool {
        self.history.push_front(errors);
        self.history.truncate(20);
        let n = self.history.len() as f64;
        let mean = self.history.iter().map(|&e| f64::from(e)).sum::<f64>() / n;
        let var = self
            .history
            .iter()
            .map(|&e| (f64::from(e) - mean).powi(2))
            .sum::<f64>()
            / n;
        f64::from(errors) > var.sqrt()
    }
}

/// Errors and spikes since the last [`Regenerator::take_call_counts`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallCounts {
    pub errors: u64,
    pub spikes: u64,
}

pub struct Regenerator {
    demod: Demodulator,
    framer: Framer,
    soft: Vec<f32>,
    frames: Vec<Frame>,
    /// Hard dibits not yet released, the first at symbol index `line_start`.
    line: VecDeque<u8>,
    line_start: u64,
    stats: VoiceStats,
    spikes: SpikeDetector,
    call: CallCounts,
}

impl Regenerator {
    pub fn new(demod: DemodConfig) -> Self {
        Self {
            demod: Demodulator::new(demod),
            framer: Framer::new(),
            soft: Vec::new(),
            frames: Vec::new(),
            line: VecDeque::new(),
            line_start: 0,
            stats: VoiceStats::default(),
            spikes: SpikeDetector::default(),
            call: CallCounts::default(),
        }
    }

    /// Start a new call with `demod` (the modulation may differ from the
    /// last call's): loops and NAC start over.
    pub fn start_call(&mut self, demod: DemodConfig) {
        self.demod = Demodulator::new(demod);
        self.reset();
    }

    /// Start a new call: loops and NAC start over, and symbols still held
    /// from the last call are dropped. They came from its channel, so let
    /// out they would open this call with the last one's frames.
    pub fn reset(&mut self) {
        self.demod.reset();
        self.framer.reset();
        self.line.clear();
        self.line_start = self.framer.position();
        self.spikes = SpikeDetector::default();
        self.call = CallCounts::default();
    }

    pub fn stats(&self) -> VoiceStats {
        VoiceStats {
            demod: self.demod.stats(),
            framer: self.framer.stats,
            ..self.stats
        }
    }

    pub const fn take_call_counts(&mut self) -> CallCounts {
        std::mem::replace(
            &mut self.call,
            CallCounts {
                errors: 0,
                spikes: 0,
            },
        )
    }

    /// Demodulate `iq`; append regenerated symbols (ideal levels) to `out`
    /// and the decoded events to `events`.
    pub fn process(
        &mut self,
        iq: &[Complex32],
        out: &mut Vec<f32>,
        events: &mut Vec<Received<VoiceEvent>>,
    ) {
        let mut soft = std::mem::take(&mut self.soft);
        soft.clear();
        self.demod.process(iq, &mut soft);
        self.process_symbols(&soft, out, events);
        self.soft = soft;
    }

    pub fn process_symbols(
        &mut self,
        soft: &[f32],
        out: &mut Vec<f32>,
        events: &mut Vec<Received<VoiceEvent>>,
    ) {
        for &s in soft {
            self.line.push_back(slice(s));
            self.framer.push(s, &mut self.frames);
            for frame in std::mem::take(&mut self.frames) {
                let Some(event) = decode_frame(&frame) else {
                    continue;
                };
                self.count(&event);
                self.overwrite(frame.start, &regenerate(&frame, &event));
                events.push(Received {
                    symbol: self.framer.position(),
                    message: event,
                });
            }
            let settled = self.framer.settled_before();
            while self.line_start < settled {
                let Some(d) = self.line.pop_front() else {
                    break;
                };
                out.push(LEVELS[usize::from(d)]);
                self.line_start += 1;
            }
        }
    }

    fn overwrite(&mut self, start: u64, dibits: &[u8]) {
        for (i, &d) in dibits.iter().enumerate() {
            let at = start + i as u64;
            if let Some(slot) = at
                .checked_sub(self.line_start)
                .and_then(|o| self.line.get_mut(usize::try_from(o).ok()?))
            {
                *slot = d;
            }
        }
    }

    fn count(&mut self, event: &VoiceEvent) {
        self.stats.count(event);
        if let Some(frames) = event.frames() {
            for f in frames {
                self.call.errors += u64::from(f.errors);
                self.call.spikes += u64::from(self.spikes.is_spike(f.errors));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demod::Modulation;
    use crate::framer::Duid;
    use crate::testgen::{self, Channel, Rng};
    use crate::types::Nac;
    use crate::voice::{ImbeParams, VoiceDecoder, imbe, signalling};

    const NAC: Nac = Nac::new(0x38d).unwrap();
    const LC: [u8; 9] = [0x00, 0x00, 0x00, 0x00, 0x03, 0x61, 0x12, 0x34, 0x56];

    fn call(rng: &mut Rng, superframes: usize) -> (Vec<u8>, Vec<ImbeParams>) {
        let mut sent = Vec::new();
        #[allow(clippy::cast_possible_truncation)]
        let mut d: Vec<u8> = (0..300).map(|_| (rng.next_u64() & 3) as u8).collect();
        d.extend(testgen::hdu(NAC, &[0; 9], 0, 0x80, 0, 865));
        for _ in 0..superframes {
            for duid in [Duid::Ldu1, Duid::Ldu2] {
                let p: [ImbeParams; 9] = std::array::from_fn(|_| testgen::random_imbe(rng));
                sent.extend(p);
                let sig = if duid == Duid::Ldu1 {
                    signalling::encode::link_control_ldu1(&LC)
                } else {
                    signalling::encode::encryption_sync(&[0; 9], 0x80, 0)
                };
                d.extend(testgen::ldu(NAC, duid, &p, &sig));
            }
        }
        d.extend(testgen::tdulc(NAC, &LC));
        d.extend(vec![0u8; 300]);
        (d, sent)
    }

    #[test]
    fn a_clean_stream_comes_out_unchanged() {
        let mut rng = Rng::new(91);
        let (dibits, _) = call(&mut rng, 2);
        let mut r = Regenerator::new(DemodConfig::new(24000.0, Modulation::Cqpsk));
        let (mut out, mut events) = (Vec::new(), Vec::new());
        r.process_symbols(&crate::framer::ideal(&dibits), &mut out, &mut events);
        let back: Vec<u8> = out.iter().map(|&s| slice(s)).collect();
        assert_eq!(back[..], dibits[..back.len()]);
        assert!(
            back.len() + 30 >= dibits.len(),
            "released {} of {}",
            back.len(),
            dibits.len()
        );
    }

    #[test]
    fn a_new_call_drops_the_last_calls_held_symbols() {
        let mut rng = Rng::new(93);
        let (first, _) = call(&mut rng, 2);
        let (second, _) = call(&mut rng, 2);
        let mut r = Regenerator::new(DemodConfig::new(24000.0, Modulation::Cqpsk));
        let (mut out, mut events) = (Vec::new(), Vec::new());
        // The recorder leaves mid-frame, with some of it held back.
        let cut = &first[..first.len() / 2];
        r.process_symbols(&crate::framer::ideal(cut), &mut out, &mut events);
        assert!(out.len() < cut.len(), "some of the first call is held");

        r.start_call(DemodConfig::new(24000.0, Modulation::Cqpsk));
        let before = out.len();
        r.process_symbols(&crate::framer::ideal(&second), &mut out, &mut events);
        let back: Vec<u8> = out[before..].iter().map(|&s| slice(s)).collect();
        assert_eq!(back[..], second[..back.len()]);
    }

    #[test]
    fn regenerated_noisy_stream_decodes_cleanly_with_op25s_hard_decoders() {
        let mut rng = Rng::new(92);
        let (dibits, sent) = call(&mut rng, 6);
        let iq = Channel {
            es_n0_db: 12.0,
            freq_offset_hz: 30.0,
            delay: 0.3,
            clock_ppm: 10.0,
            seed: 6,
        }
        .apply(&testgen::modulate_cqpsk(&dibits, 5), 5);
        let cfg = DemodConfig {
            matched_filter: true,
            ..DemodConfig::new(24000.0, Modulation::Cqpsk)
        };
        let mut r = Regenerator::new(cfg);
        let (mut out, mut events) = (Vec::new(), Vec::new());
        for chunk in iq.chunks(960) {
            r.process(chunk, &mut out, &mut events);
        }
        let counts = r.take_call_counts();
        assert!(
            counts.errors > 0,
            "the channel should have needed correcting"
        );

        // Hard-decode the regenerated stream as op25 would: every IMBE frame
        // must agree with the Rust decision, with nothing left to correct.
        let mut framer = Framer::new();
        let mut regenerated = Vec::new();
        for &s in &out {
            framer.push(s, &mut regenerated);
        }
        let mut hard_params = Vec::new();
        let mut residual = 0u32;
        for f in &regenerated {
            let bits = super::super::FrameBits::new(f);
            if matches!(f.duid, Duid::Ldu1 | Duid::Ldu2) {
                for i in 0..9 {
                    let llr = bits.imbe(i);
                    let p = imbe::decode_hard(&llr);
                    residual += imbe::decode(&llr).errors;
                    hard_params.push(p);
                }
            }
        }
        let rust_params: Vec<ImbeParams> = events
            .iter()
            .filter_map(|e| e.message.frames())
            .flatten()
            .map(|f| f.params)
            .collect();
        assert_eq!(residual, 0);
        assert_eq!(hard_params, rust_params);
        let right = rust_params
            .iter()
            .zip(&sent)
            .filter(|(a, b)| a == b)
            .count();
        assert!(right * 100 >= sent.len() * 95, "{right} of {}", sent.len());

        // And the regenerated stream's link control decodes without help.
        let mut dec = VoiceDecoder::new(DemodConfig::new(24000.0, Modulation::Cqpsk));
        let mut ev = Vec::new();
        dec.process_symbols(&out, &mut ev);
        assert!(ev.iter().any(|e| matches!(
            e.message,
            VoiceEvent::Terminator { terminator: Terminator::LinkControl(Some(lc)), .. } if lc.value.0 == LC && lc.corrected == 0
        )));
    }

    #[test]
    fn spikes_follow_op25s_rule() {
        let mut s = SpikeDetector::default();
        assert!(!s.is_spike(0));
        assert!(s.is_spike(5)); // 5 > std dev of {5, 0} = 2.5
        assert!(!s.is_spike(0));
    }
}
