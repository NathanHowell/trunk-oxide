use std::sync::Arc;

use chrono::Utc;
use serenity::model::id::{ChannelId, MessageId};
use tokio::sync::{Mutex, mpsc};
use tokio::time::{self, Duration};
use trunk_oxide_proto as proto;

use crate::aggregator_state::{AggEffect, AggMessage, AggregatorState, EmbedTarget};
use crate::call_state::EmbedData;
use crate::discord::DiscordClient;
use trunk_database::Database;

#[derive(Debug)]
pub enum AggregatorMessage {
    Signal(Box<proto::SignalEvent>),
    TrunkMessage(proto::TrunkMessageEvent),
    Unit(proto::UnitEvent),
    CallsActive(proto::CallsActiveEvent),
    SetupRecorder(proto::SetupRecorderEvent),
    SetupSystem(proto::SetupSystemEvent),
    SetupSystems(proto::SetupSystemsEvent),
    SetupSources(proto::SetupSourcesEvent),
    SetupConfig(proto::SetupConfigEvent),
}

pub struct Aggregator {
    tx: mpsc::UnboundedSender<AggregatorMessage>,
}

impl Aggregator {
    pub fn new(
        discord: Arc<DiscordClient>,
        channel_id: ChannelId,
        db: Arc<Mutex<Database>>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        let inner = AggregatorExecutor {
            state: AggregatorState::new(),
            discord,
            channel_id,
            rx,
            db,
            current_thread_id: None,
            recorder_embed_id: None,
            activity_embed_id: None,
        };

        tokio::spawn(inner.run());

        Self { tx }
    }

    pub fn send(&self, msg: AggregatorMessage) {
        let _ = self.tx.send(msg);
    }
}

struct AggregatorExecutor {
    state: AggregatorState,
    discord: Arc<DiscordClient>,
    channel_id: ChannelId,
    rx: mpsc::UnboundedReceiver<AggregatorMessage>,
    db: Arc<Mutex<Database>>,
    current_thread_id: Option<ChannelId>,
    recorder_embed_id: Option<MessageId>,
    activity_embed_id: Option<MessageId>,
}

impl AggregatorExecutor {
    async fn run(mut self) {
        let mut flush_interval = time::interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                Some(msg) = self.rx.recv() => {
                    let agg_msg = convert_message(msg);
                    let effects = self.state.on_message(agg_msg);
                    self.process_effects(effects).await;
                }
                _ = flush_interval.tick() => {
                    let effects = self.state.on_flush(Utc::now().timestamp());
                    self.process_effects(effects).await;
                }
            }
        }
    }

    async fn process_effects(&mut self, effects: Vec<AggEffect>) {
        for effect in effects {
            match effect {
                AggEffect::CreateThread { name } => {
                    match self
                        .discord
                        .create_call_thread(self.channel_id, &name)
                        .await
                    {
                        Ok(thread_id) => {
                            let hour_key =
                                Utc::now().format("%Y-%m-%d %H:00").to_string();
                            self.current_thread_id = Some(thread_id);
                            self.recorder_embed_id = None;
                            self.activity_embed_id = None;
                            self.state.on_thread_created(hour_key);
                            tracing::info!(thread = %thread_id, "created hourly summary thread");
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "failed to create hourly thread");
                        }
                    }
                }
                AggEffect::PostEmbed { embed } => {
                    if let Some(thread_id) = self.current_thread_id {
                        let is_recorder = embed.title == "Recorders";
                        let is_activity = embed.title == "System Activity";
                        match self
                            .discord
                            .send_embed(thread_id, embed_data_to_serenity(&embed))
                            .await
                        {
                            Ok(msg_id) => {
                                if is_recorder {
                                    self.recorder_embed_id = Some(msg_id);
                                } else if is_activity {
                                    self.activity_embed_id = Some(msg_id);
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "failed to post embed");
                            }
                        }
                    }
                }
                AggEffect::EditEmbed { target, embed } => {
                    let (thread_id, msg_id) = match target {
                        EmbedTarget::Recorder => {
                            (self.current_thread_id, self.recorder_embed_id)
                        }
                        EmbedTarget::Activity => {
                            (self.current_thread_id, self.activity_embed_id)
                        }
                    };
                    if let (Some(tid), Some(mid)) = (thread_id, msg_id)
                        && let Err(e) = self
                            .discord
                            .edit_embed(tid, mid, embed_data_to_serenity(&embed))
                            .await
                    {
                        tracing::error!(error = %e, "failed to edit embed");
                    }
                }
                AggEffect::InsertActivity { snapshot } => {
                    let db_snapshot = trunk_database::ActivitySnapshot {
                        timestamp: snapshot.timestamp,
                        signal_counts: snapshot.signal_counts,
                        trunk_msg_counts: snapshot.trunk_msg_counts,
                        unit_counts: snapshot.unit_counts,
                        active_calls: snapshot.active_calls,
                    };
                    let db = self.db.lock().await;
                    if let Err(e) = db.insert_activity(&db_snapshot) {
                        tracing::warn!(error = %e, "failed to insert activity snapshot into db");
                    }
                }
            }
        }
    }
}

fn convert_message(msg: AggregatorMessage) -> AggMessage {
    match msg {
        AggregatorMessage::Signal(e) => AggMessage::Signal(e),
        AggregatorMessage::TrunkMessage(e) => AggMessage::TrunkMessage(e),
        AggregatorMessage::Unit(e) => AggMessage::Unit(e),
        AggregatorMessage::CallsActive(e) => AggMessage::CallsActive(e),
        AggregatorMessage::SetupRecorder(e) => AggMessage::SetupRecorder(e),
        AggregatorMessage::SetupSystem(e) => AggMessage::SetupSystem(e),
        AggregatorMessage::SetupSystems(e) => AggMessage::SetupSystems(e),
        AggregatorMessage::SetupSources(e) => AggMessage::SetupSources(e),
        AggregatorMessage::SetupConfig(e) => AggMessage::SetupConfig(e),
    }
}

fn embed_data_to_serenity(data: &EmbedData) -> serenity::all::CreateEmbed {
    use serenity::all::{CreateEmbed, CreateEmbedFooter};

    let mut embed = CreateEmbed::new()
        .title(&data.title)
        .colour(data.colour);

    if let Some(desc) = &data.description {
        embed = embed.description(desc);
    }

    if !data.footer.is_empty() {
        embed = embed.footer(CreateEmbedFooter::new(&data.footer));
    }

    for field in &data.fields {
        embed = embed.field(&field.name, &field.value, field.inline);
    }

    embed
}
