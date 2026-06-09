use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Unconditional heartbeat tick so the liveness watchdog (see `main.rs`) sees a
/// fresh timestamp whenever the pump loop is alive, independent of traffic or
/// whether coalescing is enabled.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// Upper bound on how long the pump will wait to hand a message to a single
/// call handler. A handler that won't accept within this window is treated as
/// stuck and dropped, so one wedged call can never freeze the whole dispatcher.
const CALL_SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Milliseconds since the Unix epoch, saturating to 0 on clock errors.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Send `msg` to a call handler, bounded by [`CALL_SEND_TIMEOUT`]. Returns
/// `false` if the handler is gone or stuck (never freezes the caller).
async fn send_bounded(tx: &mpsc::Sender<CallMessage>, msg: CallMessage) -> bool {
    matches!(
        tokio::time::timeout(CALL_SEND_TIMEOUT, tx.send(msg)).await,
        Ok(Ok(()))
    )
}

struct ActiveCall {
    tx: mpsc::Sender<CallMessage>,
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
    /// Live call handlers, keyed by talkgroup. The sole source of truth for
    /// which handlers exist. Each entry holds the sender half of a channel
    /// whose receiver is owned by exactly one spawned handler task (created
    /// together in the `ResolveChannel` effect), so a handler can never be
    /// orphaned by a concurrent resolve — `End` always reaches the handler
    /// that owns the matching receiver.
    calls: HashMap<CallKey, ActiveCall>,
    ctx: Arc<DispatchContext>,
    /// Handler tasks that failed to resolve a post channel report their key
    /// here so the pump can drop the dead `calls` entry and clear active state.
    dead_call_tx: mpsc::Sender<CallKey>,
    dead_call_rx: mpsc::Receiver<CallKey>,
    coalesce_enabled: bool,
    /// Last-progress timestamp (ms since epoch), bumped every loop iteration.
    /// Read by the liveness watchdog to detect a stalled pump.
    heartbeat: Arc<AtomicU64>,
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
        heartbeat: Arc<AtomicU64>,
    ) -> Self {
        let (dead_call_tx, dead_call_rx) = mpsc::channel(64);

        Self {
            state: DispatcherState::new(coalesce_timeout_ms),
            calls: HashMap::new(),
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
            dead_call_tx,
            dead_call_rx,
            coalesce_enabled: coalesce_timeout_ms > 0,
            heartbeat,
        }
    }

    pub async fn run(&mut self, stream: &mut Streaming<proto::StreamMessage>) {
        let mut reap_interval = tokio::time::interval(REAP_INTERVAL);
        let mut coalesce_interval = tokio::time::interval(COALESCE_TICK_INTERVAL);
        let mut heartbeat_interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        reap_interval.tick().await; // skip immediate first tick
        coalesce_interval.tick().await;
        heartbeat_interval.tick().await;

        loop {
            // Mark progress at the top of every iteration. As long as the pump
            // keeps looping (it wakes at least every HEARTBEAT_INTERVAL), the
            // watchdog sees a fresh timestamp; a blocked iteration goes stale.
            self.heartbeat.store(now_millis(), Ordering::Relaxed);

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
                _ = heartbeat_interval.tick() => {}
                Some(key) = self.dead_call_rx.recv() => {
                    self.handle_dead_call(key);
                }
            }
        }
    }

    /// Record that a call handler stopped accepting messages within
    /// [`CALL_SEND_TIMEOUT`]: drop it from the live set so it can no longer
    /// receive (or block the pump on) further sends.
    fn note_stuck(&mut self, key: &CallKey) {
        tracing::error!(
            talkgroup = key.talkgroup.0,
            sys_num = key.sys_num.0,
            "call handler stuck (bounded send timed out); dropping it"
        );
        self.ctx.metrics.calls_dropped_stuck.add(1, &[]);
        self.calls.remove(key);
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

    /// A handler task could not resolve its post channel (or failed to
    /// initialize). Drop the now-dead `calls` entry (its receiver was already
    /// dropped by the task) and clear the call from active state.
    fn handle_dead_call(&mut self, key: CallKey) {
        self.calls.remove(&key);
        self.state.on_channel_resolution_failed(&key);
        self.record_gauges();
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
                        let _ = send_bounded(&active.tx, CallMessage::End(call_data)).await;
                    }
                }

                DispatchEffect::ReapCall { key } => {
                    self.ctx.metrics.calls_reaped.add(1, &[]);
                    if let Some(active) = self.calls.remove(&key) {
                        let _ = send_bounded(&active.tx, CallMessage::End(None)).await;
                    }
                }

                DispatchEffect::PttEnd { key, call_data } => {
                    if let Some(tx) = self.calls.get(&key).map(|a| a.tx.clone())
                        && !send_bounded(&tx, CallMessage::PttEnd(call_data)).await
                    {
                        self.note_stuck(&key);
                    }
                }

                DispatchEffect::CoalesceResume { key, call } => {
                    if let Some(tx) = self.calls.get(&key).map(|a| a.tx.clone())
                        && !send_bounded(&tx, CallMessage::PttStart(Box::new(call))).await
                    {
                        self.note_stuck(&key);
                    }
                }

                DispatchEffect::CoalesceTimeout { key } => {
                    if let Some(active) = self.calls.remove(&key) {
                        let _ = send_bounded(&active.tx, CallMessage::End(None)).await;
                    }
                }

                DispatchEffect::ForwardToAggregator { message } => {
                    self.ctx.aggregator.send(message);
                }

                DispatchEffect::RotateDb => {
                    self.rotate_db().await;
                }

                DispatchEffect::ResolveChannel { key, call } => {
                    // End any handler still registered for this key before
                    // replacing it, so its embed closes (turns green) instead of
                    // lingering grey. self.calls is the source of truth for live
                    // handlers.
                    if let Some(old) = self.calls.remove(&key) {
                        let _ = send_bounded(&old.tx, CallMessage::End(None)).await;
                    }

                    // Create the handler's channel and spawn the handler *now*,
                    // bound to the receiver from the start. The sender lives in
                    // self.calls and is only ever removed paired with an End, so
                    // End always lands on the channel this handler owns — a
                    // concurrent resolve can never orphan it. Inbound audio/
                    // events buffer in the channel while the post channel
                    // resolves off the pump loop (inside the spawned task).
                    let (tx, rx) = mpsc::channel(1024);
                    self.calls.insert(key, ActiveCall { tx });

                    let ctx = Arc::clone(&self.ctx);
                    let dead_call_tx = self.dead_call_tx.clone();
                    let span = tracing::info_span!(
                        "call",
                        talkgroup = call.talkgroup,
                        sys_num = call.sys_num,
                        freq = call.freq,
                        encrypted = call.encrypted,
                    );
                    tokio::spawn(
                        run_call(ctx, call, rx, key, dead_call_tx).instrument(span),
                    );
                }

                DispatchEffect::Log { message } => {
                    tracing::info!("{message}");
                }
            }
        }

        self.record_gauges();
    }

    /// Publish the active/coalescing call gauges and the live-handler count.
    /// `live_handlers` is the executor's own view (`self.calls`); divergence
    /// from `active + coalescing` would indicate leaked handlers.
    fn record_gauges(&self) {
        self.ctx
            .metrics
            .active_calls
            .record(self.state.active_call_count() as u64, &[]);
        self.ctx
            .metrics
            .coalescing_calls
            .record(self.state.coalescing_call_count() as u64, &[]);
        self.ctx
            .metrics
            .live_handlers
            .record(self.calls.len() as u64, &[]);
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

/// Resolve a new call's post channel, insert its DB row, then build and run the
/// handler to completion. Spawned by the `ResolveChannel` effect with the
/// receiver end of the handler's channel, so the handler owns `rx` for its whole
/// life and can never be orphaned by a concurrent resolve. On resolution failure
/// the key is reported back to the pump via `dead_call_tx` (so it drops the dead
/// `calls` entry); dropping `rx` here closes the channel.
async fn run_call(
    ctx: Arc<DispatchContext>,
    call: proto::CallInfo,
    rx: mpsc::Receiver<CallMessage>,
    key: CallKey,
    dead_call_tx: mpsc::Sender<CallKey>,
) {
    let Some((post_channel, call_row_id)) = ctx.resolve_and_insert(&call).await else {
        let _ = dead_call_tx.send(key).await;
        return;
    };

    let handler = match CallHandler::new(
        &call,
        rx,
        ctx.whisperer.clone(),
        Arc::clone(&ctx.discord),
        ctx.config,
        post_channel,
        Arc::clone(&ctx.db),
        call_row_id,
        Arc::clone(&ctx.metrics),
    ) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "failed to create call handler");
            let _ = dead_call_tx.send(key).await;
            return;
        }
    };

    handler.run().await;
}

impl DispatchContext {
    /// Resolve the post channel for a call and insert its DB row. Returns the
    /// resolved channel and the (best-effort) DB row id, or `None` if the
    /// channel could not be resolved.
    async fn resolve_and_insert(
        &self,
        call: &proto::CallInfo,
    ) -> Option<(ChannelId, Option<proto::types::CallRowId>)> {
        let Some(post_channel) = self.resolve_channel(call).await else {
            self.metrics.channel_resolution_failures.add(1, &[]);
            return None;
        };
        let call_row_id = self.insert_call_row(call, post_channel).await;
        Some((post_channel, call_row_id))
    }

    /// Insert the call's DB row, recording the resolved thread id. A failure is
    /// non-fatal (the call still runs without persistence) and yields `None`.
    async fn insert_call_row(
        &self,
        call: &proto::CallInfo,
        post_channel: ChannelId,
    ) -> Option<proto::types::CallRowId> {
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
        let db = self.db.lock().await;
        match db.insert_call(&record) {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(error = %e, "failed to insert call into db");
                None
            }
        }
    }

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
