//! Calls from decoded voice: what op25's frame assembler does once a frame
//! or burst is decoded.
//!
//! That is which frames reach the vocoder and how, who is talking, when the
//! transmission ends, whether it is encrypted, and the talker aliases sent
//! in band.
//!
//! The vocoder itself stays out: each voice frame comes out as a [`Vocode`],
//! the input op25 would give its vocoder after regeneration, so the same
//! vocoder ([`crate::vocoder`]) makes the same audio. Phase 1 frames are regenerated whole (op25
//! then corrects nothing), so they carry no error counts; phase 2 frames are
//! the 36 dibits op25 would receive ([`crate::tdma::op25_view`]), which its
//! vocoder decodes and mutes on its own running error rate. Which bursts are
//! voice follows op25 too: a burst the regenerator does not rewrite is what
//! op25 reads of its DUID as received.
//!
//! Where op25 reads a field the Rust decoders did not trust, this follows
//! the Rust decoders: an encryption sync or link control that failed its
//! check changes nothing, where op25 may take a miscorrected one.

use crate::control::Received;
use crate::tdma::mac::MacPdu;
use crate::tdma::{BurstContent, BurstType, MacOpcode, Op25View, TdmaEvent};
use crate::voice::regenerate::SpikeDetector;
use crate::voice::signalling::ALGID_CLEAR;
use crate::voice::{CallCounts, LinkControl, Terminator, VoiceEvent};

/// One voice frame for the vocoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vocode {
    /// IMBE: op25's `u[0..8]` (u7 shifted left one bit) and the bits
    /// corrected in c0 (`e0`) and in all (`et`).
    Imbe { u: [u32; 8], e0: u32, et: u32 },
    /// AMBE+2: the frame's 36 descrambled dibits.
    Ambe { dibits: [u8; 36], encrypted: bool },
    /// An encrypted phase 1 frame: silence (op25 does not vocode it).
    Silence,
}

/// Talker alias blocks as op25 collects them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alias {
    /// Motorola (link control or MAC): the header and its `messages` blocks.
    Motorola {
        phase2: bool,
        messages: u8,
        blocks: Vec<(u8, Vec<u8>)>,
    },
    /// Harris: the alias characters, for the unit and group last heard
    /// (`cache_age_ms` before).
    Harris {
        phase2: bool,
        data: Vec<u8>,
        source: i64,
        group: i64,
        cache_age_ms: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallEvent {
    /// 160 samples.
    Frame(Vocode),
    /// The unit talking, as heard in band.
    Source(u32),
    Group(u16),
    /// An encryption report as op25 sends it: phase 1 on every encrypted
    /// frame, phase 2 on every 2V burst (clear or not).
    Encryption {
        encrypted: bool,
        algid: u8,
        keyid: u16,
    },
    /// The transmission ended (phase 1 TDU or TDULC, phase 2 `MAC_HANGTIME`).
    Terminator,
    Alias(Alias),
}

/// Bits corrected, for the vocoder's frame repeat and muting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImbeErrors {
    /// None: what op25 sees after regeneration.
    #[default]
    Regenerated,
    /// Those the decoder corrected.
    Decoded,
}

/// A call's decode errors as op25 counts them for its transmissions.
#[derive(Clone, Debug, Default)]
pub struct CallCounter {
    spikes: SpikeDetector,
    counts: CallCounts,
}

impl CallCounter {
    /// Phase 1: bits corrected in every voice frame, and frames whose count
    /// stands out from the last 20 (op25's spikes).
    pub fn phase1(&mut self, event: &VoiceEvent) {
        if let Some(frames) = event.frames() {
            for f in frames {
                self.counts.errors += u64::from(f.errors);
                self.counts.spikes += u64::from(self.spikes.is_spike(f.errors));
            }
        }
    }

    /// Phase 2: bits corrected and untrusted frames, in bursts that carried
    /// a signal ([`crate::tdma::regenerate::verified`]): noise between
    /// transmissions is not decode error.
    pub fn phase2(&mut self, event: &TdmaEvent) {
        if crate::tdma::regenerate::verified(event) {
            for f in event.frames() {
                self.counts.errors += u64::from(f.errors);
                self.counts.spikes += u64::from(!f.trusted());
            }
        }
    }

    /// The counts since the last take.
    pub fn take(&mut self) -> CallCounts {
        std::mem::take(&mut self.counts)
    }
}

/// op25's alias buffer: ten blocks, shared by every alias format.
#[derive(Clone, Debug, Default)]
struct AliasBuffer([Option<Vec<u8>>; 10]);

impl AliasBuffer {
    fn get(&self, i: usize) -> Option<&Vec<u8>> {
        self.0.get(i)?.as_ref()
    }

    fn set(&mut self, i: usize, v: Vec<u8>) {
        if let Some(b) = self.0.get_mut(i) {
            *b = Some(v);
        }
    }

    fn clear(&mut self, i: usize) {
        if let Some(b) = self.0.get_mut(i) {
            *b = None;
        }
    }

    /// Blocks `0..=messages` that hold something.
    fn blocks(&self, messages: u8) -> Vec<(u8, Vec<u8>)> {
        (0..=messages.min(9))
            .filter_map(|i| Some((i, self.get(usize::from(i))?.clone())))
            .collect()
    }
}

/// The unit and group last heard, for Harris aliases.
#[derive(Clone, Copy, Debug)]
struct Cached {
    source: i64,
    group: i64,
    /// When, in ms of signal.
    at: Option<u64>,
}

impl Default for Cached {
    fn default() -> Self {
        Self {
            source: -1,
            group: -1,
            at: None,
        }
    }
}

impl Cached {
    fn age(&self, now: u64) -> u64 {
        self.at.map_or(0, |t| now.saturating_sub(t))
    }
}

/// Phase 1: op25's `p25p1_fdma`.
#[derive(Clone, Debug, Default)]
pub struct Phase1Assembler {
    pub errors: ImbeErrors,
    /// The encryption sync in force (header or last LDU2).
    algid: u8,
    keyid: u16,
    alias: AliasBuffer,
    cached: Cached,
}

/// Symbols in 10 ms at 4800 and 6000 baud: the clock alias caches age by.
const PHASE1_SYMBOLS_PER_10MS: u64 = 48;
const PHASE2_SYMBOLS_PER_10MS: u64 = 60;

/// op25 tags a unit or group only when it is above zero.
fn heard(source: i64, group: i64, out: &mut Vec<CallEvent>) {
    if let Ok(s) = u32::try_from(source)
        && s > 0
    {
        out.push(CallEvent::Source(s));
    }
    if let Ok(g) = u16::try_from(group)
        && g > 0
    {
        out.push(CallEvent::Group(g));
    }
}

impl Phase1Assembler {
    pub fn new() -> Self {
        Self {
            algid: ALGID_CLEAR,
            ..Self::default()
        }
    }

    const fn encrypted(&self) -> bool {
        self.algid != ALGID_CLEAR
    }

    pub fn push(&mut self, r: &Received<VoiceEvent>, out: &mut Vec<CallEvent>) {
        let now_ms = r.symbol * 10 / PHASE1_SYMBOLS_PER_10MS;
        match &r.message {
            VoiceEvent::Header { header, .. } => {
                if let Some(h) = header {
                    self.algid = h.value.algid;
                    self.keyid = h.value.keyid;
                    self.cached.group = i64::from(h.value.talkgroup);
                    heard(-1, i64::from(h.value.talkgroup), out);
                }
            }
            VoiceEvent::Ldu1 {
                frames,
                link_control,
                ..
            } => {
                if let Some(lc) = link_control {
                    self.link_control(&lc.value, now_ms, out);
                }
                self.voice(frames, out);
            }
            VoiceEvent::Ldu2 {
                frames, encryption, ..
            } => {
                self.voice(frames, out);
                if let Some(es) = encryption {
                    self.algid = es.value.algid;
                    self.keyid = es.value.keyid;
                }
            }
            VoiceEvent::Terminator { terminator, .. } => {
                self.algid = ALGID_CLEAR;
                out.push(CallEvent::Terminator);
                if let Terminator::LinkControl(Some(lc)) = terminator {
                    self.link_control(&lc.value, now_ms, out);
                }
            }
        }
    }

    fn voice(&self, frames: &[crate::voice::ImbeFrame], out: &mut Vec<CallEvent>) {
        for f in frames {
            if self.encrypted() {
                out.push(CallEvent::Frame(Vocode::Silence));
                out.push(CallEvent::Encryption {
                    encrypted: true,
                    algid: self.algid,
                    keyid: self.keyid,
                });
            } else {
                let (e0, et) = match self.errors {
                    ImbeErrors::Regenerated => (0, 0),
                    ImbeErrors::Decoded => (f.errors_c0, f.errors),
                };
                out.push(CallEvent::Frame(Vocode::Imbe {
                    u: f.params.to_op25(),
                    e0,
                    et,
                }));
            }
        }
    }

    /// op25's `process_LCW`.
    fn link_control(&mut self, lc: &LinkControl, now_ms: u64, out: &mut Vec<CallEvent>) {
        let w = lc.0;
        if lc.protected() {
            return;
        }
        if lc.implicit_mfid() || !matches!(w[1], 0x00 | 0x01 | 0x90 | 0xa4) {
            // The channel updates (implicit MFID) are control-channel news.
            return;
        }
        match lc.opcode() {
            0x00 => {
                let group = u16::from_be_bytes([w[4], w[5]]);
                let source = u32::from_be_bytes([0, w[6], w[7], w[8]]);
                self.cached = Cached {
                    source: i64::from(source),
                    group: i64::from(group),
                    at: Some(now_ms),
                };
                heard(self.cached.source, self.cached.group, out);
            }
            0x15 if w[1] == 0x90 => {
                let messages = usize::from(w[4] & 0xf);
                self.alias.set(0, w.to_vec());
                for i in 1..=messages {
                    self.alias.set(i, vec![0; 9]);
                }
            }
            0x17 if w[1] == 0x90 => {
                let (messages, header_sequence) = match self.alias.get(0) {
                    Some(h) if h.len() >= 8 => (h[4] & 0xf, Some(h[7] >> 4)),
                    _ => (0, None),
                };
                let message = w[2] & 0xf;
                if (1..10).contains(&message)
                    && message <= messages
                    && Some(w[3] >> 4) == header_sequence
                {
                    self.alias.set(usize::from(message), w.to_vec());
                    if message == messages {
                        out.push(CallEvent::Alias(Alias::Motorola {
                            phase2: false,
                            messages,
                            blocks: self.alias.blocks(messages),
                        }));
                    }
                }
            }
            op @ 0x32..=0x35 if w[1] == 0xa4 => {
                // Part A at 0 (odd) or 2 (even), part B after it.
                let a = if op >= 0x34 { 2 } else { 0 };
                if op & 1 == 0 {
                    self.alias.set(a, w.to_vec());
                } else if self.alias.get(a).is_some_and(|v| v.len() >= 9) {
                    self.alias.set(a + 1, w.to_vec());
                    let data = [&self.alias.get(a).unwrap_or(&Vec::new())[2..9], &w[2..9]].concat();
                    out.push(CallEvent::Alias(Alias::Harris {
                        phase2: false,
                        data,
                        source: self.cached.source,
                        group: self.cached.group,
                        cache_age_ms: self.cached.age(now_ms),
                    }));
                    self.alias.clear(a);
                    self.alias.clear(a + 1);
                }
            }
            _ => {}
        }
    }
}

/// op25's MAC message lengths by opcode, for those that carry none.
const MAC_MSG_LEN: [u8; 256] = [
    0, 7, 8, 7, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 14, 15, 0, 0, 15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    5, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    9, 7, 9, 0, 9, 8, 9, 0, 10, 10, 9, 0, 10, 0, 0, 0, //
    0, 0, 0, 0, 9, 7, 0, 0, 10, 0, 7, 0, 10, 8, 14, 7, //
    9, 9, 0, 0, 9, 0, 0, 9, 10, 0, 7, 10, 10, 7, 0, 9, //
    9, 29, 9, 9, 9, 9, 10, 13, 9, 9, 9, 11, 9, 9, 0, 0, //
    8, 0, 0, 7, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    16, 0, 0, 11, 13, 11, 11, 11, 10, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    11, 0, 0, 8, 15, 12, 15, 32, 12, 12, 0, 27, 14, 29, 29, 32, //
    0, 0, 0, 0, 0, 0, 9, 0, 14, 29, 11, 27, 14, 0, 40, 11, //
    28, 0, 0, 14, 17, 14, 0, 0, 16, 8, 11, 0, 13, 19, 0, 0, //
    0, 0, 16, 14, 0, 0, 12, 0, 22, 0, 11, 13, 11, 0, 15, 0, //
];

/// Phase 2: op25's `p25p2_tdma`, for one logical channel.
#[derive(Clone, Debug)]
pub struct Phase2Assembler {
    /// The logical channel followed.
    pub channel: u8,
    /// Encryption in force, and the next (from `MAC_PTT` or the ESS),
    /// promoted at each 2V burst.
    algid: u8,
    keyid: u16,
    next_algid: u8,
    next_keyid: u16,
    alias: AliasBuffer,
    cached: Cached,
}

impl Phase2Assembler {
    pub fn new(channel: u8) -> Self {
        Self {
            channel,
            algid: ALGID_CLEAR,
            keyid: 0,
            next_algid: ALGID_CLEAR,
            next_keyid: 0,
            alias: AliasBuffer::default(),
            cached: Cached::default(),
        }
    }

    /// A new call (op25's `call_start`).
    pub const fn start_call(&mut self, channel: u8) {
        self.channel = channel;
        self.reset_ess();
    }

    const fn reset_ess(&mut self) {
        self.algid = ALGID_CLEAR;
        self.keyid = 0;
        self.next_algid = ALGID_CLEAR;
        self.next_keyid = 0;
    }

    const fn encrypted(&self) -> bool {
        self.algid != ALGID_CLEAR
    }

    /// A burst, with what op25 makes of it ([`crate::tdma::op25_view`]):
    /// its voice is vocoded as op25 would receive it. Bursts of the other
    /// logical channel are ignored.
    pub fn push(&mut self, r: &Received<TdmaEvent>, view: &Op25View, out: &mut Vec<CallEvent>) {
        let e = &r.message;
        if e.channel != self.channel {
            return;
        }
        let now_ms = r.symbol * 10 / PHASE2_SYMBOLS_PER_10MS;
        match view.burst_type {
            Some(BurstType::Voice4) => self.voice(&view.voice, out),
            Some(BurstType::Voice2) => {
                self.voice(&view.voice, out);
                if let BurstContent::Voice2 {
                    encryption: Some(es),
                    ..
                } = &e.content
                {
                    self.next_algid = es.value.algid;
                    self.next_keyid = es.value.keyid;
                }
                self.algid = self.next_algid;
                self.keyid = self.next_keyid;
                out.push(CallEvent::Encryption {
                    encrypted: self.encrypted(),
                    algid: self.algid,
                    keyid: self.keyid,
                });
            }
            _ => {
                if let BurstContent::Mac(Some(pdu)) = &e.content {
                    self.mac(&pdu.value, now_ms, out);
                }
            }
        }
    }

    fn voice(&self, voice: &[[u8; 36]], out: &mut Vec<CallEvent>) {
        for &dibits in voice {
            out.push(CallEvent::Frame(Vocode::Ambe {
                dibits,
                encrypted: self.encrypted(),
            }));
        }
    }

    /// op25's `process_mac_pdu`.
    fn mac(&mut self, pdu: &MacPdu, now_ms: u64, out: &mut Vec<CallEvent>) {
        let b = pdu.bytes();
        match pdu.opcode() {
            MacOpcode::Signal => self.messages(&b[..b.len().min(18)], now_ms, out),
            MacOpcode::Ptt => {
                if let Some(ptt) = pdu.ptt() {
                    self.algid = ptt.encryption.algid;
                    self.keyid = ptt.encryption.keyid;
                    self.next_algid = self.algid;
                    self.next_keyid = self.keyid;
                    self.heard(i64::from(ptt.source), i64::from(ptt.group), now_ms, out);
                }
            }
            MacOpcode::EndPtt => {
                if b.len() >= 18 {
                    heard(-1, i64::from(u16::from_be_bytes([b[16], b[17]])), out);
                }
                self.cached = Cached::default();
            }
            MacOpcode::Idle | MacOpcode::Active => self.messages(b, now_ms, out),
            MacOpcode::Hangtime => {
                self.messages(b, now_ms, out);
                out.push(CallEvent::Terminator);
                self.reset_ess();
                self.cached = Cached::default();
            }
            MacOpcode::Reserved(_) => {}
        }
    }

    fn heard(&mut self, source: i64, group: i64, now_ms: u64, out: &mut Vec<CallEvent>) {
        self.cached = Cached {
            source,
            group,
            at: Some(now_ms),
        };
        heard(source, group, out);
    }

    /// op25's `decode_mac_msg`: the PDU's messages after its header byte.
    fn messages(&mut self, b: &[u8], now_ms: u64, out: &mut Vec<CallEvent>) {
        let at = |i: usize| b.get(i).copied().unwrap_or(0);
        let mut p = 1;
        while p < b.len() {
            let op = b[p];
            let b1b2 = op >> 6;
            let mut mfid = 0;
            let len = match op {
                0x00 => b.len() - p,
                0x01 => {
                    let group = u16::from_be_bytes([at(p + 2), at(p + 3)]);
                    let source = u32::from_be_bytes([0, at(p + 4), at(p + 5), at(p + 6)]);
                    self.heard(i64::from(source), i64::from(group), now_ms, out);
                    7
                }
                0x08 => usize::from(at(p + 1) & 0x3f),
                0x11 => (usize::from(at(p + 1) & 3) + 1) * 2 + 2,
                0x12 => (usize::from(at(p + 1) & 3) + 1) * 3 + 2,
                0x21 => {
                    let group = u16::from_be_bytes([at(p + 2), at(p + 3)]);
                    let source = u32::from_be_bytes([0, at(p + 11), at(p + 12), at(p + 13)]);
                    self.heard(i64::from(source), i64::from(group), now_ms, out);
                    14
                }
                _ if b1b2 == 2 => {
                    mfid = at(p + 1);
                    usize::from(at(p + 2) & 0x3f)
                }
                _ => usize::from(MAC_MSG_LEN[usize::from(op)]),
            };
            // op25 reads on past the PDU; here a message ends with it.
            let msg = &b[p..(p + len).min(b.len())];
            match (op, mfid) {
                (0x91, 0x90) if msg.len() >= 6 => {
                    let messages = usize::from(msg[5]);
                    self.alias.set(0, msg.to_vec());
                    for i in 1..=messages.min(9) {
                        self.alias.set(i, vec![0; 17]);
                    }
                }
                (0x95, 0x90) if msg.len() >= 5 => {
                    if let Some(h) = self.alias.get(0).filter(|h| h.len() >= 9) {
                        let (messages, header_sequence) = (h[5], h[8] >> 4);
                        let block = msg[3];
                        if (1..10).contains(&block) && msg[4] >> 4 == header_sequence {
                            self.alias.set(usize::from(block), msg.to_vec());
                            if block == messages {
                                out.push(CallEvent::Alias(Alias::Motorola {
                                    phase2: true,
                                    messages,
                                    blocks: self.alias.blocks(messages),
                                }));
                            }
                        }
                    }
                }
                (0xa8, 0xa4) if !msg.is_empty() => {
                    out.push(CallEvent::Alias(Alias::Harris {
                        phase2: true,
                        data: msg.to_vec(),
                        source: self.cached.source,
                        group: self.cached.group,
                        cache_age_ms: self.cached.age(now_ms),
                    }));
                }
                _ => {}
            }
            p = if len == 0 { b.len() } else { p + len };
        }
    }
}

#[cfg(test)]
mod tests;
