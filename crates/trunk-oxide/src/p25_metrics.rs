//! Metrics for the trunk-p25 control-channel decoders, read once a second.
//!
//! The decoders keep cumulative counters; each poll exports the change since
//! the previous one, labelled by system and control-channel frequency, plus
//! gauges for the demodulator's state.

use std::collections::HashMap;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Meter};
use trunk_recorder_sys::p25::{ControlChannelReport, DecoderStats, VoiceChannelReport, VoiceStats};

/// A decoder's identity across polls: (source, channelizer port).
type Key = (i32, u32);

pub struct P25Metrics {
    tsbk: Counter<u64>,
    syncs: Counter<u64>,
    nid: Counter<u64>,
    frames: Counter<u64>,
    corrected_dibits: Counter<u64>,
    mbt: Counter<u64>,
    timeouts: Counter<u64>,
    dropped: Counter<u64>,
    symbol_snr_db: Gauge<f64>,
    residual_offset_hz: Gauge<f64>,
    clock_ppm: Gauge<f64>,
    prev: HashMap<Key, (DecoderStats, u64)>,
    voice: VoiceMetrics,
}

/// Phase 1 voice regenerators, one per recorder.
struct VoiceMetrics {
    units: Counter<u64>,
    imbe_frames: Counter<u64>,
    imbe_bit_errors: Counter<u64>,
    words: Counter<u64>,
    symbol_snr_db: Gauge<f64>,
    prev: HashMap<i32, VoiceStats>,
}

impl VoiceMetrics {
    fn new(meter: &Meter) -> Self {
        Self {
            units: meter
                .u64_counter("trunk.oxide.p25voice.units")
                .with_description("Phase 1 voice-channel data units decoded by trunk-p25, by kind")
                .build(),
            imbe_frames: meter
                .u64_counter("trunk.oxide.p25voice.imbe_frames")
                .with_description("IMBE voice frames (20 ms each)")
                .build(),
            imbe_bit_errors: meter
                .u64_counter("trunk.oxide.p25voice.imbe_bit_errors")
                .with_description("Bits the Golay/Hamming decoders corrected in IMBE frames")
                .build(),
            words: meter
                .u64_counter("trunk.oxide.p25voice.signalling")
                .with_description("Reed-Solomon signalling words (link control, encryption sync, header): ok or failed")
                .build(),
            symbol_snr_db: meter
                .f64_gauge("trunk.oxide.p25voice.symbol_snr_db")
                .with_description("Eye quality of the voice channel being recorded")
                .with_unit("dB")
                .build(),
            prev: HashMap::new(),
        }
    }

    fn poll(&mut self) {
        let reports = trunk_recorder_sys::p25::voice_channels();
        for r in &reports {
            self.export(r);
        }
    }

    fn export(&mut self, r: &VoiceChannelReport) {
        let base = [
            KeyValue::new("recorder", i64::from(r.recorder)),
            KeyValue::new("system", r.system.clone()),
        ];
        let was = self.prev.insert(r.recorder, r.stats).unwrap_or_default();
        let now = &r.stats;
        let d = |a: u64, b: u64| a.saturating_sub(b);
        for (kind, n) in [
            ("header", d(now.headers, was.headers)),
            ("ldu1", d(now.ldu1, was.ldu1)),
            ("ldu2", d(now.ldu2, was.ldu2)),
            ("terminator", d(now.terminators, was.terminators)),
        ] {
            self.units.add(n, &with(&base, "kind", kind));
        }
        self.imbe_frames.add(d(now.imbe_frames, was.imbe_frames), &base);
        self.imbe_bit_errors.add(d(now.imbe_errors, was.imbe_errors), &base);
        for (word, ok, failed) in [
            ("link_control", d(now.link_control, was.link_control), d(now.link_control_failed, was.link_control_failed)),
            ("encryption_sync", d(now.encryption_sync, was.encryption_sync), d(now.encryption_sync_failed, was.encryption_sync_failed)),
            ("header", d(now.headers, was.headers).saturating_sub(d(now.headers_failed, was.headers_failed)), d(now.headers_failed, was.headers_failed)),
        ] {
            let mut ok_attrs = with(&base, "word", word);
            ok_attrs.push(KeyValue::new("result", "ok"));
            self.words.add(ok, &ok_attrs);
            let mut failed_attrs = with(&base, "word", word);
            failed_attrs.push(KeyValue::new("result", "failed"));
            self.words.add(failed, &failed_attrs);
        }
        if d(now.imbe_frames, was.imbe_frames) > 0 {
            self.symbol_snr_db.record(f64::from(now.demod.symbol_snr_db()), &base);
        }
    }
}

/// How a TSBK came out, as the `result` label.
#[derive(Clone, Copy)]
enum TsbkResult {
    Clean,
    Corrected,
    Recovered { rank: usize },
    Failed,
}

impl TsbkResult {
    fn attrs(self, base: &[KeyValue]) -> Vec<KeyValue> {
        let mut a = base.to_vec();
        match self {
            Self::Clean => a.push(KeyValue::new("result", "clean")),
            Self::Corrected => a.push(KeyValue::new("result", "corrected")),
            Self::Recovered { rank } => {
                a.push(KeyValue::new("result", "recovered"));
                a.push(KeyValue::new("rank", i64::try_from(rank).unwrap_or(i64::MAX)));
            }
            Self::Failed => a.push(KeyValue::new("result", "failed")),
        }
        a
    }
}

fn with(base: &[KeyValue], key: &'static str, value: &'static str) -> Vec<KeyValue> {
    let mut a = base.to_vec();
    a.push(KeyValue::new(key, value));
    a
}

impl P25Metrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            tsbk: meter
                .u64_counter("trunk.oxide.p25cc.tsbk")
                .with_description(
                    "Control-channel TSBKs by result: clean, corrected (Viterbi), \
                     recovered (list decoder, by rank) or failed (CRC)",
                )
                .build(),
            syncs: meter
                .u64_counter("trunk.oxide.p25cc.syncs")
                .with_description("Frame syncs: found by search, or accepted where the last frame predicted one")
                .build(),
            nid: meter
                .u64_counter("trunk.oxide.p25cc.nid")
                .with_description("NID decodes: hard BCH, maximum-likelihood against the learned NAC, or failed")
                .build(),
            frames: meter
                .u64_counter("trunk.oxide.p25cc.frames")
                .with_description("Frames: complete, or truncated by the next sync")
                .build(),
            corrected_dibits: meter
                .u64_counter("trunk.oxide.p25cc.corrected_dibits")
                .with_description("Dibits the trellis decoder corrected in accepted blocks")
                .build(),
            mbt: meter
                .u64_counter("trunk.oxide.p25cc.mbt")
                .with_description("Multi-block trunking PDUs: ok or failed")
                .build(),
            timeouts: meter
                .u64_counter("trunk.oxide.p25cc.timeouts")
                .with_description("Seconds with nothing decoded")
                .build(),
            dropped: meter
                .u64_counter("trunk.oxide.p25cc.dropped")
                .with_description("Decoded messages dropped because the GR block fell behind")
                .build(),
            symbol_snr_db: meter
                .f64_gauge("trunk.oxide.p25cc.symbol_snr_db")
                .with_description("Eye quality: soft-symbol SNR after the demodulator")
                .with_unit("dB")
                .build(),
            residual_offset_hz: meter
                .f64_gauge("trunk.oxide.p25cc.residual_offset_hz")
                .with_description("Frequency offset left after the FLL, from the differential phase bias")
                .with_unit("Hz")
                .build(),
            clock_ppm: meter
                .f64_gauge("trunk.oxide.p25cc.clock_ppm")
                .with_description("Symbol clock error against 4800 baud")
                .build(),
            prev: HashMap::new(),
            voice: VoiceMetrics::new(meter),
        }
    }

    pub fn poll(&mut self) {
        self.voice.poll();
        let reports = trunk_recorder_sys::p25::control_channels();
        self.prev
            .retain(|k, _| reports.iter().any(|r| (r.channel.source, r.channel.port) == *k));
        for r in &reports {
            self.export(r);
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn export(&mut self, r: &ControlChannelReport) {
        let base = [
            KeyValue::new("system", r.channel.system.clone()),
            KeyValue::new("freq_hz", r.channel.freq_hz as i64),
        ];
        let key = (r.channel.source, r.channel.port);
        let (prev, prev_dropped) = self.prev.insert(key, (r.stats, r.dropped)).unwrap_or_default();
        let (now, was) = (&r.stats, &prev);
        let d = |a: u64, b: u64| a.saturating_sub(b);

        self.tsbk.add(d(now.tsbk_clean, was.tsbk_clean), &TsbkResult::Clean.attrs(&base));
        self.tsbk.add(d(now.tsbk_corrected, was.tsbk_corrected), &TsbkResult::Corrected.attrs(&base));
        for rank in 1..now.tsbk_list_rank.len() {
            let n = d(now.tsbk_list_rank[rank], was.tsbk_list_rank[rank]);
            if n > 0 {
                self.tsbk.add(n, &TsbkResult::Recovered { rank }.attrs(&base));
            }
        }
        self.tsbk.add(d(now.tsbk_failed, was.tsbk_failed), &TsbkResult::Failed.attrs(&base));

        let (f, pf) = (&now.framer, &was.framer);
        let flywheel = d(f.flywheel_syncs, pf.flywheel_syncs);
        self.syncs.add(d(f.syncs, pf.syncs).saturating_sub(flywheel), &with(&base, "kind", "search"));
        self.syncs.add(flywheel, &with(&base, "kind", "flywheel"));
        self.nid.add(d(f.nid_ok_hard, pf.nid_ok_hard), &with(&base, "method", "hard"));
        self.nid.add(d(f.nid_ok_ml, pf.nid_ok_ml), &with(&base, "method", "ml"));
        self.nid.add(d(f.nid_failed, pf.nid_failed), &with(&base, "method", "failed"));
        let truncated = d(f.truncated, pf.truncated);
        self.frames.add(d(f.frames, pf.frames).saturating_sub(truncated), &with(&base, "kind", "complete"));
        self.frames.add(truncated, &with(&base, "kind", "truncated"));

        self.corrected_dibits.add(d(now.corrected_dibits, was.corrected_dibits), &base);
        self.mbt.add(d(now.mbt_ok, was.mbt_ok), &with(&base, "result", "ok"));
        self.mbt.add(d(now.mbt_failed, was.mbt_failed), &with(&base, "result", "failed"));
        self.timeouts.add(d(now.timeouts, was.timeouts), &base);
        self.dropped.add(d(r.dropped, prev_dropped), &base);

        if now.demod.symbols > 0 {
            let demod = &now.demod;
            self.symbol_snr_db.record(f64::from(demod.symbol_snr_db()), &base);
            self.residual_offset_hz.record(f64::from(demod.bias_hz), &base);
            self.clock_ppm.record(demod.clock_ppm(), &base);
        }
    }
}
