//! P25 control-channel decoders hosted on shared-channelizer slots.
//!
//! The decoder runs on the channelizer's DSP thread, on the slot's
//! conditioned output (see [`trunk_channelizer::ChannelDecoder`]); its
//! messages wait in a bounded queue until the source thread, after each
//! channelizer block, has the [`ControlPort`] parse them and send the
//! trunking messages on. Counters are readable from any thread for metrics.

use std::collections::VecDeque;
use std::fmt;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{LineWriter, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use num_complex::Complex32;
use trunk_channelizer::ChannelDecoder;
use trunk_p25::voice::{Encryption, EncryptionTracker};
use trunk_p25::{
    ControlChannelDecoder, DecoderConfig, DemodConfig, Message, Modulation, Nac, Received,
};

pub use trunk_p25::DecoderStats;
pub use trunk_p25::tdma::TdmaStats;
pub use trunk_p25::voice::VoiceStats;

use crate::p25_parse::P25TrunkParser;
use crate::types::TrTrunkMessage;

/// Messages held for the drain; beyond this the oldest are dropped (it
/// drains every channelizer block, so this only fills if it stalls).
const QUEUE_CAPACITY: usize = 1024;

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
    /// Messages dropped because the queue was full.
    pub dropped: u64,
}

/// State shared by the DSP-side decoder and the drain side.
pub(crate) struct Shared {
    channel: ControlChannel,
    modulation: Modulation,
    queue: Mutex<VecDeque<Received>>,
    snapshot: Mutex<(DecoderStats, Option<Nac>)>,
    dropped: AtomicU64,
}

impl Shared {
    pub(crate) fn pop(&self) -> Option<Received> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    fn report(&self) -> ControlChannelReport {
        let (stats, nac) = *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        ControlChannelReport {
            channel: self.channel.clone(),
            modulation: self.modulation,
            nac,
            stats,
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }
}

/// Every live decoder, for [`control_channels`].
static REGISTRY: Mutex<Vec<Weak<Shared>>> = Mutex::new(Vec::new());

/// Counters of every running control-channel decoder.
pub fn control_channels() -> Vec<ControlChannelReport> {
    let mut reg = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    reg.retain(|w| w.strong_count() > 0);
    reg.iter()
        .filter_map(Weak::upgrade)
        .map(|s| s.report())
        .collect()
}

/// The DSP-thread half: owns the decoder.
struct SlotDecoder {
    decoder: ControlChannelDecoder,
    shared: Arc<Shared>,
    scratch: Vec<Received>,
}

impl fmt::Debug for SlotDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotDecoder")
            .field("channel", &self.shared.channel)
            .finish_non_exhaustive()
    }
}

impl ChannelDecoder for SlotDecoder {
    fn process(&mut self, iq: &[Complex32]) {
        self.decoder.process(iq, &mut self.scratch);
        if !self.scratch.is_empty() {
            let mut q = self
                .shared
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for m in self.scratch.drain(..) {
                if q.len() == QUEUE_CAPACITY {
                    q.pop_front();
                    self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                }
                q.push_back(m);
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
        self.shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// Build a decoder for `channel` at the slot's conditioned `rate`; returns
/// the DSP half to install on the slot and the drain half to keep.
pub(crate) fn attach(
    channel: ControlChannel,
    modulation: Modulation,
    rate: f64,
) -> (Box<dyn ChannelDecoder>, Arc<Shared>) {
    let decoder =
        ControlChannelDecoder::new(DecoderConfig::new(DemodConfig::new(rate, modulation)));
    let shared = Arc::new(Shared {
        channel,
        modulation,
        queue: Mutex::new(VecDeque::new()),
        snapshot: Mutex::new((decoder.stats(), None)),
        dropped: AtomicU64::new(0),
    });
    REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Arc::downgrade(&shared));
    (
        Box::new(SlotDecoder {
            decoder,
            shared: Arc::clone(&shared),
            scratch: Vec::new(),
        }),
        shared,
    )
}

/// The trunking side of a control channel decoder: its parser, and the raw
/// message tap when baselines are being taken.
pub(crate) struct ControlPort {
    shared: Arc<Shared>,
    parser: P25TrunkParser,
    raw: Option<RawLog>,
}

impl ControlPort {
    /// `shared`'s messages, for system `sys_num`; with a `tap_base`, every
    /// message is also written to `<tap_base>.msgs.jsonl`.
    pub(crate) fn new(shared: Arc<Shared>, sys_num: i32, tap_base: &str) -> Self {
        let parser = P25TrunkParser::new(sys_num, &shared.channel.system);
        let raw = (!tap_base.is_empty()).then(|| RawLog::open(&format!("{tap_base}.msgs.jsonl")));
        Self {
            shared,
            parser,
            raw: raw.flatten(),
        }
    }

    /// Parse every message the decoder has queued, handing each one's
    /// trunking messages to `send`.
    pub(crate) fn drain(&mut self, send: &mut dyn FnMut(Vec<TrTrunkMessage>)) {
        while let Some(r) = self.shared.pop() {
            if let Some(raw) = &mut self.raw {
                raw.write(&r.message);
            }
            let out = self.parser.parse_message(&r.message);
            if !out.is_empty() {
                send(out);
            }
        }
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

// ── Voice: a decoder per recorder, phase 1 or phase 2 per call ──

/// A recorder's voice decoder counters at one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceChannelReport {
    pub recorder: i32,
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
    recorder: i32,
    snapshot: Mutex<Snapshot>,
}

impl VoiceShared {
    /// A recorder's voice decoder, registered for [`voice_channels`].
    pub(crate) fn register(recorder: i32) -> Arc<Self> {
        let shared = Arc::new(Self {
            recorder,
            snapshot: Mutex::new(Snapshot::default()),
        });
        VOICE_REGISTRY
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&shared));
        shared
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

static VOICE_REGISTRY: Mutex<Vec<Weak<VoiceShared>>> = Mutex::new(Vec::new());

/// Counters of every recorder's voice regenerator.
pub fn voice_channels() -> Vec<VoiceChannelReport> {
    let mut reg = VOICE_REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    reg.retain(|w| w.strong_count() > 0);
    reg.iter()
        .filter_map(Weak::upgrade)
        .map(|s| {
            let snap = s
                .snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            VoiceChannelReport {
                recorder: s.recorder,
                system: snap.system,
                stats: snap.phase1,
                tdma: snap.phase2,
            }
        })
        .collect()
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

    #[test]
    fn decodes_into_the_queue_and_reports() {
        let nac = Nac::new(0x38d).unwrap();
        let mut rng = testgen::Rng::new(1);
        let mut dibits = Vec::new();
        for _ in 0..20 {
            dibits.extend(testgen::tsdu(nac, &testgen::random_tsbks(&mut rng, 3)));
        }
        let iq = testgen::modulate_cqpsk(&dibits, 5);
        let (mut dsp, shared) = attach(channel(), Modulation::Cqpsk, 24_000.0);
        for chunk in iq.chunks(600) {
            dsp.process(chunk);
        }
        let mut tsbks = 0;
        while let Some(r) = shared.pop() {
            if let Message::Tsbk { nac: n, .. } = r.message {
                assert_eq!(n, nac);
                tsbks += 1;
            }
        }
        assert!(tsbks >= 57, "{tsbks}");
        let report = control_channels()
            .into_iter()
            .find(|r| r.channel == channel())
            .unwrap();
        assert_eq!(report.nac, Some(nac));
        assert_eq!(report.stats.tsbk_ok(), tsbks);
        assert_eq!(report.dropped, 0);

        dsp.retune();
        assert!(shared.pop().is_none());
        drop((dsp, shared));
        assert!(control_channels().iter().all(|r| r.channel != channel()));
    }

    #[test]
    fn a_control_port_parses_and_taps_what_was_decoded() {
        let nac = Nac::new(0x38d).unwrap();
        let mut rng = testgen::Rng::new(2);
        let mut dibits = Vec::new();
        for _ in 0..20 {
            dibits.extend(testgen::tsdu(nac, &testgen::random_tsbks(&mut rng, 3)));
        }
        let iq = testgen::modulate_cqpsk(&dibits, 5);
        let ch = ControlChannel {
            port: 4,
            ..channel()
        };
        let (mut dsp, shared) = attach(ch, Modulation::Cqpsk, 24_000.0);
        let dir = std::env::temp_dir().join(format!("p25-control-port-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("cc_test_851000000");
        let mut port = ControlPort::new(Arc::clone(&shared), 77, &base.to_string_lossy());
        for chunk in iq.chunks(600) {
            dsp.process(chunk);
        }
        let mut sent = Vec::new();
        port.drain(&mut |m| sent.push(m));
        assert!(shared.pop().is_none());
        // Every packet yields a trunking message.
        assert!(sent.len() >= 57, "{}", sent.len());
        assert!(
            sent.iter()
                .flatten()
                .all(|m| m.sys_num == 77 && m.nac == 0x38d)
        );
        drop(port);
        let raw = std::fs::read_to_string(format!("{}.msgs.jsonl", base.display())).unwrap();
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), sent.len());
        assert_eq!(lines[0]["seq"], 0);
        assert_eq!(lines[0]["type"], 7);
        assert!(lines[0]["hex"].as_str().unwrap().starts_with("038d"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
