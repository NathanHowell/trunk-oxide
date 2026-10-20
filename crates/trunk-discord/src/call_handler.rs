use std::sync::Arc;

use anyhow::Result;
use opentelemetry::KeyValue;
use proto::types::CallRowId;
use serenity::model::id::{ChannelId, MessageId};
use tokio::sync::{Mutex, mpsc};
use tracing::Instrument;
use trunk_oxide_proto as proto;

use trunk_database::Database;

use crate::audio_shape::{self, AudioShape};
use crate::call_state::{CallEffect, CallState, CallStateConfig};
use crate::channel_writer::{ChannelWriterHandle, ChannelWriters, WriteIdentity};
use crate::discord::{DiscordClient, TRANSCRIBING_EMOJI};
use crate::grpc_transcriber::{TranscriberConnection, Transcription};
use crate::metrics::Metrics;
use crate::ogg_encoder::OggOpusEncoder;
use crate::resampler::Resampler;
use crate::tone_detect::{DetectedTone, NotchFilter, ToneDetector};

/// Keeps the `unfinalized_handlers` gauge incremented for as long as a handler's
/// embed is grey (started but not finalized). [`Self::finalized`] decrements it
/// the moment the embed turns green; `Drop` is an idempotent safety net for any
/// exit path that skips the explicit call (early return, panic). The gap between
/// this gauge and `active + coalescing` is exactly the grey-embed leak that
/// `live_handlers` (the executor's `self.calls` view) cannot see.
struct UnfinalizedGuard {
    metrics: Arc<Metrics>,
    armed: bool,
}

impl UnfinalizedGuard {
    fn new(metrics: Arc<Metrics>) -> Self {
        metrics.handler_started();
        Self {
            metrics,
            armed: true,
        }
    }

    fn finalized(&mut self) {
        if self.armed {
            self.armed = false;
            self.metrics.handler_finalized();
        }
    }
}

impl Drop for UnfinalizedGuard {
    fn drop(&mut self) {
        self.finalized();
    }
}

pub enum CallMessage {
    Audio { samples: Vec<u8>, sample_rate: i32 },
    Event(String),
    End(Option<Box<proto::CallData>>),
    PttEnd(Option<Box<proto::CallData>>),
    PttStart(Box<proto::CallInfo>),
}

/// Work item handed to the per-call transcription worker. Audio chunks are the
/// only slow item; `Boundary` is an ordering marker that rides the same FIFO so
/// the PTT separator lands after that transmission's transcript text. There is
/// no end marker: the worker exits when its job channel closes (the handler
/// drops the sender at finalization), and that closure is the "done" signal.
enum TranscribeJob {
    Audio { audio: Vec<f32>, meta: SegmentMeta },
    Boundary,
}

/// Per-segment metadata for tracing. Lets a call's trace show each transmission
/// segment in order — its position in the call (`offset_s`), length, and loudest
/// 0.5 s window — so a clipped/missing portion is visible (a `segment_gated`
/// span at `offset_s ≈ 0`, or a short `segment` span where audio was expected).
#[derive(Clone, Copy)]
struct SegmentMeta {
    /// 0-based index of this segment within the call.
    index: u32,
    /// Call-relative start of this segment, in seconds (transcription-audio clock).
    offset_s: f64,
    /// Segment length in samples (16 kHz).
    samples: usize,
    /// Where speech starts/ends in this segment (None if no voiced frame).
    shape: Option<AudioShape>,
}

/// Result handed back from the transcription worker, applied in FIFO order by
/// the handler's main loop. `Boundary` is passed straight through. It carries
/// no data: duration/PTT/signal accounting happens synchronously at the PTT
/// boundary ([`CallState::on_ptt_boundary`]); the marker only sequences the
/// transcript separator against the text chunks.
enum TranscribeResult {
    Text(String),
    Boundary,
}

/// Per-call identity used to label whisperer transcription metrics.
struct TranscribeIdentity {
    short_name: String,
    is_conventional: bool,
    tdma: bool,
    talkgroup: i64,
    /// Talkgroup priority (lower = more important) for whisperer admission order.
    priority: i32,
}

#[derive(Clone, Copy)]
pub struct CallHandlerConfig {
    pub max_segment_ms: u64,
    pub tone_detection: bool,
    pub tone_detection_duration_s: f32,
    pub audio_attachment: bool,
    /// Base URL of the Jaeger UI, for the embed's link to its trace.
    pub jaeger_url: Option<&'static str>,
}

pub struct CallHandler {
    state: CallState,
    /// Inbound dispatcher messages. `Option` so [`CallHandler::run`] can move
    /// the receiver into a local (the `select!` loop needs `&mut self` in arm
    /// bodies, which would conflict with borrowing `self.rx`).
    rx: Option<mpsc::Receiver<CallMessage>>,
    /// Enqueues audio/markers for the transcription worker. Unbounded so the
    /// handler never blocks here — a wedged transcription must not back-pressure
    /// the responsive loop (that is the whole point of this design). `Option` so
    /// finalization can drop it, closing the worker's input: the worker then
    /// drains its remaining jobs and exits, which is what ends the handler.
    job_tx: Option<mpsc::UnboundedSender<TranscribeJob>>,
    /// Receiver half handed to the worker in [`CallHandler::run`].
    job_rx: Option<mpsc::UnboundedReceiver<TranscribeJob>>,
    whisperer: TranscriberConnection,
    discord: Arc<DiscordClient>,
    config: CallHandlerConfig,
    audio_buffer: Vec<f32>,
    resampler: Option<Resampler>,
    tone_detector: Option<ToneDetector>,
    notch_filter: Option<NotchFilter>,
    post_channel: ChannelId,
    ogg_encoder: Option<OggOpusEncoder>,

    embed_message_id: Option<MessageId>,
    /// The embed's trace in Jaeger, once the embed is sent (its title links
    /// there on every edit).
    trace_url: Option<String>,
    /// Trace ids of the later transmissions coalesced into this embed, for
    /// the flag registry (the first is the embed's own trace).
    transmission_traces: Vec<String>,

    /// Per-channel serialized writer for this call's embed: send/edit/delete and
    /// the audio attach all go through it. Coalescing of transcript-stream edits
    /// lives in the writer (per-channel, latest-wins), so the handler just fires
    /// edits and never debounces or blocks on Discord I/O.
    writer: ChannelWriterHandle,
    /// Whether the in-flight-transcription reaction is currently on the embed.
    /// Reconciled against [`CallState::wants_transcribing_indicator`].
    indicator_shown: bool,

    // ── Debug instrumentation (audio-vs-duration / silence-gate diagnosis) ──
    /// Total 16 kHz samples ingested into the OGG/buffer (all captured audio).
    dbg_audio_samples: u64,
    /// Samples that passed the silence gate and were enqueued for transcription.
    dbg_kept_samples: u64,
    /// Transcription chunks kept / dropped by the silence gate.
    dbg_kept_chunks: u32,
    /// Running 0-based index of transcription segments (kept or gated), for spans.
    segment_index: u32,
    /// Cumulative samples of all prior segments — a segment's call-relative offset.
    segment_offset_samples: u64,

    metrics: Arc<Metrics>,

    db: Arc<Mutex<Database>>,
    call_row_id: Option<CallRowId>,

    // Identity for transcription attribution (whisperer metric labels + span).
    short_name: String,
    is_conventional: bool,
    /// P25 Phase 2 (TDMA) call; a metric facet for the audio-shape series.
    tdma: bool,
    talkgroup: i64,
    /// Talkgroup priority (lower = more important); drives whisperer admission.
    priority: i32,
}

impl CallHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        call_info: &proto::CallInfo,
        rx: mpsc::Receiver<CallMessage>,
        whisperer: TranscriberConnection,
        discord: Arc<DiscordClient>,
        config: CallHandlerConfig,
        post_channel: ChannelId,
        db: Arc<Mutex<Database>>,
        call_row_id: Option<CallRowId>,
        metrics: Arc<Metrics>,
        writers: &ChannelWriters,
    ) -> Result<Self> {
        let detect_ctcss = config.tone_detection && call_info.is_conventional;
        // Conventional channels only. On trunked P25 the detector's DTMF
        // matcher false-fires on speech and then strips up to the detection
        // window (median ~1.7 s) from the start of the transcription audio.
        let tone_detector = if config.tone_detection && call_info.is_conventional {
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

        let buffer_threshold = buffer_threshold(config.max_segment_ms);

        let state = CallState::new(
            call_info,
            CallStateConfig {
                buffer_threshold,
                audio_attachment: config.audio_attachment,
            },
        );

        let (job_tx, job_rx) = mpsc::unbounded_channel();

        let writer = writers.handle(
            post_channel,
            WriteIdentity {
                short_name: Arc::from(call_info.short_name.as_str()),
                talkgroup: call_info.talkgroup,
            },
        );

        Ok(Self {
            state,
            rx: Some(rx),
            job_tx: Some(job_tx),
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
            trace_url: None,
            transmission_traces: Vec::new(),
            writer,
            indicator_shown: false,
            metrics,
            db,
            call_row_id,
            short_name: call_info.short_name.clone(),
            is_conventional: call_info.is_conventional,
            tdma: call_info.phase2_tdma,
            talkgroup: call_info.talkgroup,
            priority: call_info.priority,
            dbg_audio_samples: 0,
            dbg_kept_samples: 0,
            dbg_kept_chunks: 0,
            segment_index: 0,
            segment_offset_samples: 0,
        })
    }

    pub async fn run(mut self) {
        let effects = self.state.on_start();
        if let Err(e) = self.process_effects(effects).await {
            tracing::error!(error = %e, "failed to post call embed");
            return;
        }
        tracing::info!(thread = %self.post_channel, "call started");

        // Count this handler as an unfinalized (grey) embed until phase 1 ends.
        // Decremented at finalize below — not at task exit — so a handler wedged
        // grey after being removed from the executor's `self.calls` still shows.
        let mut unfinalized = UnfinalizedGuard::new(Arc::clone(&self.metrics));

        // Spawn the transcription worker on the "slow lane". It owns the gRPC
        // call to whisper (bounded at 300s but potentially wedged) so the main
        // loop below never blocks on it and always stays responsive to the
        // dispatcher — which is what kept a wedged transcription from getting
        // the handler dropped (note_stuck) and orphaning the embed.
        //
        // Instrumenting the worker with the current ("call") span keeps W3C
        // trace-context propagation working: `TranscriberConnection::transcribe`
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
                    tdma: self.tdma,
                    talkgroup: self.talkgroup,
                    priority: self.priority,
                },
                job_rx,
                result_tx,
            )
            .instrument(tracing::Span::current()),
        );

        // ── Phase 1: handle the call, staying responsive to the dispatcher.
        // Transcription is enqueued to the worker and applied opportunistically;
        // it NEVER gates anything here. The call finalizes — audio attached,
        // embed turned green, DB written — the instant the dispatcher ends it
        // (`finalize_now`), independent of how far behind transcription is. This
        // is the fix: audio attachment no longer waits on the whisper backlog.
        // Transcript-stream edits are coalesced per-channel inside the writer, so
        // there is no debounce timer here: the handler just fires each edit.
        let mut results_open = true;
        loop {
            tokio::select! {
                // Apply transcription results as they arrive (in FIFO order).
                result = result_rx.recv(), if results_open => {
                    match result {
                        Some(r) => self.apply_transcript(r).await,
                        None => results_open = false, // worker gone; lose late text
                    }
                }
                // Dispatcher messages. End (or a dropped channel) finalizes the
                // call immediately and drops into the phase-2 transcript drain.
                msg = rx.recv() => {
                    match msg {
                        Some(CallMessage::End(call_data)) => {
                            self.finalize_now(call_data).await;
                            break;
                        }
                        Some(m) => self.handle_message(m).await,
                        None => {
                            // Dropped by the dispatcher without an explicit End
                            // (e.g. note_stuck or handler replacement). Finalize
                            // anyway so the embed never lingers grey.
                            self.finalize_now(None).await;
                            break;
                        }
                    }
                }
            }
        }

        // The finalizing (green) embed edit has been enqueued to the writer: the
        // handler's work is done, so this is no longer a grey-embed leak even
        // though the edit may still be in the writer's queue. Phase 2 is
        // late-transcript drain.
        unfinalized.finalized();

        // ── Phase 2: the embed is already finalized (its green edit + audio
        // attach are enqueued). Keep applying late transcription results as
        // best-effort edits so the transcript fills in. `finalize_now` dropped
        // the worker's input, so the worker drains its queued jobs, emits their
        // results, and exits — closing this channel. That closure (not a timer)
        // is what ends the handler: it lives exactly as long as there is
        // transcription in flight. Each edit is fire-and-forget into the writer,
        // which coalesces and flushes the latest even after the handler exits.
        while let Some(r) = result_rx.recv().await {
            self.apply_transcript(r).await;
        }

        // Safety net: if any results were lost (e.g. the worker task died),
        // clear the in-flight count and drop a stale ⏳ reaction.
        self.state.on_transcription_drained();
        self.reconcile_indicator().await;
    }

    /// Dispatch a single non-terminal inbound message. Transcription-bearing
    /// paths only *enqueue* work for the worker; they never await whisper inline.
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
                // Later transmissions coalesced into this embed are separate
                // traces on the recorder side; link the embed's span to each so
                // every transmission is one click away in Jaeger.
                if let Some(trace) =
                    link_transmission(&call_info.traceparent, self.state.ptt_count())
                {
                    if let Some(msg_id) = self.embed_message_id {
                        crate::reactions::FLAGS.add_transmission(msg_id, trace.clone());
                    }
                    self.transmission_traces.push(trace);
                }
                let effects = self.state.on_ptt_start(call_info.as_ref());
                if let Err(e) = self.process_effects(effects).await {
                    tracing::error!(error = %e, "PTT start handling error");
                }
            }
            // End is intercepted in the run loop's select; never routed here.
            CallMessage::End(_) => {
                tracing::error!("End message routed through handle_message");
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn process_effects(&mut self, effects: Vec<CallEffect>) -> Result<()> {
        // Use a queue to avoid async recursion (Transcribe → on_transcription → EditEmbed)
        let mut queue: std::collections::VecDeque<CallEffect> = effects.into();
        while let Some(effect) = queue.pop_front() {
            match effect {
                CallEffect::SendEmbed { mut embed } => {
                    let trace_id = current_trace_id();
                    self.trace_url = self
                        .config
                        .jaeger_url
                        .filter(|_| !trace_id.is_empty())
                        .map(|base| format!("{}/trace/{trace_id}", base.trim_end_matches('/')));
                    embed.url.clone_from(&self.trace_url);
                    // The one writer call the handler awaits: it needs the new
                    // message id for subsequent edits/attach. Sends ride the fast
                    // create bucket, so this is brief.
                    let msg_id = self.writer.send_embed(embed).await?;
                    self.embed_message_id = Some(msg_id);
                    // Remember the call so a reaction on this embed can be
                    // attributed (ground truth for clipping / lead-in loss).
                    crate::reactions::FLAGS.register(
                        msg_id,
                        crate::reactions::FlagInfo {
                            short_name: self.short_name.clone(),
                            is_conventional: self.is_conventional,
                            tdma: self.tdma,
                            talkgroup: self.talkgroup,
                            trace_id,
                            transmission_traces: self.transmission_traces.clone(),
                        },
                    );
                }
                // Prompt and throttled edits are identical now: the writer
                // coalesces per-channel (latest-wins per message) and paces
                // itself to the edit bucket, so there is no debounce here.
                CallEffect::EditEmbed { mut embed }
                | CallEffect::EditEmbedThrottled { mut embed } => {
                    embed.url.clone_from(&self.trace_url);
                    if let Some(msg_id) = self.embed_message_id {
                        self.writer.edit_embed(msg_id, embed);
                    }
                }
                CallEffect::DeleteEmbed => {
                    if let Some(msg_id) = self.embed_message_id {
                        self.writer.delete_embed(msg_id);
                    }
                }
                CallEffect::AttachAudio { ogg_data, filename } => {
                    // The green embed edit was already enqueued (just above, in
                    // this same on_end batch). Hand the OGG to the writer's
                    // create lane, which posts it as a reply on the independent
                    // message-create bucket — concurrent with, and never stalled
                    // by, the edit lane. Never blocks; sheds if the lane is full.
                    if let Some(msg_id) = self.embed_message_id {
                        self.writer.attach(msg_id, ogg_data, filename);
                    }
                }
                CallEffect::Transcribe { audio } => {
                    // Hand the audio to the slow-lane worker (silence-gated); the
                    // resulting text is applied later, in FIFO order, via
                    // `apply_transcript`. The send never blocks (unbounded). Any
                    // in-flight-marker edit rides the same effect queue.
                    queue.extend(self.enqueue_audio(audio));
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

        // Reconcile the in-flight reaction against the state after every batch.
        // Cheap no-op unless the pending-transcription count just crossed
        // zero (the only times it does Discord I/O), and it lives on a separate
        // rate-limit bucket from the edits above.
        self.reconcile_indicator().await;
        Ok(())
    }

    /// Reconcile the ⏳ reaction against whether transcription is in flight.
    /// Only touches Discord when the desired state actually flips, and only
    /// once an embed message exists to react to.
    async fn reconcile_indicator(&mut self) {
        let want = self.state.wants_transcribing_indicator();
        if want == self.indicator_shown {
            return;
        }
        let Some(msg_id) = self.embed_message_id else {
            return; // nothing to react to yet; revisit on a later batch
        };
        if want {
            match self
                .discord
                .add_reaction(self.post_channel, msg_id, TRANSCRIBING_EMOJI)
                .await
            {
                Ok(()) => self.indicator_shown = true,
                Err(e) => tracing::warn!(error = %e, "failed to add transcribing reaction"),
            }
        } else {
            if let Err(e) = self
                .discord
                .remove_reaction(self.post_channel, msg_id, TRANSCRIBING_EMOJI)
                .await
            {
                tracing::warn!(error = %e, "failed to remove transcribing reaction");
            }
            self.indicator_shown = false;
        }
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

        let sample_count: u32 = samples.len().try_into().expect("audio chunk exceeds u32");

        if let Some(enc) = &mut self.ogg_encoder {
            enc.process(&samples)?;
        }

        self.audio_buffer.extend_from_slice(&samples);
        self.dbg_audio_samples += u64::from(sample_count);

        if self
            .tone_detector
            .as_ref()
            .is_some_and(ToneDetector::is_complete)
        {
            self.finish_tone_detection().await?;
        }

        // Normally we do NOT flush mid-transmission: the whole transmission is
        // transcribed as one request at the PTT/squelch boundary (handle_ptt_end)
        // or at call end. This is only a safety cap that force-flushes a
        // sub-segment if a single transmission grows pathologically long, to
        // bound memory and decode latency. `max_segment_ms` defaults to 2 min,
        // far above any real transmission.
        let buffer_len = self.audio_buffer.len();
        let audio_drain: Vec<f32> = if buffer_len >= buffer_threshold(self.config.max_segment_ms) {
            std::mem::take(&mut self.audio_buffer)
        } else {
            Vec::new()
        };

        let effects = self.state.on_audio_processed(buffer_len, audio_drain);
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

            // A false-positive tone eats the start of the speech, so count what
            // was found and how much audio was removed because of it.
            let labels = self.labels();
            for tone in &tones {
                let kind = [KeyValue::new("kind", tone_kind(tone))];
                self.metrics
                    .tones_detected
                    .add(1, &[&labels[..], &kind[..]].concat());
            }
            self.metrics
                .tone_strip_seconds
                .record(strip as f64 / 16000.0, &labels);
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

    /// Metric facets for the audio-shape series (low cardinality).
    fn labels(&self) -> [KeyValue; 3] {
        shape_labels(&self.short_name, self.is_conventional, self.tdma)
    }

    /// Enqueue a job for the transcription worker. The channel is unbounded so
    /// this never blocks; if the worker's input has been closed at finalization
    /// (or the worker exited), the job is dropped.
    fn enqueue_job(&self, job: TranscribeJob) {
        if let Some(tx) = &self.job_tx {
            let _ = tx.send(job);
        }
    }

    /// Enqueue a transmission segment for transcription. trunk-oxide only
    /// sends audio of transmissions that proved audible (its recorders hold
    /// a transmission back until a half-second window clears their RMS
    /// threshold and drop it otherwise), so everything that arrives is
    /// worth transcribing and attaching.
    fn enqueue_audio(&mut self, audio: Vec<f32>) -> Vec<CallEffect> {
        let samples = audio.len();
        if samples == 0 {
            return Vec::new();
        }
        // Shape metrics: a clipped lead-in or tail shows here.
        let shape = audio_shape::analyze(&audio);
        if let Some(s) = &shape {
            record_audio_shape(&self.metrics, &self.labels(), s);
        }

        let meta = SegmentMeta {
            index: self.segment_index,
            offset_s: self.segment_offset_samples as f64 / 16000.0,
            samples,
            shape,
        };
        self.segment_index += 1;
        self.segment_offset_samples += samples as u64;
        self.dbg_kept_samples += samples as u64;
        self.dbg_kept_chunks += 1;
        self.enqueue_job(TranscribeJob::Audio { audio, meta });
        // Track it as in-flight so the executor shows the ⏳ reaction until
        // the matching result comes back via `apply_transcript`. Emits no
        // effect (reaction is reconciled after each batch).
        self.state.on_transcription_enqueued()
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
    /// Returns the in-flight-marker effect; callers that re-render the embed
    /// immediately afterwards (PTT boundary / finalize) can discard it.
    fn enqueue_remaining_audio(&mut self) -> Vec<CallEffect> {
        let audio: Vec<f32> = std::mem::take(&mut self.audio_buffer);
        self.enqueue_audio(audio)
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
        // Bumps the in-flight count; the `on_ptt_boundary` render below already
        // rebuilds the embed with it, so discard this effect.
        let _ = self.enqueue_remaining_audio();

        // Account for this transmission *now* — duration, PTT count, signal —
        // decoupled from transcription. The embed then shows correct metadata
        // even when whisper yields no text (clicks/static) or lags, instead of
        // a bare embed whose duration only ever lands if a transcript happens
        // to re-render it. The separator marker still rides the worker FIFO
        // (enqueued after this PTT's audio) to stay sequenced with the text.
        let effects = self.state.on_ptt_boundary(call_data.as_deref());
        self.process_effects(effects).await?;
        self.enqueue_job(TranscribeJob::Boundary);

        // Reset resampler for next PTT (may have different sample rate).
        self.resampler = None;
        Ok(())
    }

    /// Finalize the call *now*, decoupled from transcription. Flushes DSP,
    /// enqueues the trailing audio for (best-effort, late) transcription, then
    /// closes the worker's input and runs `finalize` immediately — so the audio
    /// attaches and the embed turns green without waiting on the whisper
    /// backlog. Dropping `job_tx` lets the worker drain its queue and exit;
    /// late transcript text then edits the already-final embed in phase 2.
    async fn finalize_now(&mut self, call_data: Option<Box<proto::CallData>>) {
        if let Err(e) = self.flush_dsp().await {
            tracing::warn!(error = %e, "pre-finalization flush failed, proceeding to end call");
        }
        // Bumps the in-flight count; `finalize`'s `on_end` render reflects it.
        let _ = self.enqueue_remaining_audio();
        self.job_tx = None; // close worker input → it drains then exits
        self.finalize(call_data).await;
    }

    /// Apply one worker result as a best-effort embed edit. Safe before *and*
    /// after finalization: a plain embed edit preserves the green color,
    /// duration, and audio attachment (the state machine derives those from
    /// `call_ended` + accumulators, and a serenity edit without `new_attachment`
    /// omits the attachments field, so Discord keeps the existing audio file).
    async fn apply_transcript(&mut self, result: TranscribeResult) {
        let effects = match result {
            TranscribeResult::Text(text) => self.state.on_transcription(&text),
            TranscribeResult::Boundary => self.state.on_ptt_separator(),
        };
        if let Err(e) = self.process_effects(effects).await {
            tracing::error!(error = %e, "transcript edit error");
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

        // Audio-vs-duration diagnosis: compare the total captured audio (what's
        // in the OGG) against the summed recording_length (what drives Duration).
        let audio_secs = self.dbg_audio_samples as f64 / 16000.0;
        let transcribed_secs = self.dbg_kept_samples as f64 / 16000.0;
        tracing::info!(
            target: "audio_debug",
            audio_secs,
            duration_secs = self.state.accumulated_duration().as_secs_f64(),
            transcribed_secs,
            ptt_count = self.state.ptt_count(),
            chunks = self.dbg_kept_chunks,
            "call audio summary: OGG audio vs Duration vs transcribed"
        );
    }
}

/// Per-call transcription worker — the "slow lane". Consumes jobs in FIFO order
/// and emits results in the same order, so transcript text and PTT separators
/// stay correctly sequenced. It owns the (300s-bounded, but possibly wedged)
/// gRPC transcription call, keeping it off the handler's responsive loop. Exits
/// when the job channel closes (handler finalized/dropped) or the handler stops
/// receiving results; that exit closes the result channel, which ends the
/// handler's phase-2 drain.
async fn transcription_worker(
    whisperer: TranscriberConnection,
    metrics: Arc<Metrics>,
    identity: TranscribeIdentity,
    mut job_rx: mpsc::UnboundedReceiver<TranscribeJob>,
    result_tx: mpsc::Sender<TranscribeResult>,
) {
    while let Some(job) = job_rx.recv().await {
        let result = match job {
            TranscribeJob::Audio { audio, meta } => {
                // One `segment` span per transmission segment, parented to the
                // call span (the worker is instrumented with it). It carries the
                // segment's place in the call (offset_s/index) and length, and
                // is the parent of the downstream transcribe RPC
                // spans — so a trace shows each segment's position and decode.
                let span = tracing::info_span!(
                    "segment",
                    chunk_index = meta.index,
                    offset_s = meta.offset_s,
                    samples = meta.samples,
                );
                let samples = audio.len();
                let t = transcribe_one(&whisperer, &metrics, &identity, audio)
                    .instrument(span)
                    .await;
                record_transcript_coverage(&metrics, &identity, &meta, samples, &t);
                TranscribeResult::Text(t.text)
            }
            TranscribeJob::Boundary => TranscribeResult::Boundary,
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
    whisperer: &TranscriberConnection,
    metrics: &Metrics,
    identity: &TranscribeIdentity,
    audio: Vec<f32>,
) -> Transcription {
    if audio.is_empty() {
        return Transcription::empty();
    }
    let start = std::time::Instant::now();
    let outcome = whisperer
        .transcribe(
            audio,
            &identity.short_name,
            identity.is_conventional,
            identity.talkgroup,
            identity.priority,
        )
        .await;
    metrics
        .transcription_duration
        .record(start.elapsed().as_secs_f64(), &[]);
    match outcome {
        Ok(Some(t)) => t,
        Ok(None) => {
            metrics.transcriptions_timed_out.add(1, &[]);
            Transcription::empty()
        }
        Err(e) => {
            tracing::error!(error = %e, "transcription request failed");
            Transcription::empty()
        }
    }
}

/// Trace id of the active span (shared with trunk-oxide's call span through
/// the propagated `traceparent`), for linking a flagged call to its trace.
fn current_trace_id() -> String {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let cx = tracing::Span::current().context();
    let span = cx.span();
    let sc = span.span_context();
    if sc.is_valid() {
        sc.trace_id().to_string()
    } else {
        String::new()
    }
}

/// Link the current (embed) span to a later transmission's recorder-side call
/// span, given its W3C `traceparent`, and return that trace's id. `None` when
/// the traceparent is empty or invalid.
fn link_transmission(traceparent: &str, index: u32) -> Option<String> {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    if traceparent.is_empty() {
        return None;
    }
    let cx = opentelemetry::global::get_text_map_propagator(|prop| {
        let mut carrier = std::collections::HashMap::new();
        carrier.insert("traceparent".to_string(), traceparent.to_string());
        prop.extract(&carrier)
    });
    let span_context = cx.span().span_context().clone();
    if !span_context.is_valid() {
        return None;
    }
    let trace = span_context.trace_id().to_string();
    tracing::Span::current().add_link_with_attributes(
        span_context,
        vec![KeyValue::new("transmission", i64::from(index))],
    );
    Some(trace)
}

fn shape_labels(short_name: &str, is_conventional: bool, tdma: bool) -> [KeyValue; 3] {
    [
        KeyValue::new("short_name", short_name.to_string()),
        KeyValue::new("is_conventional", is_conventional),
        KeyValue::new("tdma", tdma),
    ]
}

fn record_audio_shape(metrics: &Metrics, labels: &[KeyValue; 3], s: &AudioShape) {
    metrics.audio_leading_silence.record(s.onset_s, labels);
    metrics
        .audio_trailing_silence
        .record(s.trailing_silence_s, labels);
    metrics.audio_end_level_db.record(s.end_level_db, labels);
    if s.abrupt_end() {
        metrics.audio_abrupt_ends.add(1, labels);
    }
    metrics.audio_gaps.record(f64::from(s.gaps), labels);
    if s.gaps > 0 {
        metrics
            .audio_longest_gap_ms
            .record(s.longest_gap_s * 1000.0, labels);
    }
}

/// Stable, low-cardinality name for a detected tone.
const fn tone_kind(tone: &DetectedTone) -> &'static str {
    match tone {
        DetectedTone::TwoTone { .. } => "two_tone",
        DetectedTone::LongTone { .. } => "long_tone",
        DetectedTone::HiLowWarble { .. } => "hi_low_warble",
        DetectedTone::Dtmf { .. } => "dtmf",
        DetectedTone::Ctcss { .. } => "ctcss",
    }
}

/// Compare where the model put its text against where the audio's speech is.
/// A positive leading gap means text starts later than the speech does — the
/// "first second or two is missing" symptom — and the audio's own onset tells
/// us whether that is the model or the capture.
fn record_transcript_coverage(
    metrics: &Metrics,
    identity: &TranscribeIdentity,
    meta: &SegmentMeta,
    samples: usize,
    t: &Transcription,
) {
    let labels = shape_labels(
        &identity.short_name,
        identity.is_conventional,
        identity.tdma,
    );
    if t.text.trim().is_empty() {
        metrics.transcript_empty.add(1, &labels);
        return;
    }
    let (Some(first), Some(last)) = (t.first_start_s, t.last_end_s) else {
        return; // backend gave no timing
    };
    let (first, last) = (f64::from(first), f64::from(last));
    metrics.transcript_first_text_offset.record(first, &labels);
    if let Some(shape) = &meta.shape {
        metrics
            .transcript_leading_gap
            .record(first - shape.onset_s, &labels);
        let audio_end = samples as f64 / 16000.0 - shape.trailing_silence_s;
        metrics
            .transcript_trailing_gap
            .record(audio_end - last, &labels);
    }
}

/// Samples for a given duration in ms at 16 kHz mono (16 samples/ms).
fn buffer_threshold(duration_ms: u64) -> usize {
    usize::try_from(duration_ms)
        .unwrap_or(usize::MAX)
        .saturating_mul(16)
}

fn decode_audio(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| f32::from(i16::from_le_bytes(pair)) / 32768.0)
        .collect()
}
