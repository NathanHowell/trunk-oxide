use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use tokio::sync::Semaphore;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub struct Transcriber {
    contexts: Vec<Arc<WhisperContext>>,
    semaphore: Arc<Semaphore>,
    next_gpu: AtomicUsize,
    language: String,
    beam_size: usize,
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
            if config.beam_size > 0 { "beam search" } else { "greedy" }
        );

        Ok(Self {
            contexts,
            semaphore: Arc::new(Semaphore::new(config.max_concurrent)),
            next_gpu: AtomicUsize::new(0),
            language: config.language.clone(),
            beam_size: config.beam_size,
        })
    }

    pub async fn transcribe(&self, audio: Vec<f32>) -> Result<String> {
        let permit = Arc::clone(&self.semaphore).acquire_owned().await?;
        let gpu_idx = self.next_gpu.fetch_add(1, Ordering::Relaxed) % self.contexts.len();
        let ctx = Arc::clone(&self.contexts[gpu_idx]);
        let language = self.language.clone();

        let beam_size = self.beam_size;
        let result = tokio::task::spawn_blocking(move || {
            let mut state = ctx.create_state()?;

            let strategy = if beam_size > 0 {
                SamplingStrategy::BeamSearch {
                    beam_size: beam_size as std::ffi::c_int,
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
        .await??;

        Ok(result)
    }
}
