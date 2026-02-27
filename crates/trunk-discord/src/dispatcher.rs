use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serenity::model::id::ChannelId;
use tokio::sync::{RwLock, mpsc};
use tracing::Instrument;
use trunk_oxide_proto as proto;

use proto::plugin_event::Event;

use crate::aggregator::{Aggregator, AggregatorMessage};
use crate::call_handler::{CallHandler, CallHandlerConfig, CallMessage};
use crate::discord::DiscordClient;
use crate::grpc_whisperer::WhispererConnection;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct CallKey {
    sys_num: i32,
    talkgroup: i64,
}

/// Key for reusing Discord threads: one thread per talkgroup per day.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct ThreadCacheKey {
    sys_num: i32,
    talkgroup: i64,
    date: String, // "YYYY-MM-DD"
}

pub struct Dispatcher {
    calls: RwLock<HashMap<CallKey, mpsc::Sender<CallMessage>>>,
    thread_cache: RwLock<HashMap<ThreadCacheKey, ChannelId>>,
    whisperer: WhispererConnection,
    discord: Arc<DiscordClient>,
    aggregator: Arc<Aggregator>,
    buffer_duration_ms: u64,
    channel_id: u64,
    tone_detection: bool,
    tone_detection_duration_s: f32,
}

impl Dispatcher {
    pub fn new(
        whisperer: WhispererConnection,
        discord: Arc<DiscordClient>,
        aggregator: Arc<Aggregator>,
        buffer_duration_ms: u64,
        channel_id: u64,
        tone_detection: bool,
        tone_detection_duration_s: f32,
    ) -> Self {
        Self {
            calls: RwLock::new(HashMap::new()),
            thread_cache: RwLock::new(HashMap::new()),
            whisperer,
            discord,
            aggregator,
            buffer_duration_ms,
            channel_id,
            tone_detection,
            tone_detection_duration_s,
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
            sys_num: sample.sys_num,
            talkgroup: sample.talkgroup,
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
        }
    }

    async fn handle_call_start(&self, event: proto::CallStartEvent) {
        let Some(call) = event.call else { return };

        let key = CallKey {
            sys_num: call.sys_num,
            talkgroup: call.talkgroup,
        };

        // Look up or create a per-talkgroup-per-day thread
        let start_time =
            DateTime::<Utc>::from_timestamp(call.start_time, 0).unwrap_or_else(Utc::now);
        let date_str = start_time.format("%Y-%m-%d").to_string();

        let cache_key = ThreadCacheKey {
            sys_num: call.sys_num,
            talkgroup: call.talkgroup,
            date: date_str.clone(),
        };

        let thread_id = {
            let cache = self.thread_cache.read().await;
            cache.get(&cache_key).copied()
        };

        let thread_id = match thread_id {
            Some(id) => id,
            None => {
                let tag = if call.talkgroup_tag.is_empty() {
                    call.talkgroup_display.clone()
                } else {
                    call.talkgroup_tag.clone()
                };

                let thread_name = format!("TG {} - {} [{}]", call.talkgroup, tag, date_str);
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
                        self.thread_cache
                            .write()
                            .await
                            .insert(cache_key, id);
                        id
                    }
                    Err(e) => {
                        tracing::error!(
                            talkgroup = call.talkgroup,
                            error = %e,
                            "failed to create discord thread"
                        );
                        return;
                    }
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
        );

        tokio::spawn(handler.run().instrument(span));

        self.calls.write().await.insert(key, tx);
    }

    async fn handle_call_end(&self, event: proto::CallEndEvent) {
        let Some(ref call_data) = event.call_data else {
            return;
        };

        let key = CallKey {
            sys_num: call_data.sys_num,
            talkgroup: call_data.talkgroup,
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
                sys_num: call.sys_num,
                talkgroup: call.talkgroup,
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
        if event.talkgroup != 0 {
            if let Some(system) = &event.system {
                let key = CallKey {
                    sys_num: system.sys_num,
                    talkgroup: event.talkgroup,
                };
                let calls = self.calls.read().await;
                if let Some(tx) = calls.get(&key) {
                    let text = format_unit_event(&event);
                    let _ = tx.try_send(CallMessage::Event(text));
                    return;
                }
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
