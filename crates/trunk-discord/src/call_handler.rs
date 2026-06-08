use std::sync::Arc;

use anyhow::Result;
use opentelemetry::KeyValue;
use proto::types::CallRowId;
use serenity::all::{CreateEmbed, CreateEmbedFooter};
use serenity::model::id::{ChannelId, MessageId};
use tokio::sync::{Mutex, mpsc};
use tracing::Instrument;
use trunk_oxide_proto as proto;

use trunk_database::Database;

use crate::call_state::{CallEffect, CallState, CallStateConfig, EmbedData};
use crate::discord::DiscordClient;
use crate::grpc_whisperer::WhispererConnection;
use crate::metrics::Metrics;
use crate::ogg_encoder::OggOpusEncoder;
use crate::resampler::Resampler;
use crate::tone_detect::{DetectedTone, NotchFilter, ToneDetector};

pub enum CallMessage {
    Audio { samples: Vec<u8>, sample_rate: i32 },
    Event(String),
    End(Option<Box<proto::CallData>>),
    PttEnd(Option<Box<proto::CallData>>),
    PttStart(Box<proto::CallInfo>),
}

/// Work item handed to the per-call transcription worker. Audio chunks are the
/// only slow item; `Boundary`/`End` are ordering markers that ride the same
/// FIFO so PTT separators and finalization stay correctly sequenced relative to
/// the transcription text that precedes them.
enum TranscribeJob {
    Audio(Vec<f32>),
    Boundary(Option<Box<proto::CallData>>),
    End(Option<Box<proto::CallData>>),
}

/// Result handed back from the transcription worker, applied in FIFO order by
/// the handler's main loop. `Boundary`/`End` are passed straight through.
enum TranscribeResult {
    Text(String),
    Boundary(Option<Box<proto::CallData>>),
    End(Option<Box<proto::CallData>>),
}

/// Per-call identity used to label whisperer transcription metrics.
struct TranscribeIdentity {
    short_name: String,
    is_conventional: bool,
    talkgroup: i64,
}

#[derive(Clone, Copy)]
pub struct CallHandlerConfig {
    pub buffer_duration_ms: u64,
    pub tone_detection: bool,
    pub tone_detection_duration_s: f32,
    pub audio_attachment: bool,
}

pub struct CallHandler {
    state: CallState,
    /// Inbound dispatcher messages. `Option` so [`CallHandler::run`] can move
    /// the receiver into a local (the `select!` loop needs `&mut self` in arm
    /// bodies, which would conflict with borrowing `self.rx`).
    rx: Option<mpsc::Receiver<CallMessage>>,
    /// Enqueues audio/markers for the transcription worker. Unbounded so the
    /// handler never blocks here — a wedged transcription must not back-pressure
    /// the responsive loop (that is the whole point of this design).
    job_tx: mpsc::UnboundedSender<TranscribeJob>,
    /// Receiver half handed to the worker in [`CallHandler::run`].
    job_rx: Option<mpsc::UnboundedReceiver<TranscribeJob>>,
    whisperer: WhispererConnection,
    discord: Arc<DiscordClient>,
    config: CallHandlerConfig,
    audio_buffer: Vec<f32>,
    resampler: Option<Resampler>,
    tone_detector: Option<ToneDetector>,
    notch_filter: Option<NotchFilter>,
    post_channel: ChannelId,
    ogg_encoder: Option<OggOpusEncoder>,

    embed_message_id: Option<MessageId>,

    /// Set once an end has been initiated (explicit `End` message or a dropped
    /// dispatcher channel), so the synthesized-end path fires at most once.
    ending: bool,

    metrics: Arc<Metrics>,

    db: Arc<Mutex<Database>>,
    call_row_id: Option<CallRowId>,

    // Identity for transcription attribution (whisperer metric labels + span).
    short_name: String,
    is_conventional: bool,
    talkgroup: i64,
}

impl CallHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        call_info: &proto::CallInfo,
        rx: mpsc::Receiver<CallMessage>,
        whisperer: WhispererConnection,
        discord: Arc<DiscordClient>,
        config: CallHandlerConfig,
        post_channel: ChannelId,
        db: Arc<Mutex<Database>>,
        call_row_id: Option<CallRowId>,
        metrics: Arc<Metrics>,
    ) -> Result<Self> {
        let detect_ctcss = config.tone_detection && call_info.is_conventional;
        let tone_detector = if config.tone_detection {
            Some(if detect_ctcss {
                ToneDetector::with_ctcss(16000, config.tone_detection_duration_s, true)
            } else {
                ToneDetector::new(16000, config.tone_detection_duration_s)
            })
        } else {
            None
        };

        let ogg_encoder = if config.audio_attachment {
            Some(OggOpusEncoder::new()?)
        } else {
            None
        };

        let buffer_threshold = buffer_threshold(config.buffer_duration_ms);

        let state = CallState::new(
            call_info,
            CallStateConfig {
                buffer_threshold,
                audio_attachment: config.audio_attachment,
            },
        );

        let (job_tx, job_rx) = mpsc::unbounded_channel();

        Ok(Self {
            state,
            rx: Some(rx),
            job_tx,
            job_rx: Some(job_rx),
            whisperer,
            discord,
            config,
            audio_buffer: Vec::new(),
            resampler: None,
            tone_detector,
            notch_filter: None,
            post_channel,
            ogg_encoder,
            embed_message_id: None,
            ending: false,
            metrics,
            db,
            call_row_id,
            short_name: call_info.short_name.clone(),
            is_conventional: call_info.is_conventional,
            talkgroup: call_info.talkgroup,
        })
    }

    pub async fn run(mut self) {
        let effects = self.state.on_start();
        if let Err(e) = self.process_effects(effects).await {
            tracing::error!(error = %e, "failed to post call embed");
            return;
        }
        tracing::info!(thread = %self.post_channel, "call started");

        // Spawn the transcription worker on the "slow lane". It owns the gRPC
        // call to whisper (bounded at 300s but potentially wedged) so the main
        // loop below never blocks on it and always stays responsive to the
        // dispatcher — which is what kept a wedged transcription from getting
        // the handler dropped (note_stuck) and orphaning the embed.
        //
        // Instrumenting the worker with the current ("call") span keeps W3C
        // trace-context propagation working: `WhispererConnection::transcribe`
        // reads `Span::current()` to parent the whisperer's transcribe span.
        let mut rx = self.rx.take().expect("rx taken once");
        let job_rx = self.job_rx.take().expect("job_rx taken once");
        let (result_tx, mut result_rx) = mpsc::channel::<TranscribeResult>(64);
        tokio::spawn(
            transcription_worker(
                self.whisperer.clone(),
                Arc::clone(&self.metrics),
                TranscribeIdentity {
                    short_name: self.short_name.clone(),
                    is_conventional: self.is_conventional,
                    talkgroup: self.talkgroup,
                },
                job_rx,
                result_tx,
            )
            .instrument(tracing::Span::current()),
        );

        let mut rx_open = true;
        loop {
            tokio::select! {
                // Apply transcription results in FIFO order. `None` means the
                // worker exited (job channel closed) — finalize with whatever we
                // have so the embed never lingers in its grey in-progress state.
                result = result_rx.recv() => {
                    if let Some(r) = result {
                        if self.apply_result(r).await {
                            return; // End marker → finalized
                        }
                    } else {
                        // Worker exited before an End marker — finalize anyway.
                        self.finalize(None).await;
                        return;
                    }
                }
                // Dispatcher messages. Only polled while the channel is open;
                // once it closes we synthesize an end (insurance against being
                // dropped from the dispatcher without an explicit End).
                msg = rx.recv(), if rx_open => {
                    if let Some(m) = msg {
                        self.handle_message(m).await;
                    } else {
                        rx_open = false;
                        if !self.ending {
                            self.ending = true;
                            self.begin_end(None).await;
                        }
                    }
                }
            }
        }
    }

    /// Dispatch a single inbound message. Transcription-bearing paths only
    /// *enqueue* work for the worker; they never await whisper inline.
    async fn handle_message(&mut self, msg: CallMessage) {
        match msg {
            CallMessage::Audio {
                samples,
                sample_rate,
            } => {
                if let Err(e) = self.handle_audio(&samples, sample_rate).await {
                    tracing::error!(error = %e, "audio handling error");
                }
            }
            CallMessage::Event(text) => {
                let effects = self.state.on_event(text);
                if let Err(e) = self.process_effects(effects).await {
                    tracing::error!(error = %e, "event posting error");
                }
            }
            CallMessage::PttEnd(call_data) => {
                if let Err(e) = self.handle_ptt_end(call_data).await {
                    tracing::error!(error = %e, "PTT end handling error");
                }
            }
            CallMessage::PttStart(call_info) => {
                let effects = self.state.on_ptt_start(call_info.as_ref());
                if let Err(e) = self.process_effects(effects).await {
                    tracing::error!(error = %e, "PTT start handling error");
                }
            }
            CallMessage::End(call_data) => {
                self.ending = true;
                self.begin_end(call_data).await;
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn process_effects(&mut self, effects: Vec<CallEffect>) -> Result<()> {
        // Use a queue to avoid async recursion (Transcribe → on_transcription → EditEmbed)
        let mut queue: std::collections::VecDeque<CallEffect> = effects.into();
        while let Some(effect) = queue.pop_front() {
            match effect {
                CallEffect::SendEmbed { embed } => {
                    let serenity_embed = embed_data_to_serenity(&embed);
                    let start = std::time::Instant::now();
                    let msg_id = self
                        .discord
                        .send_embed(self.post_channel, serenity_embed)
                        .await?;
                    self.metrics.discord_api_duration.record(
                        start.elapsed().as_secs_f64(),
                        &[KeyValue::new("operation", "send_embed")],
                    );
                    self.embed_message_id = Some(msg_id);
                }
                CallEffect::EditEmbed { embed } => {
                    if let Some(msg_id) = self.embed_message_id {
                        let serenity_embed = embed_data_to_serenity(&embed);
                        let start = std::time::Instant::now();
                        self.discord
                            .edit_embed(self.post_channel, msg_id, serenity_embed)
                            .await?;
                        self.metrics.discord_api_duration.record(
                            start.elapsed().as_secs_f64(),
                            &[KeyValue::new("operation", "edit_embed")],
                        );
                    }
                }
                CallEffect::DeleteEmbed => {
                    if let Some(msg_id) = self.embed_message_id {
                        let start = std::time::Instant::now();
                        if let Err(e) = self.discord.delete_message(self.post_channel, msg_id).await
                        {
                            tracing::warn!(error = %e, "failed to delete empty call embed");
                        }
                        self.metrics.discord_api_duration.record(
                            start.elapsed().as_secs_f64(),
                            &[KeyValue::new("operation", "delete_embed")],
                        );
                    }
                }
                CallEffect::AttachAudio {
                    embed,
                    ogg_data,
                    filename,
                } => {
                    if let Some(msg_id) = self.embed_message_id {
                        let serenity_embed = embed_data_to_serenity(&embed);
                        let start = std::time::Instant::now();
                        if let Err(e) = self
                            .discord
                            .edit_embed_with_attachment(
                                self.post_channel,
                                msg_id,
                                serenity_embed,
                                ogg_data,
                                &filename,
                            )
                            .await
                        {
                            tracing::warn!(error = %e, "failed to attach audio to call embed");
                        }
                        self.metrics.discord_api_duration.record(
                            start.elapsed().as_secs_f64(),
                            &[KeyValue::new("operation", "attach_audio")],
                        );
                    }
                }
                CallEffect::Transcribe { audio } => {
                    // Hand the audio to the slow-lane worker; the resulting text
                    // is applied later (in FIFO order) via `apply_result`. The
                    // send is on an unbounded channel and never blocks.
                    if !audio.is_empty() {
                        self.enqueue_job(TranscribeJob::Audio(audio));
                    }
                }
                CallEffect::UpdateDb { update } => {
                    if let Some(row_id) = self.call_row_id {
                        let db_update = trunk_database::CallUpdate {
                            duration: update.duration,
                            signal: update.signal,
                            transcription: update.transcription,
                            tones: update.tones,
                            events: update.events,
                        };
                        let db = self.db.lock().await;
                        if let Err(e) = db.update_call(row_id, &db_update) {
                            tracing::warn!(error = %e, "failed to update call in db");
                        }
                    }
                }
                CallEffect::Log { message } => {
                    tracing::info!("{message}");
                }
            }
        }
        Ok(())
    }

    async fn handle_audio(&mut self, raw_samples: &[u8], sample_rate: i32) -> Result<()> {
        let decoded = decode_audio(raw_samples);

        let samples = if sample_rate == 16000 {
            decoded
        } else {
            let input_rate = u32::try_from(sample_rate)
                .map_err(|_| anyhow::anyhow!("invalid input sample rate: {sample_rate}"))?;
            let resampler = self.resampler.get_or_insert_with(|| {
                Resampler::new(input_rate).expect("failed to create resampler")
            });
            resampler.process(&decoded)?
        };

        if let Some(detector) = &mut self.tone_detector {
            detector.process(&samples);

            // Check for CTCSS early-fire to start notch filtering as soon as possible
            if let Some(ctcss_freq) = detector.take_early_ctcss()
                && self.notch_filter.is_none()
            {
                tracing::info!(freq = ctcss_freq, "CTCSS early-fire, starting notch filter");
                let filter = NotchFilter::new(16000, ctcss_freq, 30.0);
                self.notch_filter = Some(filter);
            }
        }

        let mut samples = samples;
        if let Some(filter) = &mut self.notch_filter {
            filter.process(&mut samples);
        }

        // Compute energy for silence detection
        let energy: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
        let sample_count: u32 = samples.len().try_into().expect("audio chunk exceeds u32");

        if let Some(enc) = &mut self.ogg_encoder {
            enc.process(&samples)?;
        }

        self.audio_buffer.extend_from_slice(&samples);

        if self
            .tone_detector
            .as_ref()
            .is_some_and(ToneDetector::is_complete)
        {
            self.finish_tone_detection().await?;
        }

        let buffer_len = self.audio_buffer.len();
        let audio_drain: Vec<f32> =
            if buffer_len >= buffer_threshold(self.config.buffer_duration_ms) {
                self.audio_buffer.drain(..).collect()
            } else {
                Vec::new()
            };

        let effects = self
            .state
            .on_audio_processed(sample_count, energy, buffer_len, audio_drain);
        self.process_effects(effects).await?;

        Ok(())
    }

    async fn finish_tone_detection(&mut self) -> Result<()> {
        let Some(detector) = self.tone_detector.take() else {
            return Ok(());
        };

        let result = detector.finish();

        if !result.tones.is_empty() {
            // Extract CTCSS freq before moving tones
            let ctcss_freq = result.tones.iter().find_map(|t| match t {
                DetectedTone::Ctcss { freq } => Some(*freq),
                _ => None,
            });

            let tones = result.tones;

            // Strip dispatch tone samples from the front of the audio buffer
            let strip = result.tone_end_sample.min(self.audio_buffer.len());
            if strip > 0 {
                self.audio_buffer.drain(..strip);
            }

            // If CTCSS detected and notch filter not already active (via early-fire),
            // create one and retroactively filter buffered audio
            if let Some(freq) = ctcss_freq
                && self.notch_filter.is_none()
            {
                let mut filter = NotchFilter::new(16000, freq, 30.0);
                filter.process(&mut self.audio_buffer);
                self.notch_filter = Some(filter);
            }

            let effects = self.state.on_tones_detected(tones);
            self.process_effects(effects).await?;
        }

        Ok(())
    }

    /// Enqueue a job for the transcription worker. The channel is unbounded so
    /// this never blocks; if the worker has already exited the job is dropped.
    fn enqueue_job(&self, job: TranscribeJob) {
        let _ = self.job_tx.send(job);
    }

    /// Flush the resampler tail and force-finish tone detection into
    /// `audio_buffer`. Shared by the PTT-boundary and call-end paths.
    async fn flush_dsp(&mut self) -> Result<()> {
        if let Some(resampler) = &mut self.resampler {
            let mut flushed = resampler.flush()?;
            if let Some(filter) = &mut self.notch_filter {
                filter.process(&mut flushed);
            }
            if let Some(enc) = &mut self.ogg_encoder {
                enc.process(&flushed)?;
            }
            self.audio_buffer.extend_from_slice(&flushed);
        }
        if self.tone_detector.is_some() {
            self.finish_tone_detection().await?;
        }
        Ok(())
    }

    /// Drain whatever remains in `audio_buffer` and enqueue it for transcription.
    fn enqueue_remaining_audio(&mut self) {
        if !self.audio_buffer.is_empty() {
            let audio: Vec<f32> = std::mem::take(&mut self.audio_buffer);
            if !audio.is_empty() {
                self.enqueue_job(TranscribeJob::Audio(audio));
            }
        }
    }

    /// Handle a PTT boundary within a coalesced call. Flushes audio for
    /// transcription and enqueues a boundary marker, but keeps the handler
    /// alive. The boundary (separator + duration/signal accumulation) is applied
    /// later — after the worker has transcribed this PTT's audio — so the PTT
    /// separator lands after that PTT's text, preserving ordering.
    async fn handle_ptt_end(&mut self, call_data: Option<Box<proto::CallData>>) -> Result<()> {
        if let Err(e) = self.flush_dsp().await {
            tracing::warn!(error = %e, "PTT-end flush failed");
        }
        self.enqueue_remaining_audio();
        self.enqueue_job(TranscribeJob::Boundary(call_data));

        // Reset resampler for next PTT (may have different sample rate).
        self.resampler = None;
        Ok(())
    }

    /// Begin finalization: flush DSP, enqueue the trailing audio, then enqueue an
    /// End marker. The actual terminal transition (`finalize`) runs once the
    /// worker has drained everything ahead of that marker — see `apply_result`.
    async fn begin_end(&mut self, call_data: Option<Box<proto::CallData>>) {
        if let Err(e) = self.flush_dsp().await {
            tracing::warn!(error = %e, "pre-finalization flush failed, proceeding to end call");
        }
        self.enqueue_remaining_audio();
        self.enqueue_job(TranscribeJob::End(call_data));
    }

    /// Apply one worker result in FIFO order. Returns `true` once the call has
    /// been finalized, signalling the run loop to exit.
    async fn apply_result(&mut self, result: TranscribeResult) -> bool {
        match result {
            TranscribeResult::Text(text) => {
                let effects = self.state.on_transcription(&text);
                if let Err(e) = self.process_effects(effects).await {
                    tracing::error!(error = %e, "transcription posting error");
                }
                false
            }
            TranscribeResult::Boundary(call_data) => {
                let effects = self.state.on_ptt_end(call_data.as_deref());
                if let Err(e) = self.process_effects(effects).await {
                    tracing::error!(error = %e, "PTT boundary posting error");
                }
                false
            }
            TranscribeResult::End(call_data) => {
                self.finalize(call_data).await;
                true
            }
        }
    }

    /// Finalize the call: finish the OGG stream and run the state machine's
    /// terminal transition (turns the embed green / attaches audio / deletes the
    /// embed / persists to the DB). This is the one place an embed is closed
    /// out, and it runs for *every* call exit — including being dropped by the
    /// dispatcher or the worker dying — so an embed can never be orphaned grey.
    async fn finalize(&mut self, call_data: Option<Box<proto::CallData>>) {
        // All audio was fed to the encoder synchronously at ingestion and in
        // `flush_dsp`, so nothing further is encoded after this point.
        let ogg_data = self
            .ogg_encoder
            .take()
            .and_then(|encoder| match encoder.finish() {
                Ok(data) => Some(data),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to encode OGG audio");
                    None
                }
            });

        let effects = self.state.on_end(call_data.as_deref(), ogg_data);
        if let Err(e) = self.process_effects(effects).await {
            tracing::error!(error = %e, "call end finalization error");
        }
    }
}

/// Per-call transcription worker — the "slow lane". Consumes jobs in FIFO order
/// and emits results in the same order, so transcription text, PTT separators,
/// and finalization stay correctly sequenced. It owns the (300s-bounded, but
/// possibly wedged) gRPC transcription call, keeping it off the handler's
/// responsive loop. Exits when the job channel closes (handler dropped) or the
/// handler stops receiving results.
async fn transcription_worker(
    whisperer: WhispererConnection,
    metrics: Arc<Metrics>,
    identity: TranscribeIdentity,
    mut job_rx: mpsc::UnboundedReceiver<TranscribeJob>,
    result_tx: mpsc::Sender<TranscribeResult>,
) {
    while let Some(job) = job_rx.recv().await {
        let result = match job {
            TranscribeJob::Audio(audio) => {
                TranscribeResult::Text(transcribe_one(&whisperer, &metrics, &identity, audio).await)
            }
            TranscribeJob::Boundary(call_data) => TranscribeResult::Boundary(call_data),
            TranscribeJob::End(call_data) => TranscribeResult::End(call_data),
        };
        if result_tx.send(result).await.is_err() {
            break; // handler gone
        }
    }
}

/// Transcribe a single audio chunk, recording client-side latency and timeout
/// metrics. A timeout or RPC error yields an empty transcript (treated like
/// silence downstream); timeouts are counted separately.
async fn transcribe_one(
    whisperer: &WhispererConnection,
    metrics: &Metrics,
    identity: &TranscribeIdentity,
    audio: Vec<f32>,
) -> String {
    if audio.is_empty() {
        return String::new();
    }
    let start = std::time::Instant::now();
    let outcome = whisperer
        .transcribe(
            audio,
            &identity.short_name,
            identity.is_conventional,
            identity.talkgroup,
        )
        .await;
    metrics
        .transcription_duration
        .record(start.elapsed().as_secs_f64(), &[]);
    match outcome {
        Ok(Some(text)) => text,
        Ok(None) => {
            metrics.transcriptions_timed_out.add(1, &[]);
            String::new()
        }
        Err(e) => {
            tracing::error!(error = %e, "transcription request failed");
            String::new()
        }
    }
}

fn buffer_threshold(buffer_duration_ms: u64) -> usize {
    usize::try_from(buffer_duration_ms)
        .unwrap_or(usize::MAX)
        .saturating_mul(16)
}

fn decode_audio(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|chunk| {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            f32::from(sample) / 32768.0
        })
        .collect()
}

/// Convert pure [`EmbedData`] to a serenity [`CreateEmbed`].
fn embed_data_to_serenity(data: &EmbedData) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .title(&data.title)
        .colour(data.colour)
        .footer(CreateEmbedFooter::new(&data.footer));

    if let Some(desc) = &data.description {
        embed = embed.description(desc);
    }

    for field in &data.fields {
        embed = embed.field(&field.name, &field.value, field.inline);
    }

    embed
}
