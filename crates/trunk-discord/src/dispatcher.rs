use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serenity::model::id::ChannelId;
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::Instrument;
use trunk_oxide_proto as proto;
use proto::types::{SysNum, Talkgroup};

use trunk_database::Database;

use proto::plugin_event::Event;

use crate::aggregator::{Aggregator, AggregatorMessage};
use crate::call_handler::{CallHandler, CallHandlerConfig, CallMessage};
use crate::channel_manager::ChannelLookup;
use crate::discord::DiscordClient;
use crate::grpc_whisperer::WhispererConnection;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct CallKey {
    sys_num: SysNum,
    talkgroup: Talkgroup,
}

/// Key for reusing Discord threads: one thread per talkgroup per day.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct ThreadCacheKey {
    sys_num: SysNum,
    talkgroup: Talkgroup,
    date: String, // "YYYY-MM-DD"
}

pub struct Dispatcher {
    calls: RwLock<HashMap<CallKey, mpsc::Sender<CallMessage>>>,
    thread_cache: RwLock<HashMap<ThreadCacheKey, ChannelId>>,
    whisperer: WhispererConnection,
    discord: Arc<DiscordClient>,
    aggregator: Arc<Aggregator>,
    buffer_duration_ms: u64,
    channel_id: ChannelId,
    channel_lookup: ChannelLookup,
    tone_detection: bool,
    tone_detection_duration_s: f32,
    db: Arc<Mutex<Database>>,
    db_path: PathBuf,
    current_hour: RwLock<String>,
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
        db: Arc<Mutex<Database>>,
        db_path: PathBuf,
    ) -> Self {
        let current_hour = Utc::now().format("%Y-%m-%dT%H").to_string();
        Self {
            calls: RwLock::new(HashMap::new()),
            thread_cache: RwLock::new(HashMap::new()),
            whisperer,
            discord,
            aggregator,
            buffer_duration_ms,
            channel_id,
            channel_lookup,
            tone_detection,
            tone_detection_duration_s,
            db,
            db_path,
            current_hour: RwLock::new(current_hour),
        }
    }

    async fn ensure_current_hour(&self) {
        let now_hour = Utc::now().format("%Y-%m-%dT%H").to_string();
        let current = self.current_hour.read().await;
        if *current == now_hour {
            return;
        }
        drop(current);

        let mut current = self.current_hour.write().await;
        // Double-check after acquiring write lock
        if *current == now_hour {
            return;
        }

        let today = Utc::now().format("%Y-%m-%d").to_string();
        match Database::open_hourly(&self.db_path, &now_hour) {
            Ok(new_db) => {
                let mut db = self.db.lock().await;
                if let Err(e) = new_db.copy_threads_from(&db, &today) {
                    tracing::warn!(error = %e, "failed to copy thread cache to new hourly db");
                }
                *db = new_db;
                drop(db);
                *current = now_hour;
                tracing::info!(hour = %*current, "rotated to new hourly database");
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to open new hourly database");
            }
        }
    }

    pub async fn handle_audio(&self, sample: proto::AudioSample) {
        if sample.sample_rate == 0 {
            tracing::warn!(
                talkgroup = sample.talkgroup,
                "dropping audio with sample_rate=0"
            );
            return;
        }

        let key = CallKey {
            sys_num: SysNum(sample.sys_num),
            talkgroup: Talkgroup(sample.talkgroup),
        };

        let calls = self.calls.read().await;
        if let Some(tx) = calls.get(&key) {
            let _ = tx.try_send(CallMessage::Audio {
                samples: sample.samples,
                sample_rate: sample.sample_rate,
            });
        }
        // Audio arriving before CallStartEvent is silently dropped
    }

    pub async fn handle_event(&self, event: proto::PluginEvent) {
        let Some(event) = event.event else { return };

        match event {
            Event::CallStart(e) => self.handle_call_start(e).await,
            Event::CallEnd(e) => self.handle_call_end(e).await,
            Event::Signal(e) => self.handle_signal(e).await,
            Event::Unit(e) => self.handle_unit(e).await,
            Event::CallsActive(e) => {
                self.aggregator.send(AggregatorMessage::CallsActive(e));
            }
            Event::TrunkMessage(e) => {
                self.aggregator.send(AggregatorMessage::TrunkMessage(e));
            }
            Event::SystemRates(e) => {
                self.aggregator.send(AggregatorMessage::SystemRates(e));
            }
            Event::SetupRecorder(e) => {
                self.aggregator.send(AggregatorMessage::SetupRecorder(e));
            }
            Event::SetupSystem(e) => {
                self.aggregator.send(AggregatorMessage::SetupSystem(e));
            }
            Event::SetupSystems(e) => {
                self.aggregator.send(AggregatorMessage::SetupSystems(e));
            }
            Event::SetupSources(e) => {
                self.aggregator.send(AggregatorMessage::SetupSources(e));
            }
            Event::SetupConfig(e) => {
                self.aggregator.send(AggregatorMessage::SetupConfig(e));
            }
            Event::UnitAlias(e) => {
                tracing::debug!(
                    radio_id = e.radio_id,
                    alias = %e.alias,
                    source = %e.source,
                    talkgroup_id = e.talkgroup_id,
                    "OTA unit alias discovered"
                );
            }
        }
    }

    async fn handle_call_start(&self, event: proto::CallStartEvent) {
        let Some(call) = event.call else { return };

        self.ensure_current_hour().await;

        let talkgroup = Talkgroup(call.talkgroup);
        let sys_num = SysNum(call.sys_num);

        let key = CallKey { sys_num, talkgroup };

        // Look up or create a per-talkgroup-per-day thread
        let start_time =
            DateTime::<Utc>::from_timestamp(call.start_time, 0).unwrap_or_else(Utc::now);
        let date_str = start_time.format("%Y-%m-%d").to_string();

        let cache_key = ThreadCacheKey {
            sys_num,
            talkgroup,
            date: date_str.clone(),
        };

        // Check in-memory cache first, then DB
        let thread_id = {
            let cache = self.thread_cache.read().await;
            cache.get(&cache_key).copied()
        };

        let thread_id = if let Some(id) = thread_id {
            Some(id)
        } else {
            self.resolve_or_create_thread(&call, sys_num, talkgroup, cache_key, &date_str)
                .await
        };

        let Some(thread_id) = thread_id else { return };

        // Insert call record into DB
        let call_row_id = {
            let record = trunk_database::CallRecord {
                talkgroup,
                sys_num,
                freq: proto::types::Freq(call.freq),
                encrypted: call.encrypted,
                emergency: call.emergency,
                start_time: call.start_time,
                tag: call.talkgroup.to_string(),
                short_name: call.short_name.clone(),
                thread_id: Some(thread_id.get()),
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

        let handler = CallHandler::new(
            call,
            rx,
            self.whisperer.clone(),
            Arc::clone(&self.discord),
            CallHandlerConfig {
                buffer_duration_ms: self.buffer_duration_ms,
                tone_detection: self.tone_detection,
                tone_detection_duration_s: self.tone_detection_duration_s,
            },
            thread_id,
            Arc::clone(&self.db),
            call_row_id,
        );

        tokio::spawn(handler.run().instrument(span));

        self.calls.write().await.insert(key, tx);
    }

    async fn resolve_or_create_thread(
        &self,
        call: &proto::CallInfo,
        sys_num: SysNum,
        talkgroup: Talkgroup,
        cache_key: ThreadCacheKey,
        date_str: &str,
    ) -> Option<ChannelId> {
        // Check DB for thread from earlier this session/hour
        let db_thread = {
            let db = self.db.lock().await;
            db.get_thread(sys_num, talkgroup, date_str)
                .unwrap_or(None)
                .and_then(|id| std::num::NonZeroU64::new(id).map(ChannelId::from))
        };

        if let Some(id) = db_thread {
            self.thread_cache.write().await.insert(cache_key, id);
            return Some(id);
        }

        let thread_name = format!("TG {} [{}]", call.talkgroup, date_str);
        let thread_name = if thread_name.len() > 100 {
            thread_name[..100].to_string()
        } else {
            thread_name
        };

        // Resolve target channel: check lookup for mapped channel, else fallback
        let target_channel = {
            let map = self.channel_lookup.read().await;
            map.get(&(call.short_name.clone(), call.talkgroup))
                .copied()
                .unwrap_or(self.channel_id)
        };

        match self
            .discord
            .create_call_thread(target_channel, &thread_name)
            .await
        {
            Ok(id) => {
                self.thread_cache.write().await.insert(cache_key, id);

                // Persist to DB
                let db = self.db.lock().await;
                if let Err(e) = db.set_thread(sys_num, talkgroup, date_str, id.get()) {
                    tracing::warn!(error = %e, "failed to persist thread to db");
                }
                drop(db);

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

    async fn handle_call_end(&self, event: proto::CallEndEvent) {
        let Some(ref call_data) = event.call_data else {
            return;
        };

        let key = CallKey {
            sys_num: SysNum(call_data.sys_num),
            talkgroup: Talkgroup(call_data.talkgroup),
        };

        let tx = self.calls.write().await.remove(&key);
        if let Some(tx) = tx {
            let _ = tx
                .send(CallMessage::End(event.call_data.map(Box::new)))
                .await;
        }
    }

    async fn handle_signal(&self, event: proto::SignalEvent) {
        if let Some(call) = &event.call {
            let key = CallKey {
                sys_num: SysNum(call.sys_num),
                talkgroup: Talkgroup(call.talkgroup),
            };
            let calls = self.calls.read().await;
            if let Some(tx) = calls.get(&key) {
                let text = format_signal_event(&event);
                let _ = tx.try_send(CallMessage::Event(text));
                return;
            }
        }
        self.aggregator
            .send(AggregatorMessage::Signal(Box::new(event)));
    }

    async fn handle_unit(&self, event: proto::UnitEvent) {
        if event.talkgroup != 0
            && let Some(system) = &event.system
        {
            let key = CallKey {
                sys_num: SysNum(system.sys_num),
                talkgroup: Talkgroup(event.talkgroup),
            };
            let calls = self.calls.read().await;
            if let Some(tx) = calls.get(&key) {
                let text = format_unit_event(&event);
                let _ = tx.try_send(CallMessage::Event(text));
                return;
            }
        }
        self.aggregator.send(AggregatorMessage::Unit(event));
    }
}

fn format_signal_event(event: &proto::SignalEvent) -> String {
    let signal_type = proto::SignalType::try_from(event.signal_type).map_or_else(
        |_| format!("UNKNOWN({})", event.signal_type),
        |st| {
            st.as_str_name()
                .strip_prefix("SIGNAL_TYPE_")
                .unwrap_or(st.as_str_name())
                .to_string()
        },
    );

    format!(
        "**Signal**: unit {} - {} ({})",
        event.unit_id, signal_type, event.signaling_type,
    )
}

fn format_unit_event(event: &proto::UnitEvent) -> String {
    let event_type = proto::UnitEventType::try_from(event.r#type).map_or_else(
        |_| format!("UNKNOWN({})", event.r#type),
        |et| {
            et.as_str_name()
                .strip_prefix("UNIT_EVENT_TYPE_")
                .unwrap_or(et.as_str_name())
                .to_string()
        },
    );

    format!(
        "**Unit**: {} - source {} (tg {})",
        event_type, event.source_id, event.talkgroup,
    )
}
