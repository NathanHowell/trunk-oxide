use std::sync::Arc;

use anyhow::Result;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use parakeet_rs::{ExecutionConfig, ExecutionProvider, Parakeet, ParakeetTDT, Transcriber as _};
use tracing::Instrument;

use crate::config::ParakeetConfig;
use crate::priority_semaphore::PrioritySemaphore;

/// Wraps either a TDT model (`NeMo` export: encoder + decoder + vocab.txt) or a
/// standard Parakeet model (`HuggingFace` export: model.onnx + tokenizer.json).
/// Auto-detected from the model directory contents.
enum ParakeetModel {
    Tdt(Box<ParakeetTDT>),
    Standard(Box<Parakeet>),
}

/// Recognised text and where it sits in the submitted audio (seconds). The
/// timing comes from the model's token timestamps and is `None` when there are
/// no tokens.
pub struct Transcript {
    pub text: String,
    pub first_start_s: Option<f32>,
    pub last_end_s: Option<f32>,
}

impl From<parakeet_rs::TranscriptionResult> for Transcript {
    fn from(r: parakeet_rs::TranscriptionResult) -> Self {
        Self {
            first_start_s: r.tokens.first().map(|t| t.start),
            last_end_s: r.tokens.last().map(|t| t.end),
            text: r.text,
        }
    }
}

impl ParakeetModel {
    fn transcribe_samples(&mut self, audio: Vec<f32>) -> parakeet_rs::Result<Transcript> {
        match self {
            Self::Tdt(m) => m
                .transcribe_samples(audio, 16000, 1, None)
                .map(Transcript::from),
            Self::Standard(m) => m
                .transcribe_samples(audio, 16000, 1, None)
                .map(Transcript::from),
        }
    }
}

pub struct ParakeetTranscriber {
    // A single model instance behind a mutex: ONNX decode is serialized here, so
    // the priority gate (not this mutex) is what decides admission order.
    model: Arc<tokio::sync::Mutex<ParakeetModel>>,
    semaphore: Arc<PrioritySemaphore>,
    max_concurrent: usize,
    transcription_duration: Histogram<f64>,
    queue_wait_duration: Histogram<f64>,
    audio_input_duration: Histogram<f64>,
    transcriptions_completed: Counter<u64>,
    transcription_errors: Counter<u64>,
}

impl ParakeetTranscriber {
    pub fn new(config: &ParakeetConfig) -> Result<Self> {
        let exec_config =
            ExecutionConfig::new().with_execution_provider(select_provider(&config.provider));

        let model_dir = std::path::Path::new(&config.model_dir);
        let model = if model_dir.join("vocab.txt").exists() {
            tracing::info!(model_dir = %config.model_dir, "detected NeMo TDT format");
            ParakeetModel::Tdt(Box::new(
                ParakeetTDT::from_pretrained(model_dir, Some(exec_config))
                    .map_err(|e| anyhow::anyhow!("parakeet TDT init failed: {e}"))?,
            ))
        } else {
            tracing::info!(model_dir = %config.model_dir, "detected HuggingFace format");
            ParakeetModel::Standard(Box::new(
                Parakeet::from_pretrained(model_dir, Some(exec_config))
                    .map_err(|e| anyhow::anyhow!("parakeet init failed: {e}"))?,
            ))
        };

        tracing::info!(
            model_dir = %config.model_dir,
            provider = %config.provider,
            max_concurrent = config.max_concurrent,
            "parakeet transcriber initialized"
        );

        let meter = opentelemetry::global::meter(env!("CARGO_PKG_NAME"));

        Ok(Self {
            model: Arc::new(tokio::sync::Mutex::new(model)),
            semaphore: PrioritySemaphore::new(config.max_concurrent),
            max_concurrent: config.max_concurrent,
            transcription_duration: meter
                .f64_histogram("trunk.parakeet.transcription_duration_seconds")
                .with_description(
                    "Time spent decoding audio in parakeet (after acquiring a permit)",
                )
                .build(),
            queue_wait_duration: meter
                .f64_histogram("trunk.parakeet.queue_wait_seconds")
                .with_description(
                    "Time a request waited for a concurrency permit before decoding (backlog)",
                )
                .build(),
            audio_input_duration: meter
                .f64_histogram("trunk.parakeet.audio_input_duration_seconds")
                .build(),
            transcriptions_completed: meter
                .u64_counter("trunk.parakeet.transcriptions_completed")
                .build(),
            transcription_errors: meter
                .u64_counter("trunk.parakeet.transcription_errors")
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
    ) -> Result<Transcript> {
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
            let transcript = model
                .blocking_lock()
                .transcribe_samples(audio)
                .map_err(|e| anyhow::anyhow!("parakeet transcription failed: {e}"))?;
            drop(permit);
            Ok::<Transcript, anyhow::Error>(transcript)
        })
        .instrument(tracing::info_span!("decode"))
        .await?;
        let elapsed = start.elapsed().as_secs_f64();
        self.transcription_duration.record(elapsed, &attrs);

        // Throughput/errors carry priority_bucket (like queue_wait above) so the
        // served rate can be split per tier. Decode duration stays on `attrs`
        // (no bucket): decode cost is priority-independent.
        match result {
            Ok(transcript) => {
                self.transcriptions_completed.add(1, &wait_attrs);
                Ok(transcript)
            }
            Err(e) => {
                self.transcription_errors.add(1, &wait_attrs);
                Err(e)
            }
        }
    }
}

fn select_provider(provider: &str) -> ExecutionProvider {
    if provider != "cpu" {
        tracing::warn!(provider, "unsupported execution provider; using CPU");
    }
    ExecutionProvider::Cpu
}
