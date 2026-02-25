use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serenity::model::id::{ChannelId, MessageId};
use tokio::sync::mpsc;

use crate::discord::DiscordClient;
use crate::dispatcher::CallKey;
use crate::proto;
use crate::resampler::Resampler;
use crate::tone_detect::ToneDetector;
use crate::transcriber::Transcriber;

pub enum CallMessage {
    Audio {
        samples: Vec<u8>,
        sample_rate: i32,
    },
    Event(String),
    End(Option<proto::CallData>),
}

pub struct CallHandler {
    key: CallKey,
    call_info: proto::CallInfo,
    rx: mpsc::Receiver<CallMessage>,
    transcriber: Arc<Transcriber>,
    discord: Arc<DiscordClient>,
    buffer_duration_ms: u64,
    channel_id: u64,
    audio_buffer: Vec<f32>,
    resampler: Option<Resampler>,
    tone_detector: Option<ToneDetector>,
    thread_id: Option<ChannelId>,
    current_message_id: Option<MessageId>,
    current_message_text: String,
}

impl CallHandler {
    pub fn new(
        key: CallKey,
        call_info: proto::CallInfo,
        rx: mpsc::Receiver<CallMessage>,
        transcriber: Arc<Transcriber>,
        discord: Arc<DiscordClient>,
        buffer_duration_ms: u64,
        channel_id: u64,
        tone_detection: bool,
        tone_detection_duration_s: f32,
    ) -> Self {
        let tone_detector = if tone_detection {
            Some(ToneDetector::new(16000, tone_detection_duration_s))
        } else {
            None
        };

        Self {
            key,
            call_info,
            rx,
            transcriber,
            discord,
            buffer_duration_ms,
            channel_id,
            audio_buffer: Vec::new(),
            resampler: None,
            tone_detector,
            thread_id: None,
            current_message_id: None,
            current_message_text: String::new(),
        }
    }

    pub async fn run(mut self) {
        if let Err(e) = self.create_thread().await {
            tracing::error!(
                talkgroup = self.key.talkgroup,
                error = %e,
                "failed to create discord thread"
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

    async fn create_thread(&mut self) -> Result<()> {
        let start_time = DateTime::<Utc>::from_timestamp(self.call_info.start_time, 0)
            .unwrap_or_else(Utc::now);
        let time_str = start_time.format("%H:%M:%S").to_string();

        let tag = if self.call_info.talkgroup_tag.is_empty() {
            self.call_info.talkgroup_display.clone()
        } else {
            self.call_info.talkgroup_tag.clone()
        };

        let thread_name = format!("TG {} - {} [{}]", self.call_info.talkgroup, tag, time_str);
        // Discord thread names are limited to 100 chars
        let thread_name = if thread_name.len() > 100 {
            thread_name[..100].to_string()
        } else {
            thread_name
        };

        let thread_id = self
            .discord
            .create_call_thread(self.channel_id, &thread_name)
            .await?;
        self.thread_id = Some(thread_id);

        let info = format!(
            "**Talkgroup**: {} ({})\n**Freq**: {:.4} MHz\n**System**: {}\n**Encrypted**: {}",
            self.call_info.talkgroup,
            tag,
            self.call_info.freq / 1_000_000.0,
            self.call_info.short_name,
            if self.call_info.encrypted {
                "Yes"
            } else {
                "No"
            },
        );
        let msg_id = self.discord.send_message(thread_id, &info).await?;

        // Start a new message for transcription text
        self.current_message_id = None;
        self.current_message_text.clear();
        let _ = msg_id;

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
            let resampler = self.resampler.get_or_insert_with(|| {
                Resampler::new(sample_rate as u32)
                    .expect("failed to create resampler")
            });
            resampler.process(&decoded)?
        };

        if let Some(detector) = &mut self.tone_detector {
            detector.process(&samples);
        }

        self.audio_buffer.extend_from_slice(&samples);

        if self
            .tone_detector
            .as_ref()
            .is_some_and(|d| d.is_complete())
        {
            self.finish_tone_detection().await?;
        }

        let buffer_threshold = (self.buffer_duration_ms as usize * 16000) / 1000;
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
                    .map(|t| format!("**{}**", t))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.discord.send_message(thread_id, &msg).await?;
            }

            // Strip tone samples from the front of the audio buffer
            let strip = result.tone_end_sample.min(self.audio_buffer.len());
            if strip > 0 {
                self.audio_buffer.drain(..strip);
            }
        }

        Ok(())
    }

    async fn transcribe_and_post(&mut self, audio: Vec<f32>) -> Result<()> {
        if audio.is_empty() {
            return Ok(());
        }

        let text = self.transcriber.transcribe(audio).await?;
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
                self.discord
                    .edit_message(thread_id, msg_id, first)
                    .await?;
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

    async fn post_event(&mut self, text: &str) -> Result<()> {
        let Some(thread_id) = self.thread_id else {
            return Ok(());
        };
        self.discord.send_message(thread_id, text).await?;
        Ok(())
    }

    async fn handle_end(&mut self, call_data: Option<proto::CallData>) -> Result<()> {
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
            sample as f32 / 32768.0
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
