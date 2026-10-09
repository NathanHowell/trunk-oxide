use std::sync::Arc;

use anyhow::Result;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use tracing::Instrument;

use crate::config::WhistleConfig;
use crate::needle::Needle;
use crate::priority_semaphore::PrioritySemaphore;

pub struct WhistleTranscriber {
    // The needle engine is one process-global, non-thread-safe model: decode is serialized here, so
    // the priority gate (not this mutex) is what decides admission order.
    model: Arc<tokio::sync::Mutex<Needle>>,
    semaphore: Arc<PrioritySemaphore>,
    max_concurrent: usize,
    transcription_duration: Histogram<f64>,
    queue_wait_duration: Histogram<f64>,
    audio_input_duration: Histogram<f64>,
    transcriptions_completed: Counter<u64>,
    transcription_errors: Counter<u64>,
}

impl WhistleTranscriber {
    pub fn new(config: &WhistleConfig) -> Result<Self> {
        let model = Needle::load(
            &config.model_path,
            config.language.as_deref(),
            &config.keywords,
        )?;

        tracing::info!(
            model_path = %config.model_path,
            language = config.language.as_deref().unwrap_or("auto"),
            keywords = config.keywords.len(),
            max_concurrent = config.max_concurrent,
            "whistle transcriber initialized"
        );

        let meter = opentelemetry::global::meter(env!("CARGO_PKG_NAME"));

        Ok(Self {
            model: Arc::new(tokio::sync::Mutex::new(model)),
            semaphore: PrioritySemaphore::new(config.max_concurrent),
            max_concurrent: config.max_concurrent,
            transcription_duration: meter
                .f64_histogram("trunk.whistle.transcription_duration_seconds")
                .with_description("Time spent decoding audio in whistle (after acquiring a permit)")
                .build(),
            queue_wait_duration: meter
                .f64_histogram("trunk.whistle.queue_wait_seconds")
                .with_description(
                    "Time a request waited for a concurrency permit before decoding (backlog)",
                )
                .build(),
            audio_input_duration: meter
                .f64_histogram("trunk.whistle.audio_input_duration_seconds")
                .build(),
            transcriptions_completed: meter
                .u64_counter("trunk.whistle.transcriptions_completed")
                .build(),
            transcription_errors: meter
                .u64_counter("trunk.whistle.transcription_errors")
                .build(),
        })
    }

    pub const fn semaphore(&self) -> &Arc<PrioritySemaphore> {
        &self.semaphore
    }

    pub const fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    pub async fn transcribe(
        &self,
        audio: Vec<f32>,
        short_name: &str,
        is_conventional: bool,
        priority: i32,
    ) -> Result<String> {
        // Attribute the decode cost by system. short_name + is_conventional are
        // low-cardinality, so they are safe as metric labels (talkgroup is not —
        // it rides on the span instead). priority_bucket is also low-cardinality
        // (a handful of tiers) and lets us watch the wait drop for high-priority
        // calls once the priority gate is in effect.
        let attrs = [
            KeyValue::new("short_name", short_name.to_string()),
            KeyValue::new("is_conventional", is_conventional),
        ];
        let wait_attrs = [
            KeyValue::new("short_name", short_name.to_string()),
            KeyValue::new("is_conventional", is_conventional),
            KeyValue::new("priority_bucket", i64::from(priority.max(0))),
        ];

        #[allow(clippy::cast_precision_loss)]
        self.audio_input_duration
            .record(audio.len() as f64 / 16000.0, &attrs);

        // Child span of `transcribe`: time spent waiting for a concurrency
        // permit (backlog), separated from the decode below so traces show the
        // split per-request. Admission is priority-ordered: a more important
        // talkgroup parked here is woken ahead of a lower-priority one.
        let queue_start = std::time::Instant::now();
        let permit = Arc::clone(&self.semaphore)
            .acquire(priority)
            .instrument(tracing::info_span!("queue_wait"))
            .await;
        self.queue_wait_duration
            .record(queue_start.elapsed().as_secs_f64(), &wait_attrs);

        let model = Arc::clone(&self.model);
        let start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            let text = model
                .blocking_lock()
                .transcribe(&audio)
                .map_err(|e| anyhow::anyhow!("whistle transcription failed: {e}"))?;
            drop(permit);
            Ok::<String, anyhow::Error>(text)
        })
        .instrument(tracing::info_span!("decode"))
        .await?;
        let elapsed = start.elapsed().as_secs_f64();
        self.transcription_duration.record(elapsed, &attrs);

        // Throughput/errors carry priority_bucket (like queue_wait above) so the
        // served rate can be split per tier. Decode duration stays on `attrs`
        // (no bucket): decode cost is priority-independent.
        match result {
            Ok(text) => {
                self.transcriptions_completed.add(1, &wait_attrs);
                Ok(text)
            }
            Err(e) => {
                self.transcription_errors.add(1, &wait_attrs);
                Err(e)
            }
        }
    }
}
