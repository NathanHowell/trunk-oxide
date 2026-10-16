//! Per-channel Discord write serialization.
//!
//! Discord rate-limits REST writes per `(route, channel)` bucket, and *edit* and
//! *create* are **separate buckets** even on the same channel. This module
//! mirrors that structure: each Discord channel gets a writer with two
//! independent lanes —
//!
//! - an **edit lane** (`PATCH` — embed edits and deletes), which coalesces: it
//!   keeps only the latest pending embed per message and re-edits as fast as the
//!   bucket allows, so transcript churn never queues stale intermediate states;
//! - a **create lane** (`POST` — new embeds and audio replies), bounded, which
//!   sheds audio attaches (with a metric) when saturated.
//!
//! One request per lane is in flight at a time (matching the bucket), but the
//! two lanes run concurrently, and different channels are fully parallel —
//! bounded only by serenity's internal global 50 req/s limiter. This replaces
//! the old single global attach semaphore, whose shared pool let a few hot
//! channels' slow uploads starve every other channel's audio (a calm channel's
//! sub-second reply waited behind unrelated hot-channel edits).
//!
//! All calls on a channel share its one writer, so coalescing and ordering are
//! per-channel, not per-call — which also fixes the original edit-bucket
//! saturation, where N concurrent calls each debounced independently and
//! together still hammered the one bucket.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow};
use opentelemetry::KeyValue;
use serenity::model::id::{ChannelId, MessageId};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::call_state::EmbedData;
use crate::discord::{DiscordClient, embed_data_to_serenity};
use crate::metrics::Metrics;

/// Create-lane queue depth before audio attaches are dropped (bounds the OGG
/// bytes held in memory under sustained overload). Embed sends await this
/// capacity; attaches shed with a metric rather than block or grow memory.
const CREATE_LANE_CAPACITY: usize = 128;

/// Per-channel identity carried on each op for `discord_api_duration` labels
/// (`short_name` + `talkgroup`, so a hot channel is visible, not aggregated).
#[derive(Clone)]
pub struct WriteIdentity {
    pub short_name: Arc<str>,
    pub talkgroup: i64,
}

impl WriteIdentity {
    fn labels(&self, operation: &'static str) -> [KeyValue; 3] {
        [
            KeyValue::new("operation", operation),
            KeyValue::new("short_name", self.short_name.to_string()),
            KeyValue::new("talkgroup", self.talkgroup),
        ]
    }
}

/// A pending edit-lane operation for a given message. Latest-wins per message:
/// a newer op replaces an older one before either is sent, so we never transmit
/// a superseded embed. (A `Delete` followed by an `Update` for the same message
/// does not occur in practice — empty calls delete and never edit again.)
enum PendingEdit {
    Update(EmbedData),
    Delete,
}

/// Edit lane state: the coalescing map plus a wakeup. The worker drains the map,
/// sends each pending edit one at a time, then parks on `notify`.
struct EditLane {
    pending: Mutex<HashMap<MessageId, (PendingEdit, WriteIdentity)>>,
    notify: Notify,
}

/// A create-lane operation (`POST`). Both ride one bounded queue: sends await
/// capacity (rare, one per call, on the fast create bucket); attaches `try_send`
/// and shed on a full queue.
enum CreateOp {
    SendEmbed {
        embed: EmbedData,
        identity: WriteIdentity,
        reply: oneshot::Sender<Result<MessageId>>,
    },
    Attach {
        message_id: MessageId,
        ogg: Vec<u8>,
        filename: String,
        identity: WriteIdentity,
    },
}

/// The per-channel pair of lanes. Held by the registry (so the worker tasks
/// never see their inputs fully drop and exit); handles clone out of it.
struct ChannelWriterInner {
    create_tx: mpsc::Sender<CreateOp>,
    edit: Arc<EditLane>,
}

/// Cheap, cloneable handle a call handler uses to write to one channel.
#[derive(Clone)]
pub struct ChannelWriterHandle {
    channel: ChannelId,
    create_tx: mpsc::Sender<CreateOp>,
    edit: Arc<EditLane>,
    metrics: Arc<Metrics>,
    identity: WriteIdentity,
}

impl ChannelWriterHandle {
    /// Post a new embed (create bucket) and return its message id. Awaits the
    /// lane — the only blocking writer call, but sends are one-per-call on the
    /// fast create bucket, so the wait is negligible in practice.
    pub async fn send_embed(&self, embed: EmbedData) -> Result<MessageId> {
        let (reply, rx) = oneshot::channel();
        self.create_tx
            .send(CreateOp::SendEmbed {
                embed,
                identity: self.identity.clone(),
                reply,
            })
            .await
            .map_err(|_| anyhow!("channel writer create lane closed"))?;
        rx.await
            .map_err(|_| anyhow!("channel writer dropped send_embed reply"))?
    }

    /// Enqueue an embed edit (edit bucket), coalesced. Never blocks.
    pub fn edit_embed(&self, message_id: MessageId, embed: EmbedData) {
        self.enqueue_edit(message_id, PendingEdit::Update(embed));
    }

    /// Enqueue an embed delete (edit bucket). Never blocks.
    pub fn delete_embed(&self, message_id: MessageId) {
        self.enqueue_edit(message_id, PendingEdit::Delete);
    }

    fn enqueue_edit(&self, message_id: MessageId, edit: PendingEdit) {
        {
            let mut pending = self
                .edit
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.insert(message_id, (edit, self.identity.clone()));
        }
        self.edit.notify.notify_one();
    }

    /// Enqueue an audio attach (create bucket, posted as a reply). Never blocks;
    /// sheds with the `attach_dropped` metric if the create lane is saturated.
    pub fn attach(&self, message_id: MessageId, ogg: Vec<u8>, filename: String) {
        let op = CreateOp::Attach {
            message_id,
            ogg,
            filename,
            identity: self.identity.clone(),
        };
        if let Err(mpsc::error::TrySendError::Full(_)) = self.create_tx.try_send(op) {
            self.metrics.attach_dropped.add(1, &[]);
            tracing::warn!(channel = %self.channel, "audio attach dropped: create lane saturated");
        }
        // A `Closed` error means shutdown — nothing to do.
    }
}

/// Registry of per-channel writers. One per process; cloned handles are handed
/// to call handlers via [`ChannelWriters::handle`].
pub struct ChannelWriters {
    discord: Arc<DiscordClient>,
    metrics: Arc<Metrics>,
    channels: Mutex<HashMap<ChannelId, Arc<ChannelWriterInner>>>,
}

impl ChannelWriters {
    pub fn new(discord: Arc<DiscordClient>, metrics: Arc<Metrics>) -> Self {
        Self {
            discord,
            metrics,
            channels: Mutex::new(HashMap::new()),
        }
    }

    /// Number of distinct channels with a spawned writer (test introspection).
    #[cfg(test)]
    fn channel_count(&self) -> usize {
        self.channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Get (or lazily spawn) the writer for `channel` and return a handle bound
    /// to `identity` for metric labelling. The two lane tasks live for the
    /// process lifetime — channels map to talkgroups (bounded), and a parked
    /// task is cheap; this avoids any reap race.
    pub fn handle(&self, channel: ChannelId, identity: WriteIdentity) -> ChannelWriterHandle {
        let inner = {
            let mut map = self
                .channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(map.entry(channel).or_insert_with(|| {
                let (create_tx, create_rx) = mpsc::channel(CREATE_LANE_CAPACITY);
                let edit = Arc::new(EditLane {
                    pending: Mutex::new(HashMap::new()),
                    notify: Notify::new(),
                });
                tokio::spawn(run_create_lane(
                    Arc::clone(&self.discord),
                    Arc::clone(&self.metrics),
                    channel,
                    create_rx,
                ));
                tokio::spawn(run_edit_lane(
                    Arc::clone(&self.discord),
                    Arc::clone(&self.metrics),
                    channel,
                    Arc::clone(&edit),
                ));
                Arc::new(ChannelWriterInner { create_tx, edit })
            }))
        };
        ChannelWriterHandle {
            channel,
            create_tx: inner.create_tx.clone(),
            edit: Arc::clone(&inner.edit),
            metrics: Arc::clone(&self.metrics),
            identity,
        }
    }
}

/// Create-lane worker: one POST in flight at a time for this channel.
async fn run_create_lane(
    discord: Arc<DiscordClient>,
    metrics: Arc<Metrics>,
    channel: ChannelId,
    mut rx: mpsc::Receiver<CreateOp>,
) {
    while let Some(op) = rx.recv().await {
        match op {
            CreateOp::SendEmbed {
                embed,
                identity,
                reply,
            } => {
                let start = Instant::now();
                let res = discord
                    .send_embed(channel, embed_data_to_serenity(&embed))
                    .await;
                metrics.discord_api_duration.record(
                    start.elapsed().as_secs_f64(),
                    &identity.labels("send_embed"),
                );
                // Receiver gone (handler exited) just drops the result.
                let _ = reply.send(res);
            }
            CreateOp::Attach {
                message_id,
                ogg,
                filename,
                identity,
            } => {
                let start = Instant::now();
                if let Err(e) = discord
                    .attach_file(channel, message_id, ogg, &filename)
                    .await
                {
                    tracing::warn!(error = %e, "failed to attach audio");
                }
                metrics.discord_api_duration.record(
                    start.elapsed().as_secs_f64(),
                    &identity.labels("attach_audio"),
                );
            }
        }
    }
}

/// Edit-lane worker: drains the coalescing map, sending one PATCH/DELETE at a
/// time, then parks until the next edit. New edits enqueued while a request is
/// in flight are coalesced (latest-wins per message) and picked up on the next
/// drain — so a burst collapses to a single edit per message.
async fn run_edit_lane(
    discord: Arc<DiscordClient>,
    metrics: Arc<Metrics>,
    channel: ChannelId,
    lane: Arc<EditLane>,
) {
    loop {
        lane.notify.notified().await;
        loop {
            let batch: Vec<(MessageId, (PendingEdit, WriteIdentity))> = {
                let mut pending = lane
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if pending.is_empty() {
                    break;
                }
                pending.drain().collect()
            };
            for (message_id, (edit, identity)) in batch {
                let start = Instant::now();
                match edit {
                    PendingEdit::Update(embed) => {
                        if let Err(e) = discord
                            .edit_embed(channel, message_id, embed_data_to_serenity(&embed))
                            .await
                        {
                            tracing::warn!(error = %e, "failed to edit embed");
                        }
                        metrics.discord_api_duration.record(
                            start.elapsed().as_secs_f64(),
                            &identity.labels("edit_embed"),
                        );
                    }
                    PendingEdit::Delete => {
                        if let Err(e) = discord.delete_message(channel, message_id).await {
                            tracing::warn!(error = %e, "failed to delete embed");
                        }
                        metrics.discord_api_duration.record(
                            start.elapsed().as_secs_f64(),
                            &identity.labels("delete_embed"),
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_writers() -> ChannelWriters {
        let discord = Arc::new(DiscordClient::for_test());
        let meter = opentelemetry::global::meter("test");
        let metrics = Arc::new(Metrics::new(&meter));
        ChannelWriters::new(discord, metrics)
    }

    fn ident() -> WriteIdentity {
        WriteIdentity {
            short_name: Arc::from("smco"),
            talkgroup: 42,
        }
    }

    #[test]
    fn labels_carry_operation_and_identity() {
        let labels = ident().labels("edit_embed");
        let keys: Vec<_> = labels.iter().map(|kv| kv.key.as_str().to_owned()).collect();
        assert!(keys.contains(&"operation".to_owned()));
        assert!(keys.contains(&"short_name".to_owned()));
        assert!(keys.contains(&"talkgroup".to_owned()));
    }

    #[tokio::test]
    async fn handle_reuses_one_writer_per_channel() {
        let writers = test_writers();
        let ch = ChannelId::new(100);
        // All calls on a channel must share its single writer (so writes are
        // serialized + coalesced per-channel), while a different channel gets
        // its own — the property the whole design rests on.
        let _h1 = writers.handle(ch, ident());
        let _h2 = writers.handle(ch, ident());
        assert_eq!(writers.channel_count(), 1);
        let _h3 = writers.handle(ChannelId::new(200), ident());
        assert_eq!(writers.channel_count(), 2);
    }
}
