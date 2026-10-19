//! Regeneration of a Phase 2 channel, as for Phase 1.
//!
//! As with [`crate::voice::Regenerator`], the symbol stream goes out again
//! with every field the Rust decoders are sure of re-encoded, so op25's TDMA
//! decoder downstream sees clean bursts and does the rest (vocoder,
//! decryption, MAC message handling, call tags) unchanged.
//!
//! What is rewritten:
//! - every ISCH while the framer holds lock (framing is known whatever a
//!   burst carries), so op25 holds sync exactly as long as the flywheel;
//! - the DUID of bursts that carried something verifiable (a MAC PDU whose
//!   CRC checked, a decoded encryption sync, mostly trusted voice frames, a
//!   near-clean ISCH) or whose DUID decision is sure;
//! - voice codewords whose decision is trusted ([`AmbeFrame::trusted`]);
//!   the rest pass through as received, so op25 decodes them as it always
//!   has (and mutes them on its running error rate);
//! - MAC PDUs that checked, and the encryption sync once it decodes — both
//!   the 2V parity and the ESS-B shares of the four 4V bursts before it,
//!   which is why output is held back six bursts (180 ms, about one Phase 1
//!   LDU).

use std::collections::VecDeque;

use num_complex::Complex32;

use super::ambe::AmbeFrame;
use super::layout::{BURST_DIBITS, DUID_AT, ESS_A, ESS_B, VOICE_2V, VOICE_4V, payload_dibit};
use super::mac::{self, MacChannel};
use super::{
    Burst, BurstContent, BurstFramer, BurstType, Scrambler, TdmaDecoder, TdmaEvent, TdmaStats,
};
use crate::assemble::CallCounter;
use crate::control::Received;
use crate::demod::{DemodConfig, Demodulator};
use crate::symbols::{LEVELS, slice};
use crate::voice::CallCounts;

/// Output is held this many symbols behind what the framer has settled.
const HOLD: u64 = 6 * BURST_DIBITS as u64;
/// A DUID decision this sure is re-sent even when nothing else in the burst
/// checked (about two bits' worth of evidence over the runner-up).
const DUID_MARGIN: f32 = 8.0;
/// An ISCH this clean shows a signal by itself.
const CLEAN_ISCH_ERRORS: u32 = 4;

/// A recent 4V burst, for writing its ESS-B share later.
#[derive(Clone, Copy, Debug)]
struct Voice4 {
    sequence: u64,
    start: u64,
    slot: usize,
}

/// Whether a burst carried something verifiable: a MAC PDU whose CRC
/// checked, a decoded encryption sync, mostly trusted voice frames, or a
/// near-clean ISCH.
pub fn verified(event: &TdmaEvent) -> bool {
    let frames = event.frames();
    let trusted = frames.iter().filter(|f| f.trusted()).count();
    let content = match &event.content {
        BurstContent::Mac(pdu) => pdu.is_some(),
        BurstContent::Voice2 { encryption, .. } => {
            encryption.is_some() || 2 * trusted >= frames.len()
        }
        BurstContent::Voice4 { .. } => 2 * trusted >= frames.len(),
        BurstContent::Unread => false,
    };
    content || event.isch.errors <= CLEAN_ISCH_ERRORS
}

/// Whether [`TdmaRegenerator`] re-sends a burst's DUID and content (else
/// only its ISCH): verified, or its DUID decision sure.
pub fn rewritten(event: &TdmaEvent) -> bool {
    verified(event) || event.duid.margin >= DUID_MARGIN
}

pub struct TdmaRegenerator {
    demod: Demodulator,
    framer: BurstFramer,
    decoder: TdmaDecoder,
    scrambler: Option<Scrambler>,
    soft: Vec<f32>,
    bursts: Vec<Burst>,
    /// Hard dibits not yet released, the first at symbol index `line_start`.
    line: VecDeque<u8>,
    line_start: u64,
    /// Per logical channel, by voice position (slot / 2).
    voice4: [[Option<Voice4>; 5]; 2],
    call: CallCounter,
    /// Bursts whose ISCH/DUID were rewritten.
    pub rewritten: u64,
}

impl TdmaRegenerator {
    pub fn new(demod: DemodConfig, scrambler: Option<Scrambler>) -> Self {
        Self {
            demod: Demodulator::new(demod),
            framer: BurstFramer::new(),
            decoder: TdmaDecoder::new(demod, scrambler.clone()),
            scrambler,
            soft: Vec::new(),
            bursts: Vec::new(),
            line: VecDeque::new(),
            line_start: 0,
            voice4: [[None; 5]; 2],
            call: CallCounter::default(),
            rewritten: 0,
        }
    }

    /// Start a new call: loops and sync start over, and symbols still held
    /// from the last call are dropped. They came from its channel (up to
    /// six bursts of them), so let out they would open this call with the last
    /// one's bursts: on another slot's recorder, its encryption sync too.
    pub fn start_call(&mut self, demod: DemodConfig, scrambler: Option<Scrambler>) {
        self.demod = Demodulator::new(demod);
        self.framer.reset();
        self.line.clear();
        self.line_start = self.framer.position();
        self.decoder.reset();
        self.decoder.set_scrambler(scrambler.clone());
        self.scrambler = scrambler;
        self.voice4 = [[None; 5]; 2];
        self.call = CallCounter::default();
    }

    pub fn stats(&self) -> TdmaStats {
        TdmaStats {
            demod: self.demod.stats(),
            framer: self.framer.stats,
            ..self.decoder.decode_stats()
        }
    }

    /// Bits corrected and untrusted frames ("spikes") since the last call, in
    /// bursts that carried a signal.
    pub fn take_call_counts(&mut self) -> CallCounts {
        self.call.take()
    }

    pub fn process(
        &mut self,
        iq: &[Complex32],
        out: &mut Vec<f32>,
        events: &mut Vec<Received<TdmaEvent>>,
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
        events: &mut Vec<Received<TdmaEvent>>,
    ) {
        for &s in soft {
            self.line.push_back(slice(s));
            self.framer.push(s, &mut self.bursts);
            for burst in std::mem::take(&mut self.bursts) {
                let event = self.decoder.decode(&burst);
                self.rewrite(&burst, &event);
                events.push(Received {
                    symbol: self.framer.position(),
                    message: event,
                });
            }
            let settled = self.framer.settled_before().saturating_sub(HOLD);
            while self.line_start < settled {
                let Some(d) = self.line.pop_front() else {
                    break;
                };
                out.push(LEVELS[usize::from(d)]);
                self.line_start += 1;
            }
        }
    }

    /// Release everything held (the input has ended).
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        out.extend(self.line.drain(..).map(|d| LEVELS[usize::from(d)]));
        self.line_start = self.framer.position();
    }

    fn put(&mut self, at: u64, dibit: u8) {
        if let Some(slot) = at
            .checked_sub(self.line_start)
            .and_then(|o| self.line.get_mut(usize::try_from(o).ok()?))
        {
            *slot = dibit;
        }
    }

    /// Payload bits from payload dibit `p`, scrambled for `slot` if asked.
    fn put_payload(&mut self, start: u64, slot: usize, p: usize, bits: &[bool], scrambled: bool) {
        for (k, pair) in bits.chunks(2).enumerate() {
            let clear = u8::from(pair[0]) << 1 | u8::from(pair.get(1).copied().unwrap_or(false));
            let m = match &self.scrambler {
                Some(s) if scrambled => s.payload(slot, p + k),
                _ => 0,
            };
            self.put(start + payload_dibit(p + k) as u64, clear ^ m);
        }
    }

    fn rewrite(&mut self, burst: &Burst, event: &TdmaEvent) {
        // The call's counts cover bursts with a signal.
        self.call.phase2(event);
        // The ISCH is framing, which the flywheel knows whatever the burst
        // holds; it is rewritten throughout lock so op25 keeps sync too.
        let cw = event.isch.isch.codeword();
        for i in 0..20u64 {
            #[allow(clippy::cast_possible_truncation)]
            self.put(burst.start + i, ((cw >> (38 - 2 * i)) & 3) as u8);
        }
        if !rewritten(event) {
            return;
        }
        self.rewritten += 1;
        let duid = event.burst_type.codeword();
        for (i, &at) in DUID_AT.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            self.put(burst.start + at as u64, ((duid >> (6 - 2 * i)) & 3) as u8);
        }
        let scrambled = event.burst_type.scrambled();
        match &event.content {
            BurstContent::Voice4 { frames } => {
                self.voice(burst, &VOICE_4V, frames);
                if let Some(v) = self.voice4[usize::from(event.channel)].get_mut(burst.slot / 2) {
                    *v = Some(Voice4 {
                        sequence: burst.sequence,
                        start: burst.start,
                        slot: burst.slot,
                    });
                }
            }
            BurstContent::Voice2 { frames, encryption } => {
                self.voice(burst, &VOICE_2V, frames);
                if let Some(es) = encryption {
                    let ess = mac::encode::ess(&es.value);
                    self.put_payload(
                        burst.start,
                        burst.slot,
                        ESS_A.start,
                        &mac::encode::hexbit_bits(&ess[16..]),
                        true,
                    );
                    let at = burst.slot / 2;
                    for i in 0..4 {
                        let Some(v) = self.voice4[usize::from(event.channel)]
                            .get((at + 1 + i) % 5)
                            .copied()
                            .flatten()
                        else {
                            continue;
                        };
                        if burst.sequence.saturating_sub(v.sequence) < 12 {
                            let bits = mac::encode::hexbit_bits(&ess[4 * i..4 * i + 4]);
                            self.put_payload(v.start, v.slot, ESS_B.start, &bits, true);
                        }
                    }
                }
            }
            BurstContent::Mac(Some(pdu)) => {
                let channel = pdu.value.channel;
                let bits = mac::encode::acch(channel, pdu.value.bytes());
                for p in (0..super::layout::PAYLOAD_DIBITS).filter(|&p| channel.carries(p)) {
                    self.put_payload(
                        burst.start,
                        burst.slot,
                        p,
                        &bits[2 * p..2 * p + 2],
                        scrambled,
                    );
                }
                let _ = MacChannel::Facch;
            }
            BurstContent::Mac(None) | BurstContent::Unread => {}
        }
        let _ = BurstType::Voice4;
    }

    fn voice(&mut self, burst: &Burst, starts: &[usize], frames: &[AmbeFrame]) {
        for (&p, f) in starts.iter().zip(frames) {
            if f.trusted() {
                let cw = f.params.encode();
                let bits: Vec<bool> = (0..72).map(|j| cw.bit(j)).collect();
                self.put_payload(burst.start, burst.slot, p, &bits, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framer::ideal;
    use crate::tdma::AmbeParams;
    use crate::testgen::tdma::{Sent, call};
    use crate::testgen::{Channel, Rng, modulate_cqpsk};
    use crate::types::Nac;
    use crate::voice::EncryptionSync;

    fn scrambler() -> Scrambler {
        Scrambler::new(0xbee00, 0x5e4, Nac::new(0x5e1).unwrap())
    }

    const ES: EncryptionSync = EncryptionSync {
        mi: [0; 9],
        algid: 0x80,
        keyid: 0,
    };

    #[test]
    fn a_clean_stream_comes_out_unchanged() {
        let mut rng = Rng::new(31);
        let mut sent = Sent::default();
        let d = call(&mut rng, &scrambler(), &ES, 2, 4, &mut sent);
        let mut r = TdmaRegenerator::new(DemodConfig::phase2(24000.0), Some(scrambler()));
        let (mut out, mut events) = (Vec::new(), Vec::new());
        r.process_symbols(&ideal(&d), &mut out, &mut events);
        let back: Vec<u8> = out.iter().map(|&s| slice(s)).collect();
        assert_eq!(back[..], d[..back.len()]);
        assert!(
            back.len() + 8 * 180 >= d.len(),
            "released {} of {}",
            back.len(),
            d.len()
        );
    }

    #[test]
    fn a_new_call_drops_the_last_calls_held_symbols() {
        // An encrypted call on one channel, then the recorder moves to a
        // clear call elsewhere: nothing of the first may open the second.
        let mut rng = Rng::new(33);
        let mut sent = Sent::default();
        let encrypted = EncryptionSync {
            algid: 0x84,
            keyid: 0xa100,
            ..ES
        };
        let first = call(&mut rng, &scrambler(), &encrypted, 2, 4, &mut sent);
        let second = call(&mut rng, &scrambler(), &ES, 2, 4, &mut sent);
        let mut r = TdmaRegenerator::new(DemodConfig::phase2(24000.0), Some(scrambler()));
        let (mut out, mut events) = (Vec::new(), Vec::new());
        r.process_symbols(&ideal(&first), &mut out, &mut events);
        assert!(out.len() < first.len(), "some of the first call is held");

        r.start_call(DemodConfig::phase2(24000.0), Some(scrambler()));
        out.clear();
        r.flush(&mut out);
        assert_eq!(out, [], "the first call's held symbols are gone");
        r.process_symbols(&ideal(&second), &mut out, &mut events);
        let back: Vec<u8> = out.iter().map(|&s| slice(s)).collect();
        assert_eq!(back[..], second[..back.len()]);
    }

    #[test]
    fn regenerated_noisy_stream_decodes_cleanly_with_op25s_hard_decoders() {
        let mut rng = Rng::new(32);
        let mut sent = Sent::default();
        let mut d = call(&mut rng, &scrambler(), &ES, 6, 12, &mut sent);
        d.extend(vec![0u8; 1500]);
        let iq = Channel {
            // The edge of what it survives (13 dB fails), as 12 dB was with the
            // root-raised-cosine transmitter and receive filter it had before: that
            // pair let in noise from one symbol rate, the channel filter from 12.5 kHz.
            es_n0_db: 14.0,
            freq_offset_hz: 20.0,
            delay: 0.3,
            clock_ppm: 10.0,
            seed: 7,
        }
        .apply(&modulate_cqpsk(&d, 4), 4);
        let cfg = DemodConfig::phase2(24000.0);
        let mut r = TdmaRegenerator::new(cfg, Some(scrambler()));
        let (mut out, mut events) = (Vec::new(), Vec::new());
        for chunk in iq.chunks(960) {
            r.process(chunk, &mut out, &mut events);
        }
        assert!(
            r.take_call_counts().errors > 0,
            "the channel should need correcting"
        );

        // Hard-decode the regenerated stream as op25 would.
        let mut dec = TdmaDecoder::new(DemodConfig::phase2(24000.0), Some(scrambler()));
        let mut ev = Vec::new();
        dec.process_symbols(&out, &mut ev);
        // Every trusted frame comes back as itself with nothing to correct.
        let again: std::collections::HashMap<u64, &TdmaEvent> =
            ev.iter().map(|e| (e.message.start, &e.message)).collect();
        let (mut trusted, mut right, mut untrusted) = (0, 0, 0);
        for e in events.iter().map(|e| &e.message).filter(|e| e.channel == 0) {
            // The last bursts are still held back.
            let Some(back) = again.get(&e.start) else {
                assert!(
                    e.start + HOLD + 2 * BURST_DIBITS as u64 > out.len() as u64,
                    "{}",
                    e.start
                );
                continue;
            };
            for (f, g) in e.frames().iter().zip(back.frames()) {
                if f.trusted() {
                    trusted += 1;
                    assert_eq!((g.params, g.errors), (f.params, 0));
                    // u2 and u3 are unprotected: compare what the code protects.
                    right += usize::from(sent.voice.iter().any(|v| v.0[..2] == f.params.0[..2]));
                } else {
                    untrusted += 1;
                }
            }
        }
        assert!(
            trusted > 10 * untrusted,
            "trusted {trusted} untrusted {untrusted}"
        );
        assert_eq!(right, trusted);
        let _: Option<AmbeParams> = None;
        let s = dec.stats();
        assert_eq!(s.mac_failed, 0, "{s:?}");
    }
}
