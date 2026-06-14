use std::time::Duration;

use proto::types::{Freq, SignalDb};
use trunk_oxide_proto as proto;

use crate::tone_detect::DetectedTone;

/// Max chars for embed description before we truncate from the front.
const MAX_DESCRIPTION_LEN: usize = 3800;

/// RMS threshold below which post-filter audio is considered silent.
/// Samples are normalized to [-1.0, 1.0]; an RMS of 0.005 is ~-46 dBFS.
/// Shared with the call handler, which uses it to skip handing silent chunks to
/// the whisperer (a chunk too quiet to transcribe is also too quiet to attach).
pub const SILENCE_RMS_THRESHOLD: f64 = 0.005;

// ── Effects ──────────────────────────────────────────────────────────────────

/// Side-effect commands returned by [`CallState`] for the executor to perform.
#[derive(Debug)]
pub enum CallEffect {
    /// Post a new embed to Discord.
    SendEmbed { embed: EmbedData },
    /// Edit the existing embed in place, promptly (call-state changes the user
    /// must see right away: first content, finalization, tones, events).
    EditEmbed { embed: EmbedData },
    /// Edit the existing embed, but coalesced/debounced by the executor.
    ///
    /// Used for the transcript stream (one result per PTT transmission, often
    /// bursting as the transcription backlog drains). The executor keeps only
    /// the latest and flushes at most every couple of seconds, so transcript
    /// churn stops hammering the per-channel edit rate-limit bucket.
    /// Always carries the *full* current embed, so a later prompt `EditEmbed`
    /// (e.g. finalization) supersedes any pending throttled edit.
    EditEmbedThrottled { embed: EmbedData },
    /// Delete the embed (call produced no useful content).
    DeleteEmbed,
    /// Attach the OGG audio file to the (already-green) embed. The green
    /// transition is a separate prompt `EditEmbed`/`SendEmbed` emitted just
    /// before this, so the attach only adds the file — the executor uploads it
    /// off the critical path and it never touches the embed body.
    AttachAudio { ogg_data: Vec<u8>, filename: String },
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

    /// Whether any audio segment cleared the silence gate (i.e. was kept for
    /// transcription). The single notion of "this call has audible audio",
    /// driving both `has_content` and the audio attachment decision — so a call
    /// that is transcribed is exactly a call whose audio is attached. Set via
    /// [`Self::on_transcription_enqueued`], which the handler calls when a
    /// segment passes the per-chunk RMS gate.
    audio_kept: bool,

    /// Audio segments handed to the transcription worker that haven't returned
    /// a result yet. Surfaced as a reaction-based in-flight indicator (see
    /// [`Self::wants_transcribing_indicator`]) rather than embed text, so the
    /// "still transcribing" signal rides a cheap, separate rate-limit bucket
    /// instead of churning embed edits. Incremented per enqueued segment
    /// ([`Self::on_transcription_enqueued`]), decremented per result
    /// ([`Self::on_transcription`]).
    pending_transcriptions: u32,

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
            audio_kept: false,
            pending_transcriptions: 0,
            ptt_count: 0,
            accumulated_duration: Duration::ZERO,
            accumulated_signal_sum: 0.0,
            accumulated_signal_count: 0,
        }
    }

    /// Called when the call starts. Embed creation is deferred until we have
    /// content worth showing (transcription, interesting tones, or events).
    #[allow(
        clippy::unused_self,
        clippy::needless_pass_by_ref_mut,
        clippy::missing_const_for_fn
    )]
    pub fn on_start(&mut self) -> Vec<CallEffect> {
        vec![]
    }

    /// Sum of per-transmission `recording_length` values accumulated so far
    /// (what drives the embed Duration). Exposed for audio-vs-duration debug
    /// instrumentation.
    pub(crate) fn accumulated_duration(&self) -> Duration {
        self.accumulated_duration
    }

    /// Number of coalesced PTTs counted so far. Exposed for debug logging.
    pub(crate) fn ptt_count(&self) -> u32 {
        self.ptt_count
    }

    /// Check whether the audio buffer is ready for transcription. Returns a
    /// `Transcribe` effect (carrying the drained chunk) when the buffer exceeds
    /// the configured threshold; the handler applies the per-chunk silence gate
    /// and, if the chunk is kept, reports it via [`Self::on_transcription_enqueued`].
    pub fn on_audio_processed(
        &mut self,
        buffer_len: usize,
        audio_drain: Vec<f32>,
    ) -> Vec<CallEffect> {
        if buffer_len >= self.config.buffer_threshold {
            vec![CallEffect::Transcribe { audio: audio_drain }]
        } else {
            vec![]
        }
    }

    /// Note that an audio segment was handed to the transcription worker but
    /// hasn't come back yet. Bumps the in-flight count that drives the
    /// reaction-based [`Self::wants_transcribing_indicator`]; the executor
    /// reconciles the reaction, so this emits no embed effect (and never an
    /// edit per enqueued chunk — that churn is what jammed the edit bucket).
    /// Never creates an embed on its own — pending work alone is not content.
    pub fn on_transcription_enqueued(&mut self) -> Vec<CallEffect> {
        // A kept segment is the definition of "this call has audible audio",
        // used by `has_content` and the attachment gate.
        self.audio_kept = true;
        self.pending_transcriptions = self.pending_transcriptions.saturating_add(1);
        vec![]
    }

    /// Whether the executor should be showing the "transcription in flight"
    /// reaction. The executor reconciles the actual reaction against this after
    /// each batch of effects (it owns the embed message id), so the pure state
    /// machine never touches Discord.
    pub fn wants_transcribing_indicator(&self) -> bool {
        self.pending_transcriptions > 0
    }

    /// Force-clear the in-flight count. A safety net for the rare case where
    /// transcription results are lost (e.g. the worker task dies) before the
    /// count drains to zero, so the executor drops a stale reaction at exit.
    /// Emits no effect: the reaction is reconciled by the executor.
    pub fn on_transcription_drained(&mut self) {
        self.pending_transcriptions = 0;
    }

    /// Handle transcription text from Whisper. Each call corresponds to one
    /// previously enqueued audio segment (even an empty/blank result), so it
    /// always decrements the in-flight count.
    pub fn on_transcription(&mut self, text: &str) -> Vec<CallEffect> {
        self.pending_transcriptions = self.pending_transcriptions.saturating_sub(1);
        let text = text.trim();
        if text.is_empty() || is_blank_audio(text) {
            if !self.transcription_text.is_empty() {
                self.transcription_text.push_str(" ...");
                // The gap marker is cosmetic — debounce it so a burst of empty
                // results doesn't generate an edit each.
                if self.has_embed {
                    return vec![CallEffect::EditEmbedThrottled {
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
            // Transcript stream is high-frequency and bursty — debounce.
            vec![CallEffect::EditEmbedThrottled {
                embed: self.build_embed(),
            }]
        } else {
            // First content must create the message promptly so later edits
            // (and the reaction) have something to attach to.
            self.has_embed = true;
            vec![CallEffect::SendEmbed {
                embed: self.build_embed(),
            }]
        }
    }

    /// Handle completed tone detection results.
    pub fn on_tones_detected(&mut self, tones: Vec<DetectedTone>) -> Vec<CallEffect> {
        if tones.is_empty() {
            return vec![];
        }
        self.detected_tones = tones;

        if !self.has_embed && self.has_interesting_tones() {
            self.has_embed = true;
            return vec![CallEffect::SendEmbed {
                embed: self.build_embed(),
            }];
        }

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

        if !self.has_embed {
            self.has_embed = true;
            return vec![CallEffect::SendEmbed {
                embed: self.build_embed(),
            }];
        }

        vec![CallEffect::EditEmbed {
            embed: self.build_embed(),
        }]
    }

    /// Handle a PTT boundary within a coalesced call.
    /// Account for a completed transmission at a PTT boundary: bumps the PTT
    /// count and folds the transmission's duration and signal into the call
    /// accumulators.
    ///
    /// Runs **synchronously** at the boundary, decoupled from transcription, so
    /// the embed's Duration / PTTs / Signal are correct even when whisper
    /// produces no text (clicks, static) or lags behind. The matching
    /// transcript separator is inserted later by [`Self::on_ptt_separator`],
    /// which rides the transcription worker's FIFO to stay sequenced with the
    /// text — keeping metadata accounting off the slow path.
    pub fn on_ptt_boundary(&mut self, call_data: Option<&proto::CallData>) -> Vec<CallEffect> {
        self.ptt_count += 1;

        if let Some(data) = call_data {
            self.accumulated_duration += Duration::from_secs_f64(data.length);
            if let Some(signal) = data.signal {
                self.accumulated_signal_sum += signal;
                self.accumulated_signal_count += 1;
            }
        }

        if self.has_embed {
            vec![CallEffect::EditEmbed {
                embed: self.build_embed(),
            }]
        } else {
            vec![]
        }
    }

    /// Insert a PTT separator into the accumulated transcript.
    ///
    /// Runs from the transcription worker's FIFO (one marker per boundary, after
    /// that transmission's text chunks) so the `---` stays correctly ordered
    /// between the text of consecutive transmissions. Metadata accounting is
    /// handled separately and synchronously by [`Self::on_ptt_boundary`].
    pub fn on_ptt_separator(&mut self) -> Vec<CallEffect> {
        if !self.transcription_text.is_empty() {
            self.transcription_text.push_str("\n---\n");
        }

        if self.has_embed {
            // Rides the transcript FIFO — debounce alongside the text edits.
            vec![CallEffect::EditEmbedThrottled {
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
            effects.push(CallEffect::Log {
                message: "no audible audio, tones, or events".into(),
            });
            if self.has_embed {
                effects.push(CallEffect::DeleteEmbed);
                effects.push(CallEffect::Log {
                    message: "call ended (empty, deleted embed)".into(),
                });
            } else {
                effects.push(CallEffect::Log {
                    message: "call ended (empty, no embed created)".into(),
                });
            }
            return effects;
        }

        // Finalize the embed (create it now if content only became apparent at end)
        if self.has_embed {
            effects.push(CallEffect::EditEmbed {
                embed: self.build_embed_with(final_duration, final_signal),
            });
        } else {
            self.has_embed = true;
            effects.push(CallEffect::SendEmbed {
                embed: self.build_embed_with(final_duration, final_signal),
            });
        }

        // Attach audio if available and the call had audible audio (same
        // condition that gated transcription, so transcribed ⟺ attached).
        if self.config.audio_attachment
            && let Some(ogg_data) = ogg_data
        {
            if !self.audio_kept {
                tracing::debug!(
                    talkgroup = self.talkgroup,
                    ogg_bytes = ogg_data.len(),
                    "skipping audio attachment (no audible segment)"
                );
            } else {
                let filename = format!(
                    "call_{}_{}.ogg",
                    self.talkgroup,
                    self.start_time.format("%Y%m%dT%H%M%SZ"),
                );
                effects.push(CallEffect::AttachAudio { ogg_data, filename });
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

    /// Are there non-CTCSS tones (dispatch tones, two-tone sequences)?
    fn has_interesting_tones(&self) -> bool {
        self.detected_tones
            .iter()
            .any(|t| !matches!(t, DetectedTone::Ctcss { .. }))
    }

    /// Does this call have content worth posting?
    ///
    /// Audible audio is content on its own — the call is a real transmission and
    /// gets an embed with its audio attached, whether or not a transcript ever
    /// arrives (transcription is decoupled and may lag, fail, or be empty).
    /// "Audible" means at least one segment cleared the silence gate
    /// (`audio_kept`); a transcript over otherwise-silent audio is treated as a
    /// hallucination and does not count, so dead-air calls are still dropped.
    fn has_content(&self) -> bool {
        self.audio_kept || self.has_interesting_tones() || !self.events.is_empty()
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
        let title = {
            let base = if self.call_ended {
                self.tag.clone()
            } else {
                format!("{} ...", self.tag)
            };
            if self.talkgroup_tag.is_empty() {
                base
            } else {
                format!("{base} · {}", self.talkgroup_tag)
            }
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
            footer: format!(
                "TG {} · {} · {}",
                self.talkgroup,
                self.short_name,
                self.start_time.format("%H:%M:%S UTC"),
            ),
        }
    }

    fn build_description(&self) -> Option<String> {
        // In-flight transcription is surfaced as a reaction (a separate
        // rate-limit bucket), not embed text — so the description is purely the
        // transcript and doesn't churn an edit per pending-count change.
        let out = if self.transcription_text.is_empty() {
            String::new()
        } else if self.transcription_text.len() > MAX_DESCRIPTION_LEN {
            let skip = self.transcription_text.len() - MAX_DESCRIPTION_LEN;
            let start = self.transcription_text[skip..]
                .char_indices()
                .next()
                .map_or(skip, |(i, _)| skip + i);
            format!("...{}", &self.transcription_text[start..])
        } else {
            self.transcription_text.clone()
        };

        if out.is_empty() { None } else { Some(out) }
    }

    fn build_fields(
        &self,
        final_duration: Option<Duration>,
        final_signal: Option<SignalDb>,
    ) -> Vec<EmbedField> {
        let mut fields = Vec::new();

        fields.push(EmbedField {
            name: "Frequency".into(),
            value: Freq(self.freq).to_string(),
            inline: true,
        });

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
                inline: true,
            });
        }

        if !self.events.is_empty() {
            fields.push(EmbedField {
                name: "Events".into(),
                value: self.events.join("\n"),
                inline: true,
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
    fn on_start_defers_embed() {
        let mut s = make_state();
        let effects = s.on_start();
        assert!(effects.is_empty());
        assert!(!s.has_embed);
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
    fn first_transcription_creates_embed() {
        let mut s = make_state();
        s.on_start();
        let effects = s.on_transcription("hello world");
        assert_eq!(effects.len(), 1);
        let CallEffect::SendEmbed { embed } = &effects[0] else {
            panic!("expected SendEmbed");
        };
        assert_eq!(embed.title, "Fire Dispatch ... · Fire");
        assert_eq!(embed.colour, 0x0095_a5a6); // grey (not ended)
        assert_eq!(embed.description.as_deref(), Some("hello world"));
        assert!(s.has_embed);
    }

    #[test]
    fn transcription_accumulates() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello");
        let effects = s.on_transcription("world");
        let CallEffect::EditEmbedThrottled { embed } = &effects[0] else {
            panic!("expected EditEmbedThrottled");
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
        let CallEffect::EditEmbedThrottled { embed } = &effects[0] else {
            panic!("expected EditEmbedThrottled");
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

    // ── in-flight transcription indicator (reaction-driven) ──────────────

    #[test]
    fn pending_transcription_does_not_create_embed() {
        let mut s = make_state();
        s.on_start();
        let effects = s.on_transcription_enqueued();
        assert!(
            effects.is_empty(),
            "pending work alone must not post an embed"
        );
        assert!(!s.has_embed);
    }

    #[test]
    fn enqueued_segment_sets_indicator_without_edit() {
        let mut s = make_state();
        s.on_start();
        s.on_event("**Signal**: unit 123".into()); // creates embed
        assert!(!s.wants_transcribing_indicator());

        // Enqueuing pending work flips the indicator but emits NO embed edit —
        // the executor reconciles a reaction instead (separate rate bucket).
        let effects = s.on_transcription_enqueued();
        assert!(effects.is_empty(), "enqueue must not churn an embed edit");
        assert!(s.wants_transcribing_indicator());
        // And it never leaks into the description text.
        assert_eq!(s.build_embed().description.as_deref(), None);
    }

    #[test]
    fn transcription_results_drain_indicator() {
        let mut s = make_state();
        s.on_start();
        s.on_event("**Signal**: unit 123".into());
        s.on_transcription_enqueued();
        s.on_transcription_enqueued();
        assert!(s.wants_transcribing_indicator());

        // First result: real text, one segment still pending → indicator stays.
        s.on_transcription("hello world");
        assert!(s.wants_transcribing_indicator());
        assert_eq!(s.build_embed().description.as_deref(), Some("hello world"));

        // Second (final) result drains the count → indicator clears. The empty
        // segment leaves the existing " ..." gap marker on the text.
        s.on_transcription("");
        assert!(!s.wants_transcribing_indicator());
        assert_eq!(
            s.build_embed().description.as_deref(),
            Some("hello world ...")
        );
    }

    #[test]
    fn drained_clears_stale_indicator() {
        let mut s = make_state();
        s.on_start();
        s.on_event("**Signal**: unit 123".into());
        s.on_transcription_enqueued();
        assert!(s.wants_transcribing_indicator());
        // Result never arrives; draining clears the indicator (no embed edit).
        s.on_transcription_drained();
        assert!(!s.wants_transcribing_indicator());
    }

    // ── audio_kept: transcribed ⟺ attached ───────────────────────────────

    #[test]
    fn kept_segment_attaches_audio_and_keeps_embed() {
        // The real path: a segment cleared the per-chunk silence gate (so it was
        // enqueued for transcription), which makes the call audible — its audio
        // attaches and the embed is kept, with no separate whole-call check.
        let mut s = make_state();
        s.on_start();
        s.on_transcription_enqueued();
        let effects = s.on_end(None, Some(vec![1, 2, 3]));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, CallEffect::AttachAudio { .. })),
            "a transcribed call must attach its audio"
        );
        assert!(!effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
    }

    #[test]
    fn no_kept_segment_skips_attach_and_drops_call() {
        // No segment cleared the gate and nothing else of interest → not audible
        // → dropped, and the ogg bytes are not attached even though present.
        let mut s = make_state();
        s.on_start();
        let effects = s.on_end(None, Some(vec![1, 2, 3]));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, CallEffect::AttachAudio { .. }))
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, CallEffect::UpdateDb { .. })),
            "an inaudible, event-less call is dropped"
        );
    }

    // ── on_audio_processed ───────────────────────────────────────────────

    #[test]
    fn audio_below_threshold_no_transcribe() {
        let mut s = make_state();
        let effects = s.on_audio_processed(100, vec![]);
        assert!(effects.is_empty());
    }

    #[test]
    fn audio_above_threshold_emits_transcribe() {
        let mut s = make_state();
        let audio = vec![0.0f32; 16000];
        let effects = s.on_audio_processed(16000, audio);
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
    fn interesting_tones_create_embed() {
        let mut s = make_state();
        s.on_start();
        let tones = vec![DetectedTone::LongTone {
            freq: 1000.0,
            duration: 3.0,
        }];
        let effects = s.on_tones_detected(tones);
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], CallEffect::SendEmbed { .. }));
        assert!(s.has_embed);
    }

    #[test]
    fn ctcss_only_tones_do_not_create_embed() {
        let mut s = make_state();
        s.on_start();
        let tones = vec![DetectedTone::Ctcss { freq: 131.8 }];
        let effects = s.on_tones_detected(tones);
        assert!(effects.is_empty());
        assert!(!s.has_embed);
    }

    #[test]
    fn tones_on_existing_embed_emit_edit() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello"); // creates embed
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
    fn first_event_creates_embed() {
        let mut s = make_state();
        s.on_start();
        let effects = s.on_event("**Signal**: unit 123 - NORMAL".into());
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], CallEffect::SendEmbed { .. }));
        assert!(s.has_embed);
    }

    #[test]
    fn second_event_emits_edit() {
        let mut s = make_state();
        s.on_start();
        s.on_event("**Signal**: unit 123 - NORMAL".into());
        let effects = s.on_event("**Signal**: unit 456 - NORMAL".into());
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], CallEffect::EditEmbed { .. }));
    }

    // ── on_end ───────────────────────────────────────────────────────────

    #[test]
    fn end_no_content_no_embed_created() {
        let mut s = make_state();
        s.on_start();
        let effects = s.on_end(None, None);
        // No embed was ever created, so no delete needed
        assert!(!effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, CallEffect::SendEmbed { .. }))
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, CallEffect::UpdateDb { .. }))
        );
    }

    #[test]
    fn end_with_audio_but_no_transcription_attaches() {
        // Decoupled transcription: a real (non-silent) transmission whose
        // transcript never arrived (lagging or failed) still posts an embed and
        // attaches its audio. The transcript, if it ever comes, edits later.
        let mut s = make_state();
        s.on_start();
        s.audio_kept = true; // simulate an audible (kept) segment
        let effects = s.on_end(None, Some(vec![1, 2, 3]));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, CallEffect::SendEmbed { .. })),
            "embed should be created at end even without a transcript"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, CallEffect::AttachAudio { .. })),
            "non-silent audio should be attached"
        );
        assert!(!effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
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
        s.audio_kept = true; // simulate an audible (kept) segment
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
    fn end_with_only_ctcss_no_embed() {
        let mut s = make_state();
        s.on_start();
        s.on_tones_detected(vec![DetectedTone::Ctcss { freq: 131.8 }]);
        let effects = s.on_end(None, None);
        // CTCSS-only doesn't create an embed, so no delete needed
        assert!(!effects.iter().any(|e| matches!(e, CallEffect::DeleteEmbed)));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, CallEffect::SendEmbed { .. }))
        );
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
        s.audio_kept = true; // simulate an audible (kept) segment
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
        assert_eq!(embed.title, "Fire Dispatch · Fire"); // no ellipsis
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
    fn ptt_boundary_accounts_and_separator_inserts() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello");
        let call_data = proto::CallData {
            length: 3.0,
            signal: Some(-15.0),
            ..Default::default()
        };
        // Accounting is synchronous at the boundary (decoupled from transcript).
        let effects = s.on_ptt_boundary(Some(&call_data));
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], CallEffect::EditEmbed { .. }));
        assert_eq!(s.ptt_count, 1);
        assert_eq!(s.accumulated_duration.as_secs_f64(), 3.0);
        // The separator rides the worker FIFO and lands afterward.
        let effects = s.on_ptt_separator();
        assert_eq!(effects.len(), 1);
        assert!(s.transcription_text.ends_with("\n---\n"));
    }

    #[test]
    fn ptt_start_clears_ended_state() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello");
        s.on_ptt_boundary(None);
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
        s.on_transcription("test");
        s.audio_kept = true; // simulate an audible (kept) segment
        s.on_ptt_boundary(Some(&proto::CallData {
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
        let Some(CallEffect::UpdateDb { update }) = effects
            .iter()
            .find(|e| matches!(e, CallEffect::UpdateDb { .. }))
        else {
            panic!("expected UpdateDb");
        };
        assert_eq!(update.duration.unwrap().as_secs_f64(), 8.0);
    }

    #[test]
    fn multi_ptt_averages_signal() {
        let mut s = make_state();
        s.on_start();
        s.audio_kept = true; // simulate an audible (kept) segment
        s.on_transcription("test");
        s.on_ptt_boundary(Some(&proto::CallData {
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
        s.on_ptt_boundary(None);
        s.on_ptt_start(&test_call_info());
        s.on_ptt_boundary(None);
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
        s.on_ptt_separator();
        s.on_ptt_start(&test_call_info());
        s.on_transcription("second ptt");
        assert_eq!(s.transcription_text, "first ptt\n---\nsecond ptt");
    }

    #[test]
    fn single_ptt_on_end_same_as_before() {
        let mut s = make_state();
        s.on_start();
        s.on_transcription("hello");
        s.audio_kept = true; // simulate an audible (kept) segment
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
