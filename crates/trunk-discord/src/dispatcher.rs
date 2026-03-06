use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serenity::model::id::ChannelId;
use tokio::sync::{Mutex, mpsc};
use tracing::Instrument;
use trunk_oxide_proto as proto;

use proto::types::{SysNum, Talkgroup};
use trunk_database::Database;

use crate::aggregator::Aggregator;
use crate::call_handler::{CallHandler, CallHandlerConfig, CallMessage};
use crate::channel_manager::ChannelLookup;
use crate::discord::DiscordClient;
use crate::dispatcher_state::{CallKey, DispatchEffect, DispatcherState, ThreadCacheKey};
use crate::grpc_whisperer::WhispererConnection;

/// How often to check for stuck calls.
const REAP_INTERVAL: Duration = Duration::from_secs(60);

struct ActiveCall {
    tx: mpsc::Sender<CallMessage>,
}

pub struct Dispatcher {
    state: Mutex<DispatcherState>,
    calls: Mutex<HashMap<CallKey, ActiveCall>>,
    whisperer: WhispererConnection,
    discord: Arc<DiscordClient>,
    aggregator: Arc<Aggregator>,
    buffer_duration_ms: u64,
    channel_id: ChannelId,
    channel_lookup: ChannelLookup,
    tone_detection: bool,
    tone_detection_duration_s: f32,
    audio_attachment: bool,
    db: Arc<Mutex<Database>>,
    db_path: PathBuf,
}

impl Dispatcher {
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
        db: Arc<Mutex<Database>>,
        db_path: PathBuf,
    ) -> Self {
        Self {
            state: Mutex::new(DispatcherState::new()),
            calls: Mutex::new(HashMap::new()),
            whisperer,
            discord,
            aggregator,
            buffer_duration_ms,
            channel_id,
            channel_lookup,
            tone_detection,
            tone_detection_duration_s,
            audio_attachment,
            db,
            db_path,
        }
    }

    pub async fn handle_audio(&self, sample: proto::AudioSample) {
        let effects = {
            let state = self.state.lock().await;
            state.on_audio(&sample)
        };
        self.process_effects(effects).await;
    }

    pub async fn handle_event(&self, event: proto::PluginEvent) {
        let effects = {
            let mut state = self.state.lock().await;
            state.on_event(event)
        };
        self.process_effects(effects).await;
    }

    pub fn spawn_reaper(self: &Arc<Self>) {
        let dispatcher = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(REAP_INTERVAL);
            interval.tick().await; // first tick is immediate, skip it
            loop {
                interval.tick().await;
                let effects = {
                    let mut state = dispatcher.state.lock().await;
                    state.on_reap_tick()
                };
                dispatcher.process_effects(effects).await;
            }
        });
    }

    async fn process_effects(&self, effects: Vec<DispatchEffect>) {
        for effect in effects {
            match effect {
                DispatchEffect::SendAudio {
                    key,
                    samples,
                    sample_rate,
                } => {
                    let calls = self.calls.lock().await;
                    if let Some(active) = calls.get(&key)
                        && let Err(e) = active.tx.try_send(CallMessage::Audio {
                            samples,
                            sample_rate,
                        })
                    {
                        tracing::warn!(talkgroup = key.talkgroup.0, "audio packet dropped: {e}");
                    }
                }

                DispatchEffect::SendEvent { key, text } => {
                    let calls = self.calls.lock().await;
                    if let Some(active) = calls.get(&key)
                        && let Err(e) = active.tx.try_send(CallMessage::Event(text))
                    {
                        tracing::warn!(talkgroup = key.talkgroup.0, "event dropped: {e}");
                    }
                }

                DispatchEffect::EndCall { key, call_data } => {
                    let active = self.calls.lock().await.remove(&key);
                    if let Some(active) = active {
                        let _ = active.tx.send(CallMessage::End(call_data)).await;
                    }
                }

                DispatchEffect::ReapCall { key } => {
                    let active = self.calls.lock().await.remove(&key);
                    if let Some(active) = active {
                        let _ = active.tx.try_send(CallMessage::End(None));
                    }
                }

                DispatchEffect::ForwardToAggregator { message } => {
                    self.aggregator.send(message);
                }

                DispatchEffect::RotateDb => {
                    self.rotate_db().await;
                }

                DispatchEffect::ResolveChannel { call } => {
                    let post_channel = self.resolve_channel(&call).await;
                    if let Some(post_channel) = post_channel {
                        let follow_up = {
                            let mut state = self.state.lock().await;
                            state.on_channel_resolved(call, post_channel)
                        };
                        // on_channel_resolved returns SpawnCallHandler — process inline
                        for effect in follow_up {
                            if let DispatchEffect::SpawnCallHandler {
                                key,
                                call,
                                post_channel,
                            } = effect
                            {
                                self.spawn_call_handler(key, call, post_channel).await;
                            }
                        }
                    }
                }

                DispatchEffect::SpawnCallHandler {
                    key,
                    call,
                    post_channel,
                } => {
                    self.spawn_call_handler(key, call, post_channel).await;
                }

                DispatchEffect::Log { message } => {
                    tracing::info!("{message}");
                }
            }
        }
    }

    async fn resolve_channel(&self, call: &proto::CallInfo) -> Option<ChannelId> {
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

        let cache_key = ThreadCacheKey {
            sys_num,
            talkgroup,
            date: date_str.clone(),
        };

        // Check in-memory thread cache
        {
            let state = self.state.lock().await;
            if let Some(channel) = state.get_cached_thread(&cache_key) {
                return Some(channel);
            }
        }

        // Check DB for thread from earlier this session/hour
        let db_thread = {
            let db = self.db.lock().await;
            db.get_thread(sys_num, talkgroup, &date_str)
                .unwrap_or(None)
                .and_then(|id| std::num::NonZeroU64::new(id).map(ChannelId::from))
        };

        if let Some(id) = db_thread {
            self.state.lock().await.cache_thread(cache_key, id);
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
                self.state.lock().await.cache_thread(cache_key, id);

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

    async fn spawn_call_handler(
        &self,
        key: CallKey,
        call: proto::CallInfo,
        post_channel: ChannelId,
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
            let db = self.db.lock().await;
            match db.insert_call(&record) {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to insert call into db");
                    None
                }
            }
        };

        let (tx, rx) = mpsc::channel(1024);

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
            self.whisperer.clone(),
            Arc::clone(&self.discord),
            CallHandlerConfig {
                buffer_duration_ms: self.buffer_duration_ms,
                tone_detection: self.tone_detection,
                tone_detection_duration_s: self.tone_detection_duration_s,
                audio_attachment: self.audio_attachment,
            },
            post_channel,
            Arc::clone(&self.db),
            call_row_id,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(error = %e, "failed to create call handler");
                return;
            }
        };

        tokio::spawn(handler.run().instrument(span));

        self.calls.lock().await.insert(key, ActiveCall { tx });
    }

    async fn rotate_db(&self) {
        let now_hour = Utc::now().format("%Y-%m-%dT%H").to_string();
        let today = Utc::now().format("%Y-%m-%d").to_string();

        match Database::open_hourly(&self.db_path, &now_hour) {
            Ok(new_db) => {
                {
                    let mut db = self.db.lock().await;
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
