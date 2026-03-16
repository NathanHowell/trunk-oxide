use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use opentelemetry::KeyValue;
use serenity::model::id::ChannelId;
use tokio::sync::{Mutex, mpsc};
use tonic::Streaming;
use tracing::Instrument;
use trunk_oxide_proto as proto;

use proto::types::{SysNum, Talkgroup};
use trunk_database::Database;

use crate::aggregator::Aggregator;
use crate::call_handler::{CallHandler, CallHandlerConfig, CallMessage};
use crate::channel_manager::ChannelLookup;
use crate::discord::DiscordClient;
use crate::dispatcher_state::{CallKey, DispatchEffect, DispatcherState};
use crate::grpc_whisperer::WhispererConnection;
use crate::metrics::Metrics;

/// How often to check for stuck calls.
const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// How often to check for expired coalescing calls.
const COALESCE_TICK_INTERVAL: Duration = Duration::from_secs(1);

struct ActiveCall {
    tx: mpsc::Sender<CallMessage>,
}

struct ResolveResult {
    key: CallKey,
    call: proto::CallInfo,
    post_channel: Option<ChannelId>,
}

/// Shared resources accessed by the pump and spawned tasks.
pub struct DispatchContext {
    whisperer: WhispererConnection,
    discord: Arc<DiscordClient>,
    aggregator: Arc<Aggregator>,
    config: CallHandlerConfig,
    channel_id: ChannelId,
    channel_lookup: ChannelLookup,
    db: Arc<Mutex<Database>>,
    db_path: PathBuf,
    metrics: Arc<Metrics>,
}

/// Single-owner pump loop. Owns `DispatcherState` directly — no Mutex.
pub struct DispatchPump {
    state: DispatcherState,
    calls: HashMap<CallKey, ActiveCall>,
    pending_rx: HashMap<CallKey, mpsc::Receiver<CallMessage>>,
    ctx: Arc<DispatchContext>,
    resolve_tx: mpsc::Sender<ResolveResult>,
    resolve_rx: mpsc::Receiver<ResolveResult>,
    coalesce_enabled: bool,
}

impl DispatchPump {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        whisperer: WhispererConnection,
        discord: Arc<DiscordClient>,
        aggregator: Arc<Aggregator>,
        buffer_duration_ms: u64,
        channel_id: ChannelId,
        channel_lookup: ChannelLookup,
        tone_detection: bool,
        tone_detection_duration_s: f32,
        audio_attachment: bool,
        coalesce_timeout_ms: u64,
        db: Arc<Mutex<Database>>,
        db_path: PathBuf,
        metrics: Arc<Metrics>,
    ) -> Self {
        let (resolve_tx, resolve_rx) = mpsc::channel(64);

        Self {
            state: DispatcherState::new(coalesce_timeout_ms),
            calls: HashMap::new(),
            pending_rx: HashMap::new(),
            ctx: Arc::new(DispatchContext {
                whisperer,
                discord,
                aggregator,
                config: CallHandlerConfig {
                    buffer_duration_ms,
                    tone_detection,
                    tone_detection_duration_s,
                    audio_attachment,
                },
                channel_id,
                channel_lookup,
                db,
                db_path,
                metrics,
            }),
            resolve_tx,
            resolve_rx,
            coalesce_enabled: coalesce_timeout_ms > 0,
        }
    }

    pub async fn run(&mut self, stream: &mut Streaming<proto::StreamMessage>) {
        let mut reap_interval = tokio::time::interval(REAP_INTERVAL);
        let mut coalesce_interval = tokio::time::interval(COALESCE_TICK_INTERVAL);
        reap_interval.tick().await; // skip immediate first tick
        coalesce_interval.tick().await;

        loop {
            tokio::select! {
                msg = stream.message() => {
                    match msg {
                        Ok(Some(msg)) => self.handle_stream_message(msg).await,
                        Ok(None) => {
                            tracing::info!("gRPC stream closed by server");
                            break;
                        }
                        Err(status) => {
                            tracing::error!(%status, "gRPC stream error");
                            break;
                        }
                    }
                }
                _ = reap_interval.tick() => {
                    let effects = self.state.on_reap_tick();
                    self.process_effects(effects).await;
                }
                _ = coalesce_interval.tick(), if self.coalesce_enabled => {
                    let effects = self.state.on_coalesce_tick();
                    self.process_effects(effects).await;
                }
                Some(result) = self.resolve_rx.recv() => {
                    self.handle_resolve_result(result).await;
                }
            }
        }
    }

    async fn handle_stream_message(&mut self, msg: proto::StreamMessage) {
        use proto::stream_message::Message;

        let Some(message) = msg.message else {
            return;
        };

        match message {
            Message::Audio(sample) => {
                let effects = self.state.on_audio(&sample);
                self.process_effects(effects).await;
            }
            Message::Event(event) => {
                let effects = self.state.on_event(event);
                self.process_effects(effects).await;
            }
        }
    }

    async fn handle_resolve_result(&mut self, result: ResolveResult) {
        if let Some(post_channel) = result.post_channel {
            let effects = self.state.on_channel_resolved(result.call, post_channel);
            self.process_effects(effects).await;
        } else {
            self.ctx.metrics.channel_resolution_failures.add(1, &[]);
            self.state.on_channel_resolution_failed(&result.key);
            self.calls.remove(&result.key);
            self.pending_rx.remove(&result.key);
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn process_effects(&mut self, effects: Vec<DispatchEffect>) {
        for effect in effects {
            match effect {
                DispatchEffect::SendAudio {
                    key,
                    samples,
                    sample_rate,
                } => {
                    if let Some(active) = self.calls.get(&key)
                        && let Err(e) = active.tx.try_send(CallMessage::Audio {
                            samples,
                            sample_rate,
                        })
                    {
                        self.ctx
                            .metrics
                            .audio_packets_dropped
                            .add(1, &[KeyValue::new("talkgroup", key.talkgroup.0)]);
                        tracing::warn!(talkgroup = key.talkgroup.0, "audio packet dropped: {e}");
                    }
                }

                DispatchEffect::SendEvent { key, text } => {
                    if let Some(active) = self.calls.get(&key)
                        && let Err(e) = active.tx.try_send(CallMessage::Event(text))
                    {
                        self.ctx
                            .metrics
                            .events_dropped
                            .add(1, &[KeyValue::new("talkgroup", key.talkgroup.0)]);
                        tracing::warn!(talkgroup = key.talkgroup.0, "event dropped: {e}");
                    }
                }

                DispatchEffect::EndCall { key, call_data } => {
                    if let Some(active) = self.calls.remove(&key) {
                        let _ = active.tx.send(CallMessage::End(call_data)).await;
                    }
                }

                DispatchEffect::ReapCall { key } => {
                    self.ctx.metrics.calls_reaped.add(1, &[]);
                    if let Some(active) = self.calls.remove(&key) {
                        let _ = active.tx.try_send(CallMessage::End(None));
                    }
                }

                DispatchEffect::PttEnd { key, call_data } => {
                    if let Some(active) = self.calls.get(&key) {
                        let _ = active.tx.send(CallMessage::PttEnd(call_data)).await;
                    }
                }

                DispatchEffect::CoalesceResume { key, call } => {
                    if let Some(active) = self.calls.get(&key) {
                        let _ = active.tx.send(CallMessage::PttStart(Box::new(call))).await;
                    }
                }

                DispatchEffect::CoalesceTimeout { key } => {
                    if let Some(active) = self.calls.remove(&key) {
                        let _ = active.tx.send(CallMessage::End(None)).await;
                    }
                }

                DispatchEffect::ForwardToAggregator { message } => {
                    self.ctx.aggregator.send(message);
                }

                DispatchEffect::RotateDb => {
                    self.rotate_db().await;
                }

                DispatchEffect::ResolveChannel { key, call } => {
                    // Create mpsc for buffering audio during resolution
                    let (tx, rx) = mpsc::channel(1024);
                    self.calls.insert(key.clone(), ActiveCall { tx });
                    self.pending_rx.insert(key.clone(), rx);

                    let ctx = Arc::clone(&self.ctx);
                    let resolve_tx = self.resolve_tx.clone();
                    tokio::spawn(async move {
                        let post_channel = ctx.resolve_channel(&call).await;
                        let _ = resolve_tx
                            .send(ResolveResult {
                                key,
                                call,
                                post_channel,
                            })
                            .await;
                    });
                }

                DispatchEffect::SpawnCallHandler {
                    key,
                    call,
                    post_channel,
                } => {
                    let rx = self.pending_rx.remove(&key);
                    self.spawn_call_handler(key, call, post_channel, rx).await;
                }

                DispatchEffect::Log { message } => {
                    tracing::info!("{message}");
                }
            }
        }

        self.ctx
            .metrics
            .active_calls
            .record(self.state.active_call_count() as u64, &[]);
        self.ctx
            .metrics
            .coalescing_calls
            .record(self.state.coalescing_call_count() as u64, &[]);
    }

    async fn spawn_call_handler(
        &mut self,
        key: CallKey,
        call: proto::CallInfo,
        post_channel: ChannelId,
        pending_rx: Option<mpsc::Receiver<CallMessage>>,
    ) {
        let call_row_id = {
            let record = trunk_database::CallRecord {
                talkgroup: Talkgroup(call.talkgroup),
                sys_num: SysNum(call.sys_num),
                freq: proto::types::Freq(call.freq),
                encrypted: call.encrypted,
                emergency: call.emergency,
                start_time: call.start_time.as_ref().map_or(0, |ts| ts.seconds),
                tag: call.talkgroup.to_string(),
                short_name: call.short_name.clone(),
                thread_id: Some(post_channel.get()),
                embed_message_id: None,
            };
            let db = self.ctx.db.lock().await;
            match db.insert_call(&record) {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to insert call into db");
                    None
                }
            }
        };

        // If we have a pending receiver (from buffering during resolution),
        // use it. Otherwise create a fresh channel (e.g. CoalesceResume path).
        let (tx, rx) = if let Some(rx) = pending_rx {
            // Reuse existing sender from self.calls
            let existing = self.calls.remove(&key);
            if let Some(active) = existing {
                (active.tx, rx)
            } else {
                // Shouldn't happen, but handle gracefully
                let (tx, _) = mpsc::channel(1024);
                (tx, rx)
            }
        } else {
            let (tx, rx) = mpsc::channel(1024);
            (tx, rx)
        };

        let span = tracing::info_span!(
            "call",
            talkgroup = call.talkgroup,
            sys_num = call.sys_num,
            freq = call.freq,
            encrypted = call.encrypted,
        );

        let handler = match CallHandler::new(
            &call,
            rx,
            self.ctx.whisperer.clone(),
            Arc::clone(&self.ctx.discord),
            self.ctx.config,
            post_channel,
            Arc::clone(&self.ctx.db),
            call_row_id,
            Arc::clone(&self.ctx.metrics),
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(error = %e, "failed to create call handler");
                return;
            }
        };

        tokio::spawn(handler.run().instrument(span));

        self.calls.insert(key, ActiveCall { tx });
    }

    async fn rotate_db(&self) {
        let now_hour = Utc::now().format("%Y-%m-%dT%H").to_string();
        let today = Utc::now().format("%Y-%m-%d").to_string();

        match Database::open_hourly(&self.ctx.db_path, &now_hour) {
            Ok(new_db) => {
                {
                    let mut db = self.ctx.db.lock().await;
                    if let Err(e) = new_db.copy_threads_from(&db, &today) {
                        tracing::warn!(error = %e, "failed to copy thread cache to new hourly db");
                    }
                    *db = new_db;
                }
                tracing::info!(hour = %now_hour, "rotated to new hourly database");
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to open new hourly database");
            }
        }
    }
}

impl DispatchContext {
    async fn resolve_channel(&self, call: &proto::CallInfo) -> Option<ChannelId> {
        let start = std::time::Instant::now();
        let result = self.resolve_channel_inner(call).await;
        self.metrics
            .channel_resolution_duration
            .record(start.elapsed().as_secs_f64(), &[]);
        result
    }

    async fn resolve_channel_inner(&self, call: &proto::CallInfo) -> Option<ChannelId> {
        let talkgroup = Talkgroup(call.talkgroup);
        let sys_num = SysNum(call.sys_num);

        // Check mapped channels first
        let mapped_channel = {
            let map = self.channel_lookup.read().await;
            map.get(&(call.short_name.clone(), call.talkgroup)).copied()
        };

        if let Some(channel) = mapped_channel {
            return Some(channel);
        }

        // Fallback: create/reuse a per-talkgroup-per-day thread
        let start_time = call
            .start_time
            .as_ref()
            .and_then(|ts| {
                DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos.try_into().unwrap_or(0))
            })
            .unwrap_or_else(Utc::now);
        let date_str = start_time.format("%Y-%m-%d").to_string();

        // Check DB for thread from earlier this session/hour
        let db_thread = {
            let db = self.db.lock().await;
            db.get_thread(sys_num, talkgroup, &date_str)
                .unwrap_or(None)
                .and_then(|id| std::num::NonZeroU64::new(id).map(ChannelId::from))
        };

        if let Some(id) = db_thread {
            return Some(id);
        }

        // Create new thread
        let thread_name = format!("TG {} [{}]", call.talkgroup, date_str);
        let thread_name = if thread_name.len() > 100 {
            thread_name[..100].to_string()
        } else {
            thread_name
        };

        match self
            .discord
            .create_call_thread(self.channel_id, &thread_name)
            .await
        {
            Ok(id) => {
                {
                    let db = self.db.lock().await;
                    if let Err(e) = db.set_thread(sys_num, talkgroup, &date_str, id.get()) {
                        tracing::warn!(error = %e, "failed to persist thread to db");
                    }
                }

                Some(id)
            }
            Err(e) => {
                tracing::error!(
                    talkgroup = call.talkgroup,
                    error = %e,
                    "failed to create discord thread"
                );
                None
            }
        }
    }
}
