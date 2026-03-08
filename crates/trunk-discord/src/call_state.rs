use std::time::Duration;

use proto::types::{Freq, SignalDb};
use trunk_oxide_proto as proto;

use crate::tone_detect::DetectedTone;

/// Max chars for embed description before we truncate from the front.
const MAX_DESCRIPTION_LEN: usize = 3800;

/// RMS threshold below which post-filter audio is considered silent.
/// Samples are normalized to [-1.0, 1.0]; an RMS of 0.005 is ~-46 dBFS.
const SILENCE_RMS_THRESHOLD: f64 = 0.005;

// ── Effects ──────────────────────────────────────────────────────────────────

/// Side-effect commands returned by [`CallState`] for the executor to perform.
#[derive(Debug)]
pub enum CallEffect {
    /// Post a new embed to Discord.
    SendEmbed { embed: EmbedData },
    /// Edit the existing embed in place.
    EditEmbed { embed: EmbedData },
    /// Delete the embed (call produced no useful content).
    DeleteEmbed,
    /// Replace embed and attach OGG audio file.
    AttachAudio {
        embed: EmbedData,
        ogg_data: Vec<u8>,
        filename: String,
    },
    /// Request transcription of buffered audio.
    Transcribe { audio: Vec<f32> },
    /// Persist final call state to database.
    UpdateDb { update: CallUpdate },
    /// Emit a log message.
    Log { message: String },
}

/// Database update payload (mirrors `trunk_database::CallUpdate` without the dep).
#[derive(Debug, Clone)]
pub struct CallUpdate {
    pub duration: Option<Duration>,
    pub signal: Option<SignalDb>,
    pub transcription: Option<String>,
    pub tones: Option<Vec<String>>,
    pub events: Option<Vec<String>>,
}

// ── Embed data ───────────────────────────────────────────────────────────────

/// Pure representation of a Discord embed (no serenity types).
#[derive(Debug, Clone)]
pub struct EmbedData {
    pub title: String,
    pub colour: u32,
    pub description: Option<String>,
    pub fields: Vec<EmbedField>,
    pub footer: String,
}

#[derive(Debug, Clone)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

// ── Config ───────────────────────────────────────────────────────────────────

/// Configuration snapshot taken at call creation time.
#[derive(Debug, Clone)]
pub struct CallStateConfig {
    pub buffer_threshold: usize,
    pub audio_attachment: bool,
}

// ── State machine ────────────────────────────────────────────────────────────

/// Pure state machine for a single call's Discord presentation.
///
/// Owns the decision logic for embed content, transcription accumulation,
/// silence detection, and end-of-call content gating. Returns [`CallEffect`]s
/// for the executor to perform — never touches Discord, gRPC, or the database.
pub struct CallState {
    // Identity (immutable)
    talkgroup: i64,
    talkgroup_alpha_tag: String,
    talkgroup_tag: String,
    freq: f64,
    short_name: String,
    encrypted: bool,
    start_time: chrono::DateTime<chrono::Utc>,
    tag: String,

    // Config
    config: CallStateConfig,

    // Mutable state
    has_embed: bool,
    transcription_text: String,
    detected_tones: Vec<DetectedTone>,
    events: Vec<String>,
    call_ended: bool,
    audio_energy_sum: f64,
    audio_sample_count: u32,

    // PTT coalescing accumulators
    ptt_count: u32,
    accumulated_duration: Duration,
    accumulated_signal_sum: f64,
    accumulated_signal_count: u32,
}

impl CallState {
    pub fn new(call_info: &proto::CallInfo, config: CallStateConfig) -> Self {
        let start_time = call_info
            .start_time
            .as_ref()
            .and_then(|ts| {
                chrono::DateTime::<chrono::Utc>::from_timestamp(
                    ts.seconds,
                    ts.nanos.try_into().unwrap_or(0),
                )
            })
            .unwrap_or_else(chrono::Utc::now);

        let tag = if call_info.talkgroup_alpha_tag.is_empty() {
            call_info.talkgroup.to_string()
        } else {
            call_info.talkgroup_alpha_tag.clone()
        };

        Self {
            talkgroup: call_info.talkgroup,
            talkgroup_alpha_tag: call_info.talkgroup_alpha_tag.clone(),
            talkgroup_tag: call_info.talkgroup_tag.clone(),
            freq: call_info.freq,
            short_name: call_info.short_name.clone(),
            encrypted: call_info.encrypted,
            start_time,
            tag,
            config,
            has_embed: false,
            transcription_text: String::new(),
            detected_tones: Vec::new(),
            events: Vec::new(),
            call_ended: false,
            audio_energy_sum: 0.0,
            audio_sample_count: 0,
            ptt_count: 0,
            accumulated_duration: Duration::ZERO,
            accumulated_signal_sum: 0.0,
            accumulated_signal_count: 0,
        }
    }

    /// Emit the initial embed when the call starts.
    pub fn on_start(&mut self) -> Vec<CallEffect> {
        self.has_embed = true;
        vec![CallEffect::SendEmbed {
            embed: self.build_embed(),
        }]
    }

    /// Accumulate audio energy and check if the audio buffer is ready for transcription.
    /// Returns a `Transcribe` effect when the buffer exceeds the configured threshold.
    pub fn on_audio_processed(
        &mut self,
        sample_count: u32,
        energy: f64,
        buffer_len: usize,
        audio_drain: Vec<f32>,
    ) -> Vec<CallEffect> {
        self.audio_energy_sum += energy;
        self.audio_sample_count += sample_count;

        if buffer_len >= self.config.buffer_threshold {
            vec![CallEffect::Transcribe { audio: audio_drain }]
        } else {
            vec![]
        }
    }

    /// Handle transcription text from Whisper.
    pub fn on_transcription(&mut self, text: &str) -> Vec<CallEffect> {
        let text = text.trim();
        if text.is_empty() || is_blank_audio(text) {
            if !self.transcription_text.is_empty() {
                self.transcription_text.push_str(" ...");
                if self.has_embed {
                    return vec![CallEffect::EditEmbed {
                        embed: self.build_embed(),
                    }];
                }
            }
            return vec![];
        }

        if !self.transcription_text.is_empty() && !self.transcription_text.ends_with("---\n") {
            self.transcription_text.push(' ');
        }
        self.transcription_text.push_str(text);

        if self.has_embed {
            vec![CallEffect::EditEmbed {
                embed: self.build_embed(),
            }]
        } else {
            vec![]
        }
    }

    /// Handle completed tone detection results.
    pub fn on_tones_detected(&mut self, tones: Vec<DetectedTone>) -> Vec<CallEffect> {
        if tones.is_empty() {
            return vec![];
        }
        self.detected_tones = tones;
        if self.has_embed {
            vec![CallEffect::EditEmbed {
                embed: self.build_embed(),
            }]
        } else {
            vec![]
        }
    }

    /// Handle a signal or unit event during the call.
    pub fn on_event(&mut self, text: String) -> Vec<CallEffect> {
        self.events.push(text);
        if self.has_embed {
            vec![CallEffect::EditEmbed {
                embed: self.build_embed(),
            }]
        } else {
            vec![]
        }
    }

    /// Handle a PTT boundary within a coalesced call.
    /// Accumulates duration/signal, inserts a separator in the transcription.
    pub fn on_ptt_end(&mut self, call_data: Option<&proto::CallData>) -> Vec<CallEffect> {
        self.ptt_count += 1;

        if let Some(data) = call_data {
            self.accumulated_duration += Duration::from_secs_f64(data.length);
            if let Some(signal) = data.signal {
                self.accumulated_signal_sum += signal;
                self.accumulated_signal_count += 1;
            }
        }

        // Insert PTT separator in transcription
        if !self.transcription_text.is_empty() {
            self.transcription_text.push_str("\n---\n");
        }

        if self.has_embed {
            vec![CallEffect::EditEmbed {
                embed: self.build_embed(),
            }]
        } else {
            vec![]
        }
    }

    /// Handle a new PTT starting on a coalesced call.
    /// Resets the "ended" appearance and updates freq if it changed.
    pub fn on_ptt_start(&mut self, call_info: &proto::CallInfo) -> Vec<CallEffect> {
        self.call_ended = false;
        self.freq = call_info.freq;

        if self.has_embed {
            vec![CallEffect::EditEmbed {
                embed: self.build_embed(),
            }]
        } else {
            vec![]
        }
    }

    /// Handle call end. Returns effects to finalize or delete the embed.
    ///
    /// `ogg_data` is provided by the executor if audio attachment is enabled and
    /// the encoder produced valid output.
    pub fn on_end(
        &mut self,
        call_data: Option<&proto::CallData>,
        ogg_data: Option<Vec<u8>>,
    ) -> Vec<CallEffect> {
        self.call_ended = true;

        // Accumulate final PTT's data
        if let Some(data) = call_data {
            self.accumulated_duration += Duration::from_secs_f64(data.length);
            if let Some(signal) = data.signal {
                self.accumulated_signal_sum += signal;
                self.accumulated_signal_count += 1;
            }
        }

        // Set final duration/signal from accumulators
        if !self.accumulated_duration.is_zero() {
            self.call_ended = true;
        }
        let final_duration = if self.accumulated_duration.is_zero() {
            None
        } else {
            Some(self.accumulated_duration)
        };
        let final_signal = if self.accumulated_signal_count > 0 {
            Some(SignalDb(
                self.accumulated_signal_sum / f64::from(self.accumulated_signal_count),
            ))
        } else {
            None
        };

        let mut effects = Vec::new();

        // Check if the call produced useful content
        if !self.has_content() {
            let audio_rms = self.audio_rms();
            effects.push(CallEffect::Log {
                message: format!("audio below silence threshold (rms={audio_rms:.6})"),
            });
            if self.has_embed {
                effects.push(CallEffect::DeleteEmbed);
            }
            effects.push(CallEffect::Log {
                message: "call ended (empty, deleted embed)".into(),
            });
            return effects;
        }

        // Update the embed with final state
        if self.has_embed {
            effects.push(CallEffect::EditEmbed {
                embed: self.build_embed_with(final_duration, final_signal),
            });
        }

        // Attach audio if available and non-silent
        if self.config.audio_attachment
            && let Some(ogg_data) = ogg_data
        {
            let audio_is_silent = self.audio_rms() < SILENCE_RMS_THRESHOLD;
            if audio_is_silent {
                tracing::debug!(
                    talkgroup = self.talkgroup,
                    rms = self.audio_rms(),
                    ogg_bytes = ogg_data.len(),
                    "skipping audio attachment (silent)"
                );
            } else {
                let filename = format!(
                    "call_{}_{}.ogg",
                    self.talkgroup,
                    self.start_time.format("%Y%m%dT%H%M%SZ"),
                );
                effects.push(CallEffect::AttachAudio {
                    embed: self.build_embed_with(final_duration, final_signal),
                    ogg_data,
                    filename,
                });
            }
        }

        // Persist to DB
        effects.push(CallEffect::UpdateDb {
            update: self.build_db_update(final_duration, final_signal),
        });

        effects.push(CallEffect::Log {
            message: "call ended".into(),
        });

        effects
    }

    /// Compute audio RMS from accumulated energy.
    fn audio_rms(&self) -> f64 {
        if self.audio_sample_count > 0 {
            (self.audio_energy_sum / f64::from(self.audio_sample_count)).sqrt()
        } else {
            0.0
        }
    }

    /// Does this call have content worth posting?
    fn has_content(&self) -> bool {
        let has_interesting_tones = self
            .detected_tones
            .iter()
            .any(|t| !matches!(t, DetectedTone::Ctcss { .. }));
        let audio_is_silent = self.audio_rms() < SILENCE_RMS_THRESHOLD;

        (!self.transcription_text.is_empty() && !audio_is_silent)
            || has_interesting_tones
            || !self.events.is_empty()
    }

    /// Build a pure embed representation from current state.
    fn build_embed(&self) -> EmbedData {
        self.build_embed_with(None, None)
    }

    /// Build embed with explicit final duration/signal (used at call end).
    fn build_embed_with(
        &self,
        final_duration: Option<Duration>,
        final_signal: Option<SignalDb>,
    ) -> EmbedData {
        let title = if self.call_ended {
            self.tag.clone()
        } else {
            format!("{} ...", self.tag)
        };

        let colour = if self.call_ended {
            if self.encrypted {
                0x00e7_4c3c_u32 // red
            } else {
                0x002e_cc71_u32 // green
            }
        } else {
            0x0095_a5a6_u32 // grey
        };

        let description = self.build_description();
        let fields = self.build_fields(final_duration, final_signal);

        EmbedData {
            title,
            colour,
            description,
            fields,
            footer: self.start_time.format("%H:%M:%S UTC").to_string(),
        }
    }

    fn build_description(&self) -> Option<String> {
        if self.transcription_text.is_empty() {
            None
        } else if self.transcription_text.len() > MAX_DESCRIPTION_LEN {
            let skip = self.transcription_text.len() - MAX_DESCRIPTION_LEN;
            let start = self.transcription_text[skip..]
                .char_indices()
                .next()
                .map_or(skip, |(i, _)| skip + i);
            Some(format!("...{}", &self.transcription_text[start..]))
        } else {
            Some(self.transcription_text.clone())
        }
    }

    fn build_fields(
        &self,
        final_duration: Option<Duration>,
        final_signal: Option<SignalDb>,
    ) -> Vec<EmbedField> {
        let mut fields = Vec::new();

        let tg_display = if self.talkgroup_alpha_tag.is_empty() {
            self.talkgroup.to_string()
        } else {
            format!("{} — {}", self.talkgroup, self.talkgroup_alpha_tag)
        };
        fields.push(EmbedField {
            name: "Talkgroup".into(),
            value: tg_display,
            inline: true,
        });
        fields.push(EmbedField {
            name: "Frequency".into(),
            value: Freq(self.freq).to_string(),
            inline: true,
        });
        fields.push(EmbedField {
            name: "System".into(),
            value: self.short_name.clone(),
            inline: true,
        });

        if !self.talkgroup_tag.is_empty() {
            fields.push(EmbedField {
                name: "Category".into(),
                value: self.talkgroup_tag.clone(),
                inline: true,
            });
        }

        if self.encrypted {
            fields.push(EmbedField {
                name: "Encrypted".into(),
                value: "Yes".into(),
                inline: true,
            });
        }

        if !self.detected_tones.is_empty() {
            let tones_str: Vec<String> =
                self.detected_tones.iter().map(|t| format!("{t}")).collect();
            fields.push(EmbedField {
                name: "Tones".into(),
                value: tones_str.join(", "),
                inline: false,
            });
        }

        if !self.events.is_empty() {
            fields.push(EmbedField {
                name: "Events".into(),
                value: self.events.join("\n"),
                inline: false,
            });
        }

        if self.ptt_count > 1 {
            fields.push(EmbedField {
                name: "PTTs".into(),
                value: format!("{}", self.ptt_count),
                inline: true,
            });
        }

        let duration = final_duration.or_else(|| {
            if self.accumulated_duration.is_zero() {
                None
            } else {
                Some(self.accumulated_duration)
            }
        });
        if let Some(duration) = duration
            && !duration.is_zero()
        {
            fields.push(EmbedField {
                name: "Duration".into(),
                value: format!("{:.1}s", duration.as_secs_f64()),
                inline: true,
            });
        }

        let signal = final_signal.or_else(|| {
            if self.accumulated_signal_count > 0 {
                Some(SignalDb(
                    self.accumulated_signal_sum / f64::from(self.accumulated_signal_count),
                ))
            } else {
                None
            }
        });
        if let Some(signal) = signal {
            fields.push(EmbedField {
                name: "Signal".into(),
                value: signal.to_string(),
                inline: true,
            });
        }

        fields
    }

    /// Build the database update payload.
    fn build_db_update(
        &self,
        final_duration: Option<Duration>,
        final_signal: Option<SignalDb>,
    ) -> CallUpdate {
        CallUpdate {
            duration: final_duration,
            signal: final_signal,
            transcription: if self.transcription_text.is_empty() {
                None
            } else {
                Some(self.transcription_text.clone())
            },
            tones: if self.detected_tones.is_empty() {
                None
            } else {
                Some(self.detected_tones.iter().map(|t| format!("{t}")).collect())
            },
            events: if self.events.is_empty() {
                None
            } else {
                Some(self.events.clone())
            },
        }
    }
}

/// Check if Whisper returned a "blank audio" placeholder.
pub fn is_blank_audio(text: &str) -> bool {
    let t = text
        .trim()
        .trim_start_matches(['[', '('])
        .trim_end_matches([']', ')'])
        .trim()
        .to_lowercase()
        .replace('_', " ");
    t == "blank audio" || t == "no speech" || t == "silence" || t == "..." || t == "."
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_call_info() -> proto::CallInfo {
        proto::CallInfo {
            talkgroup: 12345,
            talkgroup_alpha_tag: "Fire Dispatch".into(),
            talkgroup_tag: "Fire".into(),
            freq: 851_125_000.0,
            short_name: "smco".into(),
            encrypted: false,
            is_conventional: false,
            ..Default::default()
        }
    }

    fn test_config() -> CallStateConfig {
        CallStateConfig {
            buffer_threshold: 16000, // 1 second at 16kHz
            audio_attachment: true,
        }
    }

    fn make_state() -> CallState {
        CallState::new(&test_call_info(), test_config())
    }

    // ── on_start ─────────────────────────────────────────────────────────

    #[test]
    fn on_start_emits_send_embed() {
        let mut s = make_state();
        let effects = s.on_start();
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], CallEffect::SendEmbed { .. }));
        assert!(s.has_embed);
    }

    #[test]
    fn on_start_embed_has_tag_title_with_ellipsis() {
        let mut s = make_state();
        let effects = s.on_start();
        let CallEffect::SendEmbed { embed } = &effects[0] else {
            panic!("expected SendEmbed");
        };
        assert_eq!(embed.title, "Fire Dispatch ...");
        assert_eq!(embed.colour, 0x0095_a5a6); // grey (not ended)
    }

    // ── on_transcription ─────────────────────────────────────────────────

    #[test]
    fn transcription_empty_no_effect() {
        let mut s = make_state();
        s.on_start();
        let effects = s.on_transcription("");
        assert!(effects.is_empty());
    }

    #[test]
    fn transcription_blank_audio_no_effect() {
        let mut s = make_state();
        s.on_start();
        assert!(s.on_transcription("[blank audio]").is_empty());
        assert!(s.on_transcription("(no speech)").is_empty());
        assert!(s.on_transcription("[silence]").is_empty());
    }

    #[test]
    fn transcription_text_emits_edit() {
        let mut s = make_state();
        s.on_start();
        let effects = s.on_transcription("hello world");
        assert_eq!(effects.len(), 1);
        let CallEffect::EditEmbed { embed } = &effects[0] else {
            panic!("expected EditEmbed");
        };
        assert_eq!(embed.description.as_deref(), Some("hello world"));
    }

    #[test]
    fn transcription_accumulates() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello");
        let effects = s.on_transcription("world");
        let CallEffect::EditEmbed { embed } = &effects[0] else {
            panic!("expected EditEmbed");
        };
        assert_eq!(embed.description.as_deref(), Some("hello world"));
    }

    #[test]
    fn transcription_blank_after_text_appends_ellipsis() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello");
        let effects = s.on_transcription("");
        assert_eq!(effects.len(), 1);
        let CallEffect::EditEmbed { embed } = &effects[0] else {
            panic!("expected EditEmbed");
        };
        assert_eq!(embed.description.as_deref(), Some("hello ..."));
    }

    #[test]
    fn transcription_truncates_from_front() {
        let mut s = make_state();
        s.on_start();
        let long_text = "x".repeat(MAX_DESCRIPTION_LEN + 100);
        s.on_transcription(&long_text);
        let embed = s.build_embed();
        let desc = embed.description.unwrap();
        assert!(desc.starts_with("..."));
        assert!(desc.len() <= MAX_DESCRIPTION_LEN + 4); // "..." prefix
    }

    // ── on_audio_processed ───────────────────────────────────────────────

    #[test]
    fn audio_below_threshold_no_transcribe() {
        let mut s = make_state();
        let effects = s.on_audio_processed(100, 0.01, 100, vec![]);
        assert!(effects.is_empty());
        assert_eq!(s.audio_sample_count, 100);
    }

    #[test]
    fn audio_above_threshold_emits_transcribe() {
        let mut s = make_state();
        let audio = vec![0.0f32; 16000];
        let effects = s.on_audio_processed(16000, 0.0, 16000, audio);
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], CallEffect::Transcribe { .. }));
    }

    // ── on_tones_detected ────────────────────────────────────────────────

    #[test]
    fn tones_empty_no_effect() {
        let mut s = make_state();
        s.on_start();
        let effects = s.on_tones_detected(vec![]);
        assert!(effects.is_empty());
    }

    #[test]
    fn tones_detected_emits_edit() {
        let mut s = make_state();
        s.on_start();
        let tones = vec![DetectedTone::LongTone {
            freq: 1000.0,
            duration: 3.0,
        }];
        let effects = s.on_tones_detected(tones);
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], CallEffect::EditEmbed { .. }));
    }

    // ── on_event ─────────────────────────────────────────────────────────

    #[test]
    fn event_emits_edit() {
        let mut s = make_state();
        s.on_start();
        let effects = s.on_event("**Signal**: unit 123 - NORMAL".into());
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], CallEffect::EditEmbed { .. }));
    }

    // ── on_end ───────────────────────────────────────────────────────────

    #[test]
    fn end_no_content_deletes_embed() {
        let mut s = make_state();
        s.on_start();
        let effects = s.on_end(None, None);
        assert!(effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, CallEffect::UpdateDb { .. }))
        );
    }

    #[test]
    fn end_with_transcription_but_silent_audio_deletes() {
        let mut s = make_state();
        s.on_start();
        // Add transcription but leave audio energy at zero (silent)
        s.on_transcription("hello");
        let effects = s.on_end(None, None);
        assert!(effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
    }

    #[test]
    fn end_with_transcription_and_audio_keeps_embed() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello world");
        // Simulate non-silent audio: 1000 samples with energy summing to 1.0
        s.audio_energy_sum = 1.0;
        s.audio_sample_count = 1000;
        let call_data = proto::CallData {
            length: 5.5,
            signal: Some(-20.0),
            ..Default::default()
        };
        let effects = s.on_end(Some(&call_data), Some(vec![1, 2, 3]));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, CallEffect::EditEmbed { .. }))
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, CallEffect::AttachAudio { .. }))
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, CallEffect::UpdateDb { .. }))
        );
        assert!(!effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
    }

    #[test]
    fn end_with_events_keeps_embed_even_if_silent() {
        let mut s = make_state();
        s.on_start();
        s.on_event("**Signal**: unit 123".into());
        let effects = s.on_end(None, None);
        assert!(!effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, CallEffect::UpdateDb { .. }))
        );
    }

    #[test]
    fn end_with_events_and_silent_audio_skips_attachment() {
        let mut s = make_state();
        s.on_start();
        s.on_event("**Signal**: unit 123".into());
        // ogg_data present but audio is silent (no energy)
        let effects = s.on_end(None, Some(vec![0; 127]));
        assert!(!effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, CallEffect::AttachAudio { .. }))
        );
    }

    #[test]
    fn end_with_tones_and_silent_audio_skips_attachment() {
        let mut s = make_state();
        s.on_start();
        s.on_tones_detected(vec![DetectedTone::TwoTone {
            tone_a: 500.0,
            tone_b: 800.0,
            duration_a: 1.0,
            duration_b: 3.0,
        }]);
        let effects = s.on_end(None, Some(vec![0; 127]));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, CallEffect::AttachAudio { .. }))
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, CallEffect::UpdateDb { .. }))
        );
    }

    #[test]
    fn end_with_dispatch_tones_keeps_embed() {
        let mut s = make_state();
        s.on_start();
        s.on_tones_detected(vec![DetectedTone::TwoTone {
            tone_a: 500.0,
            tone_b: 800.0,
            duration_a: 1.0,
            duration_b: 3.0,
        }]);
        let effects = s.on_end(None, None);
        assert!(!effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
    }

    #[test]
    fn end_with_only_ctcss_deletes_embed() {
        let mut s = make_state();
        s.on_start();
        s.on_tones_detected(vec![DetectedTone::Ctcss { freq: 131.8 }]);
        let effects = s.on_end(None, None);
        assert!(effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
    }

    #[test]
    fn end_without_audio_attachment_config() {
        let mut s = CallState::new(
            &test_call_info(),
            CallStateConfig {
                buffer_threshold: 16000,
                audio_attachment: false,
            },
        );
        s.on_start();
        s.on_transcription("hello");
        s.audio_energy_sum = 1.0;
        s.audio_sample_count = 1000;
        let effects = s.on_end(None, Some(vec![1, 2, 3]));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, CallEffect::AttachAudio { .. }))
        );
    }

    // ── build_embed fields ───────────────────────────────────────────────

    #[test]
    fn embed_ended_changes_colour_and_title() {
        let mut s = make_state();
        s.on_start();
        s.call_ended = true;
        let embed = s.build_embed();
        assert_eq!(embed.title, "Fire Dispatch"); // no ellipsis
        assert_eq!(embed.colour, 0x002e_cc71); // green
    }

    #[test]
    fn embed_encrypted_is_red() {
        let mut info = test_call_info();
        info.encrypted = true;
        let mut s = CallState::new(&info, test_config());
        s.on_start();
        s.call_ended = true;
        let embed = s.build_embed();
        assert_eq!(embed.colour, 0x00e7_4c3c); // red
    }

    #[test]
    fn embed_has_duration_and_signal_at_end() {
        let mut s = make_state();
        s.on_start();
        s.call_ended = true;
        s.accumulated_duration = Duration::from_secs_f64(12.3);
        s.accumulated_signal_sum = -18.5;
        s.accumulated_signal_count = 1;
        let embed = s.build_embed();
        let dur_field = embed.fields.iter().find(|f| f.name == "Duration");
        assert_eq!(dur_field.unwrap().value, "12.3s");
        let sig_field = embed.fields.iter().find(|f| f.name == "Signal");
        assert_eq!(sig_field.unwrap().value, "-18.5 dB");
    }

    #[test]
    fn embed_numeric_talkgroup_when_no_alpha_tag() {
        let mut info = test_call_info();
        info.talkgroup_alpha_tag = String::new();
        let s = CallState::new(&info, test_config());
        assert_eq!(s.tag, "12345");
    }

    // ── is_blank_audio ───────────────────────────────────────────────────

    #[test]
    fn blank_audio_detection() {
        assert!(is_blank_audio("[blank audio]"));
        assert!(is_blank_audio("(no speech)"));
        assert!(is_blank_audio("[silence]"));
        assert!(is_blank_audio("..."));
        assert!(is_blank_audio("."));
        assert!(is_blank_audio("[BLANK_AUDIO]"));

        assert!(!is_blank_audio("hello world"));
        assert!(!is_blank_audio("the silence was deafening"));
        assert!(!is_blank_audio(""));
    }

    // ── PTT coalescing ──────────────────────────────────────────────────

    #[test]
    fn ptt_end_inserts_separator() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello");
        let call_data = proto::CallData {
            length: 3.0,
            signal: Some(-15.0),
            ..Default::default()
        };
        let effects = s.on_ptt_end(Some(&call_data));
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], CallEffect::EditEmbed { .. }));
        assert!(s.transcription_text.ends_with("\n---\n"));
        assert_eq!(s.ptt_count, 1);
    }

    #[test]
    fn ptt_start_clears_ended_state() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello");
        s.on_ptt_end(None);
        s.call_ended = false; // simulating the natural state

        let call_info = test_call_info();
        let effects = s.on_ptt_start(&call_info);
        assert_eq!(effects.len(), 1);
        assert!(!s.call_ended);
    }

    #[test]
    fn multi_ptt_accumulates_duration() {
        let mut s = make_state();
        s.on_start();
        s.on_ptt_end(Some(&proto::CallData {
            length: 3.0,
            ..Default::default()
        }));
        let effects = s.on_end(
            Some(&proto::CallData {
                length: 5.0,
                ..Default::default()
            }),
            None,
        );
        // Duration should be 3.0 + 5.0 = 8.0
        if let Some(CallEffect::UpdateDb { update }) = effects
            .iter()
            .find(|e| matches!(e, CallEffect::UpdateDb { .. }))
        {
            assert_eq!(update.duration.unwrap().as_secs_f64(), 8.0);
        }
    }

    #[test]
    fn multi_ptt_averages_signal() {
        let mut s = make_state();
        s.on_start();
        s.audio_energy_sum = 1.0;
        s.audio_sample_count = 1000;
        s.on_transcription("test");
        s.on_ptt_end(Some(&proto::CallData {
            length: 3.0,
            signal: Some(-15.0),
            ..Default::default()
        }));
        let effects = s.on_end(
            Some(&proto::CallData {
                length: 5.0,
                signal: Some(-25.0),
                ..Default::default()
            }),
            None,
        );
        if let Some(CallEffect::UpdateDb { update }) = effects
            .iter()
            .find(|e| matches!(e, CallEffect::UpdateDb { .. }))
        {
            let signal = update.signal.unwrap();
            assert!((signal.0 - -20.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn ptt_count_shown_in_embed() {
        let mut s = make_state();
        s.on_start();
        s.on_ptt_end(None);
        s.on_ptt_start(&test_call_info());
        s.on_ptt_end(None);
        s.on_ptt_start(&test_call_info());
        // ptt_count is 2, current PTT not yet ended
        let embed = s.build_embed();
        let ptts_field = embed.fields.iter().find(|f| f.name == "PTTs");
        assert_eq!(ptts_field.unwrap().value, "2");
    }

    #[test]
    fn transcription_after_ptt_separator_no_extra_space() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("first ptt");
        s.on_ptt_end(None);
        s.on_ptt_start(&test_call_info());
        s.on_transcription("second ptt");
        assert_eq!(s.transcription_text, "first ptt\n---\nsecond ptt");
    }

    #[test]
    fn single_ptt_on_end_same_as_before() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello");
        s.audio_energy_sum = 1.0;
        s.audio_sample_count = 1000;
        let call_data = proto::CallData {
            length: 5.5,
            signal: Some(-20.0),
            ..Default::default()
        };
        let effects = s.on_end(Some(&call_data), None);
        if let Some(CallEffect::UpdateDb { update }) = effects
            .iter()
            .find(|e| matches!(e, CallEffect::UpdateDb { .. }))
        {
            assert_eq!(update.duration.unwrap().as_secs_f64(), 5.5);
            assert!((update.signal.unwrap().0 - -20.0).abs() < f64::EPSILON);
        } else {
            panic!("expected UpdateDb");
        }
    }
}
