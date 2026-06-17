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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Smoothed edit-lane latency (seconds) at/above which a channel's audio
/// attaches fall back to a *reply* (create bucket), and at/below which they go
/// *inline* on the embed (edit bucket). The gap is hysteresis. The signal is
/// **embed-edit** latency only — small `PATCH`es that reflect pure bucket
/// congestion; inline-attach uploads are larger and would bias the reading, so
/// they are gated by it but never feed it. A channel starts inline (calm) and
/// only falls back once its own edits are observed to be slow.
const INLINE_REPLY_ENTER_SECS: f64 = 4.0;
const INLINE_REPLY_EXIT_SECS: f64 = 1.5;
/// Weight of the newest sample in the edit-latency EWMA.
const EDIT_LATENCY_EWMA_ALPHA: f64 = 0.3;
/// Cap on inline attaches queued on the edit lane — bounds OGG bytes if a
/// channel goes hot with attaches already queued. Excess sheds with the metric.
const INLINE_ATTACH_CAP: usize = 32;

/// Decide the next inline/reply latch from the smoothed edit latency, with
/// hysteresis: only flip to reply above the enter threshold, back to inline
/// below the exit threshold, otherwise hold. Pure for testing.
fn next_inline_ok(edit_latency_ewma: f64, current: bool) -> bool {
    if edit_latency_ewma >= INLINE_REPLY_ENTER_SECS {
        false
    } else if edit_latency_ewma <= INLINE_REPLY_EXIT_SECS {
        true
    } else {
        current
    }
}

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

    /// Labels for an `attach_audio` sample, carrying the `path` (inline/reply)
    /// so the split between buckets is observable per channel.
    fn attach_labels(&self, path: &'static str) -> [KeyValue; 4] {
        [
            KeyValue::new("operation", "attach_audio"),
            KeyValue::new("path", path),
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

/// A pending *inline* audio attach (a `PATCH` adding the OGG to the embed). Rides
/// the edit lane so it serializes with that channel's embed edits on the one
/// edit bucket. Order vs the embed's green edit doesn't matter — they touch
/// disjoint message fields (attachments vs embed).
struct InlineAttach {
    message_id: MessageId,
    ogg: Vec<u8>,
    filename: String,
    identity: WriteIdentity,
}

/// Edit-lane work: coalescing embed edits (latest-wins per message) plus a FIFO
/// of inline attaches.
#[derive(Default)]
struct EditQueue {
    edits: HashMap<MessageId, (PendingEdit, WriteIdentity)>,
    attaches: VecDeque<InlineAttach>,
}

impl EditQueue {
    fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.attaches.is_empty()
    }
}

/// Edit lane state: the work queue, a wakeup, and the inline/reply latch the
/// worker maintains from observed edit latency. The worker drains the queue,
/// sends each request one at a time, then parks on `notify`.
struct EditLane {
    pending: Mutex<EditQueue>,
    notify: Notify,
    /// `true` = this channel's edit bucket is calm, so attach audio inline;
    /// `false` = backed up, so attach as a reply on the create lane. Written
    /// only by the edit-lane worker (from embed-edit latency); read by handles.
    inline_ok: AtomicBool,
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
            pending.edits.insert(message_id, (edit, self.identity.clone()));
        }
        self.edit.notify.notify_one();
    }

    /// Enqueue an audio attach. Inline on the embed (edit lane) while this
    /// channel's edit bucket is calm, else as a reply (create lane) so it can't
    /// stall behind a backed-up edit bucket. Never blocks; sheds with the
    /// `attach_dropped` metric if the chosen lane is saturated.
    pub fn attach(&self, message_id: MessageId, ogg: Vec<u8>, filename: String) {
        if self.edit.inline_ok.load(Ordering::Relaxed) {
            self.attach_inline(message_id, ogg, filename);
        } else {
            self.attach_reply(message_id, ogg, filename);
        }
    }

    fn attach_inline(&self, message_id: MessageId, ogg: Vec<u8>, filename: String) {
        {
            let mut pending = self
                .edit
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.attaches.len() >= INLINE_ATTACH_CAP {
                drop(pending);
                self.metrics.attach_dropped.add(1, &[]);
                tracing::warn!(channel = %self.channel, "audio attach dropped: inline lane saturated");
                return;
            }
            pending.attaches.push_back(InlineAttach {
                message_id,
                ogg,
                filename,
                identity: self.identity.clone(),
            });
        }
        self.edit.notify.notify_one();
    }

    fn attach_reply(&self, message_id: MessageId, ogg: Vec<u8>, filename: String) {
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
                    pending: Mutex::new(EditQueue::default()),
                    notify: Notify::new(),
                    // Optimistic: a fresh channel attaches inline until its own
                    // edits are observed slow.
                    inline_ok: AtomicBool::new(true),
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
                metrics
                    .discord_api_duration
                    .record(start.elapsed().as_secs_f64(), &identity.labels("send_embed"));
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
                if let Err(e) = discord.attach_file(channel, message_id, ogg, &filename).await {
                    tracing::warn!(error = %e, "failed to attach audio (reply)");
                }
                metrics.discord_api_duration.record(
                    start.elapsed().as_secs_f64(),
                    &identity.attach_labels("reply"),
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
    // Smoothed embed-edit latency, worker-local (single writer). Drives the
    // inline/reply latch the handles read.
    let mut edit_latency_ewma = 0.0_f64;
    loop {
        lane.notify.notified().await;
        loop {
            let (edits, attaches) = {
                let mut pending = lane
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if pending.is_empty() {
                    break;
                }
                let edits: Vec<(MessageId, (PendingEdit, WriteIdentity))> =
                    pending.edits.drain().collect();
                let attaches: Vec<InlineAttach> = pending.attaches.drain(..).collect();
                (edits, attaches)
            };
            // Embed edits first: they finalize the embed and are the latency
            // signal for the latch. (Order vs attaches is irrelevant — disjoint
            // fields — but doing edits first means the latch reflects the most
            // recent congestion before the next attach decision is read.)
            for (message_id, (edit, identity)) in edits {
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
                // Update the inline/reply latch from this edit's latency.
                let elapsed = start.elapsed().as_secs_f64();
                edit_latency_ewma = if edit_latency_ewma == 0.0 {
                    elapsed
                } else {
                    EDIT_LATENCY_EWMA_ALPHA * elapsed
                        + (1.0 - EDIT_LATENCY_EWMA_ALPHA) * edit_latency_ewma
                };
                let current = lane.inline_ok.load(Ordering::Relaxed);
                lane.inline_ok
                    .store(next_inline_ok(edit_latency_ewma, current), Ordering::Relaxed);
            }
            // Then any inline attaches (same edit bucket, one at a time).
            for attach in attaches {
                let start = Instant::now();
                if let Err(e) = discord
                    .attach_file_inline(channel, attach.message_id, attach.ogg, &attach.filename)
                    .await
                {
                    tracing::warn!(error = %e, "failed to attach audio (inline)");
                }
                metrics.discord_api_duration.record(
                    start.elapsed().as_secs_f64(),
                    &attach.identity.attach_labels("inline"),
                );
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
    fn inline_latch_has_hysteresis() {
        // Calm by default.
        assert!(next_inline_ok(0.2, true));
        // A clearly-slow edit bucket flips to reply.
        assert!(!next_inline_ok(INLINE_REPLY_ENTER_SECS + 1.0, true));
        // In the hysteresis band the latch holds its current value.
        let mid = (INLINE_REPLY_ENTER_SECS + INLINE_REPLY_EXIT_SECS) / 2.0;
        assert!(!next_inline_ok(mid, false), "stays reply in the band");
        assert!(next_inline_ok(mid, true), "stays inline in the band");
        // Recovery below the exit threshold returns to inline.
        assert!(next_inline_ok(INLINE_REPLY_EXIT_SECS - 0.1, false));
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
