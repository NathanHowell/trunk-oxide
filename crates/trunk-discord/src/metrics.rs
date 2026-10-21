use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, ObservableGauge};

/// Bucket edges for per-segment durations in seconds.
const SECONDS_BOUNDARIES: [f64; 11] = [0.0, 0.1, 0.2, 0.4, 0.6, 0.8, 1.0, 1.5, 2.0, 3.0, 5.0];

/// Bucket edges for request latencies in seconds (sub-second to minutes).
const LATENCY_BOUNDARIES: [f64; 12] = [
    0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

pub struct Metrics {
    pub audio_packets_dropped: Counter<u64>,
    pub events_dropped: Counter<u64>,
    pub calls_reaped: Counter<u64>,
    pub calls_dropped_stuck: Counter<u64>,
    pub channel_resolution_failures: Counter<u64>,
    /// Audio attachments dropped because the background upload backlog was
    /// saturated (sustained overload — uploads arriving faster than Discord
    /// accepts them). The embed is still green; only the audio file is skipped.
    pub attach_dropped: Counter<u64>,
    pub channel_resolution_duration: Histogram<f64>,
    pub discord_api_duration: Histogram<f64>,
    pub transcription_duration: Histogram<f64>,
    pub transcriptions_timed_out: Counter<u64>,
    pub active_calls: Gauge<u64>,

    // ── Audio shape / transcription coverage (clipping + missing-lead-in diagnosis) ──
    // All labelled short_name / is_conventional / tdma, per transmission segment.
    /// Seconds of leading non-speech before the first voiced frame.
    pub audio_leading_silence: Histogram<f64>,
    /// Seconds after the last voiced frame.
    pub audio_trailing_silence: Histogram<f64>,
    /// Peak 0.5 s-window RMS of each segment (linear, full scale 1.0); the
    /// value the silence gate compares against `SILENCE_RMS_THRESHOLD`.
    /// Level of the last voiced frame relative to the segment peak (dB, <= 0).
    pub audio_end_level_db: Histogram<f64>,
    /// Segments whose speech ends while still loud (a clipped tail).
    pub audio_abrupt_ends: Counter<u64>,
    /// Digital-silence holes (>= 40 ms) inside speech, and the longest (ms).
    pub audio_gaps: Histogram<f64>,
    pub audio_longest_gap_ms: Histogram<f64>,
    /// Transcription coverage: when the model's first/last text lands vs the
    /// audio's own onset/end. Positive leading gap = text starts later than
    /// the speech does.
    pub transcript_first_text_offset: Histogram<f64>,
    pub transcript_leading_gap: Histogram<f64>,
    pub transcript_trailing_gap: Histogram<f64>,
    /// Segments the gate kept that came back with no text at all.
    pub transcript_empty: Counter<u64>,
    /// Calls a listener flagged with a reaction (ground truth for audio problems).
    pub calls_flagged: Counter<u64>,
    pub coalescing_calls: Gauge<u64>,
    /// Number of live call-handler tasks the executor is tracking. Should stay
    /// equal to `active_calls + coalescing_calls`; a persistent excess means
    /// handlers are leaking (orphaned grey embeds).
    pub live_handlers: Gauge<u64>,

    /// Backing counter for [`Self::unfinalized_handlers`]: handler tasks that
    /// have started but not yet finalized (turned the embed green). Bumped by
    /// [`Self::handler_started`] / [`Self::handler_finalized`].
    unfinalized_handlers: Arc<AtomicU64>,
    /// Observable gauge reporting `unfinalized_handlers`. Unlike `live_handlers`
    /// (the executor's `self.calls` view, which is decremented the instant the
    /// pump sends `End`), this is decremented only when the handler *actually*
    /// finalizes — so a handler wedged grey *after* being removed from
    /// `self.calls` (e.g. stuck in a slow Discord edit) stays counted here. A
    /// persistent excess over `active + coalescing` is the grey-embed leak that
    /// `live_handlers` is structurally blind to. Held to keep the callback
    /// registered.
    _unfinalized_handlers_gauge: ObservableGauge<u64>,
}

impl Metrics {
    pub fn new(meter: &Meter) -> Self {
        let unfinalized_handlers = Arc::new(AtomicU64::new(0));
        let unfinalized_for_cb = Arc::clone(&unfinalized_handlers);
        Self {
            audio_packets_dropped: meter
                .u64_counter("trunk.discord.audio_packets_dropped")
                .with_description("Audio packets dropped due to full channel")
                .build(),
            events_dropped: meter
                .u64_counter("trunk.discord.events_dropped")
                .with_description("Events dropped due to full channel")
                .build(),
            calls_reaped: meter
                .u64_counter("trunk.discord.calls_reaped")
                .with_description("Calls reaped due to timeout")
                .build(),
            calls_dropped_stuck: meter
                .u64_counter("trunk.discord.calls_dropped_stuck")
                .with_description(
                    "Calls dropped because the handler stopped draining (bounded send timed out)",
                )
                .build(),
            channel_resolution_failures: meter
                .u64_counter("trunk.discord.channel_resolution_failures")
                .with_description("Failed channel resolutions")
                .build(),
            attach_dropped: meter
                .u64_counter("trunk.discord.attach_dropped")
                .with_description("Audio attachments dropped due to a saturated upload backlog")
                .build(),
            channel_resolution_duration: meter
                .f64_histogram("trunk.discord.channel_resolution_duration_seconds")
                .with_description("Time to resolve a Discord channel for a call")
                .build(),
            discord_api_duration: meter
                .f64_histogram("trunk.discord.discord_api_duration_seconds")
                .with_description("Duration of Discord API calls")
                .with_boundaries(LATENCY_BOUNDARIES.to_vec())
                .build(),
            transcription_duration: meter
                .f64_histogram("trunk.discord.transcription_duration_seconds")
                .with_description("Duration of whisper transcription requests")
                .with_boundaries(LATENCY_BOUNDARIES.to_vec())
                .build(),
            transcriptions_timed_out: meter
                .u64_counter("trunk.discord.transcriptions_timed_out")
                .with_description("Transcription requests abandoned after exceeding the timeout")
                .build(),
            audio_leading_silence: meter
                .f64_histogram("trunk.discord.audio_leading_silence_seconds")
                .with_description("Seconds before the first voiced frame in a transmission segment")
                .with_boundaries(SECONDS_BOUNDARIES.to_vec())
                .build(),
            audio_trailing_silence: meter
                .f64_histogram("trunk.discord.audio_trailing_silence_seconds")
                .with_description("Seconds after the last voiced frame in a transmission segment")
                .with_boundaries(SECONDS_BOUNDARIES.to_vec())
                .build(),
            audio_end_level_db: meter
                .f64_histogram("trunk.discord.audio_end_level_db")
                .with_description("Last voiced frame level relative to segment peak (dB)")
                .with_boundaries(vec![
                    -30.0, -24.0, -20.0, -18.0, -15.0, -12.0, -9.0, -6.0, -3.0, 0.0,
                ])
                .build(),
            audio_abrupt_ends: meter
                .u64_counter("trunk.discord.audio_abrupt_ends")
                .with_description("Segments whose speech ends while still loud (clipped tail)")
                .build(),
            audio_gaps: meter
                .f64_histogram("trunk.discord.audio_gaps")
                .with_description("Digital-silence holes (>= 40 ms) inside speech, per segment")
                .with_boundaries(vec![0.0, 1.0, 2.0, 3.0, 5.0, 8.0, 13.0, 21.0])
                .build(),
            audio_longest_gap_ms: meter
                .f64_histogram("trunk.discord.audio_longest_gap_ms")
                .with_description("Longest digital-silence hole inside speech, per segment (ms)")
                .with_boundaries(vec![40.0, 60.0, 80.0, 120.0, 200.0, 400.0, 800.0])
                .build(),
            transcript_first_text_offset: meter
                .f64_histogram("trunk.discord.transcript_first_text_offset_seconds")
                .with_description("Seconds into a segment where the model's first text starts")
                .with_boundaries(SECONDS_BOUNDARIES.to_vec())
                .build(),
            transcript_leading_gap: meter
                .f64_histogram("trunk.discord.transcript_leading_gap_seconds")
                .with_description("First text offset minus audio onset (positive = text starts late)")
                .with_boundaries(vec![
                    -1.0, -0.5, -0.2, 0.0, 0.2, 0.5, 1.0, 1.5, 2.0, 3.0, 5.0,
                ])
                .build(),
            transcript_trailing_gap: meter
                .f64_histogram("trunk.discord.transcript_trailing_gap_seconds")
                .with_description("Audio end minus last text end (positive = text stops early)")
                .with_boundaries(vec![
                    -1.0, -0.5, -0.2, 0.0, 0.2, 0.5, 1.0, 1.5, 2.0, 3.0, 5.0,
                ])
                .build(),
            transcript_empty: meter
                .u64_counter("trunk.discord.transcript_empty")
                .with_description("Segments kept by the silence gate that returned no text")
                .build(),
            calls_flagged: meter
                .u64_counter("trunk.discord.calls_flagged")
                .with_description("Calls flagged via emoji reaction, by reason")
                .build(),
            active_calls: meter
                .u64_gauge("trunk.discord.active_calls")
                .with_description("Number of currently active calls")
                .build(),
            coalescing_calls: meter
                .u64_gauge("trunk.discord.coalescing_calls")
                .with_description("Number of calls in coalescing state")
                .build(),
            live_handlers: meter
                .u64_gauge("trunk.discord.live_handlers")
                .with_description(
                    "Live call-handler tasks tracked by the executor (should equal active + coalescing)",
                )
                .build(),
            unfinalized_handlers,
            _unfinalized_handlers_gauge: meter
                .u64_observable_gauge("trunk.discord.unfinalized_handlers")
                .with_description(
                    "Handler tasks started but not yet finalized (grey embeds); persistent excess over active + coalescing is a leak live_handlers can't see",
                )
                .with_callback(move |observer| {
                    observer.observe(unfinalized_for_cb.load(Ordering::Relaxed), &[]);
                })
                .build(),
        }
    }

    /// A handler task has started running (embed not yet finalized).
    pub fn handler_started(&self) {
        self.unfinalized_handlers.fetch_add(1, Ordering::Relaxed);
    }

    /// A handler task has finalized its embed (or exited). Idempotency is the
    /// caller's responsibility (see the guard in `call_handler.rs`).
    pub fn handler_finalized(&self) {
        self.unfinalized_handlers.fetch_sub(1, Ordering::Relaxed);
    }
}
