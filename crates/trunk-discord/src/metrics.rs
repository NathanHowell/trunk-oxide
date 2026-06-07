use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

pub struct Metrics {
    pub audio_packets_dropped: Counter<u64>,
    pub events_dropped: Counter<u64>,
    pub calls_reaped: Counter<u64>,
    pub calls_dropped_stuck: Counter<u64>,
    pub channel_resolution_failures: Counter<u64>,
    pub channel_resolution_duration: Histogram<f64>,
    pub discord_api_duration: Histogram<f64>,
    pub transcription_duration: Histogram<f64>,
    pub active_calls: Gauge<u64>,
    pub coalescing_calls: Gauge<u64>,
}

impl Metrics {
    pub fn new(meter: &Meter) -> Self {
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
                .with_description("Calls dropped because the handler stopped draining (bounded send timed out)")
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
            active_calls: meter
                .u64_gauge("trunk.discord.active_calls")
                .with_description("Number of currently active calls")
                .build(),
            coalescing_calls: meter
                .u64_gauge("trunk.discord.coalescing_calls")
                .with_description("Number of calls in coalescing state")
                .build(),
        }
    }
}
