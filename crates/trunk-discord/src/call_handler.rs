use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serenity::model::id::{ChannelId, MessageId};
use tokio::sync::mpsc;
use trunk_oxide_proto as proto;

use crate::discord::DiscordClient;
use crate::dispatcher::CallKey;
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

pub struct CallHandler {
    key: CallKey,
    call_info: proto::CallInfo,
    rx: mpsc::Receiver<CallMessage>,
    whisperer: WhispererConnection,
    discord: Arc<DiscordClient>,
    buffer_duration_ms: u64,
    audio_buffer: Vec<f32>,
    resampler: Option<Resampler>,
    tone_detector: Option<ToneDetector>,
    notch_filter: Option<NotchFilter>,
    thread_id: Option<ChannelId>,
    current_message_id: Option<MessageId>,
    current_message_text: String,
}

impl CallHandler {
    pub fn new(
        key: CallKey,
        call_info: proto::CallInfo,
        rx: mpsc::Receiver<CallMessage>,
        whisperer: WhispererConnection,
        discord: Arc<DiscordClient>,
        config: CallHandlerConfig,
        thread_id: ChannelId,
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

        Self {
            key,
            call_info,
            rx,
            whisperer,
            discord,
            buffer_duration_ms: config.buffer_duration_ms,
            audio_buffer: Vec::new(),
            resampler: None,
            tone_detector,
            notch_filter: None,
            thread_id: Some(thread_id),
            current_message_id: None,
            current_message_text: String::new(),
        }
    }

    pub async fn run(mut self) {
        if let Err(e) = self.post_call_start().await {
            tracing::error!(
                talkgroup = self.key.talkgroup,
                error = %e,
                "failed to post call start"
            );
            return;
        }

        while let Some(msg) = self.rx.recv().await {
            match msg {
                CallMessage::Audio {
                    samples,
                    sample_rate,
                } => {
                    if let Err(e) = self.handle_audio(&samples, sample_rate).await {
                        tracing::error!(
                            talkgroup = self.key.talkgroup,
                            error = %e,
                            "audio handling error"
                        );
                    }
                }
                CallMessage::Event(text) => {
                    if let Err(e) = self.post_event(&text).await {
                        tracing::error!(
                            talkgroup = self.key.talkgroup,
                            error = %e,
                            "event posting error"
                        );
                    }
                }
                CallMessage::End(call_data) => {
                    if let Err(e) = self.handle_end(call_data).await {
                        tracing::error!(
                            talkgroup = self.key.talkgroup,
                            error = %e,
                            "call end handling error"
                        );
                    }
                    return;
                }
            }
        }
    }

    async fn post_call_start(&mut self) -> Result<()> {
        let thread_id = self.thread_id.expect("thread_id must be set before run()");

        let start_time =
            DateTime::<Utc>::from_timestamp(self.call_info.start_time, 0).unwrap_or_else(Utc::now);
        let time_str = start_time.format("%H:%M:%S").to_string();

        let info = format!(
            "---\n**Call @ {time_str}** | Freq: {:.4} MHz | System: {} | Encrypted: {}",
            self.call_info.freq / 1_000_000.0,
            self.call_info.short_name,
            if self.call_info.encrypted {
                "Yes"
            } else {
                "No"
            },
        );
        self.discord.send_message(thread_id, &info).await?;

        // Start a new message for transcription text
        self.current_message_id = None;
        self.current_message_text.clear();

        tracing::info!(
            talkgroup = self.key.talkgroup,
            sys_num = self.key.sys_num,
            thread = %thread_id,
            "call started"
        );

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
            if let Some(thread_id) = self.thread_id {
                let msg = result
                    .tones
                    .iter()
                    .map(|t| format!("**{t}**"))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.discord.send_message(thread_id, &msg).await?;
            }

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
        if text.is_empty() {
            return Ok(());
        }

        self.post_transcription(text).await
    }

    async fn post_transcription(&mut self, text: &str) -> Result<()> {
        let Some(thread_id) = self.thread_id else {
            return Ok(());
        };

        let new_text = if self.current_message_text.is_empty() {
            text.to_string()
        } else {
            format!("{} {}", self.current_message_text, text)
        };

        if new_text.len() <= 2000 {
            self.current_message_text = new_text;
            if let Some(msg_id) = self.current_message_id {
                self.discord
                    .edit_message(thread_id, msg_id, &self.current_message_text)
                    .await?;
            } else {
                let msg_id = self
                    .discord
                    .send_message(thread_id, &self.current_message_text)
                    .await?;
                self.current_message_id = Some(msg_id);
            }
        } else {
            // Split at last word boundary before 2000 chars
            let split_pos = find_word_boundary(&new_text, 2000);
            let (first, rest) = new_text.split_at(split_pos);

            if let Some(msg_id) = self.current_message_id {
                self.discord.edit_message(thread_id, msg_id, first).await?;
            } else {
                self.discord.send_message(thread_id, first).await?;
            }

            self.current_message_text = rest.trim_start().to_string();
            let msg_id = self
                .discord
                .send_message(thread_id, &self.current_message_text)
                .await?;
            self.current_message_id = Some(msg_id);
        }

        Ok(())
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn post_event(&mut self, text: &str) -> Result<()> {
        let Some(thread_id) = self.thread_id else {
            return Ok(());
        };
        self.discord.send_message(thread_id, text).await?;
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

        // Post call summary
        if let Some(thread_id) = self.thread_id {
            if let Some(data) = &call_data {
                let summary = format!(
                    "---\n**Call ended** | Duration: {:.1}s | Freq: {:.4} MHz | Signal: {:.1} dB",
                    data.length,
                    data.freq / 1_000_000.0,
                    data.signal,
                );
                self.discord.send_message(thread_id, &summary).await?;
            }
        }

        tracing::info!(
            talkgroup = self.key.talkgroup,
            sys_num = self.key.sys_num,
            "call ended"
        );

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

fn find_word_boundary(text: &str, max_len: usize) -> usize {
    if text.len() <= max_len {
        return text.len();
    }
    // Find last char boundary at or before max_len
    let mut pos = max_len;
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    // Find last space at or before that position
    text[..pos].rfind(' ').unwrap_or(pos)
}
