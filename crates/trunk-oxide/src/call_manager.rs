use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use trunk_radio::{
    ChannelNum, ChannelReport, DetectedSignal, Radio, Squelch, TrunkMessage, Trunking,
};

use trunk_config::SystemType;
use trunk_oxide_grpc::EventBridge;
use trunk_oxide_grpc::proto;
use trunk_oxide_grpc::proto::SystemType as ProtoSystemType;

use crate::ConventionalMetricsSnapshot;

use crate::call_state::{
    CallEndReason, CallId, CallState, CallView, Effect, Effects, Freq, Talkgroup, TrunkStateMachine,
};

pub use crate::call_state::{
    CallManagerConfig, ConventionalChannel, SystemPolicy, TalkgroupPolicy,
};

/// Serialize a span's trace context to a W3C `traceparent` string via the global
/// propagator. Empty when tracing is disabled (no exporter/propagator), which is
/// the correct "no parent" signal for the consumer.
fn traceparent_of(span: &tracing::Span) -> String {
    let cx = span.context();
    let mut carrier = HashMap::<String, String>::new();
    opentelemetry::global::get_text_map_propagator(|prop| {
        prop.inject_context(&cx, &mut carrier);
    });
    carrier.remove("traceparent").unwrap_or_default()
}

use trunk_oxide_grpc::AudioMeta;

/// A system's type as the wire says it.
pub(crate) const fn proto_system_type(st: SystemType) -> ProtoSystemType {
    match st {
        SystemType::Conventional => ProtoSystemType::Conventional,
        SystemType::ConventionalP25 => ProtoSystemType::ConventionalP25,
        SystemType::ConventionalDMR => ProtoSystemType::ConventionalDmr,
        SystemType::ConventionalSigmf => ProtoSystemType::ConventionalSigmf,
        SystemType::Smartnet => ProtoSystemType::Smartnet,
        SystemType::P25 => ProtoSystemType::P25,
    }
}

/// Voice decode-quality counters (errored / spiked voice codewords and total
/// samples) summed over a call's transmissions. A channel's recording runs
/// across a trunked call's per-speaker boundaries, so they are cumulative;
/// each boundary reports the change since the last (see [`Tally`]) so each
/// per-speaker CallEnd carries only its own slice and the slices still sum to
/// the call total.
#[derive(Default, Clone, Copy)]
struct VoiceCounters {
    errors: i64,
    spikes: i64,
    samples: i64,
    /// op25 terminator (TDU) events seen, and audio samples produced after the
    /// terminator position within the same work call (candidate end-of-call
    /// clipping; see `headless_sink`).
    term_events: i64,
    after_term_samples: i64,
}

impl VoiceCounters {
    fn from_transmissions(list: &[trunk_radio::Transmission]) -> Self {
        let mut c = Self::default();
        for t in list {
            c.errors += t.error_count;
            c.spikes += t.spike_count;
            c.samples += t.sample_count;
            c.term_events += t.term_events;
            c.after_term_samples += t.after_term_samples;
        }
        c
    }

    /// This transmission's slice since the prior snapshot (clamped at 0).
    fn since(self, prior: Self) -> Self {
        Self {
            errors: (self.errors - prior.errors).max(0),
            spikes: (self.spikes - prior.spikes).max(0),
            samples: (self.samples - prior.samples).max(0),
            term_events: (self.term_events - prior.term_events).max(0),
            after_term_samples: (self.after_term_samples - prior.after_term_samples).max(0),
        }
    }
}

/// Voice this recent when a timeout stops a call means it was cut mid-speech.
const CUT_VOICE_AGE_S: f64 = 0.5;

/// What a channel last carried, and when it closed.
#[derive(Clone, Copy, Debug)]
struct ChannelUse {
    talkgroup: Talkgroup,
    freq: Freq,
    slot: i32,
    closed: Instant,
}

/// What a call's channels recorded, until its (or its transmission's) end
/// reports it.
#[derive(Default)]
struct Tally {
    /// What its closed (and rotated) recordings recorded.
    transmissions: Vec<trunk_radio::Transmission>,
    /// The last channel that closed (or rotated) for it.
    last: Option<ChannelReport>,
    /// The counters per-speaker boundaries already reported.
    reported: VoiceCounters,
}

impl Tally {
    fn add(&mut self, report: ChannelReport) {
        self.transmissions.extend(report.transmissions());
        self.last = Some(report);
    }
}

/// Executor: processes effects from the state machine against real FFI/bridge.
pub struct CallManager {
    state: TrunkStateMachine,
    bridge: Arc<EventBridge>,
    /// The radio: sources and the channels open on them.
    radio: Arc<Radio>,
    /// Recorder-side tracing spans, opened at `EmitCallStart` and closed at
    /// conclude. Held (not entered) so each span's lifetime tracks the call's
    /// real duration; its context is serialized into the CallStart event so the
    /// downstream (discord) call span parents to it, and its stop_reason /
    /// recording_length / segments are recorded at close.
    call_spans: HashMap<CallId, tracing::Span>,
    /// What each call's channels recorded, until its end reports it.
    tallies: HashMap<CallId, Tally>,
    /// What each channel last carried, for the next call's span: a slot
    /// reused from another frequency can carry that into its next call.
    channel_last: HashMap<ChannelNum, ChannelUse>,
    /// Each source's clock correction (Hz), polled from the radio.
    freq_correction_gauge: Gauge<f64>,
    calls_started: Counter<u64>,
    calls_ended: Counter<u64>,
    /// Errored / spiked voice codewords reported by the op25 (P25) decoder,
    /// summed per call. High rates — especially on TDMA — indicate decode loss
    /// (the cause of P25 audio clipping), typically driven by frequency error.
    voice_frame_errors: Counter<u64>,
    voice_frame_spikes: Counter<u64>,
    /// op25 terminator (TDU) events per call and the audio produced after the
    /// terminator position (ms at 8 kHz). Tests whether end-of-call clipping
    /// coincides with audio landing behind the terminator tag.
    terminator_events: Counter<u64>,
    /// Seconds since the recorder last wrote audio, sampled when a call is
    /// concluded, by stop reason. A grant-timeout stop with audio still flowing
    /// (age near 0) cut a live transmission; idle-timeout stops sit at the
    /// call timeout by construction.
    call_stop_audio_age: Histogram<f64>,
    /// Same, but counting only blocks with non-silent audio: the decoder pads
    /// with zeros while nobody is talking, which keeps `call_stop_audio_age`
    /// near 0. A grant-timeout stop with a small *voice* age cut speech.
    call_stop_voice_age: Histogram<f64>,
    calls_cut: Counter<u64>,
    terminator_after_ms: Histogram<f64>,
    /// Per-call FLL frequency error (Hz) at recorder stop, bucketed by source.
    /// Distribution (not last value) so the per-dongle median = clock bias and
    /// the spread = decode margin, both trendable over time to watch TCXO drift.
    freq_error_hz: Histogram<f64>,
    /// 1 Hz samples of the recorder's channel power probe (dB, uncalibrated,
    /// relative to full scale) while a trunked call is carrying audio.
    signal_power_db: Histogram<f64>,
    /// Same probe while the call is still up but no audio has been written for
    /// a while: the carrier has usually dropped, so this approximates the
    /// channel noise floor (TDMA's idle slot keeps the carrier, so it reads high).
    noise_floor_db: Histogram<f64>,
    /// Shared-channelizer health, polled from the engine at the 1 Hz tick.
    chz_blocks: Counter<u64>,
    chz_rejected: Counter<u64>,
    chz_overruns: Counter<u64>,
    /// Per-second mean / worst block DSP time (µs), `stat` = "mean" | "max".
    chz_block_time_us: Histogram<f64>,
    chz_active: Gauge<u64>,
    /// Counters seen at the previous poll, by source, for delta computation.
    chz_prev: HashMap<i32, trunk_radio::ChannelizerCounters>,
    /// Host-owned SDR streams (trunk-sdr), polled at the 1 Hz tick.
    src_samples: Counter<u64>,
    src_dropped: Counter<u64>,
    src_drops: Counter<u64>,
    src_streaming: Gauge<u64>,
    /// Stream counters at the previous poll, by device serial.
    src_prev: HashMap<String, trunk_sdr::StreamStats>,
    p25_metrics: crate::p25_metrics::P25Metrics,
    fm_metrics: crate::fm_metrics::FmMetrics,
    /// Cumulative overruns per source as of the last poll.
    chz_overruns_total: HashMap<i32, u64>,
    /// Per-call channelizer context, for the call span (see `exec_conclude_call`).
    call_chz: HashMap<CallId, CallChz>,
    /// Channels that could not be opened, by reason.
    channel_open_failed: Counter<u64>,
    /// Calls that had to wait for a channel, and calls evicted from theirs,
    /// by system and talkgroup.
    channel_waits: Counter<u64>,
    channel_evictions: Counter<u64>,
    /// Channels held and claims waiting, of `max_channels`.
    channels_held: Gauge<u64>,
    channels_waiting: Gauge<u64>,
    control_channel_messages: Counter<u64>,
    control_channel_duplicates: Counter<u64>,
    active_calls_gauge: Gauge<u64>,
    conv_metrics: Arc<Mutex<ConventionalMetricsSnapshot>>,
}

/// Channelizer state captured for one call, recorded on its span at the end.
#[derive(Clone, Copy, Debug)]
struct CallChz {
    source: i32,
    /// Source's cumulative overruns when the call was first sampled.
    overruns_at_start: u64,
    /// Worst block DSP time (ns) seen on the source while the call ran.
    max_ns: u64,
}

/// Bucket edges for the block-time histogram (µs); the budget is ~8000.
const BLOCK_TIME_US_BOUNDARIES: [f64; 12] = [
    10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 6000.0, 8000.0, 16000.0,
];

/// Bucket edges for the recorder power histograms (dB re full scale, 3 dB steps).
const POWER_DB_BOUNDARIES: [f64; 27] = [
    -90.0, -87.0, -84.0, -81.0, -78.0, -75.0, -72.0, -69.0, -66.0, -63.0, -60.0, -57.0, -54.0,
    -51.0, -48.0, -45.0, -42.0, -39.0, -36.0, -33.0, -30.0, -27.0, -24.0, -21.0, -18.0, -12.0,
    -6.0,
];

impl CallManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: CallManagerConfig,
        bridge: Arc<EventBridge>,
        radio: Arc<Radio>,
        conv_metrics: Arc<Mutex<ConventionalMetricsSnapshot>>,
        clock: crate::clock::Clock,
    ) -> Self {
        // Pre-populate conventional channel metrics entries.
        if let Ok(mut snap) = conv_metrics.lock() {
            for policy in config.systems.values() {
                for ch in &policy.channels {
                    snap.channels.push(crate::ConventionalChannelMetrics {
                        system: policy.short_name.clone(),
                        talkgroup: i64::from(ch.talkgroup.0),
                        freq_hz: ch.freq.as_hz(),
                        state: "Disabled",
                        signal_power_db: 0.0,
                        noise_floor_db: 0.0,
                        detection_rssi_db: 0.0,
                        squelch_open_count: 0,
                        squelch_close_count: 0,
                        enable_count: 0,
                        disable_count: 0,
                        idle_timeout_count: 0,
                        recordings_completed: 0,
                        squelch_db: ch.squelch_db,
                    });
                }
            }
        }

        let meter = opentelemetry::global::meter("trunk-oxide");
        Self {
            state: TrunkStateMachine::with_clock(config, clock),
            bridge,
            radio,
            call_spans: HashMap::new(),
            channel_last: HashMap::new(),
            tallies: HashMap::new(),
            freq_correction_gauge: meter
                .f64_gauge("trunk.oxide.freq_correction_hz")
                .with_description("Live per-source software tune correction (Hz)")
                .build(),
            calls_started: meter
                .u64_counter("trunk.oxide.calls_started")
                .with_description("Total calls started")
                .build(),
            calls_ended: meter
                .u64_counter("trunk.oxide.calls_ended")
                .with_description("Total calls ended")
                .build(),
            voice_frame_errors: meter
                .u64_counter("trunk.oxide.voice_frame_errors")
                .with_description("op25-reported errored voice codewords, summed per call")
                .build(),
            voice_frame_spikes: meter
                .u64_counter("trunk.oxide.voice_frame_spikes")
                .with_description("op25-reported voice codeword spikes, summed per call")
                .build(),
            call_stop_audio_age: meter
                .f64_histogram("trunk.oxide.call_stop_audio_age_seconds")
                .with_description("Seconds since audio last flowed when a call was concluded")
                .with_unit("s")
                .with_boundaries(vec![0.1, 0.25, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 6.0])
                .build(),
            calls_cut: meter
                .u64_counter("trunk.oxide.calls_cut")
                .with_description(
                    "Calls stopped by a timeout or supersession while voice was still \
                     arriving (< 0.5 s before), by reason and whether the control \
                     channel had refreshed the call at all",
                )
                .build(),
            call_stop_voice_age: meter
                .f64_histogram("trunk.oxide.call_stop_voice_age_seconds")
                .with_description(
                    "Seconds since non-silent audio last flowed when a call was concluded",
                )
                .with_unit("s")
                .with_boundaries(vec![0.1, 0.25, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 6.0])
                .build(),
            terminator_events: meter
                .u64_counter("trunk.oxide.terminator_events")
                .with_description("op25 call-terminator (TDU) events, summed per call")
                .build(),
            terminator_after_ms: meter
                .f64_histogram("trunk.oxide.terminator_after_ms")
                .with_description("Per call: audio (ms) produced after the terminator position")
                .with_unit("ms")
                .with_boundaries(vec![
                    0.0, 10.0, 25.0, 50.0, 100.0, 200.0, 400.0, 800.0, 1600.0,
                ])
                .build(),
            freq_error_hz: meter
                .f64_histogram("trunk.oxide.freq_error_hz")
                .with_description("Per-call FLL frequency error (Hz) at recorder stop, by source")
                .with_boundaries(vec![
                    -1000.0, -500.0, -300.0, -200.0, -150.0, -100.0, -60.0, -30.0, 0.0, 30.0, 60.0,
                    100.0, 150.0, 200.0, 300.0, 500.0, 1000.0,
                ])
                .build(),
            signal_power_db: meter
                .f64_histogram("trunk.oxide.signal_power_db")
                .with_description("Recorder channel power (dB re full scale) while audio flows")
                .with_boundaries(POWER_DB_BOUNDARIES.to_vec())
                .build(),
            noise_floor_db: meter
                .f64_histogram("trunk.oxide.noise_floor_db")
                .with_description("Recorder channel power (dB re full scale) with no audio")
                .with_boundaries(POWER_DB_BOUNDARIES.to_vec())
                .build(),
            chz_blocks: meter
                .u64_counter("trunk.oxide.channelizer.blocks")
                .with_description("Blocks processed by the shared channelizer")
                .build(),
            chz_rejected: meter
                .u64_counter("trunk.oxide.channelizer.rejected")
                .with_description("Channel assignments rejected (outside the source passband)")
                .build(),
            chz_overruns: meter
                .u64_counter("trunk.oxide.channelizer.overruns")
                .with_description(
                    "Blocks whose DSP took longer than the block's real-time duration",
                )
                .build(),
            chz_block_time_us: meter
                .f64_histogram("trunk.oxide.channelizer.block_time_us")
                .with_description("Shared-channelizer block DSP time per second (mean and max)")
                .with_unit("us")
                .with_boundaries(BLOCK_TIME_US_BOUNDARIES.to_vec())
                .build(),
            chz_active: meter
                .u64_gauge("trunk.oxide.channelizer.active_channels")
                .with_description("Channels currently enabled on the shared channelizer")
                .build(),
            chz_prev: HashMap::new(),
            src_samples: meter
                .u64_counter("trunk.oxide.source.samples")
                .with_description(
                    "Samples a host-owned SDR stream delivered to the channelizer's queue",
                )
                .build(),
            src_dropped: meter
                .u64_counter("trunk.oxide.source.dropped_samples")
                .with_description(
                    "Samples dropped at the SDR stream because the channelizer fell behind",
                )
                .build(),
            src_drops: meter
                .u64_counter("trunk.oxide.source.drops")
                .with_description("Times an SDR stream dropped a block")
                .build(),
            src_streaming: meter
                .u64_gauge("trunk.oxide.source.streaming")
                .with_description("1 while the SDR stream's reader is delivering")
                .build(),
            src_prev: HashMap::new(),
            p25_metrics: crate::p25_metrics::P25Metrics::new(&meter),
            fm_metrics: crate::fm_metrics::FmMetrics::new(&meter),
            chz_overruns_total: HashMap::new(),
            call_chz: HashMap::new(),
            channel_open_failed: meter
                .u64_counter("trunk.oxide.channel_open_failed")
                .with_description("Channels that could not be opened, by reason")
                .build(),
            channel_waits: meter
                .u64_counter("trunk.oxide.channel_waits")
                .with_description(
                    "Calls that started without a channel: every one was held by calls \
                     that outrank them",
                )
                .build(),
            channel_evictions: meter
                .u64_counter("trunk.oxide.channel_evictions")
                .with_description("Calls whose channel a call that outranks them took")
                .build(),
            channels_held: meter
                .u64_gauge("trunk.oxide.channels_held")
                .with_description("Channels open (control channels included)")
                .build(),
            channels_waiting: meter
                .u64_gauge("trunk.oxide.channels_waiting")
                .with_description("Claims on a channel waiting for one")
                .build(),
            control_channel_messages: meter
                .u64_counter("trunk.oxide.control_channel_messages")
                .with_description("Total control channel messages decoded")
                .build(),
            control_channel_duplicates: meter
                .u64_counter("trunk.oxide.control_channel_duplicates")
                .with_description(
                    "Control channel messages dropped as copies of one admitted < 500 ms \
                     earlier (the same packet through several decoders)",
                )
                .build(),
            active_calls_gauge: meter
                .u64_gauge("trunk.oxide.active_calls")
                .with_description("Number of active calls")
                .build(),
            conv_metrics,
        }
    }

    /// Update a conventional channel's metrics entry by talkgroup + freq.
    fn update_conv_metric(
        &self,
        talkgroup: Talkgroup,
        freq: Freq,
        f: impl FnOnce(&mut crate::ConventionalChannelMetrics),
    ) {
        if let Ok(mut snap) = self.conv_metrics.lock()
            && let Some(ch) = snap.channels.iter_mut().find(|c| {
                c.talkgroup == i64::from(talkgroup) && (c.freq_hz - freq.as_hz()).abs() < 1.0
            })
        {
            f(ch);
        }
    }

    /// Sync conventional channel states from call entries into the metrics snapshot.
    fn sync_conv_states(&self) {
        if let Ok(mut snap) = self.conv_metrics.lock() {
            for view in self.state.iter_calls() {
                if !view.is_conventional() {
                    continue;
                }
                if let Some(ch) = snap.channels.iter_mut().find(|c| {
                    c.talkgroup == i64::from(view.talkgroup())
                        && (c.freq_hz - view.freq().as_hz()).abs() < 1.0
                }) {
                    ch.state = view.state().as_str();
                }
            }
        }
    }

    // ── Audio metadata management ──

    fn register_audio_meta(&self, call_id: CallId) {
        if let Some(view) = self.state.get_call_view(call_id) {
            #[allow(clippy::cast_possible_truncation)]
            let sample_rate = view.audio_rate().map_or(8_000, |r| r as i32);
            self.bridge.register_audio_meta(
                call_id,
                AudioMeta {
                    talkgroup: view.talkgroup().into(),
                    short_name: Arc::from(view.short_name()),
                    freq: view.freq().as_hz(),
                    sample_rate,
                },
            );
        }
    }

    // ── Effect processing ──

    fn process_effects(&mut self, effects: Effects) {
        for effect in effects {
            match effect {
                Effect::CreateCall { call_id } => self.register_audio_meta(call_id),
                Effect::Open { call_id } => self.exec_open(call_id),
                Effect::Close { call_id } => {
                    if let Some(report) = self.radio.close(call_id) {
                        self.bridge.setup_recorder(report.num, report.freq_error);
                        self.channel_ended(call_id, report);
                    }
                }
                Effect::Rotate { call_id } => {
                    if let Some(report) = self.radio.rotate(call_id) {
                        self.channel_ended(call_id, report);
                    }
                }
                Effect::SetFreq { call_id, freq } => {
                    self.bridge.update_audio_meta_freq(call_id, freq.as_hz());
                }
                Effect::SetSource { call_id, source_id } => {
                    self.radio.set_source(call_id, source_id);
                }
                Effect::EmitCallStart { call_id } => {
                    // Open the recorder-side call span here (held in `call_spans`,
                    // closed at conclude) and serialize its context into the
                    // CallStart event so discord parents its call span to ours.
                    let started = self.state.get_call_view(call_id).map(|view| {
                        let labels = [KeyValue::new("short_name", view.short_name().to_string())];
                        self.calls_started.add(1, &labels);
                        if view.state() == CallState::Waiting {
                            self.channel_waits.add(1, &Self::talkgroup_labels(&view));
                        }
                        let span = tracing::info_span!(
                            "call",
                            short_name = %view.short_name(),
                            talkgroup = view.talkgroup().0,
                            sys_num = view.sys_num(),
                            freq = %view.freq(),
                            tdma = view.phase2_tdma(),
                            slot = view.tdma_slot(),
                            call_num = view.call_num(),
                            is_conventional = view.is_conventional(),
                            // Recorded at conclude (see exec_conclude_call).
                            stop_reason = tracing::field::Empty,
                            channelizer_overruns = tracing::field::Empty,
                            channelizer_max_block_us = tracing::field::Empty,
                            recording_length = tracing::field::Empty,
                            segments = tracing::field::Empty,
                            elapsed = tracing::field::Empty,
                            voice_errors = tracing::field::Empty,
                            voice_spikes = tracing::field::Empty,
                            term_events = tracing::field::Empty,
                            stop_audio_age_s = tracing::field::Empty,
                            stop_voice_age_s = tracing::field::Empty,
                            // Stopped by a timeout with voice still arriving.
                            cut_mid_speech = tracing::field::Empty,
                            // Control channel grants/updates that refreshed the
                            // call, and how long since the last one at stop.
                            refreshes = tracing::field::Empty,
                            since_update_s = tracing::field::Empty,
                            after_term_ms = tracing::field::Empty,
                            voice_samples = tracing::field::Empty,
                            // The channel, and what it carried before this.
                            channel = tracing::field::Empty,
                            channel_prev_talkgroup = tracing::field::Empty,
                            channel_prev_freq = tracing::field::Empty,
                            channel_prev_slot = tracing::field::Empty,
                            channel_idle_s = tracing::field::Empty,
                            // Encryption: the flag at conclude; the voice
                            // channel's ESS report (ALGID/KeyID) and how long
                            // after the call started it came.
                            encrypted = tracing::field::Empty,
                            alg_id = tracing::field::Empty,
                            key_id = tracing::field::Empty,
                            crypto_after_ms = tracing::field::Empty,
                        );
                        let traceparent = traceparent_of(&span);
                        let info = self.build_call_info_from_view(&view);
                        (span, traceparent, info)
                    });
                    if let Some((span, traceparent, info)) = started {
                        self.bridge.call_start(info, traceparent);
                        self.call_spans.insert(call_id, span);
                        self.record_channel(call_id);
                    }
                }
                Effect::EmitCallEnd { call_id, reason } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        let mut attrs = vec![
                            KeyValue::new("short_name", view.short_name().to_string()),
                            KeyValue::new("reason", reason.as_str()),
                            KeyValue::new("encrypted", view.encrypted()),
                        ];
                        // Rejected calls (never recorded) still count here; label why.
                        if let Some(r) = view.reject_reason() {
                            attrs.push(KeyValue::new("reject_reason", r.as_str()));
                        }
                        // Only encrypted calls carry an ALGID, keeping the extra
                        // series bounded to the handful of P25 algorithms in use.
                        // A call rejected at the grant has none of its own, so fall
                        // back to the last ALGID seen on its talkgroup.
                        if let Some(alg) = self.state.alg_id_for(call_id) {
                            attrs.push(KeyValue::new("algid", format!("0x{alg:02x}")));
                        }
                        self.calls_ended.add(1, &attrs);
                        if reason == CallEndReason::Evicted {
                            self.channel_evictions
                                .add(1, &Self::talkgroup_labels(&view));
                        }
                    }
                    self.exec_conclude_call(call_id, reason);
                }
                Effect::NextTransmission { call_id, source } => {
                    let conventional = self
                        .state
                        .get_call_view(call_id)
                        .is_some_and(|v| v.is_conventional());
                    self.state.on_next_transmission(call_id, source);
                    if conventional {
                        // A conventional transmission's recordings were
                        // reported with its end; the next starts afresh.
                        self.tallies.remove(&call_id);
                    }
                }
                Effect::EmitCallsActive => {
                    let infos = self
                        .state
                        .iter_calls()
                        .map(|v| self.build_call_info_from_view(&v))
                        .collect();
                    self.bridge.calls_active(infos);
                }
                Effect::EmitSignal { call_id, source_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        let info = self.build_call_info_from_view(&view);
                        self.bridge.signal(i64::from(source_id), info);
                    }
                }
                Effect::EmitUnitEvent {
                    sys_num,
                    type_,
                    source_id,
                    talkgroup,
                } => {
                    let info = self.get_system_info(sys_num);
                    self.bridge.unit_event(
                        info,
                        type_,
                        i64::from(source_id),
                        talkgroup.map_or(0, i64::from),
                    );
                }
                Effect::EmitSetupSystem { sys_num } => {
                    let info = self.get_system_info(sys_num);
                    self.bridge.setup_system(info);
                }
                Effect::RemoveCall { call_id } => {
                    self.tallies.remove(&call_id);
                    self.state.remove_call(call_id);
                    // Unregister audio meta last: the write lock waits for any
                    // in-flight audio_stream() read lock, ensuring the final
                    // audio send completes before we remove the metadata.
                    self.bridge.unregister_audio_meta(call_id);
                }
            }
        }
    }

    /// Labels for a per-talkgroup channel metric.
    fn talkgroup_labels(view: &CallView<'_>) -> [KeyValue; 2] {
        [
            KeyValue::new("short_name", view.short_name().to_string()),
            KeyValue::new("talkgroup", i64::from(view.talkgroup())),
        ]
    }

    // ── Effect executors ──

    /// Open the channel the state machine gave `id`, and tell the state
    /// machine how it went.
    fn exec_open(&mut self, id: CallId) {
        let Some((recipe, call)) = self.state.opening(id) else {
            tracing::error!(?id, "opening a channel for nothing the state machine knows");
            return;
        };
        let opened = recipe.open(&self.radio, &call);
        if let Err(e) = &opened {
            self.channel_open_failed.add(
                1,
                &[
                    KeyValue::new("short_name", self.short_name_of(call.sys_num)),
                    KeyValue::new("kind", recipe.kind()),
                    KeyValue::new("reason", crate::call_state::RejectReason::from(e).as_str()),
                ],
            );
        }
        let followup = self.state.on_opened(id, opened);
        self.record_channel(id);
        self.process_effects(followup);
    }

    /// Records the call's channel, and what it carried last, on its span.
    fn record_channel(&self, call_id: CallId) {
        let (Some(span), Some(view)) = (
            self.call_spans.get(&call_id),
            self.state.get_call_view(call_id),
        ) else {
            return;
        };
        let Some(num) = view.channel() else {
            return;
        };
        span.record("channel", num.0);
        if let Some(last) = self.channel_last.get(&num) {
            span.record("channel_prev_talkgroup", last.talkgroup.0);
            span.record("channel_prev_freq", tracing::field::display(last.freq));
            span.record("channel_prev_slot", last.slot);
            span.record(
                "channel_idle_s",
                self.state.clock().since(last.closed).as_secs_f64(),
            );
        }
    }

    /// A call's channel closed, or its recording rotated: note what it
    /// carried, take its power reading, and keep what it recorded for the
    /// call's next end.
    fn channel_ended(&mut self, call_id: CallId, report: ChannelReport) {
        let Some(view) = self.state.get_call_view(call_id) else {
            return; // a control channel
        };
        let was_idle = view.state() == CallState::Idle;
        self.channel_last.insert(
            report.num,
            ChannelUse {
                talkgroup: view.talkgroup(),
                freq: view.freq(),
                slot: view.tdma_slot(),
                closed: self.state.clock().now(),
            },
        );
        tracing::info!(
            short_name = %view.short_name(),
            channel = %report.num,
            talkgroup = view.talkgroup().0,
            freq = %view.freq(),
            tdma = view.phase2_tdma(),
            slot = view.tdma_slot(),
            freq_error = format_args!("{:+.0} Hz", report.freq_error),
            preferred_ppm = format_args!("{:.2}", report.preferred_ppm),
            "stopping recorder"
        );
        self.state
            .feed_channel_power(call_id, report.power_db, was_idle);
        self.tallies.entry(call_id).or_default().add(report);
    }

    #[allow(clippy::too_many_lines)]
    fn exec_conclude_call(&mut self, call_id: CallId, reason: CallEndReason) {
        // Take ownership of the call span so it closes (and is exported) on every
        // exit path of this function; record the stop reason up front.
        let span = self.call_spans.remove(&call_id);
        // A boundary ends a transmission on a channel that runs on: what it
        // recorded so far is live. Any other end follows its channel's close
        // (or rotation), which the tally kept.
        let boundary = reason.is_boundary();
        let live = if boundary {
            self.radio.reading(call_id)
        } else {
            None
        };
        let mut tally = self.tallies.remove(&call_id).unwrap_or_default();
        let last = live.as_ref().or(tally.last.as_ref());
        let recording = last.and_then(|r| r.recording.as_ref());
        let audio_age = recording.map(trunk_radio::Accounting::since_write_s);
        let voice_age = recording.map(trunk_radio::Accounting::since_voice_s);
        let freq_error = last.map_or(0, |r| r.freq_error);
        let channel = last.map(|r| r.num);
        let mut transmissions = std::mem::take(&mut tally.transmissions);
        if let Some(live) = &live {
            transmissions.extend(live.transmissions());
        }
        let counters = VoiceCounters::from_transmissions(&transmissions);
        let delta = counters.since(tally.reported);

        let chz = if boundary {
            self.call_chz.get(&call_id).copied()
        } else {
            self.call_chz.remove(&call_id)
        };

        let Some(view) = self.state.get_call_view(call_id) else {
            return;
        };
        let labels = [
            KeyValue::new("reason", reason.as_str()),
            KeyValue::new("short_name", view.short_name().to_string()),
            KeyValue::new("tdma", view.phase2_tdma()),
            KeyValue::new("is_conventional", view.is_conventional()),
        ];
        if let (Some(age), Some(voice)) = (audio_age, voice_age) {
            self.call_stop_audio_age.record(age, &labels);
            self.call_stop_voice_age.record(voice, &labels);
            if reason.can_cut_speech() && voice < CUT_VOICE_AGE_S {
                let mut labels = labels.to_vec();
                labels.push(KeyValue::new("refreshed", view.refreshes() > 0));
                self.calls_cut.add(1, &labels);
            }
        }
        let recorded = last.is_some();
        let final_freq = view.freq();
        let is_conventional = view.is_conventional();
        let segments = self.build_proto_segments_final(call_id, final_freq, channel);
        let recording_length = view
            .recording_duration(self.state.clock().now())
            .as_secs_f64();
        let elapsed = self.state.clock().since(view.start_time()).as_secs();
        if let Some(s) = &span {
            s.record("refreshes", view.refreshes());
            s.record("encrypted", view.encrypted());
            s.record(
                "since_update_s",
                view.since_update(self.state.clock().now()).as_secs_f64(),
            );
            s.record(
                "cut_mid_speech",
                reason.can_cut_speech() && voice_age.is_some_and(|v| v < CUT_VOICE_AGE_S),
            );
            if let Some(age) = audio_age {
                s.record("stop_audio_age_s", age);
            }
            if let Some(age) = voice_age {
                s.record("stop_voice_age_s", age);
            }
            s.record("stop_reason", reason.as_str());
            if let Some(c) = chz {
                let now = self.chz_overruns_total.get(&c.source).copied().unwrap_or(0);
                s.record(
                    "channelizer_overruns",
                    now.saturating_sub(c.overruns_at_start),
                );
                s.record("channelizer_max_block_us", c.max_ns / 1000);
            }
            if recorded {
                s.record("recording_length", recording_length);
                s.record(
                    "segments",
                    u64::try_from(segments.len()).unwrap_or(u64::MAX),
                );
                s.record("elapsed", elapsed);
                s.record("voice_errors", delta.errors);
                s.record("voice_spikes", delta.spikes);
                s.record("term_events", delta.term_events);
                s.record("after_term_ms", delta.after_term_samples / 8);
                s.record("voice_samples", delta.samples);
            }
        }

        if !recorded {
            // Never on a channel (rejected, or still waiting): it ends all
            // the same, so the wire can finalize it.
            if view.state() == CallState::Recording {
                tracing::warn!(
                    talkgroup = view.talkgroup().0,
                    "concluding a recording call with no channel"
                );
            }
            let data = self.call_data(&view, segments, is_conventional);
            self.bridge.call_end(data, reason.to_proto());
            return;
        }

        if !boundary {
            tracing::info!(
                short_name = %view.short_name(),
                call_num = view.call_num(),
                talkgroup = view.talkgroup().0,
                elapsed,
                recording_length,
                segments = segments.len(),
                "concluding recorded call"
            );
            if is_conventional {
                self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                    m.recordings_completed += 1;
                });
            }
        }
        let source = i64::from(last.map_or(-1, |r| r.source));
        let voice_attrs = [
            KeyValue::new("short_name", view.short_name().to_string()),
            KeyValue::new("tdma", view.phase2_tdma()),
            KeyValue::new("source", source),
        ];
        self.voice_frame_errors
            .add(u64::try_from(delta.errors).unwrap_or(0), &voice_attrs);
        self.voice_frame_spikes
            .add(u64::try_from(delta.spikes).unwrap_or(0), &voice_attrs);
        self.terminator_events
            .add(u64::try_from(delta.term_events).unwrap_or(0), &voice_attrs);
        if delta.term_events > 0 {
            #[allow(clippy::cast_precision_loss)]
            self.terminator_after_ms
                .record(delta.after_term_samples as f64 / 8.0, &voice_attrs);
        }

        let mut data = self.call_data(&view, segments, is_conventional && !boundary);
        data.channel = channel.map(|c| c.0);
        data.length = recording_length;
        data.error_count = delta.errors;
        data.spike_count = delta.spikes;
        if !boundary {
            data.freq_error = freq_error;
            data.transmissions = transmissions.iter().map(proto_transmission).collect();
        }
        self.bridge.call_end(data, reason.to_proto());

        if boundary {
            // The call goes on: its later boundaries report from here.
            tally.reported = counters;
            self.tallies.insert(call_id, tally);
            return;
        }

        // Per-source clock-error distribution: median = TCXO bias,
        // spread = decode margin; both trendable to watch drift.
        self.freq_error_hz.record(
            f64::from(freq_error),
            &[
                KeyValue::new("source", source),
                KeyValue::new("short_name", view.short_name().to_string()),
            ],
        );
    }

    // ── Public API (delegates to state machine + processes effects) ──

    fn short_name_of(&self, sys_num: i32) -> String {
        self.state
            .config
            .systems
            .get(&sys_num)
            .map_or_else(String::new, |p| p.short_name.clone())
    }

    pub const fn clock(&self) -> &crate::clock::Clock {
        self.state.clock()
    }

    /// `msg` was dropped as a duplicate before reaching the state machine.
    pub fn note_duplicate(&self, msg: &TrunkMessage) {
        self.control_channel_duplicates.add(
            1,
            &[KeyValue::new("short_name", self.short_name_of(msg.sys_num))],
        );
    }

    pub fn handle_message(&mut self, msg: &TrunkMessage) {
        if msg.kind != Trunking::Invalid {
            self.control_channel_messages.add(
                1,
                &[KeyValue::new("short_name", self.short_name_of(msg.sys_num))],
            );
        }

        // The site and the system are noted once each; repeats go no further.
        let first = match msg.kind {
            Trunking::Site { rfss, site, .. } => self.radio.update_sysid(msg.sys_num, rfss, site),
            Trunking::Network { wacn, sys_id, .. } => {
                self.radio.update_status(msg.sys_num, sys_id, wacn, msg.nac)
            }
            _ => true,
        };
        if !first {
            return;
        }

        if let Trunking::Grant(v) | Trunking::UnitGrant(v) = msg.kind {
            self.note_ptt_ends(msg.sys_num, Talkgroup(v.talkgroup));
        }
        let effects = self.state.on_message(msg);
        self.process_effects(effects);
    }

    /// Before a grant reaches the state machine: tell it which of the
    /// granted talkgroup's recording calls had their voice channel end a
    /// transmission (a terminator since the current transmission began), so
    /// the grant can be read as a new key-up even from the same handset.
    fn note_ptt_ends(&mut self, sys_num: i32, talkgroup: Talkgroup) {
        let ended: Vec<CallId> = self
            .state
            .iter_calls()
            .filter(|v| {
                v.sys_num() == sys_num
                    && v.talkgroup() == talkgroup
                    && v.state() == CallState::Recording
            })
            .filter_map(|v| {
                let tally = self.tallies.get(&v.id());
                let mut transmissions = tally.map_or_else(Vec::new, |t| t.transmissions.clone());
                transmissions.extend(
                    self.radio
                        .reading(v.id())
                        .map_or_else(Vec::new, |r| r.transmissions()),
                );
                let now = VoiceCounters::from_transmissions(&transmissions);
                let then = tally.map_or_else(VoiceCounters::default, |t| t.reported);
                (now.term_events > then.term_events).then_some(v.id())
            })
            .collect();
        for id in ended {
            self.state.on_ptt_ended(id);
        }
    }

    /// Export every host-owned SDR stream's counters since the last tick.
    fn poll_sources(&mut self) {
        let streams = trunk_sdr::streams();
        self.src_prev
            .retain(|k, _| streams.iter().any(|(l, _)| l == k));
        for (label, now) in streams {
            let attrs = [KeyValue::new("device", label.clone())];
            let was = self.src_prev.insert(label, now).unwrap_or_default();
            self.src_samples
                .add(now.samples.saturating_sub(was.samples), &attrs);
            self.src_dropped
                .add(now.dropped.saturating_sub(was.dropped), &attrs);
            self.src_drops
                .add(now.drops.saturating_sub(was.drops), &attrs);
            self.src_streaming.record(u64::from(now.running), &attrs);
        }
    }

    /// Read every shared channelizer's counters (called at the 1 Hz tick),
    /// export the deltas as metrics, and fold the interval's worst block into
    /// each in-flight call on that source.
    fn poll_channelizers(&mut self) {
        let stats = trunk_radio::channelizer_stats();
        let mut worst: HashMap<i32, u64> = HashMap::new();
        for st in stats {
            let attrs = [KeyValue::new("source", i64::from(st.source))];
            let prev = self
                .chz_prev
                .insert(st.source, st.counters)
                .unwrap_or_default();
            let blocks = st.counters.blocks.saturating_sub(prev.blocks);
            self.chz_blocks.add(blocks, &attrs);
            self.chz_rejected
                .add(st.counters.rejected.saturating_sub(prev.rejected), &attrs);
            self.chz_overruns
                .add(st.counters.overruns.saturating_sub(prev.overruns), &attrs);
            self.chz_active.record(st.counters.active, &attrs);
            if blocks > 0 {
                let busy = st.counters.busy_ns.saturating_sub(prev.busy_ns);
                #[allow(clippy::cast_precision_loss)]
                let (mean_us, max_us) = (
                    busy as f64 / blocks as f64 / 1e3,
                    st.counters.max_ns as f64 / 1e3,
                );
                let stat = |v: &'static str| {
                    [
                        KeyValue::new("source", i64::from(st.source)),
                        KeyValue::new("stat", v),
                    ]
                };
                self.chz_block_time_us.record(mean_us, &stat("mean"));
                self.chz_block_time_us.record(max_us, &stat("max"));
            }
            self.chz_overruns_total
                .insert(st.source, st.counters.overruns);
            worst.insert(st.source, st.counters.max_ns);
        }

        // Attribute the channelizer's worst block to calls running on it.
        if worst.is_empty() {
            return;
        }
        let running: Vec<_> = self
            .state
            .iter_calls()
            .filter(|v| !v.is_conventional() && v.state() == CallState::Recording)
            .filter_map(|v| {
                let src = self.radio.reading(v.id())?.source;
                worst.contains_key(&src).then_some((v.id(), src))
            })
            .collect();
        for (id, src) in running {
            let max = worst.get(&src).copied().unwrap_or(0);
            let total = self.chz_overruns_total.get(&src).copied().unwrap_or(0);
            let e = self.call_chz.entry(id).or_insert(CallChz {
                source: src,
                overruns_at_start: total,
                max_ns: 0,
            });
            e.max_ns = e.max_ns.max(max);
        }
    }

    pub fn tick(&mut self) {
        self.poll_sources();
        self.poll_channelizers();
        self.p25_metrics.poll();
        self.fm_metrics.poll();
        // First, check how long each recording call's channel has gone
        // without audio: only the radio knows.
        let timeout = self.state.config.call_timeout;
        let mut timed_out_ids = Vec::new();

        for view in self.state.iter_calls() {
            if view.is_conventional() || view.state() != CallState::Recording {
                continue;
            }
            if let Some(reading) = self.radio.reading(view.id()) {
                let since_write = reading
                    .recording
                    .as_ref()
                    .map_or(f64::INFINITY, trunk_radio::Accounting::since_write_s);
                if since_write > timeout {
                    timed_out_ids.push(view.id());
                }

                // Channel power at 1 Hz (the tick rate), split by whether audio
                // is flowing. The window between the two (0.5-1 s) is skipped
                // so a sample is clearly one or the other.
                let hist = if since_write < 0.5 {
                    Some(&self.signal_power_db)
                } else if since_write >= 1.0 {
                    Some(&self.noise_floor_db)
                } else {
                    None
                };
                if let Some(hist) = hist
                    && let Some(pwr_db) = reading.power_db
                {
                    hist.record(
                        pwr_db,
                        &[
                            KeyValue::new("short_name", view.short_name().to_string()),
                            KeyValue::new("tdma", view.phase2_tdma()),
                            KeyValue::new("source", i64::from(reading.source)),
                        ],
                    );
                }
            }
        }

        // Expire timed-out-by-write calls through the effect system
        for call_id in timed_out_ids {
            let effects = self.state.expire_call(call_id);
            self.process_effects(effects);
        }

        // Now run the state machine tick (handles update timeouts + conventional)
        let effects = self.state.tick();
        self.process_effects(effects);

        self.active_calls_gauge
            .record(self.state.call_count() as u64, &[]);
        self.channels_held
            .record(self.state.channels_held() as u64, &[]);
        for (source, hz) in self.radio.corrections() {
            self.freq_correction_gauge
                .record(hz, &[KeyValue::new("source", i64::from(source))]);
        }
        self.channels_waiting
            .record(self.state.waiting() as u64, &[]);
        self.sync_conv_states();
    }

    /// Open the control channels: the first allocation.
    pub fn start(&mut self) {
        let effects = self.state.start();
        self.process_effects(effects);
    }

    /// A conventional channel's call: it claims its channel at once, or
    /// (with signal detection) once the detector finds something there.
    pub fn add_conventional_call(&mut self, channel: &ConventionalChannel, sys_num: i32) {
        let policy = self
            .state
            .config
            .systems
            .get(&sys_num)
            .cloned()
            .unwrap_or_else(|| crate::call_state::DEFAULT_POLICY.clone());
        if channel.signal_detection
            && let Err(e) = self.radio.detect_signals(channel.freq.as_hz())
        {
            tracing::error!(
                sys_num, freq = %channel.freq, talkgroup = channel.talkgroup.0, error = %e,
                "cannot watch the conventional channel for signals"
            );
        }
        let (call_id, effects) = self.state.add_conventional_call(channel, sys_num, &policy);
        self.register_audio_meta(call_id);
        self.process_effects(effects);
    }

    /// A conventional channel's transmission detector opened or closed.
    pub fn handle_squelch_event(
        &mut self,
        call_id: CallId,
        squelch: Squelch,
        power_db: Option<f64>,
    ) {
        // Record signal quality metrics before mutating state
        if let Some(view) = self
            .state
            .get_call_view(call_id)
            .filter(CallView::is_conventional)
        {
            let tg = view.talkgroup();
            let freq = view.freq();
            let db = power_db.unwrap_or(f64::NAN);
            match squelch {
                Squelch::Open => self.update_conv_metric(tg, freq, |m| {
                    m.signal_power_db = db;
                    m.squelch_open_count += 1;
                }),
                Squelch::Close(reason) => {
                    tracing::debug!(talkgroup = tg.0, freq = %freq, ?reason, ?power_db, "squelch closed");
                    self.update_conv_metric(tg, freq, |m| {
                        m.noise_floor_db = db;
                        m.squelch_close_count += 1;
                    });
                }
            }
        }

        let effects = self.state.on_squelch(call_id, squelch, power_db);
        self.process_effects(effects);
    }

    /// An in-band P25 source (unit) change decoded by op25 on the voice channel,
    /// keyed by the recorder's `call_id`. Cuts a per-speaker transmission
    /// boundary (see [`TrunkStateMachine::on_source_change`]).
    pub fn handle_source_change(&mut self, call_id: CallId, unit: u32) {
        let effects = self.state.on_source_change(call_id, unit);
        self.process_effects(effects);
    }

    /// The voice channel confirmed a call's encryption (ESS `ALGID` /
    /// `KeyID`). Stored on the call so the `calls_ended` metric can facet by
    /// algorithm.
    pub fn handle_crypto(&mut self, call_id: CallId, encryption: trunk_radio::Encryption) {
        let trunk_radio::Encryption {
            algid: alg_id,
            keyid: key_id,
        } = encryption;
        if let (Some(span), Some(view)) = (
            self.call_spans.get(&call_id),
            self.state.get_call_view(call_id),
        ) {
            let after_ms = self.state.clock().since(view.start_time()).as_millis();
            span.record("alg_id", format_args!("0x{alg_id:02x}"));
            span.record("key_id", key_id);
            span.record(
                "crypto_after_ms",
                u64::try_from(after_ms).unwrap_or(u64::MAX),
            );
            span.in_scope(|| {
                tracing::info!(
                    alg_id = format_args!("0x{alg_id:02x}"),
                    key_id,
                    freq = %view.freq(),
                    slot = view.tdma_slot(),
                    "voice channel reports encryption"
                );
            });
        }
        self.state.on_crypto(call_id, encryption);
    }

    pub fn handle_signals_detected(&mut self, source_center_freq: f64, signals: &[DetectedSignal]) {
        for signal in signals {
            let abs_freq = source_center_freq + signal.center_freq;
            tracing::info!(
                source_center_mhz = source_center_freq / 1_000_000.0,
                signal_offset_hz = signal.center_freq,
                abs_freq_mhz = abs_freq / 1_000_000.0,
                rssi = signal.max_rssi,
                threshold = signal.threshold,
                "signal detected"
            );

            for view in self.state.iter_calls() {
                if view.is_conventional()
                    && view.signal_detection()
                    && (view.freq().as_hz() - abs_freq).abs() < 30_000.0
                {
                    self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                        m.detection_rssi_db = f64::from(signal.max_rssi);
                    });
                }
            }
        }
        let effects = self.state.on_signals_detected(source_center_freq, signals);
        self.process_effects(effects);
    }

    pub fn clear_stale_patches(&mut self) {
        self.state.clear_stale_patches();
    }

    pub fn print_active_patches(&self) {
        self.state.print_active_patches();
    }

    pub fn conclude_all(&mut self) {
        let effects = self.state.conclude_all();
        self.process_effects(effects);
    }

    pub(crate) fn get_system_info(&self, sys_num: i32) -> proto::SystemInfo {
        let policy = self
            .state
            .config
            .systems
            .get(&sys_num)
            .unwrap_or(&crate::call_state::DEFAULT_POLICY);
        let id = self.state.system_identities.get(&sys_num);
        proto::SystemInfo {
            system_type: proto_system_type(policy.system_type) as i32,
            short_name: policy.short_name.clone(),
            sys_id: id.map_or(0, |id| id.sys_id),
            wacn: id.map_or(0, |id| id.wacn),
            nac: id.map_or(0, |id| id.nac),
            rfss: id.map_or(0, |id| id.rfss),
            site_id: id.map_or(0, |id| id.site_id),
            bandplan: policy.bandplan.clone(),
            analog_levels: policy.analog_levels,
            digital_levels: policy.digital_levels,
            squelch_db: policy.squelch_db,
            conversation_mode: policy.conversation_mode,
        }
    }

    pub fn print_status(&self) {
        tracing::info!(count = self.state.call_count(), "Active Calls");
        for view in self.state.iter_calls() {
            let elapsed = view.start_time();
            let elapsed = self.state.clock().since(elapsed).as_secs() as i64;
            let state = view.state().to_string();

            tracing::info!(
                system = %view.short_name(),
                call_num = view.call_num(),
                talkgroup = view.talkgroup().0,
                freq = %view.freq(),
                elapsed,
                state,
                channel = view.channel().map(|c| c.0),
                "call status"
            );
        }
        tracing::info!(
            held = self.state.channels_held(),
            max = self.state.config.max_channels,
            waiting = self.state.waiting(),
            "channels"
        );
    }

    pub fn print_channel_status(&self) {
        let channels = self.radio.channels();
        let mut current_source = -1i32;
        for c in &channels {
            if c.source_num != current_source {
                current_source = c.source_num;
                tracing::info!(
                    source = c.source_num,
                    freq = %Freq::from_hz(c.source_center),
                    device = %c.source_device,
                    "source"
                );
            }
            let control = c.call.and_then(|id| self.state.control(id));
            tracing::info!(
                channel = c.num.0,
                kind = c.kind,
                call = c.call.map(|id| id.0),
                control_channel = control.map(|cc| tracing::field::display(cc.freq)),
                "  channel"
            );
        }
    }

    // ── The wire ──

    fn build_call_info_from_view(&self, view: &CallView<'_>) -> proto::CallInfo {
        proto::CallInfo {
            talkgroup: view.talkgroup().into(),
            call_num: view.call_num(),
            freq: view.freq().as_hz(),
            signal: view.signal(),
            noise: view.noise(),
            encrypted: view.encrypted(),
            emergency: view.emergency(),
            kind: view.kind().to_owned(),
            is_conventional: view.is_conventional(),
            phase2_tdma: view.phase2_tdma(),
            tdma_slot: view.tdma_slot(),
            priority: i32::from(view.service_priority()),
            mode: view.mode(),
            duplex: view.duplex(),
            conversation_mode: view.trunked_fields().is_some_and(|t| t.conversation_mode),
            current_source_id: view.current_source().map(i64::from),
            start_time: Some(self.timestamp(view.start_time())),
            elapsed: Some(prost_duration(self.state.clock().since(view.start_time()))),
            short_name: view.short_name().to_owned(),
            call_state: view.state().to_proto(),
            ..Default::default()
        }
    }

    /// The call's end as the wire says it; the caller fills in what it
    /// recorded.
    fn call_data(
        &self,
        view: &CallView<'_>,
        segments: Vec<proto::CallSegment>,
        is_conventional: bool,
    ) -> proto::CallData {
        let unix = |t| crate::call_state::system_time_to_unix(t);
        proto::CallData {
            talkgroup: view.talkgroup().into(),
            patched_talkgroups: self
                .state
                .patch_tables
                .get(&view.sys_num())
                .map(|pt| {
                    pt.get_patch(view.talkgroup())
                        .into_iter()
                        .map(|t| u64::from(t.0))
                        .collect()
                })
                .unwrap_or_default(),
            call_num: view.call_num(),
            freq: view.freq().as_hz(),
            channel: view.channel().map(|c| c.0),
            signal: view.signal(),
            noise: view.noise(),
            start_time: unix(self.state.clock().wall(view.start_time())),
            stop_time: unix(self.state.clock().wall_now()),
            encrypted: view.encrypted(),
            emergency: view.emergency(),
            priority: i32::from(view.service_priority()),
            mode: view.mode(),
            duplex: view.duplex(),
            short_name: view.short_name().to_owned(),
            kind: view.kind().to_owned(),
            tdma_slot: view.tdma_slot(),
            phase2_tdma: view.phase2_tdma(),
            segments,
            is_conventional,
            ..Default::default()
        }
    }

    /// `at` on the wall clock.
    fn timestamp(&self, at: Instant) -> prost_types::Timestamp {
        let since_epoch = self
            .state
            .clock()
            .wall(at)
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        prost_types::Timestamp {
            seconds: i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX),
            nanos: i32::try_from(since_epoch.subsec_nanos()).unwrap_or(0),
        }
    }

    fn build_proto_segments_final(
        &self,
        call_id: CallId,
        final_freq: Freq,
        final_channel: Option<ChannelNum>,
    ) -> Vec<proto::CallSegment> {
        let view = match self.state.get_call_view(call_id) {
            Some(v) => v,
            None => return vec![],
        };

        let mut segments: Vec<_> = view
            .segments()
            .iter()
            .map(|s| self.make_proto_segment(s.freq.as_hz(), s.start, s.end, s.channel))
            .collect();
        // Append the still-open segment when concluding mid-recording. For
        // always-on conventional channels a transmission is concluded while
        // still in Recording with no prior pushed segment, so this is the one
        // segment that represents the keyed transmission.
        if view.is_trunked() || !view.segments().is_empty() || view.state() == CallState::Recording
        {
            segments.push(self.make_proto_segment(
                final_freq.as_hz(),
                view.segment_start(),
                self.state.clock().now(),
                final_channel,
            ));
        }
        segments
    }

    fn make_proto_segment(
        &self,
        freq: f64,
        start: Instant,
        end: Instant,
        channel: Option<ChannelNum>,
    ) -> proto::CallSegment {
        proto::CallSegment {
            freq,
            start_time: Some(self.timestamp(start)),
            duration: Some(prost_duration(end.saturating_duration_since(start))),
            channel: channel.map(|c| c.0),
        }
    }
}

fn prost_duration(d: std::time::Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(d.subsec_nanos()).unwrap_or(0),
    }
}

/// A recording's accounting as the wire lists it.
fn proto_transmission(t: &trunk_radio::Transmission) -> proto::Transmission {
    proto::Transmission {
        talkgroup: i64::from(t.talkgroup),
        start_time: t.start_time,
        stop_time: t.stop_time,
        sample_count: t.sample_count,
        spike_count: t.spike_count,
        error_count: t.error_count,
        freq: t.freq,
    }
}

#[cfg(test)]
mod tests {
    use super::VoiceCounters;

    fn tx(error_count: i64, spike_count: i64, sample_count: i64) -> trunk_radio::Transmission {
        trunk_radio::Transmission {
            talkgroup: 0,
            start_time: 0,
            stop_time: 0,
            sample_count,
            spike_count,
            error_count,
            term_events: 0,
            after_term_samples: 0,
            freq: 0.0,
        }
    }

    #[test]
    fn voice_counters_sum_transmission_list() {
        let c = VoiceCounters::from_transmissions(&[tx(3, 1, 100), tx(2, 4, 50)]);
        assert_eq!((c.errors, c.spikes, c.samples), (5, 5, 150));
    }

    #[test]
    fn per_speaker_deltas_sum_to_call_total() {
        // The recorder spans the whole call, so its counters are cumulative.
        // Three per-speaker boundaries snapshot the running total; each speaker's
        // reported slice is the delta since the prior snapshot, and the slices
        // must sum back to the final cumulative total (no double counting).
        let snapshots = [
            VoiceCounters::default(), // before speaker A
            VoiceCounters {
                errors: 4,
                spikes: 2,
                samples: 80,
                ..VoiceCounters::default()
            }, // after A
            VoiceCounters {
                errors: 7,
                spikes: 5,
                samples: 130,
                ..VoiceCounters::default()
            }, // after B
        ];
        let total = VoiceCounters {
            errors: 10,
            spikes: 6,
            samples: 200,
            ..VoiceCounters::default()
        }; // at final stop

        let a = snapshots[1].since(snapshots[0]);
        let b = snapshots[2].since(snapshots[1]);
        let c = total.since(snapshots[2]); // last speaker's slice at final stop

        assert_eq!((a.errors, a.spikes, a.samples), (4, 2, 80));
        assert_eq!((b.errors, b.spikes, b.samples), (3, 3, 50));
        assert_eq!((c.errors, c.spikes, c.samples), (3, 1, 70));
        assert_eq!(a.errors + b.errors + c.errors, total.errors);
        assert_eq!(a.spikes + b.spikes + c.spikes, total.spikes);
        assert_eq!(a.samples + b.samples + c.samples, total.samples);
    }

    #[test]
    fn since_clamps_when_counters_reset() {
        // A call's tally only grows, but should a snapshot ever be ahead of
        // the counters, the delta is zero, not negative.
        let after_reset = VoiceCounters {
            errors: 1,
            spikes: 0,
            samples: 10,
            ..VoiceCounters::default()
        };
        let stale = VoiceCounters {
            errors: 9,
            spikes: 9,
            samples: 999,
            ..VoiceCounters::default()
        };
        let d = after_reset.since(stale);
        assert_eq!((d.errors, d.spikes, d.samples), (0, 0, 0));
    }
}
