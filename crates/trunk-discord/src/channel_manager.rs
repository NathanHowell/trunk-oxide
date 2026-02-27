use std::collections::HashMap;
use std::sync::Arc;

use serenity::all::{ChannelType, GuildChannel};
use serenity::model::id::{ChannelId, GuildId};
use tokio::sync::RwLock;
use tokio::time::{Duration, sleep};

use crate::config::ChannelGroup;
use crate::discord::DiscordClient;

/// Key for the talkgroup-to-channel lookup: (`system_short_name`, `talkgroup_id`).
pub type TalkgroupKey = (String, i64);

/// Shared lookup from (system, talkgroup) to the Discord channel where threads should be created.
pub type ChannelLookup = Arc<RwLock<HashMap<TalkgroupKey, ChannelId>>>;

pub fn new_lookup() -> ChannelLookup {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Fetch existing guild channels and populate the lookup for any that already match the config.
/// Returns the list of guild channels for reuse by the reconciler.
pub async fn resolve_existing(
    discord: &DiscordClient,
    guild_id: GuildId,
    groups: &[ChannelGroup],
    lookup: &ChannelLookup,
) -> anyhow::Result<Vec<GuildChannel>> {
    let guild_channels = discord.get_guild_channels(guild_id).await?;

    // Index channels by ID for pinned lookups
    let channels_by_id: HashMap<ChannelId, &GuildChannel> = guild_channels
        .iter()
        .map(|ch| (ch.id, ch))
        .collect();

    // Index categories by lowercase name
    let categories: HashMap<String, ChannelId> = guild_channels
        .iter()
        .filter(|ch| ch.kind == ChannelType::Category)
        .map(|ch| (ch.name.to_lowercase(), ch.id))
        .collect();

    // Index text channels by (parent_id, lowercase name)
    let text_channels: HashMap<(Option<ChannelId>, String), ChannelId> = guild_channels
        .iter()
        .filter(|ch| ch.kind == ChannelType::Text)
        .map(|ch| ((ch.parent_id, ch.name.to_lowercase()), ch.id))
        .collect();

    let mut resolved = HashMap::new();

    for group in groups {
        // Resolve category: prefer pinned ID, fall back to name match
        let category_id = if let Some(id) = group.id {
            if channels_by_id.contains_key(&id) {
                id
            } else {
                tracing::warn!(category = %group.name, id = %id, "pinned category ID not found in guild");
                continue;
            }
        } else {
            match categories.get(&group.name.to_lowercase()) {
                Some(&id) => id,
                None => continue, // category doesn't exist yet
            }
        };

        for channel_mapping in &group.channels {
            // Resolve channel: prefer pinned ID, fall back to name match
            let channel_id = if let Some(id) = channel_mapping.id {
                if channels_by_id.contains_key(&id) {
                    id
                } else {
                    tracing::warn!(channel = %channel_mapping.name, id = %id, "pinned channel ID not found in guild");
                    continue;
                }
            } else {
                let key = (Some(category_id), channel_mapping.name.to_lowercase());
                match text_channels.get(&key) {
                    Some(&id) => id,
                    None => continue, // channel doesn't exist yet
                }
            };

            for selector in &channel_mapping.talkgroups {
                for &tg_id in &selector.ids {
                    resolved.insert((selector.system.clone(), tg_id), channel_id);
                }
            }
        }
    }

    let resolved_count = resolved.len();
    if resolved_count > 0 {
        tracing::info!(
            talkgroups = resolved_count,
            "resolved existing channel mappings"
        );
    }

    *lookup.write().await = resolved;
    Ok(guild_channels)
}

/// Background task that creates missing categories and channels, updating the lookup as it goes.
pub async fn reconcile(
    discord: Arc<DiscordClient>,
    guild_id: GuildId,
    groups: Vec<ChannelGroup>,
    lookup: ChannelLookup,
    existing_channels: Vec<GuildChannel>,
) {
    // Build indexes from existing state
    let existing_by_id: HashMap<ChannelId, &GuildChannel> = existing_channels
        .iter()
        .map(|ch| (ch.id, ch))
        .collect();

    let mut categories: HashMap<String, ChannelId> = existing_channels
        .iter()
        .filter(|ch| ch.kind == ChannelType::Category)
        .map(|ch| (ch.name.to_lowercase(), ch.id))
        .collect();

    let mut text_channels: HashMap<(Option<ChannelId>, String), ChannelId> = existing_channels
        .iter()
        .filter(|ch| ch.kind == ChannelType::Text)
        .map(|ch| ((ch.parent_id, ch.name.to_lowercase()), ch.id))
        .collect();

    let total_channels: usize = groups.iter().map(|g| g.channels.len()).sum();
    let mut created = 0usize;

    for group in &groups {
        // Ensure category exists: pinned ID > name match > create
        let category_id = if let Some(id) = group.id {
            if existing_by_id.contains_key(&id) {
                id
            } else {
                tracing::warn!(category = %group.name, id = %id, "pinned category ID not found, skipping");
                continue;
            }
        } else {
            match categories.get(&group.name.to_lowercase()) {
                Some(&id) => id,
                None => {
                    match create_with_retry(&discord, guild_id, &group.name, None).await {
                        Some(ch) => {
                            let id = ch.id;
                            categories.insert(group.name.to_lowercase(), id);
                            tracing::info!(category = %group.name, "created Discord category");
                            id
                        }
                        None => continue,
                    }
                }
            }
        };

        // Ensure each channel exists: pinned ID > name match > create
        for channel_mapping in &group.channels {
            let channel_id = if let Some(id) = channel_mapping.id {
                if existing_by_id.contains_key(&id) {
                    id
                } else {
                    tracing::warn!(channel = %channel_mapping.name, id = %id, "pinned channel ID not found, skipping");
                    continue;
                }
            } else {
                let key = (Some(category_id), channel_mapping.name.to_lowercase());
                match text_channels.get(&key) {
                    Some(&id) => id,
                    None => {
                        match create_with_retry(
                            &discord,
                            guild_id,
                            &channel_mapping.name,
                            Some(category_id),
                        )
                        .await
                        {
                            Some(ch) => {
                                let id = ch.id;
                                text_channels.insert(key, id);
                                created += 1;
                                tracing::info!(
                                    channel = %channel_mapping.name,
                                    category = %group.name,
                                    progress = format!("{created}/{total_channels}"),
                                    "created Discord channel"
                                );
                                id
                            }
                            None => continue,
                        }
                    }
                }
            };

            // Update lookup for all talkgroups mapped to this channel
            let mut map = lookup.write().await;
            for selector in &channel_mapping.talkgroups {
                for &tg_id in &selector.ids {
                    map.insert((selector.system.clone(), tg_id), channel_id);
                }
            }
        }
    }

    tracing::info!(
        created,
        total = total_channels,
        "channel reconciliation complete"
    );
}

/// Create a category (if `parent` is None) or text channel (if `parent` is Some),
/// retrying on rate limits.
async fn create_with_retry(
    discord: &DiscordClient,
    guild_id: GuildId,
    name: &str,
    parent: Option<ChannelId>,
) -> Option<GuildChannel> {
    const MAX_RETRIES: u32 = 5;

    for attempt in 0..MAX_RETRIES {
        let result = match parent {
            None => discord.create_category(guild_id, name).await,
            Some(category_id) => {
                discord
                    .create_text_channel(guild_id, name, category_id)
                    .await
            }
        };

        match result {
            Ok(channel) => {
                // Pace ourselves to avoid rate limits
                sleep(Duration::from_secs(2)).await;
                return Some(channel);
            }
            Err(e) => {
                // Check for rate limit (HTTP 429)
                let err_str = e.to_string();
                if err_str.contains("429") || err_str.to_lowercase().contains("rate limit") {
                    let backoff = Duration::from_secs(5 * 2u64.pow(attempt));
                    tracing::warn!(
                        name,
                        attempt,
                        backoff_secs = backoff.as_secs(),
                        "rate limited creating channel, backing off"
                    );
                    sleep(backoff).await;
                } else {
                    tracing::error!(name, error = %e, "failed to create channel");
                    return None;
                }
            }
        }
    }

    tracing::error!(name, "exhausted retries creating channel");
    None
}
