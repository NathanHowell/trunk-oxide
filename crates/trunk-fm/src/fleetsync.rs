//! `FleetSync` (Kenwood) data bursts in an analog channel's audio.
//!
//! A burst is 1200-baud FSK, 1200 Hz and 1800 Hz, sent as plain bits (no
//! differential coding) and heard from the recording's own 16 kHz audio:
//!
//! ```text
//! dotting (alternating bits) | sync 0x23EB | block | block ...
//! block = word1 (32 bits) | word2 (32 bits)       MSB first
//! word2 = 16 more message bits | 15 bits of CRC | 1 parity bit
//! ```
//!
//! A block holds six message bytes; a message whose second byte has bit 0 set
//! names its destination fleet in a seventh and so takes a second block.
//! Which tone is a one depends on the radio, so the sync is looked for
//! either way up.
//!
//! The audio is high-passed first: it is the channel's own, with a CTCSS
//! tone in it as loud as the data. The tones are told apart by the energy
//! of the last symbol's worth of audio at each, sampled at [`PHASES`] offsets across the symbol, each with
//! its own sync search; the first to read a block whose CRC holds is the one
//! heard, so no timing recovery loop is needed and a burst can start at any
//! sample. Only `FleetSync` 1 (sync `0x23EB`) is read; `FleetSync` 2 (sync
//! `0x052B`, with error correction) is not, nor the payload of a data
//! message.

use std::fmt;

use num_complex::Complex64;

use crate::biquad::HighPass;
use crate::signalling::{Heard, Signal};
use crate::tap::AudioTap;

const BAUD: f64 = 1_200.0;
/// Samples in a symbol, which is not a whole number.
const SYMBOL: f64 = crate::AUDIO_RATE / BAUD;
/// Samples the tone energies are summed over: a symbol, rounded down.
const WINDOW: usize = 13;
/// Symbol-timing hypotheses, each a decoder of its own.
const PHASES: usize = 8;
/// Samples in which both tones complete a whole number of cycles.
const PERIOD: usize = 80;
/// The last eight dotting bits and the sync word, as the search sees them.
const SYNC: u32 = 0x00AA_23EB;
const SYNC_BITS: u32 = 24;
/// Bits of the sync that may be wrong. Noise makes a sync of this length a
/// few times in a million per bit; the CRC then has to hold as well.
const SYNC_ERRORS: u32 = 1;
/// The sync's first eight bits alternate like the dotting before them, and
/// end `SYNC_BITS - 8` bits before the sync does.
const RUNS: usize = SYNC_BITS as usize - 8 + 1;
const BLOCK_BYTES: usize = 6;
/// A destination fleet is as long a message as is read.
const MAX_BYTES: usize = 2 * BLOCK_BYTES;
/// The running sums are rebuilt this often so rounding cannot accumulate.
const REBUILD: u64 = 1 << 16;
/// The feedback taps of the CRC's shift register.
const POLY: u16 = 0x6815;

/// A fleet and a unit in it, as radios write them: `120-2182`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id {
    pub fleet: u16,
    pub unit: u16,
}

impl Id {
    const FLEET_BASE: u16 = 99;
    const UNIT_BASE: u16 = 999;

    /// Whether the packet names a radio at all: an empty field reads as the
    /// bases the fields are offset by.
    #[must_use]
    pub const fn is_set(self) -> bool {
        self.fleet != Self::FLEET_BASE && self.unit != Self::UNIT_BASE
    }

    /// The id as one number, for keying aliases: the fleet's digits then the
    /// unit's four (a unit is at most 5094), so `120-2182` is `1202182`.
    #[must_use]
    pub const fn key(self) -> u32 {
        self.fleet as u32 * 10_000 + self.unit as u32
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.fleet, self.unit)
    }
}

/// A `FleetSync` message as sent. What `kind` and `code` ask for is not
/// established here, so they are kept as transmitted (less the flag bit each
/// carries).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub kind: u8,
    pub code: u8,
    pub from: Id,
    pub to: Id,
}

impl Message {
    /// The `(kind, code)` pairs a radio sends as a matter of course. Only
    /// the one this was written from is known: a handset's identification at
    /// every key-up, `0x80`/`0xfc`, self-addressed (Atherton PD, 2026-10-21;
    /// seen at the start of each of ten transmissions). The rest of the
    /// table is not published anywhere found, so anything else is flagged
    /// as unrecognised; add a pair here when one proves routine.
    const ROUTINE: &'static [(u8, u8)] = &[(0x80, 0xFC)];

    /// Whether it is one of the messages [`Self::ROUTINE`] knows.
    #[must_use]
    pub const fn is_routine(&self) -> bool {
        let mut i = 0;
        while i < Self::ROUTINE.len() {
            let (kind, code) = Self::ROUTINE[i];
            if kind == self.kind && code == self.code {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Bytes a message with this second byte takes.
    const fn len_for(second: u8) -> usize {
        if second & 1 == 1 {
            MAX_BYTES
        } else {
            BLOCK_BYTES
        }
    }

    /// The message in `bytes`, which holds all [`Self::len_for`] of them.
    fn parse(bytes: &[u8]) -> Self {
        let b = |i: usize| u16::from(bytes[i]);
        let from = Id {
            fleet: b(2) + Id::FLEET_BASE,
            unit: (b(3) << 4) + (b(4) >> 4) + Id::UNIT_BASE,
        };
        let to = Id {
            fleet: if bytes[1] & 1 == 1 {
                b(6) + Id::FLEET_BASE
            } else {
                from.fleet
            },
            unit: ((b(4) & 0x0F) << 8) + b(5) + Id::UNIT_BASE,
        };
        Self {
            kind: bytes[1] & 0xFE,
            code: bytes[0] & 0xFE,
            from,
            to,
        }
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FleetSync {}", self.from)?;
        if self.to != self.from {
            write!(f, " to {}", self.to)?;
        }
        write!(f, " (type 0x{:02x}, code 0x{:02x})", self.kind, self.code)
    }
}

/// Corner of the high-pass ahead of the tone energies, above every CTCSS
/// tone (67 to 254 Hz) and well below the data's 1200 Hz.
const HIGH_PASS_HZ: f64 = 700.0;

/// The CRC's shift register after the first 48 bits of a block.
fn register(bits48: u64) -> u16 {
    let mut reg = 0u16;
    for i in (0..48).rev() {
        if ((bits48 >> i) & 1 == 1) != (reg & 0x8000 != 0) {
            reg ^= POLY;
        }
        reg <<= 1;
    }
    reg ^ 0x0002
}

/// The 48 bits of a block the register is fed.
fn fed(word1: u32, word2: u32) -> u64 {
    u64::from(word1) << 16 | u64::from(word2 >> 16)
}

/// The low 16 bits a block's second word must end in: the register, with
/// its lowest bit replaced by the parity of everything before it.
fn checksum(word1: u32, word2: u32) -> u16 {
    let reg = register(fed(word1, word2));
    let ones = fed(word1, word2).count_ones() + ((word2 >> 1) & 0x7FFF).count_ones();
    reg.wrapping_add((ones & 1) as u16)
}

/// Which stage a timing hypothesis is at.
enum Stage {
    /// Looking for the sync in the bits seen.
    Hunting,
    /// Reading blocks.
    Reading(Reading),
}

struct Reading {
    /// Whether the burst's tones are the other way round.
    invert: bool,
    /// Sample the sync's first bit searched began at.
    start: u64,
    word: u32,
    bits: u32,
    word1: Option<u32>,
    bytes: [u8; MAX_BYTES],
    len: usize,
}

enum Step {
    Nothing,
    /// A block held but the message is not whole: the others stand down.
    Locked,
    Heard(Heard),
    /// What was being read failed its CRC.
    Lost,
}

/// One timing hypothesis.
struct Phase {
    /// Samples until it next reads a bit.
    until: f64,
    history: u64,
    /// Length of the run of alternating bits ending at each of the last
    /// [`RUNS`] bits, newest last.
    runs: [u32; RUNS],
    stage: Stage,
}

impl Phase {
    const fn new(until: f64) -> Self {
        Self {
            until,
            history: 0,
            runs: [0; RUNS],
            stage: Stage::Hunting,
        }
    }

    /// Back to hunting, forgetting the bits it has seen.
    const fn park(&mut self) {
        self.history = 0;
        self.runs = [0; RUNS];
        self.stage = Stage::Hunting;
    }

    /// Reads the bit, `now` samples into the audio.
    fn push(&mut self, high: bool, now: u64) -> Step {
        match &mut self.stage {
            Stage::Hunting => {
                let run = if (self.history & 1 == 1) != high {
                    self.runs[RUNS - 1] + 1
                } else {
                    1
                };
                self.runs.rotate_left(1);
                self.runs[RUNS - 1] = run;
                self.history = self.history << 1 | u64::from(high);
                for invert in [false, true] {
                    let bits = self.history ^ if invert { u64::MAX } else { 0 };
                    let seen = (bits & ((1 << SYNC_BITS) - 1)) as u32;
                    if (seen ^ SYNC).count_ones() <= SYNC_ERRORS {
                        self.stage = Stage::Reading(Reading {
                            invert,
                            start: now
                                .saturating_sub((u64::from(SYNC_BITS - 8 + self.runs[0])) * 40 / 3),
                            word: 0,
                            bits: 0,
                            word1: None,
                            bytes: [0; MAX_BYTES],
                            len: 0,
                        });
                        break;
                    }
                }
                Step::Nothing
            }
            Stage::Reading(r) => {
                r.word = r.word << 1 | u32::from(high != r.invert);
                r.bits += 1;
                if r.bits < 32 {
                    return Step::Nothing;
                }
                let word = std::mem::take(&mut r.word);
                r.bits = 0;
                let Some(word1) = r.word1.take() else {
                    r.word1 = Some(word);
                    return Step::Nothing;
                };
                if checksum(word1, word) != (word & 0xFFFF) as u16 {
                    self.park();
                    return Step::Lost;
                }
                r.bytes[r.len..r.len + 4].copy_from_slice(&word1.to_be_bytes());
                r.bytes[r.len + 4..r.len + BLOCK_BYTES]
                    .copy_from_slice(&((word >> 16) as u16).to_be_bytes());
                r.len += BLOCK_BYTES;
                if r.len < Message::len_for(r.bytes[1]) {
                    return Step::Locked;
                }
                let heard = Heard {
                    span: r.start..now,
                    signal: Signal::FleetSync(Message::parse(&r.bytes[..r.len])),
                };
                self.park();
                Step::Heard(heard)
            }
        }
    }
}

/// Reads `FleetSync` bursts out of 16 kHz audio, whatever the block sizes it
/// is fed in.
pub struct FleetSync {
    /// Samples fed since the last reset.
    n: u64,
    /// A fourth-order Butterworth high-pass.
    high_pass: HighPass,
    /// `exp(-2πjft)` at 1200 and 1800 Hz over a [`PERIOD`].
    tones: Box<[[Complex64; PERIOD]; 2]>,
    /// The last [`WINDOW`] samples mixed down by each tone.
    mixed: [[Complex64; WINDOW]; 2],
    sums: [Complex64; 2],
    phases: [Phase; PHASES],
    /// The hypothesis reading a message longer than a block, the others
    /// having stood down for it.
    reader: Option<usize>,
}

impl Default for FleetSync {
    fn default() -> Self {
        Self::new()
    }
}

impl FleetSync {
    #[must_use]
    pub fn new() -> Self {
        let tone = |hz: f64| {
            std::array::from_fn(|k| {
                Complex64::from_polar(
                    1.0,
                    -std::f64::consts::TAU * hz * k as f64 / crate::AUDIO_RATE,
                )
            })
        };
        Self {
            n: 0,
            high_pass: HighPass::new(HIGH_PASS_HZ),
            tones: Box::new([tone(1_200.0), tone(1_800.0)]),
            mixed: [[Complex64::default(); WINDOW]; 2],
            sums: [Complex64::default(); 2],
            phases: std::array::from_fn(|p| Phase::new(Self::first_read(p))),
            reader: None,
        }
    }

    fn first_read(phase: usize) -> f64 {
        1.0 + phase as f64 * SYMBOL / PHASES as f64
    }

    /// Forget the audio so far, as for a retuned channel. Spans count from here.
    pub fn reset(&mut self) {
        self.n = 0;
        self.high_pass.reset();
        self.mixed = [[Complex64::default(); WINDOW]; 2];
        self.sums = [Complex64::default(); 2];
        for (p, phase) in self.phases.iter_mut().enumerate() {
            *phase = Phase::new(Self::first_read(p));
        }
        self.reader = None;
    }

    /// Feeds `audio`, appending each message that is whole by its end.
    pub fn process(&mut self, audio: &[f32], out: &mut Vec<Heard>) {
        for &x in audio {
            let x = self.high_pass.run(f64::from(x));
            let slot = (self.n % WINDOW as u64) as usize;
            let k = (self.n % PERIOD as u64) as usize;
            for (tone, (mixed, sum)) in self.mixed.iter_mut().zip(&mut self.sums).enumerate() {
                let m = self.tones[tone][k] * x;
                *sum += m - mixed[slot];
                mixed[slot] = m;
            }
            self.n += 1;
            if self.n.is_multiple_of(REBUILD) {
                for (mixed, sum) in self.mixed.iter().zip(&mut self.sums) {
                    *sum = mixed.iter().sum();
                }
            }
            let high = self.sums[1].norm_sqr() > self.sums[0].norm_sqr();
            self.read(high, out);
        }
    }

    fn read(&mut self, high: bool, out: &mut Vec<Heard>) {
        for p in 0..PHASES {
            let phase = &mut self.phases[p];
            phase.until -= 1.0;
            if phase.until > 0.0 {
                continue;
            }
            phase.until += SYMBOL;
            if self.reader.is_some_and(|r| r != p) {
                phase.park();
                continue;
            }
            match phase.push(high, self.n) {
                Step::Nothing => {}
                Step::Locked => {
                    self.reader = Some(p);
                    for (q, other) in self.phases.iter_mut().enumerate() {
                        if q != p {
                            other.park();
                        }
                    }
                }
                Step::Heard(heard) => {
                    out.push(heard);
                    self.reader = None;
                    for other in &mut self.phases {
                        other.park();
                    }
                }
                Step::Lost => self.reader = None,
            }
        }
    }
}

impl AudioTap for FleetSync {
    fn process(&mut self, audio: &[f32], out: &mut Vec<Heard>) {
        Self::process(self, audio, out);
    }

    fn reset(&mut self) {
        Self::reset(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First burst of the Atherton PD call of 2026-10-21 16:40:42 UTC, with
    /// the noise 0.16 s before it and 0.1 s after (Opus, 16 kHz).
    fn atherton() -> Vec<f32> {
        include_bytes!("../tests/data/fleetsync_atherton_burst.s16")
            .chunks_exact(2)
            .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
            .collect()
    }

    const ATHERTON_W1: u32 = 0xFC80_1549;
    const ATHERTON_W2: u32 = 0xF49F_F19B;

    fn atherton_message() -> Message {
        Message {
            kind: 0x80,
            code: 0xFC,
            from: Id {
                fleet: 120,
                unit: 2182,
            },
            to: Id {
                fleet: 120,
                unit: 2182,
            },
        }
    }

    fn run(decoder: &mut FleetSync, audio: &[f32], chunk: usize) -> Vec<Heard> {
        let mut out = Vec::new();
        for block in audio.chunks(chunk) {
            decoder.process(block, &mut out);
        }
        out
    }

    fn messages(heard: &[Heard]) -> Vec<Message> {
        heard
            .iter()
            .map(|h| match h.signal {
                Signal::FleetSync(m) => m,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    /// The word2 a sender would put on a block of `word1` and 16 more bits.
    fn seal(word1: u32, hi: u16) -> u32 {
        let reg = register(fed(word1, u32::from(hi) << 16));
        let ones = fed(word1, u32::from(hi) << 16).count_ones() + (reg >> 1).count_ones();
        u32::from(hi) << 16 | u32::from(reg | (ones & 1) as u16)
    }

    /// A packet's blocks as bits, `true` being the tone that is a one.
    fn packet(blocks: &[(u32, u32)]) -> Vec<bool> {
        let words = std::iter::once(0xAAAA_23EB)
            .chain(blocks.iter().flat_map(|&(a, b)| <[u32; 2]>::from((a, b))));
        words
            .flat_map(|w| (0..32).rev().map(move |i| (w >> i) & 1 == 1))
            .collect()
    }

    /// AFSK of `bits` with continuous phase, between `lead` and `trail`
    /// samples of a low hiss. `ones_high`: a one is 1800 Hz.
    #[allow(clippy::cast_sign_loss)]
    fn modulate(bits: &[bool], ones_high: bool, lead: usize, trail: usize) -> Vec<f32> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut hiss = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.02
        };
        let mut audio: Vec<f32> = (0..lead).map(|_| hiss()).collect();
        let mut phase = 0.0f64;
        let samples = (bits.len() as f64 * SYMBOL) as usize;
        for i in 0..samples {
            let high = bits[(i as f64 / SYMBOL) as usize] == ones_high;
            let hz = if high { 1_800.0 } else { 1_200.0 };
            phase += std::f64::consts::TAU * hz / crate::AUDIO_RATE;
            audio.push(0.4 * phase.sin() as f32 + hiss());
        }
        audio.extend((0..trail).map(|_| hiss()));
        audio
    }

    #[test]
    fn only_the_identification_this_was_written_from_is_routine() {
        let mut message = atherton_message();
        assert!(message.is_routine());
        message.code = 0x00;
        assert!(!message.is_routine());
        message.code = 0xFC;
        message.kind = 0x42;
        assert!(!message.is_routine());
    }

    #[test]
    fn the_checksum_is_the_real_packets() {
        assert_eq!(checksum(ATHERTON_W1, ATHERTON_W2), 0xF19B);
        assert_eq!(seal(ATHERTON_W1, 0xF49F), ATHERTON_W2);
    }

    #[test]
    fn an_id_reads_as_radios_write_it_and_keys_an_alias() {
        let id = Id {
            fleet: 120,
            unit: 2182,
        };
        assert_eq!(id.to_string(), "120-2182");
        assert_eq!(id.key(), 1_202_182);
        assert!(id.is_set());
        assert!(
            !Id {
                fleet: 99,
                unit: 1000
            }
            .is_set()
        );
    }

    #[test]
    fn the_real_burst_from_atherton_pd_is_heard_once() {
        let heard = run(&mut FleetSync::new(), &atherton(), 6400);
        assert_eq!(messages(&heard), [atherton_message()]);
        // the burst's envelope rises at sample 2577 and falls by 4188, the
        // last stretch of that being the codec's and the filters' tail
        let span = &heard[0].span;
        assert!(span.start.abs_diff(2_577) <= 24, "{span:?}");
        assert!((4_000..4_200).contains(&span.end), "{span:?}");
    }

    #[test]
    fn the_span_covers_the_whole_burst_whatever_its_dotting() {
        for dots in [0usize, 8, 24, 40] {
            // `dots` more alternating bits ahead of the packet's own, ending
            // on the zero its first one follows
            let mut bits: Vec<bool> = (0..dots).map(|i| (dots - i) % 2 == 0).collect();
            bits.extend(packet(&[(ATHERTON_W1, ATHERTON_W2)]));
            let lead = 1_000;
            let audio = modulate(&bits, true, lead, 800);
            let heard = run(&mut FleetSync::new(), &audio, 160);
            let [one] = &heard[..] else {
                panic!("dots {dots}: {heard:?}")
            };
            let end = lead + (bits.len() as f64 * SYMBOL) as usize;
            assert!(
                one.span.start.abs_diff(lead as u64) <= 20,
                "dots {dots}: {:?} from {lead}",
                one.span
            );
            assert!(
                one.span.end.abs_diff(end as u64) <= 20,
                "dots {dots}: {:?} to {end}",
                one.span
            );
        }
    }

    #[test]
    fn the_blocks_it_is_fed_in_make_no_difference() {
        let audio = atherton();
        let whole = run(&mut FleetSync::new(), &audio, audio.len());
        for chunk in [1, 7, 13, 160, 1_600] {
            assert_eq!(run(&mut FleetSync::new(), &audio, chunk), whole, "{chunk}");
        }
    }

    #[test]
    fn a_reset_mid_burst_forgets_it() {
        let audio = atherton();
        let mut decoder = FleetSync::new();
        let mut out = Vec::new();
        decoder.process(&audio[..3_400], &mut out);
        decoder.reset();
        decoder.process(&audio[3_400..], &mut out);
        assert_eq!(out, []);
        assert_eq!(
            messages(&run(&mut decoder, &atherton(), 800)),
            [atherton_message()]
        );
    }

    #[test]
    fn a_burst_is_heard_with_the_tones_either_way_round_at_any_offset() {
        let blocks = [(ATHERTON_W1, ATHERTON_W2)];
        for ones_high in [true, false] {
            for lead in [1_000, 1_003, 1_005, 1_008, 1_011, 1_013] {
                let audio = modulate(&packet(&blocks), ones_high, lead, 600);
                let heard = run(&mut FleetSync::new(), &audio, 160);
                assert_eq!(
                    messages(&heard),
                    [atherton_message()],
                    "ones_high={ones_high} lead={lead}"
                );
            }
        }
    }

    #[test]
    fn a_ctcss_tone_as_loud_as_the_burst_does_not_hide_it() {
        let mut audio = modulate(&packet(&[(ATHERTON_W1, ATHERTON_W2)]), true, 800, 800);
        for (i, v) in audio.iter_mut().enumerate() {
            *v += 0.4 * (std::f32::consts::TAU * 162.2 * i as f32 / 16_000.0).sin();
        }
        assert_eq!(
            messages(&run(&mut FleetSync::new(), &audio, 160)),
            [atherton_message()]
        );
    }

    #[test]
    fn two_bursts_in_a_row_are_two_messages() {
        let blocks = [(ATHERTON_W1, ATHERTON_W2)];
        let one = modulate(&packet(&blocks), true, 800, 800);
        let audio: Vec<f32> = one.iter().chain(&one).copied().collect();
        let heard = run(&mut FleetSync::new(), &audio, 1_600);
        assert_eq!(messages(&heard).len(), 2);
        assert!(heard[0].span.end < heard[1].span.start);
    }

    #[test]
    fn a_message_naming_a_destination_fleet_takes_two_blocks() {
        // flag bit set in the second byte: byte 6 is the destination fleet
        let word1 = 0x0481_7B49;
        let first = (word1, seal(word1, 0x249F));
        let second_word1 = 0x1F00_0000;
        let second = (second_word1, seal(second_word1, 0));
        let audio = modulate(&packet(&[first, second]), true, 800, 800);
        let heard = run(&mut FleetSync::new(), &audio, 400);
        let [message] = messages(&heard)[..] else {
            panic!("{heard:?}");
        };
        assert_eq!(message.to.fleet, 0x1F + 99);
        assert_eq!(message.from.fleet, 0x7B + 99);
        assert_eq!(message.kind, 0x80);
    }

    #[test]
    fn a_message_missing_its_second_block_is_not_heard() {
        let word1 = 0x0481_7B49;
        let first = (word1, seal(word1, 0x249F));
        let audio = modulate(&packet(&[first]), true, 800, 4_000);
        assert_eq!(run(&mut FleetSync::new(), &audio, 400), []);
    }

    #[test]
    fn a_flipped_bit_fails_the_crc_and_nothing_is_heard() {
        let mut bits = packet(&[(ATHERTON_W1, ATHERTON_W2)]);
        bits[32 + 20] = !bits[32 + 20];
        let audio = modulate(&bits, true, 800, 800);
        assert_eq!(run(&mut FleetSync::new(), &audio, 400), []);
    }

    #[test]
    fn hiss_and_tones_that_are_not_a_burst_are_not_heard() {
        let mut state = 99u64;
        let mut rnd = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        };
        let hiss: Vec<f32> = (0..16_000 * 30).map(|_| rnd()).collect();
        assert_eq!(run(&mut FleetSync::new(), &hiss, 1_600), []);
        // a steady 1200 Hz and a 1200/1800 Hz alternation are not data
        let alternating = modulate(&[true, false].repeat(400), true, 0, 0);
        assert_eq!(run(&mut FleetSync::new(), &alternating, 1_600), []);
    }
}
