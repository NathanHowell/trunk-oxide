//! Metrics for the trunk-p25 control-channel decoders, read once a second.
//!
//! The decoders keep cumulative counters; each poll exports the change since
//! the previous one, labelled by system and control-channel frequency, plus
//! gauges for the demodulator's state.

use std::collections::{BTreeMap, HashMap};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Meter};
use trunk_radio::p25::{
    ControlChannelReport, DecoderStats, MacOpcode, TdmaStats, VoiceChannelReport, VoiceStats,
};
use trunk_radio::p25_calls::{self, CallTally, Phase};
use trunk_radio::p25_parse::{self, ParseStats};

/// A decoder's identity across polls (see [`trunk_radio::registry`]).
type Key = u64;

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
    calls: CallMetrics,
    parser: ParserMetrics,
}

/// The control channel parser, per system.
struct ParserMetrics {
    osp: Counter<u64>,
    channels: Counter<u64>,
    invalid: Counter<u64>,
    plan_conflicts: Counter<u64>,
    held_grants: Counter<u64>,
    prev: HashMap<i32, ParseStats>,
}

impl ParserMetrics {
    fn new(meter: &Meter) -> Self {
        Self {
            osp: meter
                .u64_counter("trunk.oxide.p25cc.osp")
                .with_description("Control channel packets parsed, by system and kind")
                .build(),
            channels: meter
                .u64_counter("trunk.oxide.p25cc.channels")
                .with_description(
                    "Channel numbers resolved to a frequency: by the control channel's own \
                     plans, another control channel of the system's (shared), or not at all",
                )
                .build(),
            invalid: meter
                .u64_counter("trunk.oxide.p25cc.invalid")
                .with_description(
                    "Invalid or empty control channel messages (op25 timeouts are empty)",
                )
                .build(),
            plan_conflicts: meter
                .u64_counter("trunk.oxide.p25cc.plan_conflicts")
                .with_description(
                    "Sites of one system announcing different plans for one identifier",
                )
                .build(),
            held_grants: meter
                .u64_counter("trunk.oxide.p25cc.held_grants")
                .with_description(
                    "Grants held until their channel's plan arrived (held), \
                     and those whose plan never came (expired)",
                )
                .build(),
            prev: HashMap::new(),
        }
    }

    fn poll(&mut self) {
        let d = |now: u64, was: u64| now.saturating_sub(was);
        for r in p25_parse::parse_stats() {
            let was = self
                .prev
                .insert(r.sys_num, r.stats.clone())
                .unwrap_or_default();
            let now = &r.stats;
            let base = [KeyValue::new("system", r.system.clone())];
            for (&kind, &n) in &now.kinds {
                let w = was.kinds.get(kind).copied().unwrap_or(0);
                self.osp.add(d(n, w), &with(&base, "kind", kind));
            }
            for (plan, n, w) in [
                ("own", now.own_plans, was.own_plans),
                ("shared", now.shared_plans, was.shared_plans),
                ("unknown", now.unknown_channels, was.unknown_channels),
            ] {
                self.channels.add(d(n, w), &with(&base, "plan", plan));
            }
            self.invalid.add(d(now.invalid, was.invalid), &base);
            self.plan_conflicts
                .add(d(now.conflicts, was.conflicts), &base);
            self.held_grants.add(
                d(now.deferred, was.deferred),
                &with(&base, "result", "held"),
            );
            self.held_grants.add(
                d(now.expired, was.expired),
                &with(&base, "result", "expired"),
            );
        }
    }
}

/// Voice decoders, one per open voice channel.
struct VoiceMetrics {
    units: Counter<u64>,
    imbe_frames: Counter<u64>,
    imbe_bit_errors: Counter<u64>,
    words: Counter<u64>,
    symbol_snr_db: Gauge<f64>,
    /// By decoder (see [`trunk_radio::registry`]).
    prev: HashMap<Key, VoiceStats>,
    tdma: TdmaMetrics,
}

/// Phase 2 (TDMA) side of the voice regenerators.
struct TdmaMetrics {
    bursts: Counter<u64>,
    ambe_frames: Counter<u64>,
    ambe_bit_errors: Counter<u64>,
    mac: Counter<u64>,
    encryption_sync: Counter<u64>,
    sync: Counter<u64>,
    symbol_snr_db: Gauge<f64>,
    prev: HashMap<Key, TdmaStats>,
}

impl TdmaMetrics {
    fn new(meter: &Meter) -> Self {
        Self {
            bursts: meter
                .u64_counter("trunk.oxide.p25tdma.bursts")
                .with_description("Phase 2 bursts decoded by trunk-p25, by kind (voice4, voice2, mac, unread)")
                .build(),
            ambe_frames: meter
                .u64_counter("trunk.oxide.p25tdma.ambe_frames")
                .with_description("AMBE+2 voice frames (20 ms each): trusted (regenerated) or untrusted (passed through)")
                .build(),
            ambe_bit_errors: meter
                .u64_counter("trunk.oxide.p25tdma.ambe_bit_errors")
                .with_description("Bits the Golay decoders corrected in AMBE+2 frames")
                .build(),
            mac: meter
                .u64_counter("trunk.oxide.p25tdma.mac")
                .with_description("FACCH/SACCH MAC PDUs: ok (Reed-Solomon and CRC-12) by opcode, failed, or foreign (a MAC_END_PTT with another system's color code)")
                .build(),
            encryption_sync: meter
                .u64_counter("trunk.oxide.p25tdma.encryption_sync")
                .with_description("Encryption sync words (ESS) at each 2V burst: ok or failed")
                .build(),
            sync: meter
                .u64_counter("trunk.oxide.p25tdma.sync")
                .with_description("Burst framing events: acquisition, loss, slip (one-symbol timing step), rephase")
                .build(),
            symbol_snr_db: meter
                .f64_gauge("trunk.oxide.p25tdma.symbol_snr_db")
                .with_description("Eye quality of the TDMA voice channel being recorded")
                .with_unit("dB")
                .build(),
            prev: HashMap::new(),
        }
    }

    fn export(&mut self, key: Key, base: &[KeyValue], now: &TdmaStats) {
        let was = self.prev.insert(key, *now).unwrap_or_default();
        let d = |a: u64, b: u64| a.saturating_sub(b);
        let mac_total = d(now.mac + now.mac_failed, was.mac + was.mac_failed);
        for (kind, n) in [
            ("voice4", d(now.voice4, was.voice4)),
            ("voice2", d(now.voice2, was.voice2)),
            ("mac", mac_total),
            ("unread", d(now.unread, was.unread)),
        ] {
            self.bursts.add(n, &with(base, "kind", kind));
        }
        let frames = d(now.ambe_frames, was.ambe_frames);
        let trusted = d(now.ambe_trusted, was.ambe_trusted);
        self.ambe_frames
            .add(trusted, &with(base, "result", "trusted"));
        self.ambe_frames.add(
            frames.saturating_sub(trusted),
            &with(base, "result", "untrusted"),
        );
        self.ambe_bit_errors
            .add(d(now.ambe_errors, was.ambe_errors), base);
        for bits in 0..8u8 {
            let i = usize::from(bits);
            let mut n = d(now.mac_by_opcode[i], was.mac_by_opcode[i]);
            let opcode = MacOpcode::from_bits(bits);
            // Only opcodes heard: eight per channel would be mostly zeros.
            if opcode == MacOpcode::EndPtt {
                let foreign = d(now.end_ptt_foreign, was.end_ptt_foreign);
                n = n.saturating_sub(foreign);
                if foreign > 0 {
                    let mut attrs = with(base, "result", "foreign");
                    attrs.push(KeyValue::new("opcode", opcode.name()));
                    self.mac.add(foreign, &attrs);
                }
            }
            if n > 0 {
                let mut attrs = with(base, "result", "ok");
                attrs.push(KeyValue::new("opcode", opcode.name()));
                self.mac.add(n, &attrs);
            }
        }
        self.mac.add(
            d(now.mac_failed, was.mac_failed),
            &with(base, "result", "failed"),
        );
        self.encryption_sync.add(
            d(now.encryption_sync, was.encryption_sync),
            &with(base, "result", "ok"),
        );
        self.encryption_sync.add(
            d(now.encryption_sync_failed, was.encryption_sync_failed),
            &with(base, "result", "failed"),
        );
        let (f, g) = (&now.framer, &was.framer);
        for (event, n) in [
            ("acquisition", d(f.acquisitions, g.acquisitions)),
            ("loss", d(f.losses, g.losses)),
            ("slip", d(f.slips, g.slips)),
            ("rephase", d(f.rephases, g.rephases)),
        ] {
            self.sync.add(n, &with(base, "event", event));
        }
        if frames > 0 {
            self.symbol_snr_db
                .record(f64::from(now.demod.symbol_snr_db()), base);
        }
    }
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
            tdma: TdmaMetrics::new(meter),
        }
    }

    fn poll(&mut self) {
        let reports = trunk_radio::p25::voice_channels();
        for e in &reports {
            self.export(e.id, &e.value);
        }
        self.prev
            .retain(|k, _| reports.iter().any(|e| e.id == *k && e.live));
        self.tdma
            .prev
            .retain(|k, _| reports.iter().any(|e| e.id == *k && e.live));
    }

    #[allow(clippy::cast_possible_truncation)]
    fn export(&mut self, key: Key, r: &VoiceChannelReport) {
        let base = [
            KeyValue::new("system", r.system.clone()),
            KeyValue::new("freq_hz", r.freq_hz as i64),
        ];
        let was = self.prev.insert(key, r.stats).unwrap_or_default();
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
        self.imbe_frames
            .add(d(now.imbe_frames, was.imbe_frames), &base);
        self.imbe_bit_errors
            .add(d(now.imbe_errors, was.imbe_errors), &base);
        for (word, ok, failed) in [
            (
                "link_control",
                d(now.link_control, was.link_control),
                d(now.link_control_failed, was.link_control_failed),
            ),
            (
                "encryption_sync",
                d(now.encryption_sync, was.encryption_sync),
                d(now.encryption_sync_failed, was.encryption_sync_failed),
            ),
            (
                "header",
                d(now.headers, was.headers)
                    .saturating_sub(d(now.headers_failed, was.headers_failed)),
                d(now.headers_failed, was.headers_failed),
            ),
        ] {
            let mut ok_attrs = with(&base, "word", word);
            ok_attrs.push(KeyValue::new("result", "ok"));
            self.words.add(ok, &ok_attrs);
            let mut failed_attrs = with(&base, "word", word);
            failed_attrs.push(KeyValue::new("result", "failed"));
            self.words.add(failed, &failed_attrs);
        }
        if d(now.imbe_frames, was.imbe_frames) > 0 {
            self.symbol_snr_db
                .record(f64::from(now.demod.symbol_snr_db()), &base);
        }
        self.tdma.export(key, &base, &r.tdma);
    }
}

/// Every P25 voice call's outcome, by system and phase.
struct CallMetrics {
    calls: Counter<u64>,
    call_audio: Counter<f64>,
    call_transmissions: Counter<u64>,
    call_bit_errors: Counter<u64>,
    call_spikes: Counter<u64>,
    call_encrypted: Counter<u64>,
    call_repeated: Counter<u64>,
    call_muted: Counter<u64>,
    prev: BTreeMap<(String, Phase), CallTally>,
}

impl CallMetrics {
    fn new(meter: &Meter) -> Self {
        Self {
            calls: meter
                .u64_counter("trunk.oxide.p25call.calls")
                .with_description("P25 voice calls recorded, by system and phase")
                .build(),
            call_audio: meter
                .f64_counter("trunk.oxide.p25call.audio")
                .with_description("Audio the calls produced")
                .with_unit("s")
                .build(),
            call_transmissions: meter
                .u64_counter("trunk.oxide.p25call.transmissions")
                .with_description("Transmissions (per source unit) the calls produced")
                .build(),
            call_bit_errors: meter
                .u64_counter("trunk.oxide.p25call.bit_errors")
                .with_description("Voice-frame bits corrected")
                .build(),
            call_spikes: meter
                .u64_counter("trunk.oxide.p25call.spikes")
                .with_description("Error spikes (phase 1) or untrusted frames (phase 2)")
                .build(),
            call_encrypted: meter
                .u64_counter("trunk.oxide.p25call.encrypted")
                .with_description("Calls whose encryption sync confirmed encryption")
                .build(),
            call_repeated: meter
                .u64_counter("trunk.oxide.p25call.repeated")
                .with_description("Voice frames the vocoder repeated for errors")
                .build(),
            call_muted: meter
                .u64_counter("trunk.oxide.p25call.muted")
                .with_description("Voice frames the vocoder muted for errors")
                .build(),
            prev: BTreeMap::new(),
        }
    }

    fn poll(&mut self) {
        let now = p25_calls::call_tally();
        for ((system, phase), t) in &now {
            let was = self
                .prev
                .get(&(system.clone(), *phase))
                .copied()
                .unwrap_or_default();
            let a = [
                KeyValue::new("system", system.clone()),
                KeyValue::new("phase", phase.label()),
            ];
            let d = |x: u64, y: u64| x.saturating_sub(y);
            self.calls.add(d(t.calls, was.calls), &a);
            self.call_audio
                .add((t.audio_seconds - was.audio_seconds).max(0.0), &a);
            self.call_transmissions
                .add(d(t.transmissions, was.transmissions), &a);
            self.call_bit_errors.add(d(t.errors, was.errors), &a);
            self.call_spikes.add(d(t.spikes, was.spikes), &a);
            self.call_encrypted.add(d(t.encrypted, was.encrypted), &a);
            self.call_repeated.add(d(t.repeated, was.repeated), &a);
            self.call_muted.add(d(t.muted, was.muted), &a);
        }
        self.prev = now;
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
                a.push(KeyValue::new(
                    "rank",
                    i64::try_from(rank).unwrap_or(i64::MAX),
                ));
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
                .with_description(
                    "Frame syncs: found by search, or accepted where the last frame predicted one",
                )
                .build(),
            nid: meter
                .u64_counter("trunk.oxide.p25cc.nid")
                .with_description(
                    "NID decodes: hard BCH, maximum-likelihood against the learned NAC, or failed",
                )
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
                .with_description(
                    "Frequency offset left after the FLL, from the differential phase bias",
                )
                .with_unit("Hz")
                .build(),
            clock_ppm: meter
                .f64_gauge("trunk.oxide.p25cc.clock_ppm")
                .with_description("Symbol clock error against 4800 baud")
                .build(),
            prev: HashMap::new(),
            voice: VoiceMetrics::new(meter),
            calls: CallMetrics::new(meter),
            parser: ParserMetrics::new(meter),
        }
    }

    pub fn poll(&mut self) {
        self.voice.poll();
        self.calls.poll();
        self.parser.poll();
        let reports = trunk_radio::p25::control_channels();
        for e in &reports {
            self.export(e.id, &e.value);
        }
        self.prev
            .retain(|k, _| reports.iter().any(|e| e.id == *k && e.live));
    }

    #[allow(clippy::cast_possible_truncation)]
    fn export(&mut self, key: Key, r: &ControlChannelReport) {
        let base = [
            KeyValue::new("system", r.channel.system.clone()),
            KeyValue::new("freq_hz", r.channel.freq_hz as i64),
        ];
        let (prev, prev_dropped) = self
            .prev
            .insert(key, (r.stats, r.dropped))
            .unwrap_or_default();
        let (now, was) = (&r.stats, &prev);
        let d = |a: u64, b: u64| a.saturating_sub(b);

        self.tsbk.add(
            d(now.tsbk_clean, was.tsbk_clean),
            &TsbkResult::Clean.attrs(&base),
        );
        self.tsbk.add(
            d(now.tsbk_corrected, was.tsbk_corrected),
            &TsbkResult::Corrected.attrs(&base),
        );
        for rank in 1..now.tsbk_list_rank.len() {
            let n = d(now.tsbk_list_rank[rank], was.tsbk_list_rank[rank]);
            if n > 0 {
                self.tsbk
                    .add(n, &TsbkResult::Recovered { rank }.attrs(&base));
            }
        }
        self.tsbk.add(
            d(now.tsbk_failed, was.tsbk_failed),
            &TsbkResult::Failed.attrs(&base),
        );

        let (f, pf) = (&now.framer, &was.framer);
        let flywheel = d(f.flywheel_syncs, pf.flywheel_syncs);
        self.syncs.add(
            d(f.syncs, pf.syncs).saturating_sub(flywheel),
            &with(&base, "kind", "search"),
        );
        self.syncs.add(flywheel, &with(&base, "kind", "flywheel"));
        self.nid.add(
            d(f.nid_ok_hard, pf.nid_ok_hard),
            &with(&base, "method", "hard"),
        );
        self.nid
            .add(d(f.nid_ok_ml, pf.nid_ok_ml), &with(&base, "method", "ml"));
        self.nid.add(
            d(f.nid_failed, pf.nid_failed),
            &with(&base, "method", "failed"),
        );
        let truncated = d(f.truncated, pf.truncated);
        self.frames.add(
            d(f.frames, pf.frames).saturating_sub(truncated),
            &with(&base, "kind", "complete"),
        );
        self.frames
            .add(truncated, &with(&base, "kind", "truncated"));

        self.corrected_dibits
            .add(d(now.corrected_dibits, was.corrected_dibits), &base);
        self.mbt
            .add(d(now.mbt_ok, was.mbt_ok), &with(&base, "result", "ok"));
        self.mbt.add(
            d(now.mbt_failed, was.mbt_failed),
            &with(&base, "result", "failed"),
        );
        self.timeouts.add(d(now.timeouts, was.timeouts), &base);
        self.dropped.add(d(r.dropped, prev_dropped), &base);

        if now.demod.symbols > 0 {
            let demod = &now.demod;
            self.symbol_snr_db
                .record(f64::from(demod.symbol_snr_db()), &base);
            self.residual_offset_hz
                .record(f64::from(demod.bias_hz), &base);
            self.clock_ppm.record(demod.clock_ppm(), &base);
        }
    }
}
