//! P25 control channels ([`P25Control`]) hosted on shared-channelizer
//! slots, and the P25 decoders' counters.
//!
//! The decoder runs on the channelizer's DSP thread, on the slot's
//! conditioned output (see [`trunk_channelizer::ChannelDecoder`]), parses
//! what it decodes into trunking messages and appends them to the block's
//! [`Decoded`] output for the source thread to send on. Counters are
//! readable from any thread for metrics.

use std::fmt;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{LineWriter, Write as _};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use num_complex::Complex32;
use trunk_channelizer::{ChannelDecoder, NARROW, Reading};
use trunk_p25::voice::{Encryption, EncryptionTracker};
use trunk_p25::{
    ControlChannelDecoder, DecoderConfig, DemodConfig, Message, Modulation, Nac, Received,
};

pub use trunk_p25::DecoderStats;
pub use trunk_p25::tdma::TdmaStats;
pub use trunk_p25::voice::VoiceStats;

use crate::kind::{Cmd, Kind, OpenError, Opening, Tuning};
use crate::p25_parse::P25TrunkParser;
use crate::p25_voice::{NARROW_RATE, p25_conditioning};
use crate::recording::Tape;
use crate::registry::{Entry, Registry};
use crate::types::{ChannelNum, Decoded, RadioEvent};

/// A P25 control channel: decoded, parsed into trunking messages.
#[derive(Debug)]
pub enum P25Control {}

impl Kind for P25Control {
    type Params = ();
    type Decoder = ControlSlot;
    const NAME: &'static str = "p25-control";
    const PROFILE: trunk_channelizer::ProfileId = NARROW;
    const AUDIO_RATE: Option<f64> = None;

    fn tuning(at: &Opening<'_, Self>) -> Tuning {
        Tuning {
            conditioning: p25_conditioning(at.channelizer, false, false),
            filter: None,
            squelch_db: None,
        }
    }

    fn decoder(at: &Opening<'_, Self>, _: Tape) -> Result<ControlSlot, OpenError> {
        let freq_hz = at.call.channel.freq_hz;
        let modulation = if at.system.qpsk {
            Modulation::Cqpsk
        } else {
            Modulation::C4fm
        };
        #[allow(clippy::cast_possible_truncation)]
        let tap_base = at.tap_dir.map_or_else(String::new, |d| {
            format!("{d}/cc_{}_{}", at.system.short_name, freq_hz as i64)
        });
        Ok(ControlSlot::new(
            ControlChannel {
                system: at.system.short_name.clone(),
                freq_hz,
                source: at.source,
                port: u32::try_from(at.port).unwrap_or(u32::MAX),
            },
            modulation,
            NARROW_RATE,
            at.system.sys_num,
            &tap_base,
        ))
    }
}

/// Which control channel a decoder serves.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlChannel {
    pub system: String,
    pub freq_hz: f64,
    pub source: i32,
    pub port: u32,
}

/// A decoder's counters at one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlChannelReport {
    pub channel: ControlChannel,
    pub modulation: Modulation,
    pub nac: Option<Nac>,
    pub stats: DecoderStats,
    /// Messages dropped before they were parsed (none since the decoder
    /// parses in place; kept for the metric).
    pub dropped: u64,
}

/// A decoder's counters, published every block for [`control_channels`].
pub(crate) struct Shared {
    channel: ControlChannel,
    modulation: Modulation,
    snapshot: Mutex<(DecoderStats, Option<Nac>)>,
}

impl Shared {
    fn report(&self) -> ControlChannelReport {
        let (stats, nac) = *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        ControlChannelReport {
            channel: self.channel.clone(),
            modulation: self.modulation,
            nac,
            stats,
            dropped: 0,
        }
    }
}

static REGISTRY: Registry<Shared> = Registry::new();

/// Counters of every control-channel decoder (see [`crate::registry`]).
pub fn control_channels() -> Vec<Entry<ControlChannelReport>> {
    REGISTRY.read(Shared::report)
}

/// A control channel on a slot: the decoder, its parser, and the raw
/// message tap when baselines are being taken.
pub struct ControlSlot {
    decoder: ControlChannelDecoder,
    parser: P25TrunkParser,
    raw: Option<RawLog>,
    shared: Arc<Shared>,
    scratch: Vec<Received>,
}

impl fmt::Debug for ControlSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlSlot")
            .field("channel", &self.shared.channel)
            .finish_non_exhaustive()
    }
}

impl ControlSlot {
    /// A decoder for `channel` at the slot's conditioned `rate`, parsing
    /// for system `sys_num`; with a `tap_base`, every message is also
    /// written to `<tap_base>.msgs.jsonl`.
    pub(crate) fn new(
        channel: ControlChannel,
        modulation: Modulation,
        rate: f64,
        sys_num: i32,
        tap_base: &str,
    ) -> Self {
        let decoder =
            ControlChannelDecoder::new(DecoderConfig::new(DemodConfig::new(rate, modulation)));
        let parser = P25TrunkParser::new(sys_num, &channel.system);
        let raw = (!tap_base.is_empty())
            .then(|| RawLog::open(&format!("{tap_base}.msgs.jsonl")))
            .flatten();
        let shared = REGISTRY.register(Shared {
            channel,
            modulation,
            snapshot: Mutex::new((decoder.stats(), None)),
        });
        Self {
            decoder,
            parser,
            raw,
            shared,
            scratch: Vec::new(),
        }
    }
}

impl ChannelDecoder for ControlSlot {
    type Output = Decoded;
    type Command = Cmd;

    fn process(&mut self, iq: &[Complex32], _: &Reading, out: &mut Decoded) {
        self.decoder.process(iq, &mut self.scratch);
        for r in self.scratch.drain(..) {
            if let Some(raw) = &mut self.raw {
                raw.write(&r.message);
            }
            let parsed = self.parser.parse_message(&r.message);
            if !parsed.is_empty() {
                out.events.push(RadioEvent::TrunkMessages(parsed));
            }
        }
        *self
            .shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = (self.decoder.stats(), self.decoder.nac());
    }

    fn retune(&mut self) {
        self.decoder.reset();
    }
}

/// Every message a decoder produced, in op25's form, one JSON object a line:
/// `seq`, `ms` since the decoder started, `type`, `arg1`, `arg2` (a
/// timeout's wall clock time, s) and the `hex` payload. The golden decoder
/// scorers read it.
struct RawLog {
    out: LineWriter<File>,
    t0: Instant,
    seq: u64,
}

impl RawLog {
    fn open(path: &str) -> Option<Self> {
        match File::create(path) {
            Ok(f) => Some(Self {
                out: LineWriter::new(f),
                t0: Instant::now(),
                seq: 0,
            }),
            Err(e) => {
                tracing::warn!(path, error = %e, "cannot open the control channel message tap");
                None
            }
        }
    }

    fn write(&mut self, m: &Message) {
        let hex = m.op25_payload().iter().fold(String::new(), |mut h, b| {
            let _ = write!(h, "{b:02x}");
            h
        });
        let arg2 = if matches!(m, Message::Timeout) {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64())
        } else {
            0.0
        };
        let ms = self.t0.elapsed().as_millis();
        let line = format!(
            r#"{{"seq":{},"ms":{ms},"type":{},"arg1":0,"arg2":{arg2},"hex":"{hex}"}}"#,
            self.seq,
            m.op25_type()
        );
        self.seq += 1;
        let _ = writeln!(self.out, "{line}");
    }
}

// ── Voice: a decoder per channel, phase 1 or phase 2 ──

/// A voice channel's decoder counters at one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceChannelReport {
    /// `None` for a decoder outside the radio (tests, tools).
    pub channel: Option<ChannelNum>,
    /// The system of the call in progress (or last call).
    pub system: String,
    /// Phase 1 calls.
    pub stats: VoiceStats,
    /// Phase 2 (TDMA) calls.
    pub tdma: TdmaStats,
}

#[derive(Clone, Default)]
struct Snapshot {
    phase1: VoiceStats,
    phase2: TdmaStats,
    system: String,
}

pub(crate) struct VoiceShared {
    channel: Option<ChannelNum>,
    snapshot: Mutex<Snapshot>,
}

impl VoiceShared {
    /// A voice decoder, registered for [`voice_channels`].
    pub(crate) fn register(channel: Option<ChannelNum>) -> Arc<Self> {
        VOICE_REGISTRY.register(Self {
            channel,
            snapshot: Mutex::new(Snapshot::default()),
        })
    }

    fn snapshot(&self) -> std::sync::MutexGuard<'_, Snapshot> {
        self.snapshot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn phase1(&self, stats: VoiceStats) {
        self.snapshot().phase1 = stats;
    }

    pub(crate) fn phase2(&self, stats: TdmaStats) {
        self.snapshot().phase2 = stats;
    }

    /// The call in progress: its system.
    pub(crate) fn call(&self, system: &str) {
        system.clone_into(&mut self.snapshot().system);
    }
}

/// One superframe's ESS on the call's own slot (`None`: it did not decode).
pub(crate) fn track_encryption(
    tracker: &mut EncryptionTracker,
    pending: &mut Option<Encryption>,
    es: Option<&trunk_p25::voice::EncryptionSync>,
) {
    match es {
        Some(es) => {
            if let Some(e) = tracker.observe(es) {
                *pending = Some(e);
            }
        }
        None => tracker.missed(),
    }
}

/// `1 << 24 | algid << 16 | keyid`, or 0: how C++ takes encryption.
pub(crate) const fn packed_encryption(e: Option<Encryption>) -> u32 {
    match e {
        Some(e) => 1 << 24 | (e.algid as u32) << 16 | e.keyid as u32,
        None => 0,
    }
}

static VOICE_REGISTRY: Registry<VoiceShared> = Registry::new();

/// Counters of every voice decoder (see [`crate::registry`]).
pub fn voice_channels() -> Vec<Entry<VoiceChannelReport>> {
    VOICE_REGISTRY.read(|s| {
        let snap = s.snapshot().clone();
        VoiceChannelReport {
            channel: s.channel,
            system: snap.system,
            stats: snap.phase1,
            tdma: snap.phase2,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunk_p25::testgen;

    fn channel() -> ControlChannel {
        ControlChannel {
            system: "test".into(),
            freq_hz: 851e6,
            source: 0,
            port: 3,
        }
    }

    fn iq(seed: u64) -> Vec<Complex32> {
        let nac = Nac::new(0x38d).unwrap();
        let mut rng = testgen::Rng::new(seed);
        let mut dibits = Vec::new();
        for _ in 0..20 {
            dibits.extend(testgen::tsdu(nac, &testgen::random_tsbks(&mut rng, 3)));
        }
        testgen::modulate_cqpsk(&dibits, 5)
    }

    fn messages(out: &Decoded) -> Vec<&crate::types::TrunkMessage> {
        out.events
            .iter()
            .filter_map(|e| match e {
                RadioEvent::TrunkMessages(m) => Some(m),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn decodes_parses_into_the_output_and_reports() {
        let mut slot = ControlSlot::new(channel(), Modulation::Cqpsk, 24_000.0, 77, "");
        let mut out = Decoded::default();
        for chunk in iq(1).chunks(600) {
            slot.process(chunk, &Reading::default(), &mut out);
        }
        let sent = messages(&out);
        assert!(sent.len() >= 57, "{}", sent.len());
        assert!(sent.iter().all(|m| m.sys_num == 77 && m.nac == 0x38d));
        assert_eq!(out.audio, Vec::new());
        let mine = || {
            control_channels()
                .into_iter()
                .filter(|e| e.value.channel == channel())
                .collect::<Vec<_>>()
        };
        let report = mine().pop().unwrap();
        assert!(report.live);
        assert_eq!(report.value.nac, Some(Nac::new(0x38d).unwrap()));
        assert!(report.value.stats.tsbk_ok() >= 57);
        assert_eq!(report.value.dropped, 0);

        slot.retune();
        drop(slot);
        let last = mine();
        assert_eq!(last.len(), 1);
        assert!(!last[0].live, "reported once more, gone");
        assert!(mine().is_empty());
    }

    #[test]
    fn the_tap_logs_every_message_decoded() {
        let ch = ControlChannel {
            port: 4,
            ..channel()
        };
        let dir = std::env::temp_dir().join(format!("p25-control-port-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("cc_test_851000000");
        let mut slot =
            ControlSlot::new(ch, Modulation::Cqpsk, 24_000.0, 77, &base.to_string_lossy());
        let mut out = Decoded::default();
        for chunk in iq(2).chunks(600) {
            slot.process(chunk, &Reading::default(), &mut out);
        }
        let sent = messages(&out).len();
        drop(slot);
        let raw = std::fs::read_to_string(format!("{}.msgs.jsonl", base.display())).unwrap();
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // Every packet yields a trunking message.
        assert_eq!(lines.len(), sent);
        assert_eq!(lines[0]["seq"], 0);
        assert_eq!(lines[0]["type"], 7);
        assert!(lines[0]["hex"].as_str().unwrap().starts_with("038d"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
