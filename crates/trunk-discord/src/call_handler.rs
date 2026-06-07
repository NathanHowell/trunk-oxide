use std::sync::Arc;

use anyhow::Result;
use opentelemetry::KeyValue;
use proto::types::CallRowId;
use serenity::all::{CreateEmbed, CreateEmbedFooter};
use serenity::model::id::{ChannelId, MessageId};
use tokio::sync::{Mutex, mpsc};
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

#[derive(Clone, Copy)]
pub struct CallHandlerConfig {
    pub buffer_duration_ms: u64,
    pub tone_detection: bool,
    pub tone_detection_duration_s: f32,
    pub audio_attachment: bool,
}

pub struct CallHandler {
    state: CallState,
    rx: mpsc::Receiver<CallMessage>,
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

    metrics: Arc<Metrics>,

    db: Arc<Mutex<Database>>,
    call_row_id: Option<CallRowId>,
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

        Ok(Self {
            state,
            rx,
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
            metrics,
            db,
            call_row_id,
        })
    }

    pub async fn run(mut self) {
        let effects = self.state.on_start();
        if let Err(e) = self.process_effects(effects).await {
            tracing::error!(error = %e, "failed to post call embed");
            return;
        }
        tracing::info!(thread = %self.post_channel, "call started");

        while let Some(msg) = self.rx.recv().await {
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
                    if let Err(e) = self.handle_end(call_data).await {
                        tracing::error!(error = %e, "call end handling error");
                    }
                    return;
                }
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
                    if !audio.is_empty() {
                        let text = self.transcribe_recorded(audio).await?;
                        let followup = self.state.on_transcription(&text);
                        for (i, e) in followup.into_iter().enumerate() {
                            queue.insert(i, e);
                        }
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

    /// Transcribe `audio`, recording client-side latency and timeout metrics.
    ///
    /// Centralises instrumentation so every transcription path (mid-call,
    /// PTT-end, call-end) is measured identically. A timeout returns an empty
    /// transcript (treated like silence) but is counted separately.
    // Takes `&mut self` (not `&self`) so the returned future is `Send`: a shared
    // `&CallHandler` would require `CallHandler: Sync`, which the resampler is not.
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn transcribe_recorded(&mut self, audio: Vec<f32>) -> Result<String> {
        // Clone the cheap (Arc-backed) handles so nothing borrows `self` across
        // the await.
        let whisperer = self.whisperer.clone();
        let duration = self.metrics.transcription_duration.clone();
        let timed_out = self.metrics.transcriptions_timed_out.clone();

        let start = std::time::Instant::now();
        let outcome = whisperer.transcribe(audio).await?;
        duration.record(start.elapsed().as_secs_f64(), &[]);
        let text = outcome.unwrap_or_else(|| {
            timed_out.add(1, &[]);
            String::new()
        });
        Ok(text)
    }

    /// Handle a PTT boundary within a coalesced call.
    /// Flushes audio for transcription and marks the boundary, but keeps the handler alive.
    async fn handle_ptt_end(&mut self, call_data: Option<Box<proto::CallData>>) -> Result<()> {
        // Flush resampler
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

        // Force-finish tone detection if still active (short call)
        if self.tone_detector.is_some() {
            self.finish_tone_detection().await?;
        }

        // Transcribe remaining audio
        if !self.audio_buffer.is_empty() {
            let audio: Vec<f32> = std::mem::take(&mut self.audio_buffer);
            if !audio.is_empty() {
                let text = self.transcribe_recorded(audio).await?;
                let effects = self.state.on_transcription(&text);
                self.process_effects(effects).await?;
            }
        }

        // Let state machine record PTT boundary
        let effects = self.state.on_ptt_end(call_data.as_deref());
        self.process_effects(effects).await?;

        // Reset resampler for next PTT (may have different sample rate)
        self.resampler = None;

        Ok(())
    }

    async fn handle_end(&mut self, call_data: Option<Box<proto::CallData>>) -> Result<()> {
        // Best-effort pre-finalization: failures here must not prevent on_end().
        if let Err(e) = self.flush_before_end().await {
            tracing::warn!(error = %e, "pre-finalization failed, proceeding to end call");
        }

        // Finalize OGG encoder
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

        // Always let the state machine finalize — this is what turns the embed
        // green / deletes it / persists to DB. Skipping this orphans the embed.
        let effects = self.state.on_end(call_data.as_deref(), ogg_data);
        self.process_effects(effects).await?;

        Ok(())
    }

    /// Flush resampler, finish tone detection, and transcribe remaining audio.
    /// Called as best-effort before `on_end`; errors are logged, not fatal.
    async fn flush_before_end(&mut self) -> Result<()> {
        // Flush resampler
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

        // Force-finish tone detection if still active (short call)
        if self.tone_detector.is_some() {
            self.finish_tone_detection().await?;
        }

        // Transcribe remaining audio
        if !self.audio_buffer.is_empty() {
            let audio: Vec<f32> = std::mem::take(&mut self.audio_buffer);
            if !audio.is_empty() {
                let text = self.transcribe_recorded(audio).await?;
                let effects = self.state.on_transcription(&text);
                self.process_effects(effects).await?;
            }
        }

        Ok(())
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
