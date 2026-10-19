use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use trunk_radio::{
    DetectedSignal, Radio, TrCallData, TrCallDataStatus, TrRecorderConfig, TrUnitEventType,
    TrunkMessage, Trunking,
};
use trunk_radio::{Squelch, SystemInfo};

use trunk_config::SystemType;
use trunk_oxide_grpc::EventBridge;
use trunk_oxide_grpc::proto::SystemType as ProtoSystemType;

use crate::ConventionalMetricsSnapshot;

use crate::call_state::{
    CallEndReason, CallId, CallState, CallView, Effect, Effects, Freq, RecorderNum,
    RecordingResult, RejectReason, Talkgroup, TrunkStateMachine, UnitEventType,
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

const fn system_type_to_proto(st: SystemType) -> i32 {
    match st {
        SystemType::Conventional => ProtoSystemType::Conventional as i32,
        SystemType::ConventionalP25 => ProtoSystemType::ConventionalP25 as i32,
        SystemType::ConventionalDMR => ProtoSystemType::ConventionalDmr as i32,
        SystemType::ConventionalSigmf => ProtoSystemType::ConventionalSigmf as i32,
        SystemType::Smartnet => ProtoSystemType::Smartnet as i32,
        SystemType::P25 => ProtoSystemType::P25 as i32,
    }
}

const fn format_recorder_state(state: i32) -> &'static str {
    match state {
        1 => "Recording",
        2 => "Inactive",
        3 => "Active",
        4 => "Idle",
        6 => "Stopped",
        7 => "Available",
        8 => "Ignored",
        _ => "Unknown",
    }
}

/// op25 decode-quality counters (errored / spiked voice codewords and total
/// samples) summed from a recorder's transmission list. The headless sink
/// accumulates these for the whole recording, so for a `Span` boundary — where
/// the recorder runs continuously across per-speaker transmissions — they are
/// cumulative; we snapshot at each boundary and report the delta so each
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
    fn from_transmissions(list: &[trunk_radio::TrTransmission]) -> Self {
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

    /// This transmission's slice since the prior snapshot (clamped at 0 so a
    /// recorder restart that zeroed the cumulative counters can't go negative).
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

/// What a recorder last recorded, and when it stopped.
#[derive(Clone, Copy, Debug)]
struct RecorderUse {
    talkgroup: i64,
    freq: Freq,
    slot: i32,
    stopped: Instant,
}

/// Executor: processes effects from the state machine against real FFI/bridge.
pub struct CallManager {
    state: TrunkStateMachine,
    bridge: Arc<EventBridge>,
    /// The radio: sources, recorders and the calls bound to them.
    radio: Arc<Radio>,
    /// Recorder-side tracing spans, opened at `EmitCallStart` and closed at
    /// conclude. Held (not entered) so each span's lifetime tracks the call's
    /// real duration; its context is serialized into the CallStart event so the
    /// downstream (discord) call span parents to it, and its stop_reason /
    /// recording_length / segments are recorded at close.
    call_spans: HashMap<CallId, tracing::Span>,
    /// Cumulative op25 decode counters already attributed to earlier per-speaker
    /// (`Span`) transmissions of a call, keyed by CallId. Lets each per-speaker
    /// CallEnd report only its own delta; removed when the call concludes.
    voice_snapshots: HashMap<CallId, VoiceCounters>,
    /// Each recorder's last call, for the next call's span: a recorder reused
    /// from another channel can carry that channel into its next call.
    recorder_last: HashMap<i32, RecorderUse>,
    /// Per-source frequency coverage, indexed by source position.
    source_ranges: Vec<RangeInclusive<f64>>,
    /// Per-source configured ppm correction, indexed by source position.
    source_ppms: Vec<f64>,
    /// Per-source software Hz tune correction, indexed by source position. When
    /// the adaptive loop is on, this is mutated at runtime to track clock drift.
    source_freq_corrections_hz: Vec<f64>,
    /// Whether the adaptive frequency-correction loop is enabled.
    adaptive_freq_correction: bool,
    /// Per-source EMA of FLL `freq_error` (Hz), the adaptive loop's smoothed input.
    source_freq_error_ema: Vec<f64>,
    /// Live per-source tune correction (Hz), so the loop is observable.
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
    recorder_denied: Counter<u64>,
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
        source_ranges: Vec<RangeInclusive<f64>>,
        source_ppms: Vec<f64>,
        source_freq_corrections_hz: Vec<f64>,
        conv_metrics: Arc<Mutex<ConventionalMetricsSnapshot>>,
        clock: crate::clock::Clock,
    ) -> Self {
        // Pre-populate conventional channel metrics entries.
        if let Ok(mut snap) = conv_metrics.lock() {
            for policy in config.systems.values() {
                for ch in &policy.channels {
                    snap.channels.push(crate::ConventionalChannelMetrics {
                        system: policy.short_name.clone(),
                        talkgroup: ch.talkgroup.0,
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

        let adaptive_freq_correction = config.adaptive_freq_correction;
        let source_count = source_ranges.len();
        let meter = opentelemetry::global::meter("trunk-oxide");
        Self {
            state: TrunkStateMachine::with_clock(config, clock),
            bridge,
            radio,
            call_spans: HashMap::new(),
            recorder_last: HashMap::new(),
            voice_snapshots: HashMap::new(),
            source_ranges,
            source_ppms,
            source_freq_corrections_hz,
            adaptive_freq_correction,
            source_freq_error_ema: vec![0.0; source_count],
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
            recorder_denied: meter
                .u64_counter("trunk.oxide.recorder_denied")
                .with_description("Calls denied a recorder")
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

    /// Find the first source whose frequency range covers `freq_hz`.
    fn find_source(&self, freq_hz: f64) -> Option<i32> {
        let found = self
            .source_ranges
            .iter()
            .position(|r| r.contains(&freq_hz))
            .map(|i| i as i32);
        if found.is_none() {
            // No source's center±rate/2 covers this freq — it falls in a gap
            // between dongles. Such a call gets no per-source ppm correction (and
            // is recorded by whichever dongle trunk-recorder assigned at the
            // edge). Surface it so coverage gaps are visible rather than silent.
            tracing::warn!(
                freq_mhz = freq_hz / 1e6,
                "freq not covered by any source range (gap between dongles); no ppm correction applied"
            );
        }
        found
    }

    /// Suggested per-source ppm correction that would pull the FLL frequency
    /// error toward zero. `freq_error_hz > 0` means the signal sat above the
    /// tuned center; empirically (confirmed against live readings on these
    /// rtlsdr dongles) raising the ppm correction lowers a positive error, so
    /// the suggestion adds the error-in-ppm to the configured value. Hardware
    /// sensitivity is sub-unity and the offset drifts with temperature, so
    /// converging on a dongle may take a step or two — treat this as a
    /// first-order estimate, not an exact target. Returns the configured ppm
    /// unchanged when there's no error, so a logged value drifting away from
    /// the config is the calibration signal.
    fn preferred_ppm(&self, freq_hz: f64, freq_error_hz: i32) -> Option<f64> {
        let idx = usize::try_from(self.find_source(freq_hz)?).ok()?;
        let current = *self.source_ppms.get(idx)?;
        Some(current + f64::from(freq_error_hz) / freq_hz * 1e6)
    }

    /// Update a conventional channel's metrics entry by talkgroup + freq.
    fn update_conv_metric(
        &self,
        talkgroup: i64,
        freq: Freq,
        f: impl FnOnce(&mut crate::ConventionalChannelMetrics),
    ) {
        if let Ok(mut snap) = self.conv_metrics.lock()
            && let Some(ch) = snap
                .channels
                .iter_mut()
                .find(|c| c.talkgroup == talkgroup && (c.freq_hz - freq.as_hz()).abs() < 1.0)
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
                    c.talkgroup == view.talkgroup() && (c.freq_hz - view.freq().as_hz()).abs() < 1.0
                }) {
                    ch.state = view.state().as_str();
                }
            }
        }
    }

    // ── Audio metadata management ──

    fn register_audio_meta(&self, call_id: CallId) {
        if let Some(view) = self.state.get_call_view(call_id) {
            self.bridge.register_audio_meta(
                call_id,
                AudioMeta {
                    talkgroup: view.talkgroup(),
                    short_name: Arc::from(view.short_name()),
                    freq: view.freq().as_hz(),
                    sample_rate: if view.is_analog() { 16_000 } else { 8_000 },
                },
            );
        }
    }

    // ── Effect processing ──

    fn process_effects(&mut self, effects: Effects) {
        for effect in effects {
            match effect {
                Effect::CreateCall { call_id } => {
                    self.exec_create_call(call_id);
                }
                Effect::RequestRecording {
                    call_id,
                    freq,
                    is_analog,
                    priority,
                } => {
                    self.exec_request_recording(call_id, freq, is_analog, priority);
                }
                Effect::StopRecording { call_id } => {
                    self.exec_stop_recorder(call_id);
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
                        self.calls_started.add(
                            1,
                            &[KeyValue::new("short_name", view.short_name().to_string())],
                        );
                        let span = tracing::info_span!(
                            "call",
                            short_name = %view.short_name(),
                            talkgroup = view.talkgroup(),
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
                            // The recorder, and what it recorded before this.
                            rec_num = tracing::field::Empty,
                            rec_prev_talkgroup = tracing::field::Empty,
                            rec_prev_freq = tracing::field::Empty,
                            rec_prev_slot = tracing::field::Empty,
                            rec_idle_s = tracing::field::Empty,
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
                        self.bridge.call_start(&info, traceparent);
                        self.call_spans.insert(call_id, span);
                        self.record_recorder(call_id);
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
                    }
                    self.exec_conclude_call(call_id, reason);
                }
                Effect::RotateTransmission { call_id, source_id } => {
                    // Soft boundary: advance to the next logical transmission on
                    // the same (still-running) recorder. No C++ recorder call —
                    // unlike ConventionalRestart, which rotates the WAV.
                    self.state.on_transmission_rotated(call_id, source_id);
                }
                Effect::EmitCallsActive => {
                    let infos: Vec<trunk_radio::TrCallInfo> = self
                        .state
                        .iter_calls()
                        .map(|v| self.build_call_info_from_view(&v))
                        .collect();
                    self.bridge.calls_active(&infos);
                }
                Effect::EmitSignal { call_id, source_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        let info = self.build_call_info_from_view(&view);
                        self.bridge.signal(source_id, &info);
                    }
                }
                Effect::EmitUnitEvent {
                    sys_num,
                    type_,
                    source_id,
                    talkgroup,
                } => {
                    let ffi_type = match type_ {
                        UnitEventType::Registration => TrUnitEventType::Registration,
                        UnitEventType::Deregistration => TrUnitEventType::Deregistration,
                        UnitEventType::Affiliation => TrUnitEventType::Affiliation,
                        UnitEventType::Acknowledge => TrUnitEventType::Acknowledge,
                        UnitEventType::Location => TrUnitEventType::Location,
                        UnitEventType::DataGrant => TrUnitEventType::DataGrant,
                        UnitEventType::AnswerRequest => TrUnitEventType::AnswerRequest,
                    };
                    let info = self.get_system_info(sys_num);
                    self.bridge
                        .unit_event(&info, ffi_type, source_id, talkgroup);
                }
                Effect::EmitSetupSystem { sys_num } => {
                    let info = self.get_system_info(sys_num);
                    self.bridge.setup_system(&info);
                }
                Effect::EmitSetupRecorder { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id)
                        && let Some(rec) = view.recorder()
                    {
                        self.bridge.setup_recorder(rec.0);
                    }
                }
                Effect::AddControlChannel { sys_num, freq } => {
                    self.radio.add_control_channel(sys_num, freq.as_hz());
                }
                Effect::ConventionalRestart { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                            m.idle_timeout_count += 1;
                        });
                        let cfg = self.build_recorder_config_from_view(&view);
                        self.radio.restart(call_id, &cfg);
                        let rec_num = self
                            .radio
                            .has_recorder(call_id)
                            .then(|| RecorderNum(self.radio.recorder_num(call_id)));
                        self.state.on_conventional_restarted(call_id, rec_num);
                    }
                }
                Effect::InitRecorder { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        let cfg = self.build_recorder_config_from_view(&view);
                        self.radio.restart(call_id, &cfg);
                    }
                }
                Effect::EnableRecorder { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        tracing::info!(
                            freq = %view.freq(),
                            talkgroup = view.talkgroup(),
                            "enabling recorder (signal detected)"
                        );
                        self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                            m.enable_count += 1;
                        });
                    }
                    self.radio.set_recorder_enabled(call_id, true);
                }
                Effect::DisableRecorder { call_id } => {
                    if let Some(view) = self.state.get_call_view(call_id) {
                        tracing::info!(
                            freq = %view.freq(),
                            talkgroup = view.talkgroup(),
                            "disabling recorder (signal lost)"
                        );
                        self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                            m.disable_count += 1;
                        });
                    }
                    self.radio.set_recorder_enabled(call_id, false);
                }
                Effect::RemoveCall { call_id } => {
                    self.radio.remove_call(call_id);
                    self.voice_snapshots.remove(&call_id);
                    self.state.remove_call(call_id);
                    // Unregister audio meta last: the write lock waits for any
                    // in-flight audio_stream() read lock, ensuring the final
                    // audio send completes before we remove the metadata.
                    self.bridge.unregister_audio_meta(call_id);
                }
            }
        }
    }

    // ── Effect executors ──

    fn exec_create_call(&mut self, call_id: CallId) {
        let Some(sys_num) = self.state.get_call_view(call_id).map(|v| v.sys_num()) else {
            return;
        };
        self.radio.create_call(call_id, sys_num);
        self.register_audio_meta(call_id);
    }

    fn exec_request_recording(
        &mut self,
        call_id: CallId,
        freq: Freq,
        is_analog: bool,
        priority: i32,
    ) {
        let view = match self.state.get_call_view(call_id) {
            Some(v) => v,
            None => {
                tracing::error!(?call_id, "exec_request_recording: CallEntry missing");
                return;
            }
        };

        // Build recorder config from Rust state (replaces C++ build_recorder_config)
        let cfg = self.build_recorder_config_from_view(&view);
        // Capture values we need after the view borrow ends
        let talkgroup = view.talkgroup();
        let sys_num = view.sys_num();

        let freq_hz = freq.as_hz();
        let source_idx = self.find_source(freq_hz);

        if !self.radio.has_call(call_id) {
            return;
        }

        let result = if let Some(source_idx) = source_idx {
            let available = self.radio.available_recorders(source_idx, is_analog);
            if priority > available {
                tracing::error!(
                    talkgroup,
                    sys_num,
                    priority,
                    available,
                    "not recording: priority exceeds available recorders"
                );
                RecordingResult::NoRecorder
            } else if let Some(rec_num) = self
                .radio
                .start_recording(source_idx, call_id, is_analog, &cfg)
            {
                RecordingResult::Started {
                    rec_num: RecorderNum(rec_num),
                }
            } else {
                RecordingResult::NoRecorder
            }
        } else {
            RecordingResult::NoSource
        };

        if matches!(
            result,
            RecordingResult::NoRecorder | RecordingResult::NoSource
        ) {
            let short_name = self
                .state
                .config
                .systems
                .get(&sys_num)
                .map_or_else(String::new, |p| p.short_name.clone());
            self.recorder_denied
                .add(1, &[KeyValue::new("short_name", short_name)]);
        }

        let followup = self.state.on_recording_result(call_id, result);
        // A retune starts another recorder under the same span.
        self.record_recorder(call_id);
        self.process_effects(followup);
    }

    /// Records the call's recorder, and what it recorded last, on its span.
    fn record_recorder(&self, call_id: CallId) {
        let (Some(span), Some(view)) = (
            self.call_spans.get(&call_id),
            self.state.get_call_view(call_id),
        ) else {
            return;
        };
        let Some(rec) = view.recorder() else {
            return;
        };
        span.record("rec_num", rec.0);
        if let Some(last) = self.recorder_last.get(&rec.0) {
            span.record("rec_prev_talkgroup", last.talkgroup);
            span.record("rec_prev_freq", tracing::field::display(last.freq));
            span.record("rec_prev_slot", last.slot);
            span.record(
                "rec_idle_s",
                self.state.clock().since(last.stopped).as_secs_f64(),
            );
        }
    }

    fn exec_stop_recorder(&mut self, call_id: CallId) {
        // Consult the radio — not the state machine — to decide whether a
        // recorder is attached. On retune, on_grant clears entry.recorder
        // *before* this StopRecording effect is processed, so a
        // `view.recorder().is_some()` guard here would always be false and the
        // old digital recorder would never be stopped, leaking the slot until
        // the source's recorder pool is exhausted.
        let id = call_id;
        if !self.radio.has_recorder(id) {
            return;
        }
        let rec_num = self.radio.recorder_num(id);

        let metrics = self.radio.stop_recording(id);

        let mut was_idle = false;
        if let Some(view) = self.state.get_call_view(call_id) {
            was_idle = view.state() == CallState::Idle;
            self.recorder_last.insert(
                rec_num,
                RecorderUse {
                    talkgroup: view.talkgroup(),
                    freq: view.freq(),
                    slot: view.tdma_slot(),
                    stopped: self.state.clock().now(),
                },
            );
            let pref_ppm = self
                .preferred_ppm(view.freq().as_hz(), metrics.freq_error)
                .unwrap_or(f64::NAN);
            tracing::info!(
                short_name = %view.short_name(),
                rec_num,
                talkgroup = view.talkgroup(),
                freq = %view.freq(),
                tdma = view.phase2_tdma(),
                slot = view.tdma_slot(),
                freq_error = format_args!("{:+.0} Hz", metrics.freq_error),
                preferred_ppm = format_args!("{pref_ppm:.2}"),
                "stopping recorder"
            );
        }

        // Update signal/noise from the recorder's power probe. Works for every
        // recorder type (analog/P25/DMR all read xlat_channelizer::get_pwr);
        // a DB_UNSET reading is mapped to None inside feed_recorder_power.
        self.state
            .feed_recorder_power(call_id, metrics.recorder_pwr, was_idle);

        self.state.on_recorder_stopped(call_id);
    }

    fn exec_conclude_call(&mut self, call_id: CallId, reason: CallEndReason) {
        // Take ownership of the call span so it closes (and is exported) on every
        // exit path of this function; record the stop reason up front.
        let span = self.call_spans.remove(&call_id);
        // How stale the audio was at the moment we concluded the call.
        let id = call_id;
        let has_call = self.radio.has_call(id);
        let audio_age = has_call.then(|| self.radio.since_last_write_secs(id));
        let voice_age = has_call.then(|| self.radio.since_last_voice_secs(id));
        if let (Some(age), Some(voice), Some(view)) =
            (audio_age, voice_age, self.state.get_call_view(call_id))
        {
            let labels = [
                KeyValue::new("reason", reason.as_str()),
                KeyValue::new("short_name", view.short_name().to_string()),
                KeyValue::new("tdma", view.phase2_tdma()),
                KeyValue::new("is_conventional", view.is_conventional()),
            ];
            self.call_stop_audio_age.record(age, &labels);
            self.call_stop_voice_age.record(voice, &labels);
            if reason.can_cut_speech() && voice < CUT_VOICE_AGE_S {
                let mut labels = labels.to_vec();
                labels.push(KeyValue::new("refreshed", view.refreshes() > 0));
                self.calls_cut.add(1, &labels);
            }
        }
        if let (Some(s), Some(view)) = (&span, self.state.get_call_view(call_id)) {
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
        }
        if let Some(s) = &span {
            if let Some(age) = audio_age {
                s.record("stop_audio_age_s", age);
            }
            if let Some(age) = voice_age {
                s.record("stop_voice_age_s", age);
            }
            s.record("stop_reason", reason.as_str());
            if let Some(c) = self.call_chz.get(&call_id) {
                let now = self.chz_overruns_total.get(&c.source).copied().unwrap_or(0);
                s.record(
                    "channelizer_overruns",
                    now.saturating_sub(c.overruns_at_start),
                );
                s.record("channelizer_max_block_us", c.max_ns / 1000);
            }
        }
        self.call_chz.remove(&call_id);

        // Logical (soft) transmission boundary: the in-band source changed
        // mid-call. End this transmission on the wire WITHOUT stopping the C++
        // recorder — the audio stream is continuous and `RotateTransmission`
        // opens the next transmission on the same recorder. The recorder is NOT
        // stopped, but its op25 decode counters are cumulative and readable
        // mid-recording, so we attribute this speaker's *delta* to the boundary
        // (see `voice_snapshots`). This is the seam where "end the transmission"
        // is separated from "stop the recorder".
        if matches!(reason, CallEndReason::SourceChange | CallEndReason::KeyUp) {
            let mut new_snapshot: Option<VoiceCounters> = None;
            if let Some(view) = self.state.get_call_view(call_id) {
                let final_freq = view.freq();
                let rec_num = view.recorder().map_or(-1, |r| r.0);
                // `recording_duration()` sums the closed segments plus the open
                // one (when Recording). For a mid-keyup source change segments
                // is empty and the call is still Recording, so this equals the
                // open segment's elapsed; for a boundary cut at a regrant the
                // prior speaker's audio is in a *closed* segment while the call
                // is Idle, which `segment_start().elapsed()` would miss.
                let recording_length = view
                    .recording_duration(self.state.clock().now())
                    .as_secs_f64();
                let segments = self.build_proto_segments_final(call_id, final_freq, rec_num);

                // Per-speaker op25 decode metrics: read the recorder's cumulative
                // counters (cheap lock-free atomics; valid mid-recording) and
                // subtract what earlier per-speaker transmissions already claimed.
                let delta = if has_call {
                    let cumulative =
                        VoiceCounters::from_transmissions(&self.radio.transmission_list(id));
                    new_snapshot = Some(cumulative);
                    cumulative.since(
                        self.voice_snapshots
                            .get(&call_id)
                            .copied()
                            .unwrap_or_default(),
                    )
                } else {
                    VoiceCounters::default()
                };

                if let Some(s) = &span {
                    s.record("recording_length", recording_length);
                    s.record(
                        "segments",
                        u64::try_from(segments.len()).unwrap_or(u64::MAX),
                    );
                    s.record("voice_errors", delta.errors);
                    s.record("voice_spikes", delta.spikes);
                    s.record("term_events", delta.term_events);
                    s.record("after_term_ms", delta.after_term_samples / 8);
                    s.record("voice_samples", delta.samples);
                }
                let voice_attrs = [
                    KeyValue::new("short_name", view.short_name().to_string()),
                    KeyValue::new("tdma", view.phase2_tdma()),
                    KeyValue::new(
                        "source",
                        i64::from(self.find_source(view.freq().as_hz()).unwrap_or(-1)),
                    ),
                ];
                self.voice_frame_errors
                    .add(u64::try_from(delta.errors).unwrap_or(0), &voice_attrs);
                self.voice_frame_spikes
                    .add(u64::try_from(delta.spikes).unwrap_or(0), &voice_attrs);
                self.terminator_events
                    .add(u64::try_from(delta.term_events).unwrap_or(0), &voice_attrs);
                if delta.term_events > 0 {
                    self.terminator_after_ms
                        .record(delta.after_term_samples as f64 / 8.0, &voice_attrs);
                }

                let mut data = self.build_call_data(call_id);
                data.length = recording_length;
                data.error_count = delta.errors;
                data.spike_count = delta.spikes;
                self.bridge.call_end(&data, &segments, false);
            }
            // Advance the snapshot AFTER the view borrow drops (needs &mut self).
            if let Some(cumulative) = new_snapshot {
                self.voice_snapshots.insert(call_id, cumulative);
            }
            return;
        }

        // Captured under the view borrow below, applied after it drops (the
        // adaptive update needs &mut self).
        let mut adaptive_input: Option<(usize, f64)> = None;

        let view = match self.state.get_call_view(call_id) {
            Some(v) => v,
            None => return,
        };

        let state = view.state();
        let final_freq = view.freq();
        let is_conventional = view.is_conventional();
        let rec_num = view.recorder();

        // Signal-detection conventional channels transition to Disabled
        // inside on_conventional_idle_timeout() *before* effects are
        // processed, so we must also accept Disabled for conventional calls.
        let should_stop_recording = matches!(
            state,
            CallState::Recording | CallState::Idle | CallState::Rejected(RejectReason::Superseded)
        ) || (is_conventional && state == CallState::Disabled);

        if should_stop_recording {
            if !has_call {
                return;
            }
            // Stop recorder and collect metrics
            if let Some(rec_num) = rec_num {
                let metrics = self.radio.stop_recording(id);

                // Feed recorder power back into state machine (all recorder
                // types; DB_UNSET maps to None inside feed_recorder_power).
                let was_idle = state == CallState::Idle;
                self.state
                    .feed_recorder_power(call_id, metrics.recorder_pwr, was_idle);

                let pref_ppm = self
                    .preferred_ppm(final_freq.as_hz(), metrics.freq_error)
                    .unwrap_or(f64::NAN);
                if let Some(view) = self.state.get_call_view(call_id) {
                    tracing::info!(
                        short_name = %view.short_name(),
                        rec_num = rec_num.0,
                        talkgroup = view.talkgroup(),
                        freq = %view.freq(),
                        tdma = view.phase2_tdma(),
                        slot = view.tdma_slot(),
                        freq_error = format_args!("{:+.0} Hz", metrics.freq_error),
                        preferred_ppm = format_args!("{pref_ppm:.2}"),
                        "stopping recorder"
                    );

                    let segments = self.build_proto_segments_final(call_id, final_freq, rec_num.0);
                    let recording_length = view
                        .recording_duration(self.state.clock().now())
                        .as_secs_f64();

                    let elapsed = view.start_time();
                    let elapsed = self.state.clock().since(elapsed).as_secs() as i64;
                    tracing::info!(
                        short_name = %view.short_name(),
                        call_num = view.call_num(),
                        talkgroup = view.talkgroup(),
                        elapsed,
                        recording_length,
                        segments = segments.len(),
                        "concluding recorded call"
                    );

                    // Surface the call's outcome on its span (parent of the
                    // downstream discord/transcribe spans).
                    if let Some(s) = &span {
                        s.record("recording_length", recording_length);
                        s.record(
                            "segments",
                            u64::try_from(segments.len()).unwrap_or(u64::MAX),
                        );
                        s.record("elapsed", elapsed);
                    }

                    if is_conventional {
                        self.update_conv_metric(view.talkgroup(), view.freq(), |m| {
                            m.recordings_completed += 1;
                        });
                    }

                    // Sum the decoder's quality counters across the call's
                    // transmissions to expose P25 audio loss (clipping).
                    let (mut tx_errors, mut tx_spikes, mut tx_samples) = (0i64, 0i64, 0i64);
                    let (mut tx_terms, mut tx_after) = (0i64, 0i64);
                    let mut tally = |list: &[trunk_radio::TrTransmission]| {
                        for t in list {
                            tx_errors += t.error_count;
                            tx_spikes += t.spike_count;
                            tx_samples += t.sample_count;
                            tx_terms += t.term_events;
                            tx_after += t.after_term_samples;
                        }
                    };
                    {
                        let mut data = self.build_call_data(call_id);
                        data.length = recording_length;
                        data.freq_error = metrics.freq_error;
                        data.transmission_list = self.radio.transmission_list(id);
                        tally(&data.transmission_list);
                        self.bridge.call_end(&data, &segments, is_conventional);
                    }

                    // Subtract whatever earlier per-speaker (Span) boundaries of
                    // this call already attributed, so this final CallEnd carries
                    // only the last speaker's slice and the per-speaker slices
                    // sum to the call total (not N×). For a single-speaker call
                    // the snapshot is zero, so the delta is the full tally.
                    let prior = self
                        .voice_snapshots
                        .get(&call_id)
                        .copied()
                        .unwrap_or_default();
                    let delta = VoiceCounters {
                        errors: tx_errors,
                        spikes: tx_spikes,
                        samples: tx_samples,
                        term_events: tx_terms,
                        after_term_samples: tx_after,
                    }
                    .since(prior);
                    if let Some(s) = &span {
                        s.record("voice_errors", delta.errors);
                        s.record("voice_spikes", delta.spikes);
                        s.record("term_events", delta.term_events);
                        s.record("after_term_ms", delta.after_term_samples / 8);
                        s.record("voice_samples", delta.samples);
                    }
                    let voice_attrs = [
                        KeyValue::new("short_name", view.short_name().to_string()),
                        KeyValue::new("tdma", view.phase2_tdma()),
                        KeyValue::new(
                            "source",
                            i64::from(self.find_source(view.freq().as_hz()).unwrap_or(-1)),
                        ),
                    ];
                    self.voice_frame_errors
                        .add(u64::try_from(delta.errors).unwrap_or(0), &voice_attrs);
                    self.voice_frame_spikes
                        .add(u64::try_from(delta.spikes).unwrap_or(0), &voice_attrs);
                    self.terminator_events
                        .add(u64::try_from(delta.term_events).unwrap_or(0), &voice_attrs);
                    if delta.term_events > 0 {
                        self.terminator_after_ms
                            .record(delta.after_term_samples as f64 / 8.0, &voice_attrs);
                    }

                    // Per-source clock-error distribution: median = TCXO bias,
                    // spread = decode margin; both trendable to watch drift.
                    let source = i64::from(self.find_source(view.freq().as_hz()).unwrap_or(-1));
                    self.freq_error_hz.record(
                        f64::from(metrics.freq_error),
                        &[
                            KeyValue::new("source", source),
                            KeyValue::new("short_name", view.short_name().to_string()),
                        ],
                    );
                    if let Ok(idx) = usize::try_from(source) {
                        adaptive_input = Some((idx, f64::from(metrics.freq_error)));
                    }
                }
            } else {
                // A call in a should-conclude state but with no recorder tracked
                // by the state machine (e.g. the recorder was dropped on a
                // retune that never re-acquired one). Audio for this call has
                // already streamed live to subscribers, so we MUST still emit
                // call_end or the call never finalizes downstream — it lingers
                // showing only a partial transcription. Stop any recorder the
                // C++ side still holds first, so we don't re-leak the slot.
                if self.radio.has_recorder(id) {
                    let _ = self.radio.stop_recording(id);
                }
                let talkgroup = self
                    .state
                    .get_call_view(call_id)
                    .map_or(0, |v| v.talkgroup());
                tracing::warn!(talkgroup, ?state, "concluding call with no recorder");
                let segments = self.build_proto_segments_final(call_id, final_freq, -1);
                let data = self.build_call_data(call_id);
                self.bridge.call_end(&data, &segments, is_conventional);
            }
        } else if matches!(state, CallState::Rejected(_)) {
            let segments = self.build_proto_segments_final(call_id, final_freq, -1);
            let data = self.build_call_data(call_id);
            self.bridge.call_end(&data, &segments, is_conventional);
        }

        // Adaptive frequency correction (view borrow is dropped by here).
        if self.adaptive_freq_correction
            && let Some((source_idx, freq_error_hz)) = adaptive_input
        {
            self.update_adaptive_correction(source_idx, freq_error_hz);
        }

        // The call is fully concluded here (the SourceChange soft-boundary path
        // returns earlier and intentionally keeps its snapshot). Drop it so the
        // map doesn't grow without bound.
        self.voice_snapshots.remove(&call_id);
    }

    /// Nudge a source's software tune correction toward whatever zeroes the FLL
    /// `freq_error`, tracking TCXO drift. Slow EMA + small gain + deadband so it
    /// follows thermal/aging drift without chasing per-call noise; clamped per
    /// step and in total. Empirically d(freq_error)/d(correction) > 0, so a
    /// negative residual calls for a larger correction (hence `-= gain*ema`).
    fn update_adaptive_correction(&mut self, source_idx: usize, freq_error_hz: f64) {
        const ALPHA: f64 = 0.2; // EMA smoothing (~10-call memory)
        const GAIN: f64 = 0.3; // fraction of the smoothed error applied per step
        const DEADBAND_HZ: f64 = 30.0; // don't chase residuals inside the noise
        const MAX_STEP_HZ: f64 = 50.0; // cap a single update
        const MAX_CORRECTION_HZ: f64 = 5000.0; // sanity bound on total correction

        let ema_val = {
            let Some(ema) = self.source_freq_error_ema.get_mut(source_idx) else {
                return;
            };
            *ema = ALPHA.mul_add(freq_error_hz, (1.0 - ALPHA) * *ema);
            *ema
        };
        if ema_val.abs() <= DEADBAND_HZ {
            return;
        }
        let new_corr = {
            let Some(corr) = self.source_freq_corrections_hz.get_mut(source_idx) else {
                return;
            };
            let step = (-GAIN * ema_val).clamp(-MAX_STEP_HZ, MAX_STEP_HZ);
            *corr = (*corr + step).clamp(-MAX_CORRECTION_HZ, MAX_CORRECTION_HZ);
            *corr
        };
        self.freq_correction_gauge.record(
            new_corr,
            &[KeyValue::new(
                "source",
                i64::try_from(source_idx).unwrap_or(-1),
            )],
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
            self.note_ptt_ends(msg.sys_num, i64::from(v.talkgroup));
        }
        let effects = self.state.on_message(msg);
        self.process_effects(effects);
    }

    /// Before a grant reaches the state machine: tell it which of the
    /// granted talkgroup's recording calls had their voice channel end a
    /// transmission (a terminator since the current transmission began), so
    /// the grant can be read as a new key-up even from the same handset.
    fn note_ptt_ends(&mut self, sys_num: i32, talkgroup: i64) {
        let ended: Vec<CallId> = self
            .state
            .iter_calls()
            .filter(|v| {
                v.sys_num() == sys_num
                    && v.talkgroup() == talkgroup
                    && v.state() == CallState::Recording
            })
            .filter_map(|v| {
                let now = VoiceCounters::from_transmissions(&self.radio.transmission_list(v.id()));
                let then = self
                    .voice_snapshots
                    .get(&v.id())
                    .copied()
                    .unwrap_or_default();
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
                let src = self.find_source(v.freq().as_hz())?;
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
        // First, check recorder write times for recording calls (needs FFI).
        // The state machine can't check this, so we do it here.
        let timeout = self.state.config.call_timeout;
        let mut timed_out_ids = Vec::new();

        for view in self.state.iter_calls() {
            if view.is_conventional() || view.state() != CallState::Recording {
                continue;
            }
            if self.radio.has_recorder(view.id()) {
                let since_write = self.radio.since_last_write_secs(view.id());
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
                    && let Some(pwr_db) = trunk_radio::signal_from_ffi(self.radio.pwr_db(view.id()))
                {
                    hist.record(
                        pwr_db,
                        &[
                            KeyValue::new("short_name", view.short_name().to_string()),
                            KeyValue::new("tdma", view.phase2_tdma()),
                            KeyValue::new(
                                "source",
                                i64::from(self.find_source(view.freq().as_hz()).unwrap_or(-1)),
                            ),
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
        self.sync_conv_states();
    }

    /// A conventional channel's call: a recorder of its own on `source_idx`,
    /// then the state machine's call bound to it.
    pub fn add_conventional_call(
        &mut self,
        channel: &ConventionalChannel,
        sys_num: i32,
        source_idx: i32,
    ) {
        let policy = self
            .state
            .config
            .systems
            .get(&sys_num)
            .cloned()
            .unwrap_or_else(|| crate::call_state::DEFAULT_POLICY.clone());

        let rec_num = match self.radio.add_conventional_recorder(
            sys_num,
            source_idx,
            &trunk_radio::TrChannelConfig {
                id: channel.talkgroup.0,
                freq: channel.freq.as_hz(),
                tone: channel.tone,
                squelch_db: channel.squelch_db,
                signal_detection: channel.signal_detection,
            },
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(
                    sys_num, freq = %channel.freq, talkgroup = channel.talkgroup.0, error = %e,
                    "cannot create the conventional channel's recorder"
                );
                return;
            }
        };

        let (call_id, effects) =
            self.state
                .add_conventional_call(channel, sys_num, &policy, Some(RecorderNum(rec_num)));
        self.radio.bind_conventional_call(call_id, sys_num, rec_num);
        self.register_audio_meta(call_id);

        self.process_effects(effects);
    }

    /// A conventional channel's transmission detector opened or closed.
    pub fn handle_squelch_event(&mut self, recorder_num: i32, squelch: Squelch, pwr_db: f64) {
        // Record signal quality metrics before mutating state
        if let Some(view) = self
            .state
            .iter_calls()
            .find(|v| v.is_conventional() && v.recorder() == Some(RecorderNum(recorder_num)))
        {
            let tg = view.talkgroup();
            let freq = view.freq();
            match squelch {
                Squelch::Open => self.update_conv_metric(tg, freq, |m| {
                    m.signal_power_db = pwr_db;
                    m.squelch_open_count += 1;
                }),
                Squelch::Close(reason) => {
                    tracing::debug!(talkgroup = tg, freq = %freq, ?reason, pwr_db, "squelch closed");
                    self.update_conv_metric(tg, freq, |m| {
                        m.noise_floor_db = pwr_db;
                        m.squelch_close_count += 1;
                    });
                }
            }
        }

        let effects = self
            .state
            .on_squelch(RecorderNum(recorder_num), squelch, pwr_db);
        self.process_effects(effects);
    }

    /// An in-band P25 source (unit) change decoded by op25 on the voice channel,
    /// keyed by the recorder's `call_id`. Cuts a per-speaker transmission
    /// boundary (see [`TrunkStateMachine::on_source_change`]).
    pub fn handle_source_change(&mut self, call_id: CallId, src_id: i64) {
        let effects = self.state.on_source_change(call_id, src_id);
        self.process_effects(effects);
    }

    /// P25 encryption parameters (ESS `ALGID` / `KeyID`) decoded by op25 for an
    /// encrypted call, keyed by the recorder's `call_id`. Stored on the
    /// call so the `calls_ended` metric can facet by algorithm.
    pub fn handle_crypto(&mut self, call_id: CallId, alg_id: u32, key_id: u32) {
        // ALGID is 8 bits and KeyID 16 on the wire; the decoder already masks.
        let call_id = call_id;
        let (alg_id, key_id) = (
            u8::try_from(alg_id & 0xFF).unwrap_or(0),
            u16::try_from(key_id & 0xFFFF).unwrap_or(0),
        );
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
        self.state.on_crypto(call_id, alg_id, key_id);
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

    pub(crate) fn get_system_info(&self, sys_num: i32) -> SystemInfo {
        let policy = self
            .state
            .config
            .systems
            .get(&sys_num)
            .unwrap_or(&crate::call_state::DEFAULT_POLICY);
        let id = self.state.system_identities.get(&sys_num);

        SystemInfo {
            sys_num,
            system_type: system_type_to_proto(policy.system_type),
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
            message_count: 0,
            decode_rate: 0,
        }
    }

    /// Build a `TrRecorderConfig` from a [`CallView`] and system metadata.
    fn build_recorder_config_from_view(&self, view: &CallView<'_>) -> TrRecorderConfig {
        let policy = self
            .state
            .config
            .systems
            .get(&view.sys_num())
            .unwrap_or(&crate::call_state::DEFAULT_POLICY);

        let xor_mask = self
            .state
            .system_identities
            .get(&view.sys_num())
            .map_or_else(Vec::new, |id| id.xor_mask.clone());

        // Software clock correction: a per-source Hz tune offset (the everyday
        // sub-ppm knob; the integer hardware ppm handles only coarse/wideband).
        // Real-time adaptive deltas will add to this term. Only the physical tune
        // shifts — the displayed call frequency stays view.freq().
        let true_freq = view.freq().as_hz();
        let correction_hz = self
            .find_source(true_freq)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| self.source_freq_corrections_hz.get(i))
            .copied()
            .unwrap_or(0.0);
        let tune_freq = true_freq + correction_hz;

        TrRecorderConfig {
            talkgroup: view.talkgroup(),
            call_num: view.call_num(),
            freq: tune_freq,
            short_name: view.short_name().to_owned(),
            temp_dir: self.state.config.temp_dir.clone(),
            phase2_tdma: view.phase2_tdma(),
            tdma_slot: view.tdma_slot(),
            xor_mask,
            squelch_db: view.squelch_db(),
            digital_levels: policy.digital_levels,
            call_id: view.id(),
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
                talkgroup = view.talkgroup(),
                freq = %view.freq(),
                elapsed,
                state,
                rec_num = view.recorder().map(|r| r.0),
                "call status"
            );
        }
    }

    pub fn print_recorder_status(&self) {
        let recorders = self.radio.recorder_status();
        let mut current_source = -1i32;
        for r in &recorders {
            if r.source_num != current_source {
                current_source = r.source_num;
                tracing::info!(
                    source = r.source_num,
                    freq = %Freq::from_hz(r.source_center),
                    device = %r.source_device,
                    "source"
                );
            }
            tracing::info!(
                rec_num = r.rec_num,
                rec_type = %r.rec_type,
                rec_state = format_recorder_state(r.rec_state),
                "  recorder"
            );
        }
    }

    // ── Helper: build TrCallInfo from CallView ──

    fn build_call_info_from_view(&self, view: &CallView<'_>) -> trunk_radio::TrCallInfo {
        let elapsed = self.state.clock().since(view.start_time());
        trunk_radio::TrCallInfo {
            talkgroup: view.talkgroup(),
            call_num: view.call_num(),
            sys_num: view.sys_num(),
            freq: view.freq().as_hz(),
            signal: trunk_radio::signal_to_ffi(view.signal()),
            noise: trunk_radio::signal_to_ffi(view.noise()),
            freq_error: 0,
            encrypted: view.encrypted(),
            emergency: view.emergency(),
            is_analog: view.is_analog(),
            is_conventional: view.is_conventional(),
            phase2_tdma: view.phase2_tdma(),
            tdma_slot: view.tdma_slot(),
            priority: view.priority(),
            mode: view.mode(),
            duplex: view.duplex(),
            conversation_mode: view.trunked_fields().is_some_and(|t| t.conversation_mode),
            current_source_id: view.current_source_id(),
            start_time: crate::call_state::system_time_to_unix(
                self.state.clock().wall(view.start_time()),
            ),
            elapsed: elapsed.as_secs_f64(),
            short_name: view.short_name().to_owned(),
            call_state: view.state().to_proto(),
        }
    }

    fn build_call_data(&self, call_id: CallId) -> TrCallData {
        let view = self.state.get_call_view(call_id).unwrap();
        let audio_type = if view.is_analog() {
            "analog"
        } else if view.phase2_tdma() {
            "digital tdma"
        } else {
            "digital"
        };

        let now = crate::call_state::system_time_to_unix(self.state.clock().wall_now());

        TrCallData {
            talkgroup: view.talkgroup(),
            color_code: 0,
            patched_talkgroups: self
                .state
                .patch_tables
                .get(&view.sys_num())
                .map(|pt| {
                    pt.get_patch(Talkgroup(view.talkgroup()))
                        .into_iter()
                        .map(|t| t.0 as u64)
                        .collect()
                })
                .unwrap_or_default(),
            call_num: view.call_num(),
            freq: view.freq().as_hz(),
            freq_error: 0,
            source_num: 0,
            recorder_num: view.recorder().map_or(-1, |r| r.0),
            signal: trunk_radio::signal_to_ffi(view.signal()),
            noise: trunk_radio::signal_to_ffi(view.noise()),
            start_time: crate::call_state::system_time_to_unix(
                self.state.clock().wall(view.start_time()),
            ),
            stop_time: now,
            error_count: 0,
            spike_count: 0,
            encrypted: view.encrypted(),
            emergency: view.emergency(),
            priority: view.priority(),
            mode: view.mode(),
            duplex: view.duplex(),
            sys_num: view.sys_num(),
            short_name: view.short_name().to_owned(),
            audio_type: audio_type.into(),
            tdma_slot: view.tdma_slot(),
            length: 0.0,
            phase2_tdma: view.phase2_tdma(),
            transmission_source_list: Vec::new(),
            transmission_error_list: Vec::new(),
            transmission_list: Vec::new(),
            status: TrCallDataStatus::Initial,
        }
    }

    fn build_proto_segments_final(
        &self,
        call_id: CallId,
        final_freq: Freq,
        final_rec_num: i32,
    ) -> Vec<trunk_oxide_grpc::proto::CallSegment> {
        let view = match self.state.get_call_view(call_id) {
            Some(v) => v,
            None => return vec![],
        };

        let mut segments: Vec<_> = view
            .segments()
            .iter()
            .map(|s| self.make_proto_segment(s.freq.as_hz(), s.start, s.end, s.rec_num.0))
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
                final_rec_num,
            ));
        }
        segments
    }

    fn make_proto_segment(
        &self,
        freq: f64,
        start: Instant,
        end: Instant,
        rec_num: i32,
    ) -> trunk_oxide_grpc::proto::CallSegment {
        use prost_types::{Duration as ProstDuration, Timestamp};
        let elapsed = end.saturating_duration_since(start);
        let wall_start = self.state.clock().wall(start);
        let since_epoch = wall_start
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        trunk_oxide_grpc::proto::CallSegment {
            freq,
            start_time: Some(Timestamp {
                seconds: since_epoch.as_secs() as i64,
                nanos: since_epoch.subsec_nanos() as i32,
            }),
            duration: Some(ProstDuration {
                seconds: elapsed.as_secs() as i64,
                nanos: elapsed.subsec_nanos() as i32,
            }),
            rec_num,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VoiceCounters;

    fn tx(error_count: i64, spike_count: i64, sample_count: i64) -> trunk_radio::TrTransmission {
        trunk_radio::TrTransmission {
            source: 0,
            talkgroup: 0,
            slot: 0,
            color_code: 0,
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
        // A recorder restart (e.g. ConventionalRestart) zeroes the cumulative
        // counters; a stale higher snapshot must not produce a negative delta.
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
