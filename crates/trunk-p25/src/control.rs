//! A control channel: IQ in, typed messages out.
//!
//! [`Message::op25_type`] and [`Message::op25_payload`] give the exact type
//! and bytes op25's `p25p1_fdma` sends for the same frame, the form
//! `trunk_recorder_sys::p25_parse` reads.
//!
//! Recovery beyond op25:
//! - every block is Viterbi decoded on soft symbols;
//! - a block whose best path fails its CRC gets a list decode, and the first
//!   runner-up that passes the CRC is taken (see [`Recovery::List`]);
//! - a bad block does not discard the blocks after it.

use num_complex::Complex32;

use crate::crc;
use crate::demod::{DemodConfig, DemodStats, Demodulator};
use crate::framer::{Duid, Frame, Framer, FramerStats, frame_dibits_for};
use crate::symbols::LEVELS;
use crate::trellis::{self, BLOCK_DIBITS, SoftBlock};
use crate::types::{Nac, Recovery, Tsbk};

/// MBT data blocks accepted after a header: our limit, not the spec's,
/// which is 4 blocks with the header (TIA-102.AABB-B §6). Lenient on
/// purpose: the header's CRC already vouches for its blocks-to-follow, and
/// the packet CRC for the blocks, so a longer one is a manufacturer's or a
/// later revision's and costs nothing to frame.
const MAX_MBT_BLOCKS: usize = 8;
/// Bytes in a decoded rate-1/2 trellis block: a PDU header or unconfirmed
/// data block (BAAA-A §6.2, §6.3.1).
const BLOCK_BYTES: usize = trellis::DATA_BYTES;
/// Bytes of a TSBK in its block; the rest is the block's CRC-16 (AABB-B
/// §5.1).
const TSBK_BYTES: usize = 10;
/// The packet CRC closing the last block of a multi-block PDU (BAAA-A
/// §6.3.3).
const PDU_CRC_BYTES: usize = 4;
/// Service access point of trunking control PDUs: $3D "Trunking Control"
/// (BAAC-A §2.9; AABB-B §6.1).
const SAP_TRUNKING_CONTROL: u8 = 61;
/// PDU formats of multi-block trunking (AABB-B §6.1): %10101 unconfirmed
/// (the unconfirmed data header of BAAA-A §6.6) and %10111 alternative.
const FORMAT_MBT_UNCONFIRMED: u8 = 0x15;
const FORMAT_MBT_ALTERNATE: u8 = 0x17;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Tsbk {
        nac: Nac,
        tsbk: Tsbk,
        recovery: Recovery,
    },
    /// Multi-block trunking PDU: the 12-byte header (CRC included, as op25
    /// sends it) and the data blocks without their CRC-32.
    Mbt {
        nac: Nac,
        header: [u8; BLOCK_BYTES],
        data: Vec<u8>,
    },
    /// Terminator; with link control it tells a control channel decoder it
    /// is sitting on a voice channel.
    Terminator { nac: Nac, link_control: bool },
    /// A voice frame on what should be a control channel.
    Voice { nac: Nac, duid: Duid },
    /// Nothing decoded for a second (op25 `M_P25_TIMEOUT`).
    Timeout,
}

impl Message {
    pub const fn nac(&self) -> Option<Nac> {
        match self {
            Self::Tsbk { nac, .. }
            | Self::Mbt { nac, .. }
            | Self::Terminator { nac, .. }
            | Self::Voice { nac, .. } => Some(*nac),
            Self::Timeout => None,
        }
    }

    /// op25's message type: the DUID, or `get_msg_type(PROTOCOL_P25,
    /// M_P25_TIMEOUT)` for a timeout.
    pub const fn op25_type(&self) -> i64 {
        match self {
            Self::Tsbk { .. } => Duid::Tsdu as i64,
            Self::Mbt { .. } => Duid::Pdu as i64,
            Self::Terminator {
                link_control: false,
                ..
            } => Duid::Tdu as i64,
            Self::Terminator {
                link_control: true, ..
            } => Duid::TduLc as i64,
            Self::Voice { duid, .. } => *duid as i64,
            Self::Timeout => 0xffff,
        }
    }

    /// op25's message body: the NAC (big endian), then the data.
    pub fn op25_payload(&self) -> Vec<u8> {
        let Some(nac) = self.nac() else {
            return Vec::new();
        };
        let mut out = nac.to_be_bytes().to_vec();
        match self {
            Self::Tsbk { tsbk, .. } => out.extend_from_slice(tsbk.bytes()),
            Self::Mbt { header, data, .. } => {
                out.extend_from_slice(header);
                out.extend_from_slice(data);
            }
            Self::Terminator { .. } | Self::Voice { .. } | Self::Timeout => {}
        }
        out
    }
}

/// A decoder output and the symbol index it was completed at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received<M = Message> {
    pub symbol: u64,
    pub message: M,
}

#[derive(Clone, Copy, Debug)]
pub struct DecoderConfig {
    pub demod: DemodConfig,
    /// Paths the list decoder may try on a CRC failure (1 disables it).
    pub list_size: usize,
    /// Largest path-metric excess over the best path a runner-up may have
    /// (squared symbol distance; one adjacent-level symbol error costs ~4).
    pub list_max_extra: f32,
    /// Fix the NAC instead of learning it.
    pub nac: Option<Nac>,
    /// Symbols without a message before a timeout (op25: 1 s).
    pub timeout_symbols: u64,
}

/// Defaults of [`DecoderConfig`]. The list-decoder excess admits a runner-up
/// up to three adjacent-level symbol errors worse than the best path.
const DEFAULT_LIST_SIZE: usize = 8;
const DEFAULT_LIST_MAX_EXTRA: f32 = 12.0;
/// One second of Phase 1 symbols.
const DEFAULT_TIMEOUT_SYMBOLS: u64 = 4800;

impl DecoderConfig {
    pub const fn new(demod: DemodConfig) -> Self {
        Self {
            demod,
            list_size: DEFAULT_LIST_SIZE,
            list_max_extra: DEFAULT_LIST_MAX_EXTRA,
            nac: None,
            timeout_symbols: DEFAULT_TIMEOUT_SYMBOLS,
        }
    }
}

/// Cumulative counters (never reset) plus the demodulator's current state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DecoderStats {
    pub demod: DemodStats,
    pub framer: FramerStats,
    pub tsdu: u64,
    pub tsbk_clean: u64,
    pub tsbk_corrected: u64,
    /// TSBKs recovered by the list decoder, by rank (index 0 unused; the last
    /// bucket holds every rank from 8 up).
    pub tsbk_list_rank: [u64; 9],
    pub tsbk_failed: u64,
    pub mbt_ok: u64,
    pub mbt_failed: u64,
    pub other_frames: u64,
    pub timeouts: u64,
    /// Dibits corrected in accepted blocks.
    pub corrected_dibits: u64,
}

impl DecoderStats {
    pub fn tsbk_recovered(&self) -> u64 {
        self.tsbk_list_rank.iter().sum()
    }

    pub fn tsbk_ok(&self) -> u64 {
        self.tsbk_clean + self.tsbk_corrected + self.tsbk_recovered()
    }

    fn count(&mut self, r: Recovery) {
        match r {
            Recovery::Clean => self.tsbk_clean += 1,
            Recovery::Corrected(_) => self.tsbk_corrected += 1,
            Recovery::List { rank, .. } => self.tsbk_list_rank[usize::from(rank).min(8)] += 1,
        }
        self.corrected_dibits += u64::from(r.corrected());
    }
}

pub struct ControlChannelDecoder {
    cfg: DecoderConfig,
    demod: Demodulator,
    framer: Framer,
    soft: Vec<f32>,
    frames: Vec<Frame>,
    since_message: u64,
    symbol: u64,
    stats: DecoderStats,
}

impl ControlChannelDecoder {
    pub fn new(cfg: DecoderConfig) -> Self {
        let mut framer = Framer::new();
        framer.set_nac(cfg.nac);
        Self {
            demod: Demodulator::new(cfg.demod),
            framer,
            soft: Vec::new(),
            frames: Vec::new(),
            since_message: 0,
            symbol: 0,
            stats: DecoderStats::default(),
            cfg,
        }
    }

    pub const fn config(&self) -> &DecoderConfig {
        &self.cfg
    }

    /// Drop timing, phase and NAC state (after a retune). Counters persist.
    pub fn reset(&mut self) {
        self.demod.reset();
        self.framer.reset();
        self.since_message = 0;
    }

    pub fn stats(&self) -> DecoderStats {
        DecoderStats {
            demod: self.demod.stats(),
            framer: self.framer.stats,
            ..self.stats
        }
    }

    pub const fn nac(&self) -> Option<Nac> {
        self.framer.nac()
    }

    /// Demodulate and decode a block of conditioned IQ.
    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<Received>) {
        let mut soft = std::mem::take(&mut self.soft);
        soft.clear();
        self.demod.process(iq, &mut soft);
        self.process_symbols(&soft, out);
        self.soft = soft;
    }

    /// Decode hard dibits (op25's slicer output), for comparisons.
    pub fn process_dibits(&mut self, dibits: &[u8], out: &mut Vec<Received>) {
        let soft: Vec<f32> = dibits.iter().map(|&d| LEVELS[usize::from(d & 3)]).collect();
        self.process_symbols(&soft, out);
    }

    /// Decode soft symbols.
    pub fn process_symbols(&mut self, soft: &[f32], out: &mut Vec<Received>) {
        for &s in soft {
            self.symbol += 1;
            self.framer.push(s, &mut self.frames);
            if let Some((Duid::Pdu, body)) = self.framer.current()
                && let Ok(head) = <&SoftBlock>::try_from(body)
                && let Some(blocks) = self.mbt_blocks(head)
            {
                self.framer.extend_current(frame_dibits_for(1 + blocks));
            }
            // A TSDU of one or two TSBKs (AABB-B §5) ends with its last.
            if let Some((Duid::Tsdu, body)) = self.framer.current()
                && let n = body.len() / BLOCK_DIBITS
                && body.len().is_multiple_of(BLOCK_DIBITS)
                && (1..3).contains(&n)
                && let Ok(last) = <&SoftBlock>::try_from(&body[(n - 1) * BLOCK_DIBITS..])
                && let Some((data, _)) = self.block(last)
                && Tsbk::new(data[..TSBK_BYTES].try_into().unwrap_or([0; TSBK_BYTES])).is_last()
            {
                self.framer.shorten_current(frame_dibits_for(n));
            }
            let before = out.len();
            for frame in std::mem::take(&mut self.frames) {
                self.handle(&frame, out);
            }
            if out.len() > before {
                self.since_message = 0;
            } else {
                self.since_message += 1;
                if self.since_message >= self.cfg.timeout_symbols {
                    self.since_message = 0;
                    self.stats.timeouts += 1;
                    out.push(Received {
                        symbol: self.symbol,
                        message: Message::Timeout,
                    });
                }
            }
        }
    }

    fn emit(&self, out: &mut Vec<Received>, message: Message) {
        out.push(Received {
            symbol: self.symbol,
            message,
        });
    }

    fn handle(&mut self, frame: &Frame, out: &mut Vec<Received>) {
        let nac = frame.nac;
        match frame.duid {
            Duid::Tsdu => self.tsdu(frame, out),
            Duid::Pdu => self.pdu(frame, out),
            Duid::Tdu => self.emit(
                out,
                Message::Terminator {
                    nac,
                    link_control: false,
                },
            ),
            Duid::TduLc => self.emit(
                out,
                Message::Terminator {
                    nac,
                    link_control: true,
                },
            ),
            duid @ (Duid::Ldu1 | Duid::Ldu2) => self.emit(out, Message::Voice { nac, duid }),
            Duid::Hdu => self.stats.other_frames += 1,
        }
    }

    /// A CRC-16 protected block, decoded as hard as the config allows.
    fn block(&self, soft: &SoftBlock) -> Option<([u8; BLOCK_BYTES], Recovery)> {
        let c = trellis::decode_checked(soft, self.cfg.list_size, self.cfg.list_max_extra)?;
        let corrected = c.decoded.corrected;
        let recovery = match (c.rank, corrected) {
            (0, 0) => Recovery::Clean,
            (0, n) => Recovery::Corrected(n),
            (rank, corrected) => Recovery::List {
                rank: u8::try_from(rank).unwrap_or(u8::MAX),
                corrected,
            },
        };
        Some((c.decoded.data, recovery))
    }

    fn tsdu(&mut self, frame: &Frame, out: &mut Vec<Received>) {
        self.stats.tsdu += 1;
        for soft in frame.body.as_chunks::<BLOCK_DIBITS>().0 {
            let Some((data, recovery)) = self.block(soft) else {
                // Unlike op25, keep going: the next block has its own CRC.
                self.stats.tsbk_failed += 1;
                continue;
            };
            self.stats.count(recovery);
            let tsbk = Tsbk::new(data[..TSBK_BYTES].try_into().unwrap_or([0; TSBK_BYTES]));
            self.emit(
                out,
                Message::Tsbk {
                    nac: frame.nac,
                    tsbk,
                    recovery,
                },
            );
            if tsbk.is_last() {
                break;
            }
        }
    }

    fn mbt_blocks(&self, head: &SoftBlock) -> Option<usize> {
        mbt_header_blocks(&self.block(head)?.0)
    }

    fn pdu(&mut self, frame: &Frame, out: &mut Vec<Received>) {
        let mut blocks = frame.body.as_chunks::<BLOCK_DIBITS>().0.iter();
        let Some((header, _)) = blocks.next().and_then(|b| self.block(b)) else {
            self.stats.mbt_failed += 1;
            return;
        };
        let Some(n) = mbt_header_blocks(&header) else {
            self.stats.other_frames += 1;
            return;
        };
        if !carries_osp_opcode(&header) {
            self.stats.other_frames += 1;
            return;
        }
        let data: Vec<u8> = blocks
            .take(n)
            .flat_map(|b| trellis::decode(b).data)
            .collect();
        if data.len() < n * BLOCK_BYTES {
            self.stats.mbt_failed += 1;
            return;
        }
        let len = n * BLOCK_BYTES - PDU_CRC_BYTES;
        let want = u32::from_be_bytes([data[len], data[len + 1], data[len + 2], data[len + 3]]);
        if crc::crc32(&data, len * 8) != want {
            self.stats.mbt_failed += 1;
            return;
        }
        self.stats.mbt_ok += 1;
        self.emit(
            out,
            Message::Mbt {
                nac: frame.nac,
                header,
                data: data[..len].to_vec(),
            },
        );
    }
}

/// Whether an MBT header is the alternate format, whose octet 7 is the OSP
/// opcode (AABB-B §6.1 Fig. 6.1-2): in the unconfirmed format octet 7 is the
/// pad octet count (Fig. 6.1-1), and one read as an OSP is made of unrelated
/// fields. Every MBT OSP of AABC-B is the alternate format.
const fn carries_osp_opcode(h: &[u8; BLOCK_BYTES]) -> bool {
    h[0] & 0x1f == FORMAT_MBT_ALTERNATE
}

/// Data blocks following an MBT header (alternate or unconfirmed trunking
/// format, SAP 61), or `None` for any other PDU; a frame's length either way.
fn mbt_header_blocks(h: &[u8; BLOCK_BYTES]) -> Option<usize> {
    let fmt = h[0] & 0x1f;
    let sap = h[1] & 0x3f;
    let blocks = usize::from(h[6] & 0x7f);
    (sap == SAP_TRUNKING_CONTROL
        && (fmt == FORMAT_MBT_ALTERNATE || fmt == FORMAT_MBT_UNCONFIRMED)
        && (1..=MAX_MBT_BLOCKS).contains(&blocks))
    .then_some(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demod::Modulation;
    use crate::testgen::{self, Channel, Rng};

    const NAC: Nac = Nac::new(0x38d).unwrap();

    #[test]
    fn short_tsdus_end_at_their_last_tsbk_so_the_next_sync_is_expected() {
        assert_eq!(
            [1, 2, 3].map(frame_dibits_for),
            [180, 288, 360],
            "to the next status symbol"
        );
        let mut rng = Rng::new(5);
        let sent = testgen::random_tsbks(&mut rng, 6);
        let mut dibits = testgen::tsdu(NAC, &sent[..1]);
        let second = dibits.len();
        dibits.extend(testgen::tsdu(NAC, &sent[1..3]));
        dibits.extend(testgen::tsdu(NAC, &sent[3..]));
        dibits.extend(vec![0u8; 200]);
        // 8 bit errors in the second sync: past the 6 a sync search takes,
        // inside the 12 allowed where the last frame says one starts.
        for d in &mut dibits[second..second + 4] {
            *d ^= 3;
        }
        let mut dec = ControlChannelDecoder::new(DecoderConfig::new(DemodConfig::new(
            24000.0,
            Modulation::Cqpsk,
        )));
        let mut out = Vec::new();
        dec.process_symbols(&crate::framer::ideal(&dibits), &mut out);
        // All six, the last-block bit aside (each TSDU's last has it set).
        let opcodes = |t: &[u8]| (t[0] & 0x7f, t[1..].to_vec());
        let got: Vec<_> = tsbks(&out).iter().map(|t| opcodes(t.bytes())).collect();
        let want: Vec<_> = sent.iter().map(|t| opcodes(t)).collect();
        assert_eq!(got, want);
        let f = dec.stats().framer;
        assert_eq!(
            (f.frames, f.truncated, f.flywheel_syncs),
            (3, 0, 1),
            "{f:?}"
        );
    }

    #[test]
    fn only_an_alternate_format_mbt_has_an_osp_opcode() {
        let mut h = [0u8; BLOCK_BYTES];
        h[1] = SAP_TRUNKING_CONTROL;
        h[6] = 1;
        for (fmt, osp) in [
            (FORMAT_MBT_ALTERNATE, true),
            (FORMAT_MBT_UNCONFIRMED, false),
        ] {
            h[0] = fmt;
            assert_eq!(mbt_header_blocks(&h), Some(1), "framed either way");
            assert_eq!(carries_osp_opcode(&h), osp);
        }
    }

    fn decoder() -> ControlChannelDecoder {
        ControlChannelDecoder::new(DecoderConfig::new(DemodConfig::new(
            24000.0,
            Modulation::Cqpsk,
        )))
    }

    fn stream(rng: &mut Rng, frames: usize) -> (Vec<u8>, Vec<[u8; 10]>) {
        // A control channel never stops; start mid-stream (random dibits),
        // 125 ms before the first frame, as a receiver that has been
        // listening a while (the receive filter slows the timing loop's
        // first lock: with 62 ms it missed the first frame at -60 ppm).
        #[allow(clippy::cast_possible_truncation)]
        let mut dibits: Vec<u8> = (0..600).map(|_| (rng.next_u64() & 3) as u8).collect();
        let mut sent = Vec::new();
        for _ in 0..frames {
            let t = testgen::random_tsbks(rng, 3);
            dibits.extend(testgen::tsdu(NAC, &t));
            sent.extend(t);
        }
        dibits.extend(vec![0u8; 400]);
        (dibits, sent)
    }

    fn tsbks(msgs: &[Received]) -> Vec<Tsbk> {
        msgs.iter()
            .filter_map(|m| match m.message {
                Message::Tsbk { tsbk, .. } => Some(tsbk),
                _ => None,
            })
            .collect()
    }

    fn expected(sent: &[[u8; 10]]) -> Vec<Tsbk> {
        sent.chunks(3)
            .flat_map(|c| {
                c.iter().enumerate().map(|(i, t)| {
                    let b = testgen::tsbk_block(t, i == 2);
                    Tsbk::new(b[..10].try_into().unwrap())
                })
            })
            .collect()
    }

    #[test]
    fn dibits_decode_to_op25_payloads() {
        let mut rng = Rng::new(31);
        let (dibits, sent) = stream(&mut rng, 10);
        let mut dec = decoder();
        let mut out = Vec::new();
        dec.process_dibits(&dibits, &mut out);
        assert_eq!(tsbks(&out), expected(&sent));
        let first = &out[0].message;
        assert_eq!(first.op25_type(), 7);
        let payload = first.op25_payload();
        assert_eq!(payload.len(), 12);
        assert_eq!(payload[..2], [0x03, 0x8d]);
        assert!(matches!(
            first,
            Message::Tsbk {
                recovery: Recovery::Clean,
                ..
            }
        ));
    }

    fn decode_iq(
        modulation: Modulation,
        es_n0_db: f64,
        list_size: usize,
    ) -> (usize, usize, DecoderStats) {
        let mut rng = Rng::new(32);
        let (dibits, sent) = stream(&mut rng, 60);
        let iq = match modulation {
            Modulation::Cqpsk => testgen::modulate_cqpsk(&dibits, 5),
            Modulation::C4fm => testgen::modulate_c4fm(&dibits, 5),
        };
        let ch = Channel {
            es_n0_db,
            freq_offset_hz: -45.0,
            delay: 1.7,
            clock_ppm: -60.0,
            seed: 8,
        };
        let iq = ch.apply(&iq, 5, crate::symbols::SYMBOL_RATE);
        // The channel's noise has been through the channel filter, as live;
        // the receiver is the live one too.
        let demod = DemodConfig::new(24000.0, modulation);
        let cfg = DecoderConfig {
            list_size,
            ..DecoderConfig::new(demod)
        };
        let mut dec = ControlChannelDecoder::new(cfg);
        let mut out = Vec::new();
        for chunk in iq.chunks(1200) {
            dec.process(chunk, &mut out);
        }
        let want = expected(&sent);
        let got = tsbks(&out);
        let hits = got.iter().filter(|g| want.contains(g)).count();
        (hits, got.len() - hits, dec.stats())
    }

    #[test]
    fn cqpsk_signal_decodes_every_tsbk() {
        let (hits, wrong, stats) = decode_iq(Modulation::Cqpsk, 25.0, 8);
        assert_eq!((hits, wrong), (180, 0), "{stats:?}");
        assert_eq!(stats.tsbk_ok(), 180);
    }

    #[test]
    fn c4fm_signal_decodes_every_tsbk() {
        let (hits, wrong, stats) = decode_iq(Modulation::C4fm, 25.0, 8);
        assert_eq!((hits, wrong), (180, 0), "{stats:?}");
    }

    #[test]
    fn list_decoding_recovers_tsbks_in_noise() {
        let (plain, wrong_plain, _) = decode_iq(Modulation::Cqpsk, 7.0, 1);
        let (listed, wrong_list, stats) = decode_iq(Modulation::Cqpsk, 7.0, 8);
        assert_eq!((wrong_plain, wrong_list), (0, 0));
        assert!(listed > plain, "list {listed} plain {plain} {stats:?}");
        assert_eq!(
            stats.tsbk_recovered(),
            u64::try_from(listed - plain).unwrap()
        );
    }

    #[test]
    fn silence_produces_timeouts() {
        let mut dec = decoder();
        let mut out = Vec::new();
        dec.process_symbols(&vec![0.0; 4800 * 3 + 10], &mut out);
        assert_eq!(
            out.iter().filter(|m| m.message == Message::Timeout).count(),
            3
        );
        assert!(
            out.iter()
                .all(|m| m.message.op25_payload().is_empty() && m.message.op25_type() == 0xffff)
        );
    }

    #[test]
    fn terminator_is_reported() {
        let mut dibits = testgen::tsdu(NAC, &[[1; 10]]);
        dibits.extend(testgen::tsdu(NAC, &[[2; 10]]));
        dibits.extend(testgen::tdu(NAC));
        dibits.extend(vec![0u8; 100]);
        let mut dec = decoder();
        let mut out = Vec::new();
        dec.process_dibits(&dibits, &mut out);
        let last = &out.last().unwrap().message;
        assert_eq!(
            *last,
            Message::Terminator {
                nac: NAC,
                link_control: false
            }
        );
        assert_eq!(
            (last.op25_type(), last.op25_payload()),
            (3, vec![0x03, 0x8d])
        );
    }
}
