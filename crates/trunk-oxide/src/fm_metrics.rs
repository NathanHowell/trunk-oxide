//! Metrics for the trunk-fm analog decoders, read once a second beside the
//! P25 ones. Counters export the change since the previous poll, labelled
//! by system, talkgroup and frequency; gauges the last hop's state.

use std::collections::HashMap;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Meter};
use trunk_radio::fm::{AnalogReport, AnalogStats, analog_channels};

/// A decoder's identity across polls (see [`trunk_radio::registry`]).
type Key = u64;

pub struct FmMetrics {
    hops: Counter<u64>,
    opens: Counter<u64>,
    closes: Counter<u64>,
    quieting_db: Gauge<f64>,
    tone_hz: Gauge<f64>,
    tone_amplitude: Gauge<f64>,
    open: Gauge<u64>,
    prev: HashMap<Key, AnalogStats>,
}

impl FmMetrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            hops: meter
                .u64_counter("trunk.oxide.fm.hops")
                .with_description("100 ms hops of a conventional analog channel, by the gate's state (open, closed, silent)")
                .build(),
            opens: meter
                .u64_counter("trunk.oxide.fm.opens")
                .with_description("Transmissions the trunk-fm gate opened for")
                .build(),
            closes: meter
                .u64_counter("trunk.oxide.fm.closes")
                .with_description("Gate closes, by reason (quieting, tone_lost, silence)")
                .build(),
            quieting_db: meter
                .f64_gauge("trunk.oxide.fm.quieting_db")
                .with_description("How far the channel's above-voice noise sits below its no-carrier level, last hop")
                .with_unit("dB")
                .build(),
            tone_hz: meter
                .f64_gauge("trunk.oxide.fm.tone_hz")
                .with_description("The CTCSS tone dominating the last hop, 0 when none stands out")
                .with_unit("Hz")
                .build(),
            tone_amplitude: meter
                .f64_gauge("trunk.oxide.fm.tone_amplitude")
                .with_description("Amplitude of the last hop's dominant tone, 1.0 = full deviation")
                .build(),
            open: meter
                .u64_gauge("trunk.oxide.fm.open")
                .with_description("1 while the trunk-fm gate is open")
                .build(),
            prev: HashMap::new(),
        }
    }

    pub fn poll(&mut self) {
        let reports = analog_channels();
        for e in &reports {
            self.export(e.id, &e.value);
        }
        self.prev
            .retain(|k, _| reports.iter().any(|e| e.id == *k && e.live));
    }

    #[allow(clippy::cast_possible_truncation)]
    fn export(&mut self, key: Key, r: &AnalogReport) {
        let base = [
            KeyValue::new("system", r.channel.system.clone()),
            KeyValue::new("talkgroup", i64::from(r.channel.talkgroup)),
            KeyValue::new("freq_hz", r.channel.freq_hz as i64),
        ];
        let with = |k: &'static str, v: &'static str| {
            let mut a = base.to_vec();
            a.push(KeyValue::new(k, v));
            a
        };
        let was = self.prev.insert(key, r.stats).unwrap_or_default();
        let now = &r.stats;
        let d = |a: u64, b: u64| a.saturating_sub(b);

        self.hops
            .add(d(now.hops_open, was.hops_open), &with("state", "open"));
        self.hops.add(
            d(now.hops_closed, was.hops_closed),
            &with("state", "closed"),
        );
        self.hops.add(
            d(now.hops_silent, was.hops_silent),
            &with("state", "silent"),
        );
        self.opens.add(d(now.opens, was.opens), &base);
        self.closes.add(
            d(now.closes_quieting, was.closes_quieting),
            &with("reason", "quieting"),
        );
        self.closes.add(
            d(now.closes_tone_lost, was.closes_tone_lost),
            &with("reason", "tone_lost"),
        );
        self.closes.add(
            d(now.closes_silence, was.closes_silence),
            &with("reason", "silence"),
        );
        self.quieting_db.record(now.quieting_db, &base);
        self.tone_hz
            .record(now.tone.map_or(0.0, |t| f64::from(t.hz())), &base);
        self.tone_amplitude
            .record(f64::from(now.tone_amplitude), &base);
        self.open.record(u64::from(now.open), &base);
    }
}
