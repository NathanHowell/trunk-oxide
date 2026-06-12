use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use tracing::Instrument;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::priority_semaphore::PrioritySemaphore;

pub struct Transcriber {
    contexts: Vec<Arc<WhisperContext>>,
    semaphore: Arc<PrioritySemaphore>,
    max_concurrent: usize,
    next_gpu: AtomicUsize,
    language: String,
    beam_size: i32,
    transcription_duration: Histogram<f64>,
    queue_wait_duration: Histogram<f64>,
    audio_input_duration: Histogram<f64>,
    transcriptions_completed: Counter<u64>,
    transcription_errors: Counter<u64>,
}

impl Transcriber {
    pub fn new(config: &crate::config::WhisperConfig) -> Result<Self> {
        let mut contexts = Vec::with_capacity(config.gpu_count);

        for device_id in 0..config.gpu_count {
            let mut ctx_params = WhisperContextParameters::default();
            ctx_params.use_gpu(true);
            let gpu_device = i32::try_from(device_id)
                .map_err(|_| anyhow::anyhow!("gpu_count exceeds i32::MAX"))?;
            ctx_params.gpu_device(gpu_device);
            ctx_params.flash_attn(config.flash_attn);

            let ctx = WhisperContext::new_with_params(&config.model_path, ctx_params)?;
            contexts.push(Arc::new(ctx));
        }

        tracing::info!(
            gpu_count = config.gpu_count,
            max_concurrent = config.max_concurrent,
            "whisper transcriber initialized"
        );

        tracing::info!(
            beam_size = config.beam_size,
            "whisper decoding strategy: {}",
            if config.beam_size > 0 {
                "beam search"
            } else {
                "greedy"
            }
        );

        let meter = opentelemetry::global::meter(env!("CARGO_PKG_NAME"));

        Ok(Self {
            contexts,
            semaphore: PrioritySemaphore::new(config.max_concurrent),
            max_concurrent: config.max_concurrent,
            next_gpu: AtomicUsize::new(0),
            language: config.language.clone(),
            beam_size: config.beam_size,
            transcription_duration: meter
                .f64_histogram("trunk.whisperer.transcription_duration_seconds")
                .with_description("Time spent decoding audio in whisper (after acquiring a permit)")
                .build(),
            queue_wait_duration: meter
                .f64_histogram("trunk.whisperer.queue_wait_seconds")
                .with_description(
                    "Time a request waited for a concurrency permit before decoding (backlog)",
                )
                .build(),
            audio_input_duration: meter
                .f64_histogram("trunk.whisperer.audio_input_duration_seconds")
                .build(),
            transcriptions_completed: meter
                .u64_counter("trunk.whisperer.transcriptions_completed")
                .build(),
            transcription_errors: meter
                .u64_counter("trunk.whisperer.transcription_errors")
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
        // split per-request (and per-talkgroup, via the parent span's tag).
        // Admission is priority-ordered: a more important talkgroup parked here
        // is woken ahead of a lower-priority one when a permit frees.
        let queue_start = std::time::Instant::now();
        let permit = Arc::clone(&self.semaphore)
            .acquire(priority)
            .instrument(tracing::info_span!("queue_wait"))
            .await;
        self.queue_wait_duration
            .record(queue_start.elapsed().as_secs_f64(), &wait_attrs);
        let gpu_idx = self.next_gpu.fetch_add(1, Ordering::Relaxed) % self.contexts.len();
        let ctx = Arc::clone(&self.contexts[gpu_idx]);
        let language = self.language.clone();

        let beam_size = self.beam_size;
        let start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            let mut state = ctx.create_state()?;

            let strategy = if beam_size > 0 {
                SamplingStrategy::BeamSearch {
                    beam_size,
                    patience: -1.0,
                }
            } else {
                SamplingStrategy::Greedy { best_of: 1 }
            };
            let mut params = FullParams::new(strategy);
            params.set_language(Some(&language));
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);

            state.full(params, &audio)?;

            let mut text = String::new();
            let n = state.full_n_segments();
            for i in 0..n {
                if let Some(segment) = state.get_segment(i) {
                    text.push_str(&segment.to_string());
                }
            }

            drop(permit);
            Ok::<String, anyhow::Error>(text)
        })
        .instrument(tracing::info_span!("decode"))
        .await?;
        let elapsed = start.elapsed().as_secs_f64();
        self.transcription_duration.record(elapsed, &attrs);

        match result {
            Ok(text) => {
                self.transcriptions_completed.add(1, &attrs);
                Ok(text)
            }
            Err(e) => {
                self.transcription_errors.add(1, &attrs);
                Err(e)
            }
        }
    }
}
