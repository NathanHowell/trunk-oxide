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
use crate::tdma::mac::{END_PTT_GROUP, MacPdu};
use crate::tdma::{AmbeFrame, BurstContent, BurstType, MacOpcode, Op25View, TdmaEvent};
use crate::types::Nac;
use crate::voice::regenerate::SpikeDetector;
use crate::voice::signalling::ALGID_CLEAR;
use crate::voice::{CallCounts, ImbeFrame, LinkControl, Terminator, VoiceEvent};

/// One voice frame for the vocoder: the frame as decoded here, and what
/// op25 would have been given ([`crate::vocoder::Policy`] picks).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Vocode {
    /// IMBE: the decoded frame, and the bits op25 would count corrected in
    /// c0 (`e0`) and in all (`et`) after regeneration.
    Imbe { frame: ImbeFrame, e0: u32, et: u32 },
    /// AMBE+2: the frame's 36 descrambled dibits as op25 receives them,
    /// and the frame decoded here from the same burst, if any (none when
    /// op25 read a voice burst where the decoder here saw something else).
    Ambe {
        dibits: [u8; 36],
        frame: Option<AmbeFrame>,
        encrypted: bool,
    },
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

#[derive(Clone, Debug, PartialEq)]
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
    /// The transmission ended.
    Terminator(Ended),
    Alias(Alias),
}

/// What ended a transmission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// Phase 1 terminator without link control.
    Tdu,
    /// Phase 1 terminator with link control.
    Tdulc,
    /// Phase 2 `MAC_END_PTT` (transmission trunking).
    EndPtt,
    /// Phase 2 `MAC_HANGTIME` (message trunking).
    Hangtime,
}

impl Ended {
    pub const ALL: [Self; 4] = [Self::Tdu, Self::Tdulc, Self::EndPtt, Self::Hangtime];

    /// Its place in [`Self::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// A name for metrics and traces.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tdu => "tdu",
            Self::Tdulc => "tdulc",
            Self::EndPtt => "end_ptt",
            Self::Hangtime => "hangtime",
        }
    }
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
                out.push(CallEvent::Terminator(match terminator {
                    Terminator::Simple => Ended::Tdu,
                    Terminator::LinkControl(_) => Ended::Tdulc,
                }));
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
                out.push(CallEvent::Frame(Vocode::Imbe { frame: *f, e0, et }));
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
    // 0xe4: BBAC-1 Table 8-2 says 14, but its Fig. 8-43 (and length octet) 17.
    28, 0, 0, 14, 17, 14, 0, 0, 16, 8, 11, 0, 13, 19, 0, 0, //
    0, 0, 16, 14, 0, 0, 12, 0, 22, 0, 11, 13, 11, 0, 15, 0, //
];

/// MAC message opcodes (B1 B2 MCO) matched here (BBAC §8.3 Table 8-2).
const NULL_INFORMATION: u8 = 0x00;
const GRP_V_CH_USR_ABBREVIATED: u8 = 0x01;
const GRP_V_CH_USR_EXTENDED: u8 = 0x21;
const INDIRECT_GROUP_PAGING: u8 = 0x11;
const INDIVIDUAL_PAGING: u8 = 0x12;

/// B1 B2: the opcode's top two bits, and the manufacturer partition
/// (BBAC §8.2.3 Table 8-1).
const B1B2_SHIFT: u32 = 6;
const B1B2_MANUFACTURER: u8 = 0b10;

/// A message's length octet holds the length in its low 6 bits (BBAC §8.3).
const LENGTH_MASK: u8 = 0x3f;
/// Every message added since BBAC's first publication carries its length
/// in its second octet, so a receiver can skip one it does not know
/// (BBAC-1 §8.3).
const LENGTH_OCTET: usize = 1;
/// Manufacturer message: MFID in octet 2, length in octet 3 (BBAC-1
/// §8.3.1.37 Fig. 8-63); offsets from the opcode octet.
const MFR_MFID: usize = 1;
const MFR_LENGTH: usize = 2;

/// Paging messages: the ID count less one in the low 2 bits of octet 2
/// (BBAC §8.3.1.38–39).
const PAGING_LEN_MASK: u8 = 0b11;

/// Individual Paging with Priority: 2 octets, then 3 per ID (BBAC
/// §8.3.1.38 Table 8-4).
const fn individual_paging_len(len: u8) -> usize {
    (len as usize + 1) * 3 + 2
}

/// Indirect Group Paging without Priority: 2 octets, then 2 per ID (BBAC
/// §8.3.1.39 Table 8-5).
const fn group_paging_len(len: u8) -> usize {
    (len as usize + 1) * 2 + 2
}

/// Group Voice Channel User, offsets from the opcode octet: group address
/// octets 3–4, source address 5–7 (abbreviated), and in the extended format
/// the Source SUID's source ID, octets 12–14 (BBAC §8.3.1.2 Figs. 8-13,
/// 8-14, 8-11).
const GVCU_GROUP: usize = 2;
const GVCU_SOURCE: usize = 4;
const GVCU_SUID_SOURCE: usize = 11;

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
    /// The system's color code, when known: a `MAC_END_PTT` carrying
    /// another is not this system's.
    color_code: Option<Nac>,
}

impl Phase2Assembler {
    pub fn new(channel: u8) -> Self {
        Self {
            channel,
            color_code: None,
            algid: ALGID_CLEAR,
            keyid: 0,
            next_algid: ALGID_CLEAR,
            next_keyid: 0,
            alias: AliasBuffer::default(),
            cached: Cached::default(),
        }
    }

    /// A new call (op25's `call_start`) on a system with `color_code`.
    pub const fn start_call(&mut self, channel: u8, color_code: Option<Nac>) {
        self.channel = channel;
        self.color_code = color_code;
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
            Some(BurstType::Voice4) => self.voice(&view.voice, e.frames(), out),
            Some(BurstType::Voice2) => {
                self.voice(&view.voice, e.frames(), out);
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

    /// op25's frames (`voice`) with the decoder's own (`frames`), which
    /// pair up by position when both read the burst as the same voice type.
    fn voice(&self, voice: &[[u8; 36]], frames: &[AmbeFrame], out: &mut Vec<CallEvent>) {
        for (i, &dibits) in voice.iter().enumerate() {
            out.push(CallEvent::Frame(Vocode::Ambe {
                dibits,
                frame: (frames.len() == voice.len()).then(|| frames[i]),
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
            // The transmission is over once the color code checks (BBAC
            // §7.3.3); op25 waited for the MAC_HANGTIME that only message
            // trunking sends.
            MacOpcode::EndPtt => {
                if b.len() < END_PTT_GROUP.end || pdu.is_foreign_end_ptt(self.color_code) {
                    return;
                }
                let g = END_PTT_GROUP.start;
                heard(-1, i64::from(u16::from_be_bytes([b[g], b[g + 1]])), out);
                out.push(CallEvent::Terminator(Ended::EndPtt));
                self.reset_ess();
                self.cached = Cached::default();
            }
            MacOpcode::Idle | MacOpcode::Active => self.messages(b, now_ms, out),
            MacOpcode::Hangtime => {
                self.messages(b, now_ms, out);
                out.push(CallEvent::Terminator(Ended::Hangtime));
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
            let b1b2 = op >> B1B2_SHIFT;
            let mut mfid = 0;
            let (grp, src, suid) = (p + GVCU_GROUP, p + GVCU_SOURCE, p + GVCU_SUID_SOURCE);
            let len = match op {
                NULL_INFORMATION => b.len() - p,
                GRP_V_CH_USR_ABBREVIATED => {
                    let group = u16::from_be_bytes([at(grp), at(grp + 1)]);
                    let source = u32::from_be_bytes([0, at(src), at(src + 1), at(src + 2)]);
                    self.heard(i64::from(source), i64::from(group), now_ms, out);
                    usize::from(MAC_MSG_LEN[usize::from(op)])
                }
                INDIRECT_GROUP_PAGING => group_paging_len(at(p + 1) & PAGING_LEN_MASK),
                INDIVIDUAL_PAGING => individual_paging_len(at(p + 1) & PAGING_LEN_MASK),
                GRP_V_CH_USR_EXTENDED => {
                    let group = u16::from_be_bytes([at(grp), at(grp + 1)]);
                    let source = u32::from_be_bytes([0, at(suid), at(suid + 1), at(suid + 2)]);
                    self.heard(i64::from(source), i64::from(group), now_ms, out);
                    usize::from(MAC_MSG_LEN[usize::from(op)])
                }
                _ if b1b2 == B1B2_MANUFACTURER => {
                    mfid = at(p + MFR_MFID);
                    usize::from(at(p + MFR_LENGTH) & LENGTH_MASK)
                }
                // One with no fixed length (unknown here, or newer than
                // this table, as 0x08) says its own.
                _ => match MAC_MSG_LEN[usize::from(op)] {
                    0 => usize::from(at(p + LENGTH_OCTET) & LENGTH_MASK),
                    n => usize::from(n),
                },
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
