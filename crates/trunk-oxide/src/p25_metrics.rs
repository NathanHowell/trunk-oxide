//! Metrics for the trunk-p25 control-channel decoders, read once a second.
//!
//! The decoders keep cumulative counters; each poll exports the change since
//! the previous one, labelled by system and control-channel frequency, plus
//! gauges for the demodulator's state.

use std::collections::HashMap;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Meter};
use trunk_recorder_sys::p25::{ControlChannelReport, DecoderStats};

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
        }
    }

    pub fn poll(&mut self) {
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
