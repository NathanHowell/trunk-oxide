use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use serenity::all::{
    AutoArchiveDuration, ChannelType, CreateAttachment, CreateChannel, CreateEmbed, CreateMessage,
    CreateThread, EditMessage, GuildChannel, ReactionType,
};
use serenity::http::Http;
use serenity::model::id::{ChannelId, GuildId, MessageId};

/// Hard cap on any single Discord HTTP call. Discord rate-limits message edits
/// per-channel and serenity sleeps on 429s with no upper bound, so a hot thread
/// can otherwise wedge a call handler for minutes (observed: `edit_embed` tail
/// in the 250–500s range, with global request rate only ~2/s — the bottleneck
/// is the per-channel edit bucket, not the global limit). Bounding each call
/// lets the handler abandon a stuck edit and keep going, so finalization
/// (grey→green + audio) can never be held hostage by the edit bucket.
const DISCORD_API_TIMEOUT: Duration = Duration::from_secs(45);

/// Emoji used as the in-flight transcription indicator. A reaction lives in a
/// different per-channel rate-limit bucket than message edits, so toggling it
/// does not compete with (or stall behind) the embed edits.
pub const TRANSCRIBING_EMOJI: &str = "⏳";

/// Run a single Discord HTTP call under [`DISCORD_API_TIMEOUT`], turning a
/// timeout into an error the caller can log and move past.
async fn with_timeout<T>(op: &str, fut: impl Future<Output = serenity::Result<T>>) -> Result<T> {
    match tokio::time::timeout(DISCORD_API_TIMEOUT, fut).await {
        Ok(res) => Ok(res?),
        Err(_) => Err(anyhow::anyhow!(
            "discord {op} timed out after {}s",
            DISCORD_API_TIMEOUT.as_secs()
        )),
    }
}

pub struct DiscordClient {
    http: Http,
}

impl DiscordClient {
    pub fn new(config: &trunk_config::DiscordConfig) -> Self {
        Self {
            http: Http::new(&config.bot_token),
        }
    }

    pub async fn create_call_thread(&self, channel_id: ChannelId, name: &str) -> Result<ChannelId> {
        let thread = with_timeout(
            "create_thread",
            channel_id.create_thread(
                &self.http,
                CreateThread::new(name)
                    .kind(ChannelType::PublicThread)
                    .auto_archive_duration(AutoArchiveDuration::OneDay),
            ),
        )
        .await?;
        Ok(thread.id)
    }

    pub async fn send_message(&self, channel_id: ChannelId, text: &str) -> Result<MessageId> {
        let msg = with_timeout(
            "send_message",
            channel_id.send_message(&self.http, CreateMessage::new().content(text)),
        )
        .await?;
        Ok(msg.id)
    }

    pub async fn send_embed(&self, channel_id: ChannelId, embed: CreateEmbed) -> Result<MessageId> {
        let msg = with_timeout(
            "send_embed",
            channel_id.send_message(&self.http, CreateMessage::new().embed(embed)),
        )
        .await?;
        Ok(msg.id)
    }

    pub async fn edit_embed(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        embed: CreateEmbed,
    ) -> Result<()> {
        with_timeout(
            "edit_embed",
            channel_id.edit_message(&self.http, message_id, EditMessage::new().embed(embed)),
        )
        .await?;
        Ok(())
    }

    pub async fn edit_embed_with_attachment(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        embed: CreateEmbed,
        file_data: Vec<u8>,
        filename: &str,
    ) -> Result<()> {
        let attachment = CreateAttachment::bytes(file_data, filename.to_owned());
        with_timeout(
            "edit_embed_with_attachment",
            channel_id.edit_message(
                &self.http,
                message_id,
                EditMessage::new().embed(embed).new_attachment(attachment),
            ),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<()> {
        with_timeout(
            "delete_message",
            channel_id.delete_message(&self.http, message_id),
        )
        .await?;
        Ok(())
    }

    /// Add the bot's own reaction to a message (in-flight transcription marker).
    pub async fn add_reaction(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: &str,
    ) -> Result<()> {
        let reaction = ReactionType::Unicode(emoji.to_owned());
        with_timeout(
            "add_reaction",
            self.http.create_reaction(channel_id, message_id, &reaction),
        )
        .await
    }

    /// Remove the bot's own reaction from a message.
    pub async fn remove_reaction(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: &str,
    ) -> Result<()> {
        let reaction = ReactionType::Unicode(emoji.to_owned());
        with_timeout(
            "remove_reaction",
            self.http
                .delete_reaction_me(channel_id, message_id, &reaction),
        )
        .await
    }

    pub async fn get_guild_channels(&self, guild_id: GuildId) -> Result<Vec<GuildChannel>> {
        let channels = with_timeout("get_guild_channels", guild_id.channels(&self.http)).await?;
        Ok(channels.into_values().collect())
    }

    pub async fn create_category(&self, guild_id: GuildId, name: &str) -> Result<GuildChannel> {
        let channel = with_timeout(
            "create_category",
            guild_id.create_channel(
                &self.http,
                CreateChannel::new(name).kind(ChannelType::Category),
            ),
        )
        .await?;
        Ok(channel)
    }

    pub async fn create_text_channel(
        &self,
        guild_id: GuildId,
        name: &str,
        category_id: ChannelId,
    ) -> Result<GuildChannel> {
        let channel = with_timeout(
            "create_text_channel",
            guild_id.create_channel(
                &self.http,
                CreateChannel::new(name)
                    .kind(ChannelType::Text)
                    .category(category_id),
            ),
        )
        .await?;
        Ok(channel)
    }
}
