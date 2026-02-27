use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serenity::all::{CreateEmbed, CreateEmbedFooter};
use serenity::model::id::{ChannelId, MessageId};
use tokio::sync::{Mutex, mpsc};
use trunk_oxide_proto as proto;
use proto::types::{CallRowId, Freq, SignalDb};

use trunk_database::Database;

use crate::discord::DiscordClient;
use crate::grpc_whisperer::WhispererConnection;
use crate::resampler::Resampler;
use crate::tone_detect::{DetectedTone, NotchFilter, ToneDetector};

pub enum CallMessage {
    Audio { samples: Vec<u8>, sample_rate: i32 },
    Event(String),
    End(Option<Box<proto::CallData>>),
}

#[derive(Clone, Copy)]
pub struct CallHandlerConfig {
    pub buffer_duration_ms: u64,
    pub tone_detection: bool,
    pub tone_detection_duration_s: f32,
}

/// Max chars for embed description before we truncate from the front.
const MAX_DESCRIPTION_LEN: usize = 3800;

pub struct CallHandler {
    call_info: proto::CallInfo,
    rx: mpsc::Receiver<CallMessage>,
    whisperer: WhispererConnection,
    discord: Arc<DiscordClient>,
    buffer_duration_ms: u64,
    audio_buffer: Vec<f32>,
    resampler: Option<Resampler>,
    tone_detector: Option<ToneDetector>,
    notch_filter: Option<NotchFilter>,
    thread_id: ChannelId,

    // Single-embed state
    embed_message_id: Option<MessageId>,
    transcription_text: String,
    detected_tones: Vec<String>,
    events: Vec<String>,
    call_ended: bool,
    call_end_duration: Option<Duration>,
    call_end_signal: Option<SignalDb>,
    start_time_str: String,
    tag: String,

    // Database
    db: Arc<Mutex<Database>>,
    call_row_id: Option<CallRowId>,
}

impl CallHandler {
    pub fn new(
        call_info: proto::CallInfo,
        rx: mpsc::Receiver<CallMessage>,
        whisperer: WhispererConnection,
        discord: Arc<DiscordClient>,
        config: CallHandlerConfig,
        thread_id: ChannelId,
        db: Arc<Mutex<Database>>,
        call_row_id: Option<CallRowId>,
    ) -> Self {
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

        let start_time = DateTime::<Utc>::from_timestamp(call_info.start_time, 0)
            .unwrap_or_else(Utc::now);
        let start_time_str = start_time.format("%H:%M:%S UTC").to_string();

        let tag = if call_info.talkgroup_tag.is_empty() {
            call_info.talkgroup_display.clone()
        } else {
            call_info.talkgroup_tag.clone()
        };

        Self {
            call_info,
            rx,
            whisperer,
            discord,
            buffer_duration_ms: config.buffer_duration_ms,
            audio_buffer: Vec::new(),
            resampler: None,
            tone_detector,
            notch_filter: None,
            thread_id,
            embed_message_id: None,
            transcription_text: String::new(),
            detected_tones: Vec::new(),
            events: Vec::new(),
            call_ended: false,
            call_end_duration: None,
            call_end_signal: None,
            start_time_str,
            tag,
            db,
            call_row_id,
        }
    }

    pub async fn run(mut self) {
        if let Err(e) = self.send_initial_embed().await {
            tracing::error!(error = %e, "failed to post call embed");
            return;
        }

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
                    if let Err(e) = self.post_event(&text).await {
                        tracing::error!(error = %e, "event posting error");
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

    fn build_embed(&self) -> CreateEmbed {
        let title = if self.call_ended {
            format!("Call — {}", self.tag)
        } else {
            format!("Call — {} ...", self.tag)
        };

        let colour = if self.call_ended {
            if self.call_info.encrypted {
                0xe74c3c_u32 // red
            } else {
                0x2ecc71_u32 // green
            }
        } else {
            0x95a5a6_u32 // grey
        };

        let mut embed = CreateEmbed::new().title(title).colour(colour);

        // Description: transcription text (truncated from front if too long)
        if !self.transcription_text.is_empty() {
            let desc = if self.transcription_text.len() > MAX_DESCRIPTION_LEN {
                let skip = self.transcription_text.len() - MAX_DESCRIPTION_LEN;
                // Find next char boundary after skip point
                let start = self.transcription_text[skip..]
                    .char_indices()
                    .next()
                    .map_or(skip, |(i, _)| skip + i);
                format!("...{}", &self.transcription_text[start..])
            } else {
                self.transcription_text.clone()
            };
            embed = embed.description(desc);
        }

        // Always-present fields
        embed = embed
            .field("Frequency", Freq(self.call_info.freq).to_string(), true)
            .field("System", &self.call_info.short_name, true);

        if self.call_info.encrypted {
            embed = embed.field("Encrypted", "Yes", true);
        }

        // Tones (added when detected)
        if !self.detected_tones.is_empty() {
            embed = embed.field("Tones", self.detected_tones.join(", "), false);
        }

        // Signal/unit events
        if !self.events.is_empty() {
            embed = embed.field("Events", self.events.join("\n"), false);
        }

        // End-of-call fields
        if let Some(duration) = self.call_end_duration {
            if !duration.is_zero() {
                embed = embed.field("Duration", format!("{:.1}s", duration.as_secs_f64()), true);
            }
        }
        if let Some(signal) = self.call_end_signal {
            embed = embed.field("Signal", signal.to_string(), true);
        }

        embed = embed.footer(CreateEmbedFooter::new(&self.start_time_str));

        embed
    }

    async fn send_initial_embed(&mut self) -> Result<()> {
        let embed = self.build_embed();
        let msg_id = self.discord.send_embed(self.thread_id, embed).await?;
        self.embed_message_id = Some(msg_id);

        tracing::info!(thread = %self.thread_id, "call started");

        Ok(())
    }

    async fn update_embed(&mut self) -> Result<()> {
        if let Some(msg_id) = self.embed_message_id {
            let embed = self.build_embed();
            self.discord
                .edit_embed(self.thread_id, msg_id, embed)
                .await?;
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
        }

        let mut samples = samples;
        if let Some(filter) = &mut self.notch_filter {
            filter.process(&mut samples);
        }

        self.audio_buffer.extend_from_slice(&samples);

        if self
            .tone_detector
            .as_ref()
            .is_some_and(ToneDetector::is_complete)
        {
            self.finish_tone_detection().await?;
        }

        let buffer_duration_ms = usize::try_from(self.buffer_duration_ms)
            .map_err(|_| anyhow::anyhow!("buffer_duration_ms too large"))?;
        let buffer_threshold = buffer_duration_ms.saturating_mul(16);
        if self.audio_buffer.len() >= buffer_threshold {
            let audio: Vec<f32> = self.audio_buffer.drain(..).collect();
            self.transcribe_and_post(audio).await?;
        }

        Ok(())
    }

    async fn finish_tone_detection(&mut self) -> Result<()> {
        let Some(detector) = self.tone_detector.take() else {
            return Ok(());
        };

        let result = detector.finish();

        if !result.tones.is_empty() {
            self.detected_tones = result
                .tones
                .iter()
                .map(|t| format!("{t}"))
                .collect();
            self.update_embed().await?;

            // Strip dispatch tone samples from the front of the audio buffer
            let strip = result.tone_end_sample.min(self.audio_buffer.len());
            if strip > 0 {
                self.audio_buffer.drain(..strip);
            }

            // If CTCSS detected, create notch filter and retroactively filter buffered audio
            if let Some(DetectedTone::Ctcss { freq }) = result
                .tones
                .iter()
                .find(|t| matches!(t, DetectedTone::Ctcss { .. }))
            {
                let mut filter = NotchFilter::new(16000, *freq, 30.0);
                filter.process(&mut self.audio_buffer);
                self.notch_filter = Some(filter);
            }
        }

        Ok(())
    }

    async fn transcribe_and_post(&mut self, audio: Vec<f32>) -> Result<()> {
        if audio.is_empty() {
            return Ok(());
        }

        let text = self.whisperer.transcribe(audio).await?;
        let text = text.trim();
        if text.is_empty() || is_blank_audio(text) {
            if !self.transcription_text.is_empty() {
                // Indicate silence gap
                self.transcription_text.push_str(" ...");
                self.update_embed().await?;
            }
            return Ok(());
        }

        if !self.transcription_text.is_empty() {
            self.transcription_text.push(' ');
        }
        self.transcription_text.push_str(text);
        self.update_embed().await?;

        Ok(())
    }

    async fn post_event(&mut self, text: &str) -> Result<()> {
        self.events.push(text.to_string());
        self.update_embed().await?;
        Ok(())
    }

    async fn handle_end(&mut self, call_data: Option<Box<proto::CallData>>) -> Result<()> {
        // Flush resampler
        if let Some(resampler) = &mut self.resampler {
            let flushed = resampler.flush()?;
            self.audio_buffer.extend_from_slice(&flushed);
        }

        // Force-finish tone detection if still active (short call)
        if self.tone_detector.is_some() {
            self.finish_tone_detection().await?;
        }

        // Transcribe remaining audio
        if !self.audio_buffer.is_empty() {
            let audio: Vec<f32> = std::mem::take(&mut self.audio_buffer);
            self.transcribe_and_post(audio).await?;
        }

        // Update embed with end-of-call data
        self.call_ended = true;
        if let Some(data) = &call_data {
            self.call_end_duration = Some(Duration::from_secs_f64(data.length));
            self.call_end_signal = data.signal.map(SignalDb);
        }
        self.update_embed().await?;

        // Persist final call state to DB
        if let Some(row_id) = self.call_row_id {
            let update = trunk_database::CallUpdate {
                duration: self.call_end_duration,
                signal: self.call_end_signal,
                transcription: if self.transcription_text.is_empty() {
                    None
                } else {
                    Some(self.transcription_text.clone())
                },
                tones: if self.detected_tones.is_empty() {
                    None
                } else {
                    Some(self.detected_tones.clone())
                },
                events: if self.events.is_empty() {
                    None
                } else {
                    Some(self.events.clone())
                },
            };
            let db = self.db.lock().await;
            if let Err(e) = db.update_call(row_id, &update) {
                tracing::warn!(error = %e, "failed to update call in db");
            }
        }

        tracing::info!("call ended");

        Ok(())
    }
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

/// Check if Whisper returned a "blank audio" placeholder.
/// Only matches when the entire transcription is a placeholder — not when
/// real words like "silence" appear in normal speech.
fn is_blank_audio(text: &str) -> bool {
    let t = text
        .trim()
        .trim_start_matches(['[', '('])
        .trim_end_matches([']', ')'])
        .trim()
        .to_lowercase()
        .replace('_', " ");
    t == "blank audio"
        || t == "no speech"
        || t == "silence"
        || t == "..."
        || t == "."
}
