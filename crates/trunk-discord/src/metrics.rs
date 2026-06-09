use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, ObservableGauge};

pub struct Metrics {
    pub audio_packets_dropped: Counter<u64>,
    pub events_dropped: Counter<u64>,
    pub calls_reaped: Counter<u64>,
    pub calls_dropped_stuck: Counter<u64>,
    pub channel_resolution_failures: Counter<u64>,
    pub channel_resolution_duration: Histogram<f64>,
    pub discord_api_duration: Histogram<f64>,
    pub transcription_duration: Histogram<f64>,
    pub transcriptions_timed_out: Counter<u64>,
    pub active_calls: Gauge<u64>,
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
            channel_resolution_duration: meter
                .f64_histogram("trunk.discord.channel_resolution_duration_seconds")
                .with_description("Time to resolve a Discord channel for a call")
                .build(),
            discord_api_duration: meter
                .f64_histogram("trunk.discord.discord_api_duration_seconds")
                .with_description("Duration of Discord API calls")
                .build(),
            transcription_duration: meter
                .f64_histogram("trunk.discord.transcription_duration_seconds")
                .with_description("Duration of whisper transcription requests")
                .build(),
            transcriptions_timed_out: meter
                .u64_counter("trunk.discord.transcriptions_timed_out")
                .with_description("Transcription requests abandoned after exceeding the timeout")
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
