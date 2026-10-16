//! Ground truth for audio problems: a listener on the Discord gateway that
//! turns a reaction on a call's embed into a "flagged" metric and a log line
//! carrying the call's trace id, so the flagged calls can be pulled up in
//! Jaeger next to their audio-shape, decoder and channelizer attributes.
//!
//! Reaction emoji (anything else is ignored; the bot's own ⏳ is not mapped):
//! 👍 good · 👎 bad (anything) · ✂️ end clipped · ⏪ start clipped ·
//! 🕳️ dropouts mid-call · 📝 transcript missing the start

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use opentelemetry::KeyValue;
use serenity::all::{Context, EventHandler, GatewayIntents, Reaction, ReactionType};
use serenity::async_trait;
use serenity::model::id::MessageId;

use crate::metrics::Metrics;

/// How many recent embeds to remember. A call is flagged within minutes, and a
/// few thousand covers hours of traffic.
const CAPACITY: usize = 4096;

/// What we remember about a call's embed so a reaction can be attributed.
#[derive(Clone, Debug)]
pub struct FlagInfo {
    pub short_name: String,
    pub is_conventional: bool,
    pub tdma: bool,
    pub talkgroup: i64,
    /// Trace id of the call (shared with trunk-oxide's call span).
    pub trace_id: String,
}

#[derive(Default)]
pub struct FlagRegistry {
    inner: Mutex<(HashMap<MessageId, FlagInfo>, VecDeque<MessageId>)>,
}

impl FlagRegistry {
    pub fn register(&self, message: MessageId, info: FlagInfo) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (map, order) = &mut *guard;
        if map.insert(message, info).is_none() {
            order.push_back(message);
        }
        while order.len() > CAPACITY {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            }
        }
    }

    pub fn get(&self, message: MessageId) -> Option<FlagInfo> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .get(&message)
            .cloned()
    }
}

/// Process-wide registry, written by call handlers and read by the gateway task.
pub static FLAGS: LazyLock<FlagRegistry> = LazyLock::new(FlagRegistry::default);

/// Map a reaction emoji to a flag reason.
pub fn reason_for(emoji: &str) -> Option<&'static str> {
    // Variation selectors (U+FE0F) are optional on some clients, and thumbs can
    // carry a skin-tone modifier (U+1F3FB..U+1F3FF); strip both.
    let base = emoji
        .trim_end_matches(|c: char| c == '\u{fe0f}' || ('\u{1f3fb}'..='\u{1f3ff}').contains(&c));
    match base {
        "\u{1f44d}" => Some("good"),
        "\u{1f44e}" => Some("bad"),
        "\u{2702}" => Some("clipped_end"),
        "\u{23ea}" => Some("clipped_start"),
        "\u{1f573}" => Some("dropout"),
        "\u{1f4dd}" => Some("transcript_missing_start"),
        _ => None,
    }
}

struct Handler {
    metrics: Arc<Metrics>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
        let ReactionType::Unicode(emoji) = &reaction.emoji else {
            return;
        };
        let Some(reason) = reason_for(emoji) else {
            return;
        };
        // The audio attachment is posted as a *reply* to the call's embed, so a
        // reaction on it arrives with the reply's message id. Resolve it to the
        // embed it replies to.
        let mut info = FLAGS.get(reaction.message_id);
        if info.is_none()
            && let Ok(msg) = reaction
                .channel_id
                .message(&ctx.http, reaction.message_id)
                .await
            && let Some(parent) = msg.message_reference.and_then(|r| r.message_id)
        {
            info = FLAGS.get(parent);
        }
        let Some(info) = info else {
            tracing::info!(
                reason,
                message = %reaction.message_id,
                "reaction on a message that is not a known call (too old, or after a restart)"
            );
            return;
        };
        self.metrics.calls_flagged.add(
            1,
            &[
                KeyValue::new("reason", reason),
                KeyValue::new("short_name", info.short_name.clone()),
                KeyValue::new("is_conventional", info.is_conventional),
                KeyValue::new("tdma", info.tdma),
            ],
        );
        tracing::info!(
            reason,
            short_name = %info.short_name,
            talkgroup = info.talkgroup,
            tdma = info.tdma,
            trace_id = %info.trace_id,
            message = %reaction.message_id,
            "call flagged by reaction"
        );
    }
}

/// Connect to the gateway and listen for reactions until the process exits.
/// serenity reconnects on its own; an error here is logged and not fatal, since
/// the bot's HTTP side works without it.
pub async fn run(token: String, metrics: Arc<Metrics>) {
    let intents = GatewayIntents::GUILD_MESSAGE_REACTIONS;
    let client = serenity::Client::builder(&token, intents)
        .event_handler(Handler { metrics })
        .await;
    match client {
        Ok(mut client) => {
            if let Err(e) = client.start().await {
                tracing::error!(error = %e, "reaction listener stopped");
            }
        }
        Err(e) => tracing::error!(error = %e, "failed to start reaction listener"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_map_accepts_with_and_without_variation_selector() {
        assert_eq!(reason_for("\u{2702}\u{fe0f}"), Some("clipped_end"));
        assert_eq!(reason_for("\u{2702}"), Some("clipped_end"));
        assert_eq!(reason_for("\u{23f3}"), None); // the bot's own ⏳
        assert_eq!(reason_for("\u{1f44d}"), Some("good"));
        assert_eq!(reason_for("\u{1f44e}\u{1f3fd}"), Some("bad")); // skin tone
    }

    #[test]
    fn registry_evicts_oldest_beyond_capacity() {
        let r = FlagRegistry::default();
        let info = FlagInfo {
            short_name: "x".into(),
            is_conventional: false,
            tdma: false,
            talkgroup: 1,
            trace_id: String::new(),
        };
        for i in 1..=(CAPACITY as u64 + 10) {
            r.register(MessageId::new(i), info.clone());
        }
        assert!(r.get(MessageId::new(1)).is_none());
        assert!(r.get(MessageId::new(CAPACITY as u64 + 10)).is_some());
    }
}
