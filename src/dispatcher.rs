use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use crate::aggregator::{Aggregator, AggregatorMessage};
use crate::call_handler::{CallHandler, CallMessage};
use crate::discord::DiscordClient;
use crate::proto;
use crate::proto::plugin_event::Event;
use crate::transcriber::Transcriber;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct CallKey {
    pub sys_num: i32,
    pub talkgroup: i64,
}

pub struct Dispatcher {
    calls: RwLock<HashMap<CallKey, mpsc::Sender<CallMessage>>>,
    transcriber: Arc<Transcriber>,
    discord: Arc<DiscordClient>,
    aggregator: Arc<Aggregator>,
    buffer_duration_ms: u64,
    channel_id: u64,
}

impl Dispatcher {
    pub fn new(
        transcriber: Arc<Transcriber>,
        discord: Arc<DiscordClient>,
        aggregator: Arc<Aggregator>,
        buffer_duration_ms: u64,
        channel_id: u64,
    ) -> Self {
        Self {
            calls: RwLock::new(HashMap::new()),
            transcriber,
            discord,
            aggregator,
            buffer_duration_ms,
            channel_id,
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

        let (tx, rx) = mpsc::channel(1024);

        let handler = CallHandler::new(
            key.clone(),
            call,
            rx,
            Arc::clone(&self.transcriber),
            Arc::clone(&self.discord),
            self.buffer_duration_ms,
            self.channel_id,
        );

        tokio::spawn(handler.run());

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

        if let Some(tx) = self.calls.write().await.remove(&key) {
            let _ = tx.send(CallMessage::End(event.call_data)).await;
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
            .send(AggregatorMessage::Signal(event));
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
    let signal_type = proto::SignalType::try_from(event.signal_type)
        .map(|st| {
            st.as_str_name()
                .strip_prefix("SIGNAL_TYPE_")
                .unwrap_or(st.as_str_name())
                .to_string()
        })
        .unwrap_or_else(|_| format!("UNKNOWN({})", event.signal_type));

    format!(
        "**Signal**: unit {} - {} ({})",
        event.unit_id, signal_type, event.signaling_type,
    )
}

fn format_unit_event(event: &proto::UnitEvent) -> String {
    let event_type = proto::UnitEventType::try_from(event.r#type)
        .map(|et| {
            et.as_str_name()
                .strip_prefix("UNIT_EVENT_TYPE_")
                .unwrap_or(et.as_str_name())
                .to_string()
        })
        .unwrap_or_else(|_| format!("UNKNOWN({})", event.r#type));

    format!(
        "**Unit**: {} - source {} (tg {})",
        event_type, event.source_id, event.talkgroup,
    )
}
